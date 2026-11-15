use crate::services::{
    settings::{ConnectionSettings, write_settings},
    store::{Entry, Store, TransferCancelled, TransferSlots},
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum TransferOperation {
    Upload {
        prefix: String,
        paths: Vec<PathBuf>,
    },
    Download {
        entry: Entry,
        destination: PathBuf,
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
    record: TransferRecord,
    _lock: File,
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
            record,
            _lock: lock,
        })
    }

    pub(crate) fn record(&self) -> &TransferRecord {
        &self.record
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
        mut self,
        store: Store,
        target: Store,
        slots: TransferSlots,
        cancel: CancellationToken,
        counter: Arc<AtomicU64>,
        upload_items: SavedUploadItems,
        mut progress: impl FnMut(String) + Send,
    ) -> Result<()> {
        let mut status = String::new();
        let mut report = |message: String| {
            status.clone_from(&message);
            progress(message);
        };
        self.record.state = JournalState::Pending;
        self.save("task.json", &self.record)?;
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
                TransferOperation::Download { entry, destination } => {
                    store
                        .journal_download(
                            &self,
                            0,
                            entry,
                            destination,
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
        self.record.progress = status;
        self.record.state = match &result {
            Ok(()) => JournalState::Complete,
            Err(error) if error.is::<TransferCancelled>() => JournalState::Cancelled,
            Err(error) => JournalState::Paused(format!("{error:#}")),
        };
        self.save("task.json", &self.record)?;
        if matches!(self.record.state, JournalState::Cancelled)
            && let TransferOperation::Download { destination, .. } = &self.record.operation
        {
            let _ = std::fs::remove_file(self.part_path(destination, 0)?);
        }
        if matches!(
            self.record.state,
            JournalState::Complete | JournalState::Cancelled
        ) && !matches!(self.record.operation, TransferOperation::Clipboard { .. })
            && let Ok(files) = std::fs::read_dir(&self.dir)
        {
            for file in files.flatten() {
                let name = file.file_name();
                if name != "task.json" && file.path().extension().is_some_and(|ext| ext == "json") {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        result
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
