use crate::services::{
    settings::{ConnectionProfiles, ConnectionSettings, TransferSettings, write_settings},
    store::{Server, TransferCancelled, TransferSlots},
    sync::{self, ConflictPolicy, Direction, SyncConflict, SyncJob, SyncProgress},
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui_kit::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const INTERVAL: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
type TransferGate = Arc<tokio::sync::RwLock<()>>;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Completed,
    Conflict,
    Failed,
    Cancelled,
}

impl Outcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Completed => "已完成",
            Self::Conflict => "有冲突",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }

    pub(crate) fn has_issue(self) -> bool {
        matches!(self, Self::Conflict | Self::Failed)
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SyncRun {
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
    automatic: bool,
    outcome: Outcome,
    message: String,
}

impl SyncRun {
    pub(crate) fn started(&self) -> DateTime<Utc> {
        self.started
    }
    pub(crate) fn finished(&self) -> DateTime<Utc> {
        self.finished
    }
    pub(crate) fn automatic(&self) -> bool {
        self.automatic
    }
    pub(crate) fn outcome(&self) -> Outcome {
        self.outcome
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

struct ActiveRun {
    id: Uuid,
    started: DateTime<Utc>,
    automatic: bool,
    progress: Arc<Mutex<SyncProgress>>,
    cancel: CancellationToken,
}

pub(crate) struct SyncService {
    runtime: Arc<tokio::runtime::Runtime>,
    gate: TransferGate,
    transfer_slots: TransferSlots,
    jobs: Vec<SyncJob>,
    active: Option<ActiveRun>,
    runs: HashMap<Uuid, SyncRun>,
    config_error: Option<String>,
    history_error: Option<String>,
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
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            loop {
                if view
                    .update(cx, |service, cx| {
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
            gate: Default::default(),
            transfer_slots: TransferSlots::new(
                TransferSettings::load().unwrap_or_default().limit(),
            ),
            jobs,
            active: None,
            runs,
            config_error,
            history_error,
            quitting: false,
        }
    }

    pub(crate) fn jobs(&self) -> &[SyncJob] {
        &self.jobs
    }
    pub(crate) fn gate(&self) -> TransferGate {
        self.gate.clone()
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

    pub(crate) fn progress(&self, id: Uuid) -> Option<(DateTime<Utc>, bool, SyncProgress)> {
        let run = self.active.as_ref().filter(|run| run.id == id)?;
        Some((
            run.started,
            run.automatic,
            run.progress.lock().unwrap().clone(),
        ))
    }

    pub(crate) fn next_run(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.last_run(id).map(|run| run.finished + INTERVAL)
    }

    pub(crate) fn save_jobs(&mut self, jobs: Vec<SyncJob>, cx: &mut Context<Self>) -> Result<()> {
        sync::save_jobs(&jobs)?;
        self.jobs = jobs;
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
        if self.quitting || self.active.is_some() {
            return;
        }
        let now = Utc::now();
        let Some(job) = self
            .jobs
            .iter()
            .filter(|job| job.automatic && self.next_run(job.id).is_none_or(|due| due <= now))
            .min_by_key(|job| self.next_run(job.id))
        else {
            return;
        };
        self.start(job.id, true, cx);
    }

    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) {
        self.quitting = true;
        if self.active.is_none() {
            cx.quit();
        } else {
            cx.notify();
        }
    }

    pub(crate) fn start(&mut self, id: Uuid, automatic: bool, cx: &mut Context<Self>) {
        self.start_with_policy(id, automatic, None, cx);
    }

    pub(crate) fn resolve_conflicts(
        &mut self,
        id: Uuid,
        finished: DateTime<Utc>,
        policy: ConflictPolicy,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(!self.quitting, "应用正在退出。");
        anyhow::ensure!(self.active.is_none(), "已有同步任务在运行，请等本轮结束。");
        if !self.last_run(id).is_some_and(|run| {
            run.finished() == finished && matches!(run.outcome(), Outcome::Conflict)
        }) {
            anyhow::bail!("冲突记录已更新，请按当前记录重新选择。");
        }
        anyhow::ensure!(
            self.start_with_policy(id, false, Some(policy), cx),
            "任务已删除或已改为单向同步。"
        );
        Ok(())
    }

    fn start_with_policy(
        &mut self,
        id: Uuid,
        automatic: bool,
        policy: Option<ConflictPolicy>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.quitting || self.active.is_some() {
            return false;
        }
        let Some(job) = self.jobs.iter().find(|job| job.id == id).cloned() else {
            return false;
        };
        if policy.is_some() && job.direction != Direction::Both {
            return false;
        }
        let started = Utc::now();
        let progress = Arc::new(Mutex::new(SyncProgress {
            stage: "等待传输队列",
            ..Default::default()
        }));
        let cancel = CancellationToken::new();
        self.active = Some(ActiveRun {
            id,
            started,
            automatic,
            progress: progress.clone(),
            cancel: cancel.clone(),
        });
        let gate = self.gate();
        let slots = self.transfer_slots();
        let worker_progress = progress.clone();
        let task = self.runtime.spawn(async move {
            let _sync = tokio::select! {
                _ = cancel.cancelled() => return Err(TransferCancelled.into()),
                guard = gate.write() => guard,
            };
            worker_progress.lock().unwrap().stage = "连接存储桶";
            let profiles = ConnectionProfiles::load(&ConnectionSettings::path()?)?;
            let settings = &profiles
                .get(job.profile)
                .context("任务所用的连接凭证已删除")?
                .settings;
            let (access, secret) = settings.keys()?;
            let credentials = (!access.is_empty()).then(|| {
                aws_sdk_s3::config::Credentials::new(access, secret, None, None, "s3fs-sync")
            });
            let server = Server::connect(
                settings.region().to_owned(),
                settings.endpoint().to_owned(),
                settings.profile().to_owned(),
                credentials,
            )
            .await?;
            let store = server
                .with_transfer_slots(slots.clone())
                .open(job.bucket.clone(), job.bucket_region.clone());
            sync::run(store, job, policy, &slots, &cancel, |state| {
                worker_progress.lock().unwrap().clone_from(state)
            })
            .await
        });
        cx.spawn(async move |view, cx| {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let (outcome, message) = match result {
                Ok(message) => (Outcome::Completed, message),
                Err(error) if error.is::<SyncConflict>() => (Outcome::Conflict, error.to_string()),
                Err(error) if error.is::<TransferCancelled>() => {
                    (Outcome::Cancelled, "同步已取消".into())
                }
                Err(error) => {
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
                service.active = None;
                service.runs.insert(
                    id,
                    SyncRun {
                        started,
                        finished: Utc::now(),
                        automatic,
                        outcome,
                        message,
                    },
                );
                service.save_history();
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
