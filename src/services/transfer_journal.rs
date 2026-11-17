use crate::services::{
    settings::{ConnectionSettings, write_settings},
    store::{Entry, FileStamp, Store, TransferCancelled, TransferSlots},
    transfer::TransferState,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicU64},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) fn is_download_partial(name: &str) -> bool {
    name.strip_prefix(".s3fs-")
        .and_then(|name| name.strip_suffix(".part"))
        .and_then(|name| name.rsplit_once('-'))
        .is_some_and(|(id, slot)| {
            id.len() == 36 && Uuid::parse_str(id).is_ok() && slot.parse::<u64>().is_ok()
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum TransferOperation {
    Upload {
        prefix: String,
        paths: Vec<PathBuf>,
    },
    Download {
        entry: Entry,
        destination: PathBuf,
        #[serde(default)]
        overwrite: Option<FileStamp>,
    },
    Clipboard {
        entries: Vec<Entry>,
    },
    Remote {
        target_bucket: String,
        targets: Vec<(Entry, String)>,
        copying: bool,
        rename: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum JournalState {
    Pending,
    Complete,
    Cancelled,
    Paused(String),
    UserPaused,
}

#[derive(Debug)]
pub(crate) struct TransferPaused;

impl std::fmt::Display for TransferPaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("传输已暂停")
    }
}

impl std::error::Error for TransferPaused {}

#[derive(Default, PartialEq, Eq)]
enum PauseState {
    #[default]
    Running,
    Requested,
    Finished,
}

#[derive(Clone)]
pub(crate) struct TransferPause {
    state: Arc<Mutex<PauseState>>,
    path: PathBuf,
    record: Arc<TransferRecord>,
}

impl TransferPause {
    pub(crate) fn is_requested(&self) -> bool {
        *self.state.lock().unwrap() == PauseState::Requested
    }

    pub(crate) fn request(&self, cancel: &CancellationToken) -> Result<bool> {
        let mut state = self.state.lock().unwrap();
        if *state != PauseState::Running || cancel.is_cancelled() {
            return Ok(false);
        }
        let mut record = (*self.record).clone();
        record.state = JournalState::UserPaused;
        // Persist the intent before waking workers so an immediate exit still restores it.
        write_settings(&self.path, &record).context("无法保存暂停状态")?;
        *state = PauseState::Requested;
        cancel.cancel();
        Ok(true)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TransferRecord {
    pub(crate) id: Uuid,
    pub(crate) connection: String,
    pub(crate) bucket: String,
    pub(crate) operation: TransferOperation,
    pub(crate) state: JournalState,
    pub(crate) started: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub(crate) progress: String,
}

impl TransferRecord {
    pub(crate) fn new(connection: String, bucket: String, operation: TransferOperation) -> Self {
        Self {
            id: Uuid::new_v4(),
            connection,
            bucket,
            operation,
            state: JournalState::Pending,
            started: chrono::Utc::now(),
            progress: String::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SavedUploadItem {
    pub(crate) key: String,
    pub(crate) size: u64,
    pub(crate) directory: bool,
    pub(crate) counter: Arc<AtomicU64>,
    pub(crate) state: Arc<Mutex<TransferState>>,
    pub(crate) cancel: CancellationToken,
}

pub(crate) type SavedUploadItems = Arc<Mutex<Option<Vec<SavedUploadItem>>>>;

pub(crate) struct TransferJournal {
    dir: PathBuf,
    record: Arc<TransferRecord>,
    pause: Arc<Mutex<PauseState>>,
    _lock: File,
}

impl Drop for TransferJournal {
    fn drop(&mut self) {
        *self.pause.lock().unwrap() = PauseState::Finished;
    }
}

impl TransferJournal {
    fn root() -> Result<PathBuf> {
        Ok(ConnectionSettings::path()?.with_file_name("transfer-journal-v1"))
    }

    pub(crate) fn records(connection: &str) -> Result<Vec<TransferRecord>> {
        let paths = match std::fs::read_dir(Self::root()?) {
            Ok(paths) => paths,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for path in paths {
            let path = path?;
            let Some(id) = path
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let Some(record) = read::<TransferRecord>(&path.path().join("task.json"))? else {
                continue;
            };
            ensure!(record.id == id, "传输记录 ID 与路径不一致：{id}");
            if record.connection == connection {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.started);
        Ok(records)
    }

    pub(crate) fn open(record: TransferRecord) -> Result<Self> {
        Self::open_at(&Self::root()?, record)
    }

    pub(crate) fn cancel_saved(id: Uuid) -> Result<JournalState> {
        Self::cancel_saved_at(&Self::root()?, id)
    }

    fn cancel_saved_at(root: &Path, id: Uuid) -> Result<JournalState> {
        let record = read::<TransferRecord>(&root.join(id.to_string()).join("task.json"))?
            .context("没有找到传输任务")?;
        ensure!(record.id == id, "传输记录 ID 与路径不一致：{id}");
        let journal = Self::open_at(root, record)?;
        let mut record = journal.record().clone();
        if matches!(
            record.state,
            JournalState::Complete | JournalState::Cancelled
        ) {
            return Ok(record.state);
        }
        record.state = JournalState::Cancelled;
        journal.save("task.json", &record)?;
        // Keep saved upload checkpoints so unfinished S3 multipart sessions remain traceable.
        journal.cleanup_finished(
            &record,
            matches!(record.operation, TransferOperation::Upload { .. }),
        );
        Ok(record.state)
    }

    pub(crate) fn open_at(root: &Path, record: TransferRecord) -> Result<Self> {
        let dir = root.join(record.id.to_string());
        std::fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("task.lock"))?;
        lock.try_lock().context("另一个窗口仍在处理此任务")?;
        let record_path = dir.join("task.json");
        let record = match read::<TransferRecord>(&record_path)? {
            Some(saved) => {
                ensure!(
                    saved.id == record.id && saved.connection == record.connection,
                    "任务所属连接不一致"
                );
                saved
            }
            None => {
                write_settings(&record_path, &record).context("无法保存传输任务")?;
                record
            }
        };
        Ok(Self {
            dir,
            record: Arc::new(record),
            pause: Default::default(),
            _lock: lock,
        })
    }

    pub(crate) fn record(&self) -> &TransferRecord {
        &self.record
    }

    pub(crate) fn pause_handle(&self) -> TransferPause {
        TransferPause {
            state: self.pause.clone(),
            path: self.dir.join("task.json"),
            record: self.record.clone(),
        }
    }

    pub(crate) fn is_pause_requested(&self) -> bool {
        *self.pause.lock().unwrap() == PauseState::Requested
    }

    pub(crate) fn load<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        read(&self.dir.join(name))
    }

    pub(crate) fn save(&self, name: &str, value: &impl Serialize) -> Result<()> {
        write_settings(&self.dir.join(name), value)
            .with_context(|| format!("无法保存传输进度 {name}"))
    }

    pub(crate) fn part_path(&self, destination: &Path, slot: u64) -> Result<PathBuf> {
        Ok(destination
            .parent()
            .context("保存路径没有上级目录")?
            .join(format!(".s3fs-{}-{slot}.part", self.record.id)))
    }

    pub(crate) fn clipboard_dir(&self) -> PathBuf {
        self.dir.join("clipboard")
    }

    #[cfg(windows)]
    pub(crate) fn clipboard_paths(connection: &str, id: Uuid) -> Result<Vec<PathBuf>> {
        let dir = Self::root()?.join(id.to_string());
        let record =
            read::<TransferRecord>(&dir.join("task.json"))?.context("没有找到剪贴板任务")?;
        ensure!(
            record.id == id
                && record.connection == connection
                && matches!(record.operation, TransferOperation::Clipboard { .. })
                && matches!(record.state, JournalState::Complete),
            "剪贴板任务尚未完成或所属连接已切换"
        );
        let paths = read::<Vec<PathBuf>>(&dir.join("clipboard-paths.json"))?
            .context("没有找到剪贴板文件清单")?;
        let root = dir.join("clipboard");
        ensure!(
            paths
                .iter()
                .all(|path| path.starts_with(&root) && path.exists()),
            "部分剪贴板文件已丢失"
        );
        Ok(paths)
    }

    pub(crate) async fn run(
        self,
        store: Store,
        target: Store,
        slots: TransferSlots,
        cancel: CancellationToken,
        counter: Arc<AtomicU64>,
        upload_items: SavedUploadItems,
        mut progress: impl FnMut(String) + Send,
    ) -> Result<()> {
        let mut record = (*self.record).clone();
        let mut status = record.progress.clone();
        let mut report = |message: String| {
            status.clone_from(&message);
            progress(message);
        };
        {
            let pause = self.pause.lock().unwrap();
            if *pause == PauseState::Running {
                record.state = JournalState::Pending;
                self.save("task.json", &record)?;
            }
        }
        let result = if cancel.is_cancelled() {
            Err(TransferCancelled.into())
        } else {
            match &self.record.operation {
                TransferOperation::Upload { prefix, paths } => {
                    store
                        .journal_upload(
                            &self,
                            prefix,
                            paths,
                            &slots,
                            &cancel,
                            upload_items,
                            &mut report,
                        )
                        .await
                }
                TransferOperation::Download {
                    entry,
                    destination,
                    overwrite,
                } => {
                    store
                        .journal_download(
                            &self,
                            0,
                            entry,
                            destination,
                            overwrite.as_ref(),
                            &slots,
                            &cancel,
                            &counter,
                            &mut report,
                        )
                        .await
                }
                TransferOperation::Clipboard { entries } => {
                    #[cfg(windows)]
                    {
                        store
                            .journal_clipboard(&self, entries, &slots, &cancel, &mut report)
                            .await
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = entries;
                        anyhow::bail!("当前系统不支持文件剪贴板");
                    }
                }
                TransferOperation::Remote {
                    targets, copying, ..
                } => {
                    store
                        .journal_remote(&target, &self, targets, *copying, &cancel, &mut report)
                        .await
                }
            }
        };
        let result = {
            let mut pause = self.pause.lock().unwrap();
            let result = if result.is_err() && *pause == PauseState::Requested {
                Err(TransferPaused.into())
            } else {
                result
            };
            record.progress = status;
            record.state = match &result {
                Ok(()) => JournalState::Complete,
                Err(error) if error.is::<TransferPaused>() => JournalState::UserPaused,
                Err(error) if error.is::<TransferCancelled>() => JournalState::Cancelled,
                Err(error) => JournalState::Paused(format!("{error:#}")),
            };
            *pause = PauseState::Finished;
            self.save("task.json", &record)?;
            result
        };
        self.cleanup_finished(&record, false);
        result
    }

    fn cleanup_finished(&self, record: &TransferRecord, keep_upload_checkpoints: bool) {
        if matches!(record.state, JournalState::Cancelled) {
            match &record.operation {
                TransferOperation::Download { destination, .. } => {
                    if let Ok(part) = self.part_path(destination, 0) {
                        let _ = std::fs::remove_file(part);
                    }
                }
                TransferOperation::Clipboard { .. } => {
                    let _ = std::fs::remove_dir_all(self.clipboard_dir());
                }
                _ => {}
            }
        }
        if matches!(
            record.state,
            JournalState::Complete | JournalState::Cancelled
        ) && !(matches!(record.state, JournalState::Complete)
            && matches!(record.operation, TransferOperation::Clipboard { .. }))
            && !keep_upload_checkpoints
            && let Ok(files) = std::fs::read_dir(&self.dir)
        {
            for file in files.flatten() {
                let name = file.file_name();
                if name != "task.json" && file.path().extension().is_some_and(|ext| ext == "json") {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
    }
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match File::open(path) {
        Ok(file) => {
            Ok(Some(serde_json::from_reader(file).with_context(|| {
                format!("无法读取任务记录 {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_saved_transfer_persists_for_all_unfinished_states() -> Result<()> {
        let root = tempfile::tempdir()?;
        for state in [
            JournalState::Pending,
            JournalState::UserPaused,
            JournalState::Paused("单个对象超过 5 GiB".into()),
            JournalState::Complete,
        ] {
            let mut record = TransferRecord::new(
                "connection".into(),
                "bucket".into(),
                TransferOperation::Remote {
                    target_bucket: "destination".into(),
                    targets: Vec::new(),
                    copying: false,
                    rename: false,
                },
            );
            let complete = matches!(state, JournalState::Complete);
            record.state = state;
            let journal = TransferJournal::open_at(root.path(), record.clone())?;
            journal.save("remote-plan.json", &"checkpoint")?;
            assert!(TransferJournal::cancel_saved_at(root.path(), record.id).is_err());
            drop(journal);

            let expected_state = |state| {
                if complete {
                    matches!(state, JournalState::Complete)
                } else {
                    matches!(state, JournalState::Cancelled)
                }
            };
            assert!(expected_state(TransferJournal::cancel_saved_at(
                root.path(),
                record.id
            )?));
            let reopened = TransferJournal::open_at(root.path(), record)?;
            assert!(expected_state(reopened.record().state.clone()));
            assert_eq!(
                reopened.load::<String>("remote-plan.json")?.is_some(),
                complete
            );
        }
        Ok(())
    }

    #[test]
    fn cancelling_clipboard_removes_all_cached_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        let record = TransferRecord::new(
            "connection".into(),
            "bucket".into(),
            TransferOperation::Clipboard {
                entries: Vec::new(),
            },
        );
        let journal = TransferJournal::open_at(root.path(), record.clone())?;
        let cache = journal.clipboard_dir();
        std::fs::create_dir_all(cache.join("nested"))?;
        std::fs::write(cache.join("finished.txt"), b"data")?;
        std::fs::write(
            journal.part_path(&cache.join("nested").join("next.txt"), 1)?,
            b"part",
        )?;
        journal.save("clipboard-plan.json", &"checkpoint")?;
        drop(journal);

        assert!(matches!(
            TransferJournal::cancel_saved_at(root.path(), record.id)?,
            JournalState::Cancelled
        ));
        assert!(!cache.exists());
        let journal = TransferJournal::open_at(root.path(), record)?;
        assert!(journal.load::<String>("clipboard-plan.json")?.is_none());
        Ok(())
    }

    #[test]
    fn pause_is_persisted_before_signalling_workers() -> Result<()> {
        let root = tempfile::tempdir()?;
        let record = TransferRecord::new(
            "connection".into(),
            "bucket".into(),
            TransferOperation::Upload {
                prefix: String::new(),
                paths: Vec::new(),
            },
        );
        let journal = TransferJournal::open_at(root.path(), record.clone())?;
        journal.save("multipart-0.json", &"checkpoint")?;
        let pause = journal.pause_handle();
        let cancel = CancellationToken::new();
        assert!(pause.request(&cancel)?);
        assert!(cancel.is_cancelled());
        assert!(matches!(
            journal.load::<TransferRecord>("task.json")?.unwrap().state,
            JournalState::UserPaused
        ));
        drop(journal);

        let reopened = TransferJournal::open_at(root.path(), record)?;
        assert!(matches!(reopened.record().state, JournalState::UserPaused));
        assert_eq!(
            reopened.load::<String>("multipart-0.json")?.as_deref(),
            Some("checkpoint")
        );
        assert!(!pause.request(&CancellationToken::new())?);
        Ok(())
    }
}
