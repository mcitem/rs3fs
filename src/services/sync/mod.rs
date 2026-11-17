use crate::services::store::{Store, TransferCancelled, TransferSlots};
use anyhow::{Result, ensure};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod baseline;
mod config;
mod execution;
mod local;
mod plan;
mod recovery;

pub(super) const DOWNLOAD_STAGING_PREFIX: &str = ".rs3fs-sync-download-";

pub(super) fn is_download_staging(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(DOWNLOAD_STAGING_PREFIX)
    })
}

pub(crate) use config::{config, load_jobs, read_config, save_jobs};

#[cfg(test)]
use local::local_file;
use plan::Action;
#[cfg(test)]
use plan::{Pair, RemoteFile};

fn file_parallelism(slots: &TransferSlots) -> usize {
    slots
        .limit()
        .clamp(1, crate::services::settings::MAX_TRANSFER_PARALLELISM)
}
pub(crate) use plan::{PlanItem, SyncPlan, scan};

pub(crate) fn local_recovery_folder() -> Result<PathBuf> {
    config(recovery::DIRECTORY)
}

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
    pub(crate) directories_up: usize,
    pub(crate) directories_down: usize,
    pub(crate) matched: usize,
    pub(crate) conflicts: usize,
    pub(crate) busy: usize,
    pub(crate) retry_files: usize,
}

