use crate::services::{
    scheduler::{Scheduler, Scope, WAITING_FOR_PATHS},
    settings::{ConnectionProfiles, ConnectionSettings, TransferSettings, write_settings},
    store::{Server, Store, TransferCancelled, TransferSlots},
    sync::{
        self, ConflictChoice, Direction, PlanItem, SyncConflict, SyncDeferred, SyncJob, SyncPlan,
        SyncProgress,
    },
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui_kit::Context;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WATCH_QUIET: Duration = Duration::from_secs(2);
const WATCH_MAX_DELAY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct DirectoryEvents {
    pending: Option<(Instant, Instant)>,
    restart: bool,
}

struct DirectoryWatch {
    _watcher: RecommendedWatcher,
    events: Arc<Mutex<DirectoryEvents>>,
}

impl DirectoryWatch {
    fn new(folder: &std::path::Path) -> notify::Result<Self> {
        let events = Arc::new(Mutex::new(DirectoryEvents::default()));
        let inbox = events.clone();
        let root = folder.to_owned();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            let mut inbox = inbox.lock().unwrap();
            match event {
                Ok(event)
                    if !event.kind.is_access()
                        && (event.paths.is_empty()
                            || event.paths.iter().any(|path| {
                                !sync::is_download_staging(path.strip_prefix(&root).unwrap_or(path))
                            })) =>
                {
                    let now = Instant::now();
                    inbox.pending.get_or_insert((now, now)).1 = now;
                    if (event.kind.is_remove() || event.kind.is_modify())
                        && event.paths.contains(&root)
                        && !root.is_dir()
                    {
                        inbox.restart = true;
                    }
                }
                Ok(_) => {}
                Err(_) => inbox.restart = true,
            }
        })?;
        watcher.watch(folder, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }

    fn due(&self) -> bool {
        self.events
            .lock()
            .unwrap()
            .pending
            .is_some_and(|(first, last)| {
                last.elapsed() >= WATCH_QUIET || first.elapsed() >= WATCH_MAX_DELAY
            })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Completed,
    Conflict,
    Failed,
    Cancelled,
    Paused,
    ReviewRequired,
    Deferred,
}

impl Outcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Completed => "已完成",
            Self::Conflict => "有冲突",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
            Self::Paused => "已暂停",
            Self::ReviewRequired => "暂停，待确认删除",
            Self::Deferred => "有文件待重试",
        }
    }

    pub(crate) fn has_issue(self) -> bool {
        matches!(
            self,
            Self::Conflict | Self::Failed | Self::ReviewRequired | Self::Deferred
        )
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SyncRun {
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
    automatic: bool,
    #[serde(default)]
    directory_change: bool,
    outcome: Outcome,
    message: String,
    #[serde(default)]
    retry_paths: Vec<String>,
    #[serde(default)]
    conflicts: BTreeMap<String, PlanItem>,
}

impl SyncRun {
    pub(crate) fn started(&self) -> DateTime<Utc> {
        self.started
    }
    pub(crate) fn finished(&self) -> DateTime<Utc> {
        self.finished
    }
    pub(crate) fn trigger_label(&self) -> &'static str {
        match (self.directory_change, self.automatic) {
            (true, _) => "本地目录变化",
            (false, true) => "自动触发",
            (false, false) => "手动触发",
        }
    }
    pub(crate) fn outcome(&self) -> Outcome {
        self.outcome
    }
    fn retry_targets(&self) -> Vec<String> {
        if !self.retry_paths.is_empty() {
            return self.retry_paths.clone();
        }
        if !matches!(self.outcome, Outcome::Deferred) {
            return Vec::new();
        }
        legacy_retry_targets(&self.message)
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

fn legacy_retry_targets(message: &str) -> Vec<String> {
    let Some((_, listed)) = message.split_once(" 个路径需重新检查或重试（最多显示 12 项）：\n")
    else {
        return Vec::new();
    };
    let mut paths = std::collections::BTreeSet::new();
    for line in listed.lines() {
        let Some((path, stage)) = line.split_once('（') else {
            continue;
        };
        let Some((stage, _)) = stage.split_once("）：") else {
            continue;
        };
        let directory = match stage {
            "创建本地目录" | "创建 S3 目录" => true,
            "上传文件" | "下载文件" | "删除本地文件" | "删除远端文件" | "记录相同文件" => {
                false
            }
            _ => continue,
        };
        if !path.is_empty() {
            paths.insert(if directory {
                format!("{path}/")
            } else {
                path.to_owned()
            });
        }
    }
    paths.into_iter().collect()
}

enum RunMode {
    Immediate,
    Preview,
    PreviewRetry(Vec<String>),
    Retry { paths: Vec<String>, automatic: bool },
    Automatic { directory_change: bool },
    Apply { reviewed: Arc<ReviewedPlan> },
    Resolve(BTreeMap<String, (PlanItem, ConflictChoice)>),
}

struct ReviewedPlan {
    plan: SyncPlan,
    connection: ConnectionSettings,
}

enum RunResult {
    Preview(ReviewedPlan),
    Completed { message: String, empty: bool },
}

struct ActiveRun {
    id: Uuid,
    started: DateTime<Utc>,
    automatic: bool,
    preview: bool,
    progress: Arc<Mutex<SyncProgress>>,
    cancel: CancellationToken,
    paused: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SyncPauses {
    all: bool,
    jobs: HashSet<Uuid>,
}

pub(crate) struct SyncService {
    runtime: Arc<tokio::runtime::Runtime>,
    scheduler: Scheduler,
    transfer_slots: TransferSlots,
    jobs: Vec<SyncJob>,
    active: Option<ActiveRun>,
    runs: HashMap<Uuid, SyncRun>,
    previews: HashMap<Uuid, std::result::Result<Arc<ReviewedPlan>, String>>,
    conflict_errors: HashMap<Uuid, String>,
    config_error: Option<String>,
    history_error: Option<String>,
    pauses: SyncPauses,
    pause_error: Option<String>,
    directory_watches: HashMap<Uuid, DirectoryWatch>,
    watch_errors: HashMap<Uuid, (Instant, String)>,
    quitting: bool,
}

impl SyncService {
    pub(crate) fn new(runtime: Arc<tokio::runtime::Runtime>, cx: &mut Context<Self>) -> Self {
        let (jobs, config_error) = match sync::load_jobs() {
            Ok(jobs) => (jobs, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };
        let (runs, history_error) = match sync::read_config("sync-runs.json") {
            Ok(runs) => (runs, None),
            Err(error) => (HashMap::new(), Some(format!("{error:#}"))),
        };
        let (pauses, pause_error) = match sync::read_config("sync-pauses.json") {
            Ok(pauses) => (pauses, None),
            Err(error) => (
                SyncPauses::default(),
                Some(format!("无法读取同步暂停状态：{error:#}")),
            ),
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            loop {
                if view
                    .update(cx, |service, cx| {
                        service.refresh_directory_watches(cx);
                        service.run_due(cx);
                        if service.active.is_some() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                executor.timer(Duration::from_secs(1)).await;
            }
        })
        .detach();
        Self {
            runtime,
            scheduler: Scheduler::default(),
            transfer_slots: TransferSlots::new(
                TransferSettings::load().unwrap_or_default().limit(),
            ),
            jobs,
            active: None,
            runs,
            previews: HashMap::new(),
            conflict_errors: HashMap::new(),
            config_error,
            history_error,
            pauses,
            pause_error,
            directory_watches: HashMap::new(),
            watch_errors: HashMap::new(),
            quitting: false,
        }
    }

    pub(crate) fn jobs(&self) -> &[SyncJob] {
        &self.jobs
    }
    pub(crate) fn scheduler(&self) -> Scheduler {
        self.scheduler.clone()
    }
    pub(crate) fn transfer_slots(&self) -> TransferSlots {
        self.transfer_slots.clone()
    }
    pub(crate) fn cancellation(&self) -> Option<&CancellationToken> {
        self.active.as_ref().map(|run| &run.cancel)
    }
    pub(crate) fn running(&self) -> Option<Uuid> {
        self.active.as_ref().map(|run| run.id)
    }
    pub(crate) fn executing(&self) -> Option<Uuid> {
        self.execution().map(|(id, _)| id)
    }

    pub(crate) fn execution(&self) -> Option<(Uuid, DateTime<Utc>)> {
        self.active
            .as_ref()
            .filter(|run| !run.preview)
            .map(|run| (run.id, run.started))
    }

    pub(crate) fn last_run(&self, id: Uuid) -> Option<&SyncRun> {
        self.runs.get(&id)
    }
    pub(crate) fn config_error(&self) -> Option<&str> {
        self.config_error.as_deref()
    }
    pub(crate) fn history_error(&self) -> Option<&str> {
        self.history_error.as_deref()
    }
    pub(crate) fn is_quitting(&self) -> bool {
        self.quitting
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.pauses.all
    }

    pub(crate) fn is_job_paused(&self, id: Uuid) -> bool {
        self.pauses.jobs.contains(&id)
    }

    pub(crate) fn pause_error(&self) -> Option<&str> {
        self.pause_error.as_deref()
    }

    pub(crate) fn set_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        let mut pauses = self.pauses.clone();
        pauses.all = paused;
        self.save_pauses(pauses, cx);
    }

    pub(crate) fn toggle_job_pause(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let mut pauses = self.pauses.clone();
        if !pauses.jobs.remove(&id) {
            pauses.jobs.insert(id);
        }
        self.save_pauses(pauses, cx);
    }

    fn save_pauses(&mut self, pauses: SyncPauses, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.pause_error = sync::config("sync-pauses.json")
            .and_then(|path| write_settings(&path, &pauses))
            .err()
            .map(|error| format!("无法保存同步暂停状态：{error:#}"));
        if self.pause_error.is_some() {
            cx.notify();
            return;
        }
        self.pauses = pauses;
        if let Some(run) = &mut self.active
            && (self.pauses.all || self.pauses.jobs.contains(&run.id))
            && !run.cancel.is_cancelled()
        {
            run.paused = true;
            run.cancel.cancel();
        }
        cx.notify();
    }

    pub(crate) fn task_status(&self, job: &SyncJob) -> String {
        if let Some(run) = self.active.as_ref().filter(|run| run.id == job.id) {
            if run.paused {
                return "正在暂停".into();
            }
            if run.cancel.is_cancelled() {
                return "正在取消".into();
            }
            let progress = run.progress.lock().unwrap();
            return if progress.total > 0 {
                format!(
                    "{} {}/{}",
                    progress.stage, progress.processed, progress.total
                )
            } else {
                progress.stage.into()
            };
        }
        if self
            .pending_preview(job.id)
            .is_some_and(|preview| preview.is_ok())
        {
            return "预览已生成".into();
        }
        if self.is_paused() || self.is_job_paused(job.id) {
            return "已暂停".into();
        }
        if let Some(run) = self.last_run(job.id)
            && !matches!(run.outcome(), Outcome::Completed | Outcome::Conflict)
        {
            return format!("上次{}", run.outcome().label());
        }
        if job.automatic {
            "等待自动同步".into()
        } else if job.watch_local {
            if self.directory_watch_error(job.id).is_some() {
                "目录监听失败".into()
            } else {
                "等待本地目录变化".into()
            }
        } else {
            "手动同步".into()
        }
    }

    pub(crate) fn progress(&self, id: Uuid) -> Option<(DateTime<Utc>, bool, SyncProgress)> {
        let run = self.active.as_ref().filter(|run| run.id == id)?;
        Some((
            run.started,
            run.automatic,
            run.progress.lock().unwrap().clone(),
        ))
    }

    pub(crate) fn next_run(&self, id: Uuid) -> Option<DateTime<Utc>> {
        let job = self.jobs.iter().find(|job| job.id == id && job.automatic)?;
        self.last_run(id)
            .filter(|run| !matches!(run.outcome, Outcome::Paused))
            .and_then(|run| {
                run.finished
                    .checked_add_signed(chrono::TimeDelta::minutes(i64::from(
                        job.interval_minutes.max(1),
                    )))
            })
    }

    pub(crate) fn directory_watch_error(&self, id: Uuid) -> Option<&str> {
        self.watch_errors.get(&id).map(|(_, error)| error.as_str())
    }

    fn refresh_directory_watches(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        for job in self.jobs.iter().filter(|job| job.watch_local) {
            let restarting = self
                .directory_watches
                .get(&job.id)
                .is_some_and(|watch| watch.events.lock().unwrap().restart);
            if restarting {
                self.directory_watches.remove(&job.id);
            }
            if self.directory_watches.contains_key(&job.id)
                || self
                    .watch_errors
                    .get(&job.id)
                    .is_some_and(|(attempt, _)| attempt.elapsed() < Duration::from_secs(10))
            {
                continue;
            }
            match DirectoryWatch::new(&job.folder) {
                Ok(watch) => {
                    if self.watch_errors.remove(&job.id).is_some() || restarting {
                        let now = Instant::now();
                        watch.events.lock().unwrap().pending = Some((now, now));
                    }
                    self.directory_watches.insert(job.id, watch);
                    cx.notify();
                }
                Err(error) => {
                    let message = format!("无法监听本地目录：{error}");
                    let old = self
                        .watch_errors
                        .insert(job.id, (Instant::now(), message.clone()));
                    if old.is_none_or(|(_, old)| old != message) {
                        cx.notify();
                    }
                }
            }
        }
    }

    pub(crate) fn save_jobs(&mut self, jobs: Vec<SyncJob>, cx: &mut Context<Self>) -> Result<()> {
        if let Some(active) = &self.active {
            anyhow::ensure!(
                jobs.iter().find(|job| job.id == active.id)
                    == self.jobs.iter().find(|job| job.id == active.id),
                "请等当前同步任务结束后再修改该任务。"
            );
        }
        sync::save_jobs(&jobs)?;
        let keep_watch = |id: &Uuid| {
            jobs.iter()
                .find(|job| &job.id == id && job.watch_local)
                .is_some_and(|job| {
                    self.jobs
                        .iter()
                        .any(|old| old.id == job.id && old.folder == job.folder)
                })
        };
        self.directory_watches.retain(|id, _| keep_watch(id));
        self.watch_errors.retain(|id, _| keep_watch(id));
        self.previews.retain(|id, _| {
            let old = self.jobs.iter().find(|job| job.id == *id);
            old.is_some() && old == jobs.iter().find(|job| job.id == *id)
        });
        self.jobs = jobs;
        self.conflict_errors
            .retain(|id, _| self.jobs.iter().any(|job| job.id == *id));
        self.refresh_directory_watches(cx);
        self.config_error = None;
        let previous = self.runs.len();
        self.runs
            .retain(|id, _| self.jobs.iter().any(|job| job.id == *id));
        if previous != self.runs.len() {
            self.save_history();
        }
        cx.notify();
        Ok(())
    }

    fn save_history(&mut self) {
        self.history_error = sync::config("sync-runs.json")
            .and_then(|path| write_settings(&path, &self.runs))
            .err()
            .map(|error| format!("无法保存同步结果：{error:#}"));
    }

    fn run_due(&mut self, cx: &mut Context<Self>) {
        if self.quitting || self.is_paused() || self.active.is_some() {
            return;
        }
        let now = Utc::now();
        let Some(job) = self
            .jobs
            .iter()
            .filter(|job| {
                !self.is_job_paused(job.id)
                    && !self
                        .previews
                        .get(&job.id)
                        .is_some_and(|preview| preview.is_ok())
                    && (self.timed_sync_due(job, now)
                        || self
                            .directory_watches
                            .get(&job.id)
                            .is_some_and(DirectoryWatch::due))
            })
            .min_by_key(|job| self.last_run(job.id).map(SyncRun::finished))
        else {
            return;
        };
        let timed = self.timed_sync_due(job, now);
        let mode = if timed && !job.automatic {
            let paths = self
                .last_run(job.id)
                .map(|run| run.retry_paths.clone())
                .unwrap_or_default();
            RunMode::Retry {
                paths,
                automatic: true,
            }
        } else {
            RunMode::Automatic {
                directory_change: !timed,
            }
        };
        self.start(job.id, mode, cx);
    }

    fn timed_sync_due(&self, job: &SyncJob, now: DateTime<Utc>) -> bool {
        if job.automatic {
            return self.next_run(job.id).is_none_or(|due| due <= now);
        }
        job.watch_local
            && self.last_run(job.id).is_some_and(|run| {
                matches!(run.outcome, Outcome::Deferred)
                    && !run.retry_paths.is_empty()
                    && (now - run.finished)
                        >= chrono::TimeDelta::minutes(i64::from(sync::default_interval_minutes()))
            })
    }

    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) {
        self.quitting = true;
        if self.active.is_none() {
            cx.quit();
        } else {
            cx.notify();
        }
    }

    pub(crate) fn conflicts(&self, id: Uuid) -> Option<&BTreeMap<String, PlanItem>> {
        self.jobs
            .iter()
            .find(|job| job.id == id && job.direction == Direction::Both)?;
        self.last_run(id).map(|run| &run.conflicts)
    }

    pub(crate) fn conflict_error(&self, id: Uuid) -> Option<&str> {
        self.conflict_errors.get(&id).map(String::as_str)
    }

    pub(crate) fn run_now(&mut self, id: Uuid, cx: &mut Context<Self>) -> bool {
        !self.pending_preview(id).is_some_and(|plan| plan.is_ok())
            && self.start(id, RunMode::Immediate, cx)
    }

    pub(crate) fn resolve_conflicts(
        &mut self,
        id: Uuid,
        choices: BTreeMap<String, (PlanItem, ConflictChoice)>,
        cx: &mut Context<Self>,
    ) -> bool {
        !choices.is_empty()
            && self.conflicts(id).is_some()
            && !self.pending_preview(id).is_some_and(|plan| plan.is_ok())
            && self.start(id, RunMode::Resolve(choices), cx)
    }

    pub(crate) fn pending_preview(&self, id: Uuid) -> Option<std::result::Result<&SyncPlan, &str>> {
        self.previews.get(&id).map(|result| {
            result
                .as_ref()
                .map(|reviewed| &reviewed.plan)
                .map_err(String::as_str)
        })
    }

    pub(crate) fn preview(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.previews.contains_key(&id) {
            self.start(id, RunMode::Preview, cx);
        }
    }

    pub(crate) fn is_previewing(&self, id: Uuid) -> bool {
        self.active
            .as_ref()
            .is_some_and(|run| run.id == id && run.preview)
    }

    pub(crate) fn has_retry_targets(&self, id: Uuid) -> bool {
        self.last_run(id)
            .is_some_and(|run| !run.retry_paths.is_empty() || !run.retry_targets().is_empty())
    }

    pub(crate) fn retry_blocked_reason(&self, id: Uuid) -> Option<&'static str> {
        if self.is_quitting() {
            Some("应用正在退出")
        } else if self.is_paused() || self.is_job_paused(id) {
            Some("恢复同步后可重试")
        } else if self.active.is_some() {
            Some("当前同步结束后可重试")
        } else if self
            .pending_preview(id)
            .is_some_and(|preview| preview.is_ok())
        {
            Some("请先执行或丢弃预览")
        } else {
            None
        }
    }

    pub(crate) fn retry(&mut self, id: Uuid, cx: &mut Context<Self>) -> bool {
        if self.retry_blocked_reason(id).is_some() {
            return false;
        }
        let Some(run) = self.last_run(id) else {
            return false;
        };
        let paths = run.retry_targets();
        if paths.is_empty() {
            return false;
        }
        let mode = if run.retry_paths.is_empty() {
            RunMode::PreviewRetry(paths)
        } else {
            RunMode::Retry {
                paths,
                automatic: false,
            }
        };
        self.start(id, mode, cx)
    }

    pub(crate) fn discard_preview(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(run) = self
            .active
            .as_ref()
            .filter(|run| run.id == id && run.preview)
        {
            run.cancel.cancel();
        }
        self.previews.remove(&id);
        cx.notify();
    }

    pub(crate) fn confirm_preview(&mut self, id: Uuid, cx: &mut Context<Self>) -> bool {
        let Some(Ok(reviewed)) = self.previews.get(&id) else {
            return false;
        };
        self.start(
            id,
            RunMode::Apply {
                reviewed: reviewed.clone(),
            },
            cx,
        )
    }

    fn start(&mut self, id: Uuid, mode: RunMode, cx: &mut Context<Self>) -> bool {
        if self.quitting || self.is_paused() || self.is_job_paused(id) || self.active.is_some() {
            return false;
        }
        let Some(job) = self.jobs.iter().find(|job| job.id == id).cloned() else {
            return false;
        };
        let automatic = matches!(
            mode,
            RunMode::Automatic { .. }
                | RunMode::Retry {
                    automatic: true,
                    ..
                }
        );
        let directory_change = matches!(
            mode,
            RunMode::Automatic {
                directory_change: true
            }
        );
        let preview = matches!(mode, RunMode::Preview | RunMode::PreviewRetry(_));
        let targeted_paths = match &mode {
            RunMode::Resolve(choices) => Some(choices.keys().cloned().collect()),
            RunMode::Retry { paths, .. } | RunMode::PreviewRetry(paths) => Some(paths.clone()),
            RunMode::Apply { reviewed, .. } => reviewed.plan.retry_paths().map(<[String]>::to_vec),
            _ => None,
        };
        let resolving = matches!(mode, RunMode::Resolve(_));
        let retry_on_failure = targeted_paths.clone().unwrap_or_default();
        if resolving {
            self.conflict_errors.remove(&id);
        }
        if !preview
            && targeted_paths.is_none()
            && let Some(watch) = self.directory_watches.get(&id)
        {
            watch.events.lock().unwrap().pending = None;
        }
        self.previews.remove(&id);
        let started = Utc::now();
        let progress = Arc::new(Mutex::new(SyncProgress {
            stage: "连接存储桶",
            ..Default::default()
        }));
        let cancel = CancellationToken::new();
        self.active = Some(ActiveRun {
            id,
            started,
            automatic,
            preview,
            progress: progress.clone(),
            cancel: cancel.clone(),
            paused: false,
        });
        let scheduler = self.scheduler();
        let slots = self.transfer_slots();
        let worker_progress = progress.clone();
        let worker_cancel = cancel.clone();
        let mut conflicts = self.conflicts(id).cloned().unwrap_or_default();
        let held = conflicts.keys().cloned().collect::<BTreeSet<_>>();
        let scanned_conflicts = Arc::new(Mutex::new(Vec::new()));
        let worker_conflicts = scanned_conflicts.clone();
        let resolution = Arc::new(Mutex::new(None));
        let worker_resolution = resolution.clone();
        let task = self.runtime.spawn(async move {
            let cancel = worker_cancel;
            let profiles = ConnectionProfiles::load(&ConnectionSettings::path()?)?;
            let settings = &profiles
                .get(job.profile)
                .context("任务所用的连接凭证已删除")?
                .settings;
            if let RunMode::Apply { reviewed, .. } = &mode {
                anyhow::ensure!(
                    &reviewed.connection == settings,
                    "连接配置已变化，请重新预览。"
                );
            }
            let store = tokio::select! {
                _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                result = open_sync_store(&job, settings, &slots) => result?,
            };
            worker_progress.lock().unwrap().stage = WAITING_FOR_PATHS;
            let scopes = vec![
                Scope::remote(
                    store.endpoint(),
                    store.bucket(),
                    &job.prefix,
                    !preview && job.direction != Direction::Download,
                ),
                Scope::local(&job.folder, !preview && job.direction != Direction::Upload),
            ];
            let _reservation = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                reservation = scheduler.acquire(scopes) => reservation,
            };
            let mut report =
                |state: &SyncProgress| worker_progress.lock().unwrap().clone_from(state);
            let planning = Instant::now();
            let mut plan = sync::scan(
                &store,
                job,
                settings,
                targeted_paths.as_deref(),
                &held,
                &slots,
                &cancel,
                &mut report,
            )
            .await?;
            let plan_time = planning.elapsed().as_secs_f64();
            let empty = plan.items().is_empty();
            if !preview {
                worker_conflicts.lock().unwrap().extend(
                    plan.items()
                        .iter()
                        .filter(|item| item.is_conflict())
                        .cloned(),
                );
            }
            let choices = match mode {
                RunMode::Preview | RunMode::PreviewRetry(_) => {
                    return Ok(RunResult::Preview(ReviewedPlan {
                        plan,
                        connection: settings.clone(),
                    }));
                }
                RunMode::Immediate | RunMode::Automatic { .. } | RunMode::Retry { .. } => {
                    BTreeMap::new()
                }
                RunMode::Apply { reviewed } => {
                    reviewed.plan.verify(&plan)?;
                    plan.require_reviewed_uploads();
                    BTreeMap::new()
                }
                RunMode::Resolve(selected) => {
                    let accepted = plan.prepare_conflict_choices(&selected);
                    let stale = selected.len() - accepted.len();
                    *worker_resolution.lock().unwrap() = Some((accepted.clone(), stale));
                    accepted
                }
            };
            let execution = Instant::now();
            sync::execute(&store, plan, &choices, &slots, &cancel, &mut report)
                .await
                .map(|summary| RunResult::Completed {
                    message: format!(
                        "{summary}\n扫描及核对 {plan_time:.2} 秒，执行及保存 {:.2} 秒",
                        execution.elapsed().as_secs_f64()
                    ),
                    empty,
                })
        });
        cx.spawn(async move |view, cx| {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let mut pending = None;
            let mut retry_paths = Vec::new();
            conflicts.extend(
                scanned_conflicts
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(|item| (item.path().to_owned(), item)),
            );
            let empty_directory_probe =
                directory_change && matches!(&result, Ok(RunResult::Completed { empty: true, .. }));
            let (outcome, message) = match result {
                Ok(RunResult::Completed { message, .. }) => (Outcome::Completed, message),
                Ok(RunResult::Preview(plan)) => {
                    pending = Some(Ok(Arc::new(plan)));
                    (Outcome::Completed, String::new())
                }
                Err(error) if error.is::<SyncDeferred>() => {
                    retry_paths = error
                        .downcast_ref::<SyncDeferred>()
                        .unwrap()
                        .paths()
                        .to_vec();
                    (Outcome::Deferred, error.to_string())
                }
                Err(error) if error.is::<SyncConflict>() => (Outcome::Conflict, error.to_string()),
                Err(error) if error.is::<TransferCancelled>() => {
                    retry_paths = retry_on_failure.clone();
                    (Outcome::Cancelled, "同步已取消".into())
                }
                Err(error) => {
                    retry_paths = retry_on_failure.clone();
                    let progress = progress.lock().unwrap();
                    let item = if progress.current.is_empty() {
                        String::new()
                    } else {
                        format!("「{}」", progress.current)
                    };
                    (
                        Outcome::Failed,
                        format!(
                            "{}{item}失败：{error:#}\n已检查 {}/{} 个文件。{}",
                            progress.stage,
                            progress.processed,
                            progress.total,
                            progress.summary(),
                        ),
                    )
                }
            };
            let _ = view.update(cx, |service, cx| {
                let paused = service.active.take().is_some_and(|run| run.paused);
                if preview && !cancel.is_cancelled() && pending.is_none() {
                    pending = Some(Err(message.clone()));
                }
                if let Some(preview_result) = pending.filter(|_| !preview || !cancel.is_cancelled())
                {
                    service.previews.insert(id, preview_result);
                }
                let keep_last_result = empty_directory_probe
                    && service
                        .last_run(id)
                        .is_some_and(|run| matches!(run.outcome(), Outcome::Completed));
                if resolving {
                    let completed = matches!(outcome, Outcome::Completed);
                    let deferred = matches!(outcome, Outcome::Deferred);
                    let report = resolution.lock().unwrap().take();
                    let mut stale = 0;
                    if let Some((accepted, stale_items)) = report {
                        stale = stale_items;
                        for (path, choice) in accepted {
                            if completed
                                || (deferred
                                    && choice != ConflictChoice::AcknowledgeMissing
                                    && retry_paths.binary_search(&path).is_err())
                            {
                                conflicts.remove(&path);
                            }
                        }
                    }
                    let mut error = if completed { String::new() } else { message };
                    if stale > 0 {
                        if !error.is_empty() {
                            error.push('\n');
                        }
                        error.push_str(&format!(
                            "{stale} 项版本已变化或暂时无法读取，请核对后重新选择。"
                        ));
                    }
                    if !error.is_empty() {
                        service.conflict_errors.insert(id, error);
                    }
                    if let Some(run) = service.runs.get_mut(&id) {
                        run.conflicts = conflicts;
                        service.save_history();
                    }
                } else if !preview && !keep_last_result {
                    let (outcome, message) = if paused && matches!(outcome, Outcome::Cancelled) {
                        (Outcome::Paused, "同步已暂停".into())
                    } else {
                        (outcome, message)
                    };
                    service.runs.insert(
                        id,
                        SyncRun {
                            started,
                            finished: Utc::now(),
                            automatic,
                            directory_change,
                            outcome,
                            message,
                            retry_paths,
                            conflicts,
                        },
                    );
                    service.save_history();
                }
                cx.notify();
                if service.quitting {
                    cx.quit();
                }
            });
        })
        .detach();
        cx.notify();
        true
    }
}

pub(crate) async fn open_sync_store(
    job: &SyncJob,
    settings: &ConnectionSettings,
    slots: &TransferSlots,
) -> Result<Store> {
    let (access, secret) = settings.keys()?;
    let credentials = (!access.is_empty())
        .then(|| aws_sdk_s3::config::Credentials::new(access, secret, None, None, "s3fs-sync"));
    let server = Server::connect(
        settings.region().to_owned(),
        settings.endpoint().to_owned(),
        settings.profile().to_owned(),
        credentials,
    )
    .await?;
    Ok(server
        .with_transfer_slots(slots.clone())
        .open(job.bucket.clone(), job.bucket_region.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_retry_recovers_visible_delete_path() {
        let message = "部分文件待重试\n1 个路径需重新检查或重试（最多显示 12 项）：\nxwechat_files/temp/a（删除远端文件）：回收站副本校验失败，源对象尚未删除。";
        assert_eq!(legacy_retry_targets(message), ["xwechat_files/temp/a"]);
    }

    #[test]
    fn legacy_job_preserves_automatic_mode_and_uses_five_minutes() -> Result<()> {
        let job: SyncJob = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(),
            "profile": Uuid::nil(),
            "bucket": "test",
            "prefix": "",
            "folder": "local",
            "direction": "Both",
            "automatic": true,
        }))?;
        assert!(job.automatic);
        assert_eq!(job.interval_minutes, 5);
        assert!(!job.watch_local);
        Ok(())
    }

    #[test]
    fn watches_nested_files_and_debounces_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested)?;
        let watch = DirectoryWatch::new(root.path())?;
        std::fs::write(nested.join("file.txt"), "changed")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while watch.events.lock().unwrap().pending.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut events = watch.events.lock().unwrap();
        let (_, last) = events.pending.context("没有收到子目录变化事件")?;
        assert!(last.elapsed() < WATCH_QUIET);
        let past = Instant::now() - WATCH_QUIET;
        events.pending = Some((past, past));
        drop(events);
        assert!(watch.due());
        Ok(())
    }
}
