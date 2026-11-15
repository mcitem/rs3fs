use crate::services::{
    settings::write_settings,
    store::{Entry, Store, TransferCancelled, TransferSlots, UploadTransfer},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod config;
mod local;
mod plan;

pub(crate) use config::{config, load_jobs, read_config, save_jobs};

use config::read;
use local::{LocalTree, is_office_temp_file, local_file, safe_target, scan_local, valid_component};
use plan::{Action, Pair, RemoteFile, decide};

#[derive(Clone, Default)]
pub(crate) struct SyncProgress {
    pub(crate) stage: &'static str,
    pub(crate) current: String,
    pub(crate) processed: usize,
    pub(crate) total: usize,
    pub(crate) local_files: usize,
    pub(crate) remote_files: usize,
    pub(crate) uploaded: usize,
    pub(crate) downloaded: usize,
    pub(crate) local_deleted: usize,
    pub(crate) remote_deleted: usize,
    pub(crate) directories_up: usize,
    pub(crate) directories_down: usize,
    pub(crate) matched: usize,
    pub(crate) conflicts: usize,
}

impl SyncProgress {
    pub(crate) fn summary(&self) -> String {
        format!(
            "上传 {}，下载 {}，目录上传 {}，目录下载 {}，本地删除 {}，远端删除 {}，已核对相同文件 {}，冲突 {}",
            self.uploaded,
            self.downloaded,
            self.directories_up,
            self.directories_down,
            self.local_deleted,
            self.remote_deleted,
            self.matched,
            self.conflicts,
        )
    }
}

#[derive(Debug)]
pub(crate) struct SyncConflict(String);

impl std::fmt::Display for SyncConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SyncConflict {}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Direction {
    Upload,
    Download,
    Both,
}

impl Direction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Upload => "仅上传",
            Self::Download => "仅下载",
            Self::Both => "双向",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ConflictPolicy {
    #[default]
    PreserveBoth,
    PreferLocal,
    PreferRemote,
}

impl ConflictPolicy {
    pub(crate) const ALL: [Self; 3] = [Self::PreserveBoth, Self::PreferLocal, Self::PreferRemote];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PreserveBoth => "保留两端",
            Self::PreferLocal => "本地优先",
            Self::PreferRemote => "S3 优先",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::PreserveBoth => "遇到同路径文件冲突时保留两端并报告冲突",
            Self::PreferLocal => "遇到同路径文件冲突时用本地版本覆盖 S3",
            Self::PreferRemote => "遇到同路径文件冲突时用 S3 版本覆盖本地",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SyncJob {
    pub(crate) id: Uuid,
    pub(crate) profile: Uuid,
    pub(crate) bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bucket_region: Option<String>,
    pub(crate) prefix: String,
    pub(crate) folder: PathBuf,
    pub(crate) direction: Direction,
    #[serde(default)]
    pub(crate) conflict_policy: ConflictPolicy,
    #[serde(default)]
    pub(crate) automatic: bool,
    #[serde(default)]
    pub(crate) propagate_deletes: bool,
}

pub(crate) async fn run(
    store: Store,
    job: SyncJob,
    resolution: Option<ConflictPolicy>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: impl FnMut(&SyncProgress),
) -> Result<String> {
    ensure!(
        Store::sync_key_allowed(&job.prefix),
        "同步前缀与回收站路径冲突或过长。"
    );
    let snapshot = config(&format!("sync-{}.json", job.id))?;
    run_with_progress(store, job, snapshot, resolution, slots, cancel, report).await
}

#[cfg(test)]
async fn run_with_snapshot(store: Store, job: SyncJob, snapshot: PathBuf) -> Result<String> {
    run_with_progress(
        store,
        job,
        snapshot,
        None,
        &TransferSlots::new(4),
        &CancellationToken::new(),
        |_| {},
    )
    .await
}

async fn run_with_progress(
    store: Store,
    job: SyncJob,
    snapshot: PathBuf,
    resolution: Option<ConflictPolicy>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    mut report: impl FnMut(&SyncProgress),
) -> Result<String> {
    ensure!(!cancel.is_cancelled(), TransferCancelled);
    let mut progress = SyncProgress {
        stage: "扫描本地目录",
        ..Default::default()
    };
    report(&progress);
    let mut previous: BTreeMap<String, Pair> = read(&snapshot)?;
    let root = job.folder.clone();
    let LocalTree {
        files: local,
        directories: local_dirs,
    } = tokio::task::spawn_blocking(move || scan_local(&root)).await??;
    progress.local_files = local.len();
    progress.stage = "列出远端对象";
    report(&progress);
    let mut remote_dirs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut remote = BTreeMap::new();
    for object in store.list(&job.prefix, false).await? {
        let key = object
            .key()
            .strip_prefix(&job.prefix)
            .context("服务器返回了同步前缀之外的对象")?;
        if key.is_empty() {
            continue;
        }
        let marker = key.ends_with('/');
        if !marker && key.rsplit('/').next().is_some_and(is_office_temp_file) {
            continue;
        }
        let key = key.strip_suffix('/').unwrap_or(key);
        ensure!(
            key.split('/').all(valid_component),
            "对象键无法映射为本地路径：{key}"
        );
        for (index, _) in key.match_indices('/') {
            remote_dirs.insert(key[..index].to_owned());
        }
        if marker {
            ensure!(object.size() == 0, "目录标记包含数据：{key}/");
            remote_dirs.insert(key.to_owned());
            markers.insert(key.to_owned());
            continue;
        }
        remote.insert(
            key.to_owned(),
            RemoteFile {
                bytes: object.size(),
                etag: object.etag().context("S3 未返回对象 ETag")?.to_owned(),
            },
        );
    }
    let keys = local
        .keys()
        .chain(remote.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    progress.remote_files = remote.len();
    progress.total = keys.len();
    progress.stage = "检查同步路径";
    report(&progress);
    let mut aliases = BTreeMap::new();
    for key in local_dirs.iter().chain(&remote_dirs).chain(&keys) {
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
    if job.direction != Direction::Download {
        let nonempty = local_dirs
            .iter()
            .chain(local.keys())
            .filter_map(|path| path.rsplit_once('/').map(|(parent, _)| parent))
            .collect::<BTreeSet<_>>();
        for dir in local_dirs
            .difference(&remote_dirs)
            .filter(|dir| !nonempty.contains(dir.as_str()))
        {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            let key = format!("{}{dir}/", job.prefix);
            ensure!(Store::sync_key_allowed(&key), "无效的同步对象键：{key}");
            progress.stage = "创建远端目录";
            progress.current.clone_from(dir);
            report(&progress);
            store.mkdir(key).await?;
            progress.directories_up += 1;
        }
    }
    if job.direction != Direction::Upload {
        for dir in markers.difference(&local_dirs) {
            ensure!(!cancel.is_cancelled(), TransferCancelled);
            progress.stage = "创建本地目录";
            progress.current.clone_from(dir);
            report(&progress);
            safe_target(&job.folder, dir, true)?;
            progress.directories_down += 1;
        }
    }
    let mut conflicts = Vec::new();
    progress.stage = "比对文件";
    progress.current.clear();
    report(&progress);
    for key in keys {
        ensure!(!cancel.is_cancelled(), TransferCancelled);
        let current_local = local.get(&key);
        let current_remote = remote.get(&key);
        let object = format!("{}{key}", job.prefix);
        ensure!(
            Store::sync_key_allowed(&object),
            "无效的同步对象键：{object}"
        );
        let action = decide(
            job.direction,
            job.propagate_deletes,
            previous.get(&key),
            current_local,
            current_remote,
        );
        let action = match resolution {
            Some(policy) => {
                policy.resolve_once(action, current_local.is_some(), current_remote.is_some())
            }
            None => job.conflict_policy.resolve(
                job.direction,
                action,
                current_local.is_some() && current_remote.is_some(),
            ),
        };
        progress.stage = match action {
            Action::Upload => "上传文件",
            Action::Download => "下载文件",
            Action::DeleteLocal => "删除本地文件",
            Action::DeleteRemote => "删除远端文件",
            Action::Conflict => "核对同名文件",
            Action::Keep => "比对文件",
        };
        progress.current.clone_from(&key);
        report(&progress);
        match action {
            Action::Upload => {
                let path = safe_target(&job.folder, &key, false)?;
                let file = current_local.context("本地文件已不存在")?;
                ensure!(
                    &local_file(&path)? == file,
                    "同步期间本地文件发生变化：{key}"
                );
                let staging = tempfile::NamedTempFile::new()?;
                let source = path.clone();
                let staged = staging.path().to_owned();
                let actual = tokio::task::spawn_blocking(move || {
                    std::fs::copy(source, &staged)?;
                    local_file(&staged)
                })
                .await??;
                ensure!(&actual == file, "上传前本地文件发生变化：{key}");
                let etag = store
                    .transfer_file(
                        &object,
                        staging.path(),
                        file.bytes,
                        current_remote.map(|remote| remote.etag.as_str()),
                        UploadTransfer {
                            slots,
                            cancel,
                            sent: Arc::new(AtomicU64::new(0)),
                            on_start: &|| {},
                        },
                    )
                    .await?
                    .context("S3 写入响应缺少 ETag，无法记录同步状态")?;
                previous.insert(
                    key,
                    Pair {
                        local: file.clone(),
                        remote: RemoteFile {
                            bytes: i64::try_from(file.bytes)?,
                            etag,
                        },
                    },
                );
                write_settings(&snapshot, &previous)?;
                progress.uploaded += 1;
            }
            Action::Download => {
                let path = safe_target(&job.folder, &key, false)?;
                let file = current_remote.context("远端文件已不存在")?;
                let staging = tempfile::tempdir_in(path.parent().context("缺少目标父目录")?)?;
                let staged = staging.path().join("download");
                store
                    .download_with_progress(
                        &Entry::file(object, file.bytes, file.etag.clone()),
                        &staged,
                        &AtomicU64::new(0),
                        slots,
                        cancel,
                    )
                    .await?;
                let downloaded = local_file(&staged)?;
                ensure!(
                    i64::try_from(downloaded.bytes)? == file.bytes,
                    "下载大小与清单不一致：{key}"
                );
                if let Some(existing) = current_local {
                    ensure!(
                        &local_file(&path)? == existing,
                        "下载期间本地文件发生变化：{key}"
                    );
                    std::fs::rename(&staged, &path)?;
                } else {
                    std::fs::hard_link(&staged, &path)?;
                }
                previous.insert(
                    key,
                    Pair {
                        local: downloaded,
                        remote: file.clone(),
                    },
                );
                write_settings(&snapshot, &previous)?;
                progress.downloaded += 1;
            }
            Action::DeleteLocal => {
                let path = safe_target(&job.folder, &key, false)?;
                ensure!(
                    Some(&local_file(&path)?) == current_local,
                    "删除前本地文件发生变化：{key}"
                );
                std::fs::remove_file(&path)?;
                previous.remove(&key);
                write_settings(&snapshot, &previous)?;
                progress.local_deleted += 1;
            }
            Action::DeleteRemote => {
                let file = current_remote.context("远端文件已不存在")?;
                store.delete_if_unchanged(&object, &file.etag).await?;
                previous.remove(&key);
                write_settings(&snapshot, &previous)?;
                progress.remote_deleted += 1;
            }
            Action::Conflict => {
                if let (Some(local), Some(remote)) = (current_local, current_remote)
                    && u64::try_from(remote.bytes) == Ok(local.bytes)
                {
                    let staging = tempfile::tempdir()?;
                    let staged = staging.path().join("remote");
                    store
                        .download_with_progress(
                            &Entry::file(object, remote.bytes, remote.etag.clone()),
                            &staged,
                            &AtomicU64::new(0),
                            slots,
                            cancel,
                        )
                        .await
                        .with_context(|| format!("无法核对冲突文件：{key}"))?;
                    let actual = tokio::task::spawn_blocking(move || local_file(&staged)).await??;
                    if actual == *local {
                        previous.insert(
                            key,
                            Pair {
                                local: local.clone(),
                                remote: remote.clone(),
                            },
                        );
                        write_settings(&snapshot, &previous)?;
                        progress.matched += 1;
                        progress.processed += 1;
                        continue;
                    }
                }
                progress.conflicts += 1;
                if conflicts.len() < 12 {
                    let reason = match (current_local, current_remote) {
                        (None, _) => "本地缺失",
                        (_, None) => "S3 缺失",
                        _ => "两端版本不同",
                    };
                    conflicts.push(format!("{key}（{reason}）"));
                }
            }
            Action::Keep => {}
        }
        progress.processed += 1;
    }
    progress.stage = "保存同步状态";
    progress.current.clear();
    report(&progress);
    let previous_len = previous.len();
    previous.retain(|key, _| local.contains_key(key) || remote.contains_key(key));
    if previous.len() != previous_len {
        write_settings(&snapshot, &previous)?;
    }
    let summary = format!("同步完成：{}", progress.summary());
    if conflicts.is_empty() {
        Ok(summary)
    } else {
        Err(SyncConflict(format!(
            "{summary}。已保留冲突文件：{}",
            conflicts.join("、")
        ))
        .into())
    }
}

#[cfg(test)]
mod tests;
