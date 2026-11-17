use super::{
    ConflictChoice, Direction, SyncJob, SyncProgress, baseline, config,
    config::Scope,
    local::{
        LocalFile, LocalTree, is_sync_temp_file, local_file, scan_local_excluding, valid_component,
    },
};
use crate::services::store::{Entry, Store, TransferCancelled, TransferSlots};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RemoteFile {
    pub(super) bytes: i64,
    pub(super) etag: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pair {
    pub(super) local: LocalFile,
    pub(super) remote: RemoteFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Action {
    Upload,
    Download,
    Keep,
    Busy,
    Retry,
    Match,
    CreateLocalDirectory,
    CreateRemoteDirectory,
    Conflict,
}

pub(super) fn decide(
    direction: Direction,
    previous: Option<&Pair>,
    local: Option<&LocalFile>,
    remote: Option<&RemoteFile>,
) -> Action {
    match (local, remote) {
        (Some(local), Some(remote))
            if previous.is_some_and(|pair| &pair.local == local && &pair.remote == remote) =>
        {
            Action::Keep
        }
        (Some(_), _) if direction == Direction::Upload => Action::Upload,
        (_, Some(_)) if direction == Direction::Download => Action::Download,
        (Some(_), Some(_)) => Action::Conflict,
        (Some(_), None) if direction == Direction::Both => Action::Upload,
        (None, Some(_)) if direction == Direction::Both => Action::Download,
        _ => Action::Keep,
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct SyncPlan {
    pub(super) job: SyncJob,
    pub(super) reviewed_uploads: bool,
    pub(super) previous: BTreeMap<String, Pair>,
    pub(super) scope: Option<Scope>,
    pub(super) items: Vec<PlanItem>,
    pub(super) absent: Vec<String>,
    pub(super) local_files: usize,
    pub(super) remote_files: usize,
    retry_paths: Option<Vec<String>>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanItem {
    pub(super) path: String,
    pub(super) action: Action,
    pub(super) local: Option<LocalFile>,
    pub(super) remote: Option<RemoteFile>,
    pub(super) reason: String,
    #[serde(default)]
    modified: [Option<chrono::DateTime<chrono::Utc>>; 2],
}

impl PlanItem {
    fn held(path: String, local: Option<&LocalFile>, remote: Option<&RemoteFile>) -> Self {
        let reason = match (local, remote) {
            (None, None) => "两端文件均已移除",
            (None, Some(_)) => "本地缺失",
            (Some(_), None) => "S3 缺失",
            (Some(_), Some(_)) => "两端文件待确认",
        };
        Self {
            path,
            action: Action::Conflict,
            local: local.cloned(),
            remote: remote.cloned(),
            reason: reason.into(),
            modified: [None; 2],
        }
    }

    pub(crate) fn modified(&self, choice: ConflictChoice) -> Option<chrono::DateTime<chrono::Utc>> {
        match choice {
            ConflictChoice::Local => self.modified[0],
            ConflictChoice::Remote => self.modified[1],
            ConflictChoice::AcknowledgeMissing => None,
        }
    }

    pub(crate) fn remote_entry(&self, job: &SyncJob) -> Option<Entry> {
        self.remote.as_ref().map(|file| {
            Entry::file(
                format!("{}{}", job.prefix, self.path),
                file.bytes,
                file.etag.clone(),
            )
        })
    }

    pub(crate) fn copy_local_version(&self, job: &SyncJob, destination: &Path) -> Result<()> {
        let expected = self.local.as_ref().context("本地版本已不存在")?;
        let metadata =
            super::local::metadata_at(&job.folder, &self.path)?.context("本地版本已不存在")?;
        ensure!(
            metadata.is_file() && !super::local::is_redirect(&metadata),
            "本地版本已变化"
        );
        std::fs::copy(job.folder.join(&self.path), destination)?;
        ensure!(
            &local_file(destination)? == expected,
            "本地版本已变化，请刷新冲突后重试。"
        );
        Ok(())
    }

    pub(crate) fn is_conflict(&self) -> bool {
        self.action == Action::Conflict
    }

    pub(crate) fn both_missing(&self) -> bool {
        self.is_conflict() && self.local.is_none() && self.remote.is_none()
    }

    pub(crate) fn same_versions(&self, current: &Self) -> bool {
        current.is_conflict()
            && self.path == current.path
            && self.local == current.local
            && self.remote == current.remote
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn label(&self) -> &'static str {
        match self.action {
            Action::Upload if self.remote.is_some() => "覆盖 S3 文件",
            Action::Download if self.local.is_some() => "覆盖本地文件",
            Action::Upload => "上传",
            Action::Download => "下载",
            Action::CreateLocalDirectory => "创建本地目录",
            Action::CreateRemoteDirectory => "创建 S3 目录",
            Action::Conflict => "冲突",
            Action::Match => "记录相同文件",
            Action::Busy => "文件占用，待重试",
            Action::Retry => "待重试",
            Action::Keep => unreachable!(),
        }
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }

    pub(super) fn action_with(
        &self,
        choice: Option<ConflictChoice>,
        direction: Direction,
    ) -> Action {
        match choice.filter(|choice| self.accepts(*choice, direction)) {
            Some(ConflictChoice::Local) => Action::Upload,
            Some(ConflictChoice::Remote) => Action::Download,
            _ => self.action,
        }
    }

    pub(crate) fn keeps_recovery_copy(&self) -> bool {
        match self.action {
            Action::Upload => self.remote.is_some(),
            Action::Download => self.local.is_some(),
            _ => false,
        }
    }

    pub(crate) fn accepts(&self, choice: ConflictChoice, direction: Direction) -> bool {
        direction == Direction::Both
            && self.is_conflict()
            && match choice {
                ConflictChoice::Local => self.local.is_some(),
                ConflictChoice::Remote => self.remote.is_some(),
                ConflictChoice::AcknowledgeMissing => self.both_missing(),
            }
    }
}

impl SyncPlan {
    pub(crate) fn require_reviewed_uploads(&mut self) {
        self.reviewed_uploads = true;
    }

    pub(crate) fn prepare_conflict_choices(
        &mut self,
        selected: &BTreeMap<String, (PlanItem, ConflictChoice)>,
    ) -> BTreeMap<String, ConflictChoice> {
        let direction = self.job.direction;
        let mut accepted = BTreeMap::new();
        self.items.retain(|current| {
            let Some((shown, choice)) = selected.get(current.path()) else {
                return false;
            };
            if !shown.same_versions(current) || !current.accepts(*choice, direction) {
                return false;
            }
            accepted.insert(current.path().to_owned(), *choice);
            if *choice == ConflictChoice::AcknowledgeMissing {
                self.absent.push(current.path().to_owned());
                return false;
            }
            true
        });
        accepted
    }

    pub(crate) fn retry_paths(&self) -> Option<&[String]> {
        self.retry_paths.as_deref()
    }

    pub(crate) fn items(&self) -> &[PlanItem] {
        &self.items
    }

    pub(crate) fn verify(&self, current: &Self) -> Result<()> {
        ensure!(self == current, "两端文件或同步设置已变化，请重新预览。");
        Ok(())
    }
}

pub(crate) async fn scan(
    store: &Store,
    job: SyncJob,
    connection: &crate::services::settings::ConnectionSettings,
    paths: Option<&[String]>,
    held: &BTreeSet<String>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<SyncPlan> {
    let snapshot = config(&format!("sync-{}.json", job.id))?;
    let scope = Scope::new(&job, connection)?;
    scope.verify(&snapshot)?;
    let mut plan =
        build_plan_inner(store, job, &snapshot, paths, held, slots, cancel, report).await?;
    plan.scope = Some(scope);
    Ok(plan)
}

#[cfg(test)]
pub(super) async fn build_plan(
    store: &Store,
    job: SyncJob,
    snapshot: &std::path::Path,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<SyncPlan> {
    build_plan_with_paths(store, job, snapshot, None, slots, cancel, report).await
}

#[cfg(test)]
pub(super) async fn build_plan_with_paths(
    store: &Store,
    job: SyncJob,
    snapshot: &Path,
    paths: Option<&[String]>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<SyncPlan> {
    build_plan_inner(
        store,
        job,
        snapshot,
        paths,
        &BTreeSet::new(),
        slots,
        cancel,
        report,
    )
    .await
}

async fn build_plan_inner(
    store: &Store,
    job: SyncJob,
    snapshot: &Path,
    paths: Option<&[String]>,
    held: &BTreeSet<String>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<SyncPlan> {
    ensure!(
        Store::sync_key_allowed(&job.prefix),
        "同步前缀与回收站路径冲突或过长。"
    );
    ensure!(
        job.prefix.is_empty() || job.prefix.ends_with('/'),
        "同步前缀必须指向文件夹。"
    );
    ensure!(!cancel.is_cancelled(), TransferCancelled);
    let mut progress = SyncProgress {
        stage: if paths.is_some() {
            "检查上次待重试路径"
        } else {
            "扫描本地与 S3"
        },
        ..Default::default()
    };
    report(&progress);
    let root = job.folder.clone();
    let recovery = super::recovery::path(snapshot)?;
    let excluded = super::local::recovery_relative(&root, &recovery)?;
    let scan_cancel = cancel.child_token();
    let _cancel_scan = scan_cancel.clone().drop_guard();
    let remote_exclusion = excluded.clone();
    let scan_exclusion = excluded.clone();
    let snapshot = snapshot.to_owned();
    let parallelism = super::file_parallelism(slots);
    let (previous, tree, objects, retry_errors) = if let Some(paths) = paths {
        let (tree, objects, errors) =
            retry_inventory(store, &job, paths, excluded.as_deref(), cancel).await?;
        (baseline::load(&snapshot)?, tree, objects, errors)
    } else {
        let local_scan = async {
            tokio::task::spawn_blocking(move || {
                let previous = baseline::load(&snapshot)?;
                let local =
                    scan_local_excluding(&root, scan_exclusion.as_deref(), Some(&scan_cancel))?;
                Ok::<_, anyhow::Error>((previous, local))
            })
            .await?
        };
        let remote_scan = async {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(TransferCancelled.into()),
                result = store.list_sync_tree(&job.prefix, parallelism) => result,
            }
        };
        let ((previous, tree), objects) = tokio::try_join!(local_scan, remote_scan)?;
        (previous, tree, objects, Vec::new())
    };
    let LocalTree {
        files: local,
        busy,
        unreadable,
        directories: local_dirs,
    } = tree;
    progress.local_files = local.len() + busy.len() + unreadable.len();
    let mut remote_dirs = BTreeSet::new();
    let mut markers = BTreeMap::new();
    let mut remote = BTreeMap::new();
    let mut remote_modified = BTreeMap::new();
    for object in objects {
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let key = object
            .key()
            .strip_prefix(&job.prefix)
            .context("服务器返回了同步前缀之外的对象")?;
        if key.is_empty()
            || remote_exclusion
                .as_deref()
                .is_some_and(|path| super::local::under_reserved(Path::new(key), path))
        {
            continue;
        }
        let marker = key.ends_with('/');
        if !marker && key.rsplit('/').next().is_some_and(is_sync_temp_file) {
            continue;
        }
        let key = key.strip_suffix('/').unwrap_or(key);
        ensure!(
            key.split('/').all(valid_component),
            "对象键无法映射为本地路径：{key}"
        );
        for (index, _) in key.rmatch_indices('/') {
            let directory = &key[..index];
            if remote_dirs.contains(directory) {
                // Every ancestor of a previously encountered directory is already indexed.
                break;
            }
            remote_dirs.insert(directory.to_owned());
        }
        if marker {
            ensure!(object.size() == 0, "目录标记包含数据：{key}/");
            remote_dirs.insert(key.to_owned());
            markers.insert(
                key.to_owned(),
                RemoteFile {
                    bytes: object.size(),
                    etag: object.etag().context("S3 目录标记缺少 ETag")?.to_owned(),
                },
            );
            continue;
        }
        if let Some(modified) = object.modified() {
            remote_modified.insert(key.to_owned(), modified);
        }
        remote.insert(
            key.to_owned(),
            RemoteFile {
                bytes: object.size(),
                etag: object.etag().context("S3 未返回对象 ETag")?.to_owned(),
            },
        );
    }
    let held_folded = held
        .iter()
        .map(|path| path.to_lowercase())
        .collect::<BTreeSet<_>>();
    let keys = local
        .keys()
        .chain(remote.keys())
        .chain(busy.iter())
        .chain(unreadable.keys())
        .chain(
            held.iter()
                .filter(|path| paths.is_none_or(|paths| paths.binary_search(path).is_ok())),
        )
        .cloned()
        .collect::<BTreeSet<_>>();
    progress.remote_files = remote.len();
    progress.total = keys.len();
    progress.stage = "检查同步路径";
    report(&progress);
    let mut aliases = BTreeMap::new();
    for key in local_dirs.iter().chain(&remote_dirs).chain(&keys) {
        if overlaps_held(key, &held_folded) {
            continue;
        }
        let is_dir = local_dirs.contains(key) || remote_dirs.contains(key);
        ensure!(
            !(is_dir && keys.contains(key)),
            "同一路径同时存在文件和目录：{key}"
        );
        let folded = key.to_lowercase();
        if let Some(existing) = aliases.insert(folded, key) {
            ensure!(
                existing == key,
                "存在仅大小写不同的同步路径：{existing}、{key}"
            );
        }
    }
    let failed_paths = retry_errors
        .iter()
        .map(|item| item.path.clone())
        .collect::<BTreeSet<_>>();
    let mut items = retry_errors;
    if job.direction != Direction::Download {
        let nonempty = local_dirs
            .iter()
            .chain(local.keys())
            .chain(busy.iter())
            .chain(unreadable.keys())
            .filter_map(|path| path.rsplit_once('/').map(|(parent, _)| parent))
            .collect::<BTreeSet<_>>();
        for dir in local_dirs
            .difference(&remote_dirs)
            .filter(|dir| !nonempty.contains(dir.as_str()))
        {
            let key = format!("{}{dir}/", job.prefix);
            ensure!(Store::sync_key_allowed(&key), "无效的同步对象键：{key}");
            items.push(PlanItem {
                path: dir.clone(),
                action: Action::CreateRemoteDirectory,
                local: None,
                remote: None,
                reason: "本地空目录".into(),
                modified: [None; 2],
            });
        }
    }
    if job.direction != Direction::Upload {
        for (dir, marker) in markers.iter().filter(|(dir, _)| !local_dirs.contains(*dir)) {
            items.push(PlanItem {
                path: dir.clone(),
                action: Action::CreateLocalDirectory,
                local: None,
                remote: Some(marker.clone()),
                reason: "S3 目录标记".into(),
                modified: [None; 2],
            });
        }
    }
    progress.stage = "生成同步计划";
    report(&progress);
    let directory_items = items.len();
    let job_ref = &job;
    let local_ref = &local;
    let remote_ref = &remote;
    let previous_ref = &previous;
    let busy_ref = &busy;
    let unreadable_ref = &unreadable;
    let held_folded_ref = &held_folded;
    let failed_paths_ref = &failed_paths;
    let local_dirs_ref = &local_dirs;
    let remote_dirs_ref = &remote_dirs;
    let mut file_plans = stream::iter(keys)
        .map(|key| async move {
            let (job, local, remote, previous, busy) =
                (job_ref, local_ref, remote_ref, previous_ref, busy_ref);
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let current_local = local.get(&key);
            let current_remote = remote.get(&key);
            let object = format!("{}{key}", job.prefix);
            ensure!(
                Store::sync_key_allowed(&object),
                "无效的同步对象键：{object}"
            );
            if busy.contains(&key) {
                return Ok::<_, anyhow::Error>((
                    key.clone(),
                    Some(PlanItem {
                        path: key,
                        action: Action::Busy,
                        local: None,
                        remote: current_remote.cloned(),
                        reason: "本地文件暂时无法读取".into(),
                        modified: [None; 2],
                    }),
                ));
            }
            if let Some(error) = unreadable_ref.get(&key) {
                return Ok((
                    key.clone(),
                    Some(PlanItem {
                        path: key,
                        action: Action::Retry,
                        local: None,
                        remote: current_remote.cloned(),
                        reason: error.clone(),
                        modified: [None; 2],
                    }),
                ));
            }
            if held.contains(&key) {
                if local_dirs_ref.contains(&key)
                    || remote_dirs_ref.contains(&key)
                    || failed_paths_ref.contains(&key)
                {
                    return Ok((key, None));
                }
                return Ok((
                    key.clone(),
                    Some(PlanItem::held(key, current_local, current_remote)),
                ));
            }
            if overlaps_held(&key, held_folded_ref) {
                return Ok((key, None));
            }
            let baseline = previous.get(&key);
            let mut action = decide(job.direction, baseline, current_local, current_remote);
            // Upload jobs can overwrite directly when the saved pair no longer matches.
            if job.direction != Direction::Upload
                && action != Action::Keep
                && let (Some(local), Some(remote)) = (current_local, current_remote)
                && u64::try_from(remote.bytes) == Ok(local.bytes)
            {
                let compared = store
                    .matches_contents(
                        &Entry::file(object, remote.bytes, remote.etag.clone()),
                        &local.digest,
                        slots,
                        cancel,
                    )
                    .await;
                match compared {
                    Ok(true) => action = Action::Match,
                    Ok(false) => {}
                    Err(error) if error.is::<TransferCancelled>() => return Err(error),
                    Err(error) => {
                        return Ok((
                            key.clone(),
                            Some(PlanItem {
                                path: key,
                                action: Action::Retry,
                                local: Some(local.clone()),
                                remote: Some(remote.clone()),
                                reason: format!("无法核对同名文件：{error:#}"),
                                modified: [None; 2],
                            }),
                        ));
                    }
                }
            }
            let reason = match (action, current_local, current_remote, baseline) {
                (Action::Match, ..) => "两端内容相同",
                (Action::Conflict, ..) => "两端内容不同",
                (Action::Upload, _, None, _) => "本地新增",
                (Action::Download, None, _, _) => "S3 新增",
                (Action::Upload, ..) => "用本地版本覆盖 S3",
                (Action::Download, ..) => "用 S3 版本覆盖本地",
                _ => "",
            };
            let item = (action != Action::Keep).then(|| PlanItem {
                path: key.clone(),
                action,
                local: current_local.cloned(),
                remote: current_remote.cloned(),
                reason: reason.into(),
                modified: [None; 2],
            });
            Ok((key, item))
        })
        .buffer_unordered(parallelism);
    while let Some((key, item)) = file_plans.try_next().await? {
        items.extend(item);
        progress.current = key;
        progress.processed += 1;
        if progress.processed % 32 == 0 || progress.processed == progress.total {
            report(&progress);
        }
    }
    drop(file_plans);
    items[directory_items..].sort_unstable_by(|a, b| a.path.cmp(&b.path));
    items.retain(|item| {
        item.is_conflict()
            || !overlaps_held(&item.path, &held_folded)
            || matches!(item.action, Action::Busy | Action::Retry)
    });
    let absent = previous
        .keys()
        .filter(|key| !overlaps_held(key, &held_folded))
        .filter(|key| paths.is_none_or(|paths| paths.binary_search(key).is_ok()))
        .filter(|key| {
            !local.contains_key(*key)
                && !remote.contains_key(*key)
                && !busy.contains(*key)
                && !unreadable.contains_key(*key)
        })
        .cloned()
        .collect();
    if items.iter().any(PlanItem::is_conflict) {
        let root = job.folder.clone();
        items = tokio::task::spawn_blocking(move || {
            for item in items.iter_mut().filter(|item| item.is_conflict()) {
                item.modified[0] = item.local.as_ref().and_then(|_| {
                    super::local::metadata_at(&root, &item.path)
                        .ok()
                        .flatten()
                        .and_then(|metadata| metadata.modified().ok())
                        .map(chrono::DateTime::from)
                });
                item.modified[1] = remote_modified.get(&item.path).copied();
            }
            items
        })
        .await?;
    }
    Ok(SyncPlan {
        job,
        reviewed_uploads: false,
        previous,
        scope: None,
        items,
        absent,
        local_files: local.len() + busy.len() + unreadable.len(),
        remote_files: remote.len(),
        retry_paths: paths.map(<[String]>::to_vec),
    })
}

fn overlaps_held(path: &str, held: &BTreeSet<String>) -> bool {
    if held.is_empty() {
        return false;
    }
    let path = path.trim_end_matches('/').to_lowercase();
    let prefix = format!("{path}/");
    held.contains(&path)
        || path
            .match_indices('/')
            .any(|(index, _)| held.contains(&path[..index]))
        || held
            .range(prefix.clone()..)
            .next()
            .is_some_and(|key| key.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_blocks_related_paths_but_not_siblings() {
        let held = BTreeSet::from(["docs/file.txt".to_owned()]);
        for path in ["docs", "docs/", "Docs/File.txt", "docs/file.txt/child"] {
            assert!(overlaps_held(path, &held), "{path}");
        }
        for path in ["docs/file.txt.bak", "docs/other.txt", "other/docs/file.txt"] {
            assert!(!overlaps_held(path, &held), "{path}");
        }
    }

    #[test]
    fn hold_survives_a_deletion_and_requires_current_versions_for_resolution() {
        let local = LocalFile {
            bytes: 4,
            digest: [0; 16],
        };
        let remote = RemoteFile {
            bytes: 9,
            etag: "old".into(),
        };
        let held = PlanItem::held("file".into(), Some(&local), Some(&remote));
        let missing_remote = PlanItem::held("file".into(), Some(&local), None);
        let missing_both = PlanItem::held("file".into(), None, None);
        let saved: PlanItem = serde_json::from_slice(&serde_json::to_vec(&held).unwrap()).unwrap();
        assert!(saved.same_versions(&held));
        assert!(!saved.same_versions(&missing_remote));
        assert_eq!(
            missing_remote.action_with(None, Direction::Both),
            Action::Conflict
        );
        assert_eq!(
            missing_remote.action_with(Some(ConflictChoice::Local), Direction::Both),
            Action::Upload
        );
        assert!(missing_both.both_missing());
    }

    #[tokio::test]
    async fn held_file_is_skipped_while_a_sibling_uploads() -> Result<()> {
        use crate::services::store::Server;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let root = tempfile::tempdir()?;
        let folder = root.path().join("local");
        std::fs::create_dir(&folder)?;
        std::fs::write(folder.join("held.txt"), "local version")?;
        std::fs::write(folder.join("ready.txt"), "new file")?;
        let snapshot = root.path().join("baseline.json");
        let job = SyncJob {
            id: uuid::Uuid::new_v4(),
            profile: uuid::Uuid::new_v4(),
            bucket: "fixture".into(),
            bucket_region: None,
            prefix: String::new(),
            folder,
            direction: Direction::Both,
            automatic: true,
            interval_minutes: 5,
            watch_local: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = Server::connect(
            "us-east-1".into(),
            endpoint,
            String::new(),
            Some(aws_sdk_s3::config::Credentials::new(
                "fixture", "fixture", None, None, "test",
            )),
        )
        .await?;
        let store = server.open(job.bucket.clone(), None);
        let fixture = async {
            for (request, body, headers) in [
                (
                    "GET /fixture/?list-type=2",
                    "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
                    "",
                ),
                ("PUT /fixture/ready.txt", "", "ETag: \"uploaded\"\r\n"),
            ] {
                let (stream, _) = listener.accept().await?;
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await?;
                ensure!(line.starts_with(request), "意外的 S3 请求：{line}");
                let mut length = 0;
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await? == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse()?;
                    }
                }
                if request.starts_with("PUT ") {
                    assert_eq!(length, b"new file".len());
                    let mut received = vec![0; length];
                    stream.read_exact(&mut received).await?;
                    assert_eq!(received, b"new file");
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
                    body.len()
                );
                stream.get_mut().write_all(response.as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let sync = async {
            let slots = TransferSlots::new(1);
            let cancel = CancellationToken::new();
            let held = BTreeSet::from(["held.txt".to_owned()]);
            let plan = build_plan_inner(
                &store,
                job.clone(),
                &snapshot,
                None,
                &held,
                &slots,
                &cancel,
                &mut |_| {},
            )
            .await?;
            ensure!(plan.items().iter().any(|item| item.path() == "held.txt"
                && item.action == Action::Conflict
                && item.remote.is_none()));
            let error = super::super::execute_plan(
                &store,
                plan,
                &snapshot,
                &BTreeMap::new(),
                &slots,
                &cancel,
                &mut |_| {},
            )
            .await
            .expect_err("held file must remain a conflict");
            ensure!(error.is::<super::super::SyncConflict>(), "{error:#}");
            let checkpoint = baseline::load(&snapshot)?;
            assert!(checkpoint.contains_key("ready.txt"));
            assert!(!checkpoint.contains_key("held.txt"));
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(fixture, sync)
        })
        .await
        .context("等待双向同步测试完成")??;

        std::fs::write(root.path().join("local/ready.txt"), "before second scan")?;
        let listing = async {
            let (stream, _) = listener.accept().await?;
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await?;
            ensure!(line.starts_with("GET /fixture/?list-type=2"), "{line}");
            loop {
                line.clear();
                if stream.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
            }
            let body = "<ListBucketResult><Contents><Key>held.txt</Key><Size>13</Size><ETag>\"remote\"</ETag></Contents></ListBucketResult>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.get_mut().write_all(response.as_bytes()).await?;
            drop(stream);

            let (stream, _) = listener.accept().await?;
            let mut stream = BufReader::new(stream);
            line.clear();
            stream.read_line(&mut line).await?;
            ensure!(line.starts_with("PUT /fixture/ready.txt"), "{line}");
            let mut length = 0;
            loop {
                line.clear();
                if stream.read_line(&mut line).await? == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse()?;
                }
            }
            let mut received = vec![0; length];
            stream.read_exact(&mut received).await?;
            assert_eq!(received, b"updated after scanning");
            std::fs::write(root.path().join("local/ready.txt"), "updated during upload")?;
            stream.get_mut().write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\nETag: \"fresh\"\r\n\r\n",
            ).await?;
            Ok::<_, anyhow::Error>(())
        };
        let upload = async {
            let slots = TransferSlots::new(1);
            let cancel = CancellationToken::new();
            let mut plan = build_plan(
                &store,
                SyncJob {
                    direction: Direction::Upload,
                    ..job
                },
                &snapshot,
                &slots,
                &cancel,
                &mut |_| {},
            )
            .await?;
            ensure!(plan.items().iter().any(|item| item.path() == "held.txt"
                && item.action == Action::Upload
                && item.remote.is_some()));
            plan.items.retain(|item| item.path() == "ready.txt");
            let path = root.path().join("local/ready.txt");
            std::fs::write(&path, "updated after scanning")?;
            let expected = local_file(&path)?;
            super::super::execute_plan(
                &store,
                plan,
                &snapshot,
                &BTreeMap::new(),
                &slots,
                &cancel,
                &mut |_| {},
            )
            .await?;
            assert!(baseline::load(&snapshot)?["ready.txt"].local == expected);
            assert!(local_file(&path)? != expected);
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(listing, upload)
        })
        .await
        .context("等待仅上传测试完成")??;
        Ok(())
    }
}

async fn retry_inventory(
    store: &Store,
    job: &SyncJob,
    paths: &[String],
    excluded: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<(LocalTree, Vec<Entry>, Vec<PlanItem>)> {
    let mut tree = LocalTree::default();
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let key = path.strip_suffix('/').unwrap_or(path);
        let directory = key != path;
        let object = format!("{}{path}", job.prefix);
        let result = async {
            ensure!(key.split('/').all(valid_component) && Store::sync_key_allowed(&object), "无效的重试路径");
            ensure!(!excluded.is_some_and(|exclude| super::local::under_reserved(Path::new(key), exclude)), "路径位于恢复目录");
            let root = job.folder.clone();
            let relative = key.to_owned();
            let local_cancel = cancel.clone();
            let local = tokio::task::spawn_blocking(move || {
                let Some(metadata) = super::local::metadata_at(&root, &relative)? else {
                    return Ok::<_, anyhow::Error>((false, None));
                };
                if directory {
                    ensure!(super::local::directory_exists(&root, &relative)?, "本地路径不再是普通目录");
                    Ok((true, None))
                } else {
                    ensure!(metadata.is_file(), "本地路径不再是普通文件");
                    Ok((false, Some(super::local::local_file_with_cancel(&root.join(relative), Some(&local_cancel))?)))
                }
            });
            let (local, remote) = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                result = async { tokio::try_join!(async { local.await? }, store.sync_retry_entry(&object)) } => result?,
            };
            Ok::<_, anyhow::Error>((local, remote))
        }.await;
        match result {
            Ok(((local_directory, local_file), remote)) => {
                if local_directory {
                    tree.directories.insert(key.to_owned());
                }
                if let Some(file) = local_file {
                    tree.files.insert(key.to_owned(), file);
                }
                entries.extend(remote);
            }
            Err(error) if error.is::<TransferCancelled>() => return Err(error),
            Err(error) => errors.push(PlanItem {
                path: path.clone(),
                action: Action::Retry,
                local: None,
                remote: None,
                reason: format!("无法检查待重试路径：{error:#}"),
                modified: [None; 2],
            }),
        }
    }
    Ok((tree, entries, errors))
}