impl SyncProgress {
    pub(crate) fn summary(&self) -> String {
        format!(
            "上传 {} 个文件，下载 {} 个文件，目录上传 {} 个，目录下载 {} 个\n已核对相同文件 {}\n冲突 {}，文件占用 {}，其他待重试 {}",
            self.uploaded,
            self.downloaded,
            self.directories_up,
            self.directories_down,
            self.matched,
            self.conflicts,
            self.busy,
            self.retry_files,
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

#[derive(Debug)]
pub(crate) struct SyncDeferred(String, Vec<String>);

impl SyncDeferred {
    pub(crate) fn paths(&self) -> &[String] {
        &self.1
    }
}

impl std::fmt::Display for SyncDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SyncDeferred {}

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
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Upload => "以本地为准。两端仍与上次同步的版本一致时跳过，否则上传本地版本。",
            Self::Download => "以 S3 为准，下载缺少的文件并覆盖内容不同的本地文件。",
            Self::Both => "补齐任一端缺少的文件。同路径文件内容不同时，等待选择保留的版本。",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictChoice {
    AcknowledgeMissing,
    Local,
    Remote,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub(crate) automatic: bool,
    #[serde(default = "default_interval_minutes")]
    pub(crate) interval_minutes: u32,
    #[serde(default)]
    pub(crate) watch_local: bool,
}

pub(crate) const fn default_interval_minutes() -> u32 {
    5
}

pub(crate) async fn execute(
    store: &Store,
    plan: SyncPlan,
    choices: &BTreeMap<String, ConflictChoice>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<String> {
    let snapshot = config(&format!("sync-{}.json", plan.job.id))?;
    execute_plan(store, plan, &snapshot, choices, slots, cancel, report).await
}

async fn execute_plan(
    store: &Store,
    plan: SyncPlan,
    snapshot: &std::path::Path,
    choices: &BTreeMap<String, ConflictChoice>,
    slots: &TransferSlots,
    cancel: &CancellationToken,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<String> {
    let SyncPlan {
        job,
        reviewed_uploads,
        previous,
        scope,
        items,
        absent,
        local_files,
        remote_files,
        ..
    } = plan;
    ensure!(!cancel.is_cancelled(), TransferCancelled);
    if let Some(scope) = scope {
        scope.bind(snapshot)?;
    }
    let mut progress = SyncProgress {
        total: items.len(),
        local_files,
        remote_files,
        ..Default::default()
    };
    let executor = execution::ItemExecutor {
        store,
        job: &job,
        snapshot,
        slots,
        cancel,
        reviewed_uploads,
    };
    let mut results = RunResults::default();
    let mut checkpoints = baseline::Checkpoints::new(snapshot, previous);
    let mut directories = Vec::new();
    let mut files = Vec::new();
    for item in items {
        let choice = choices.get(&item.path).copied();
        let action = item.action_with(choice, job.direction);
        match action {
            Action::CreateLocalDirectory | Action::CreateRemoteDirectory => {
                directories.push((item, action))
            }
            _ => files.push((item, action)),
        }
    }
    let outcome = async {
        for (items, parallelism) in [(directories, 1), (files, file_parallelism(slots))] {
            run_phase(
                &executor,
                items.into_iter(),
                parallelism,
                &mut checkpoints,
                &mut progress,
                &mut results,
                report,
            )
            .await?;
        }
        if progress.busy == 0 && progress.retry_files == 0 && !cancel.is_cancelled() {
            let mut absent = absent.into_iter();
            while absent.len() > 0 {
                for key in absent.by_ref().take(baseline::BATCH_SIZE) {
                    checkpoints.record(baseline::Change::Remove(key));
                }
                checkpoints.flush()?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if outcome.is_ok() {
        progress.stage = "保存同步状态";
        progress.current.clear();
        report(&progress);
    }
    tokio::task::spawn_blocking(move || checkpoints.finish()).await??;
    outcome?;
    ensure!(!cancel.is_cancelled(), TransferCancelled);
    let RunResults {
        conflicts,
        busy,
        retries,
        retry_paths,
        local_archives,
        remote_archives,
    } = results;
    let deferred = progress.busy > 0 || progress.retry_files > 0;
    let status = if deferred {
        "部分文件待重试"
    } else if progress.conflicts > 0 {
        "同步结束，有冲突待处理"
    } else {
        "同步完成"
    };
    let mut summary = format!("{status}\n{}", progress.summary());
    if local_archives > 0 {
        summary.push_str(&format!(
            "\n本地已保存 {local_archives} 份副本，位于 {}",
            recovery::path(snapshot)?.display()
        ));
    }
    if remote_archives > 0 {
        summary.push_str(&format!(
            "\nS3 已保存 {remote_archives} 份副本，可在存储桶回收站恢复"
        ));
    }
    if !conflicts.is_empty() {
        summary.push_str(&format!("\n已保留冲突文件：{}", conflicts.join("、")));
    }
    if !busy.is_empty() {
        summary.push_str(&format!(
            "\n以下文件占用，需在后续同步重试：{}",
            busy.join("、")
        ));
    }
    if !retries.is_empty() {
        summary.push_str(&format!(
            "\n{} 个路径需重新检查或重试（最多显示 12 项）：\n{}",
            progress.retry_files,
            retries.join("\n")
        ));
    }
    if deferred {
        Err(SyncDeferred(summary, retry_paths.into_iter().collect()).into())
    } else if !conflicts.is_empty() {
        Err(SyncConflict(summary).into())
    } else {
        Ok(summary)
    }
}

#[derive(Default)]
struct RunResults {
    conflicts: Vec<String>,
    busy: Vec<String>,
    retries: Vec<String>,
    retry_paths: std::collections::BTreeSet<String>,
    local_archives: usize,
    remote_archives: usize,
}

async fn run_phase(
    executor: &execution::ItemExecutor<'_>,
    mut items: std::vec::IntoIter<(PlanItem, Action)>,
    parallelism: usize,
    checkpoints: &mut baseline::Checkpoints,
    progress: &mut SyncProgress,
    results: &mut RunResults,
    report: &mut impl FnMut(&SyncProgress),
) -> Result<()> {
    let mut workers = FuturesUnordered::new();
    let mut failure = None;
    let mut pending = 0;
    let checkpoint_delay = std::time::Duration::from_millis(250);
    let mut checkpoint_at = tokio::time::Instant::now() + checkpoint_delay;
    loop {
        while workers.len() < parallelism && failure.is_none() && !executor.cancel.is_cancelled() {
            let Some((item, action)) = items.next() else {
                break;
            };
            progress.stage = execution::stage(action);
            progress.current.clone_from(&item.path);
            report(progress);
            workers.push(async move {
                let result = executor.execute(&item, action).await;
                (item, action, result)
            });
        }
        let completed = tokio::select! {
            completed = workers.next() => completed,
            _ = tokio::time::sleep_until(checkpoint_at), if pending > 0 => {
                if let Err(error) = checkpoints.flush() {
                    failure = Some((error, "保存同步状态", progress.current.clone()));
                }
                pending = 0;
                checkpoint_at = tokio::time::Instant::now() + checkpoint_delay;
                continue;
            }
        };
        let Some((item, action, result)) = completed else {
            break;
        };
        results.local_archives += result.local_archives;
        results.remote_archives += result.remote_archives;
        progress.current.clone_from(&item.path);
        progress.stage = execution::stage(action);
        match result.change {
            Ok(change) => {
                if let Some(change) = change {
                    checkpoints.record(change);
                    pending += 1;
                }
                match action {
                    Action::Upload => progress.uploaded += 1,
                    Action::Download => progress.downloaded += 1,
                    Action::CreateLocalDirectory => progress.directories_down += 1,
                    Action::CreateRemoteDirectory => progress.directories_up += 1,
                    Action::Match => progress.matched += 1,
                    Action::Busy => {
                        progress.busy += 1;
                        results.retry_paths.insert(item.path.clone());
                        if results.busy.len() < 12 {
                            results.busy.push(item.path);
                        }
                    }
                    Action::Retry => {
                        progress.retry_files += 1;
                        results.retry_paths.insert(item.path.clone());
                        if results.retries.len() < 12 {
                            results
                                .retries
                                .push(format!("{}：{}", item.path, item.reason));
                        }
                    }
                    Action::Conflict => {
                        progress.conflicts += 1;
                        if results.conflicts.len() < 12 {
                            results
                                .conflicts
                                .push(format!("{}（{}）", item.path, item.reason));
                        }
                    }
                    Action::Keep => unreachable!(),
                }
            }
            Err(error) if error.is::<TransferCancelled>() => {
                failure.get_or_insert((error, progress.stage, item.path));
            }
            Err(error) if local::is_busy(&error) => {
                progress.busy += 1;
                results.retry_paths.insert(retry_path(&item.path, action));
                if results.busy.len() < 12 {
                    results.busy.push(item.path);
                }
            }
            Err(error) => {
                progress.retry_files += 1;
                results.retry_paths.insert(retry_path(&item.path, action));
                if results.retries.len() < 12 {
                    results
                        .retries
                        .push(format!("{}（{}）：{error:#}", item.path, progress.stage));
                }
            }
        }
        progress.processed += 1;
        report(progress);
        if pending >= baseline::BATCH_SIZE {
            if let Err(error) = checkpoints.flush() {
                failure = Some((error, "保存同步状态", progress.current.clone()));
            }
            pending = 0;
            checkpoint_at = tokio::time::Instant::now() + checkpoint_delay;
        }
    }
    if let Some((error, stage, path)) = failure {
        progress.stage = stage;
        progress.current = path;
        report(progress);
        return Err(error);
    }
    checkpoints.flush()?;
    ensure!(!executor.cancel.is_cancelled(), TransferCancelled);
    Ok(())
}

fn retry_path(path: &str, action: Action) -> String {
    if matches!(
        action,
        Action::CreateLocalDirectory | Action::CreateRemoteDirectory
    ) {
        format!("{path}/")
    } else {
        path.to_owned()
    }
}

#[derive(Debug)]
struct ArchiveDeferred(anyhow::Error);

impl std::fmt::Display for ArchiveDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "旧版本归档未完成，已保留源对象：{:#}", self.0)
    }
}

impl std::error::Error for ArchiveDeferred {}

#[derive(Debug)]
struct SyncItemChanged;

impl std::fmt::Display for SyncItemChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("同步文件已变化")
    }
}

impl std::error::Error for SyncItemChanged {}

#[cfg(test)]
async fn run_with_snapshot(store: Store, job: SyncJob, snapshot: PathBuf) -> Result<String> {
    let slots = TransferSlots::new(4);
    let cancel = CancellationToken::new();
    let plan = plan::build_plan(&store, job, &snapshot, &slots, &cancel, &mut |_| {}).await?;
    execute_plan(
        &store,
        plan,
        &snapshot,
        &BTreeMap::new(),
        &slots,
        &cancel,
        &mut |_| {},
    )
    .await
}

#[cfg(test)]
mod tests;
