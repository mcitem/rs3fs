use super::*;
use crate::services::{
    store::TransferCancelled,
    transfer_journal::{
        JournalState, SavedUploadItem, TransferJournal, TransferOperation, TransferRecord,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartMode {
    New,
    Manual,
    Automatic,
}

impl Browser {
    pub(super) fn queue_transfer(
        &mut self,
        bucket: String,
        operation: TransferOperation,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self.journal_connection.clone() else {
            self.show_notice("请先连接存储服务", cx);
            return false;
        };
        let publish_clipboard = matches!(&operation, TransferOperation::Clipboard { .. });
        if publish_clipboard
            && self
                .transfers
                .iter()
                .any(|task| task.running && task.direction == TransferDirection::Clipboard)
        {
            self.show_notice("已有任务正在准备剪贴板文件", cx);
            return false;
        }
        self.start_saved_transfer(
            TransferRecord::new(connection, bucket, operation),
            StartMode::New,
            cx,
        )
    }

    pub(super) fn load_transfer_history(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = &self.journal_connection else {
            return;
        };
        let records = match TransferJournal::records(connection) {
            Ok(records) => records,
            Err(error) => return self.show_error(error, cx),
        };
        let mut history: Vec<_> = records
            .iter()
            .filter(|record| {
                matches!(
                    record.state,
                    JournalState::Complete | JournalState::Cancelled
                )
            })
            .rev()
            .take(20)
            .collect();
        history.reverse();
        for record in history.into_iter().chain(
            records
                .iter()
                .filter(|record| matches!(record.state, JournalState::Paused(_))),
        ) {
            if self
                .transfers
                .iter()
                .any(|task| task.saved == record.id && !task.status.starts_with("等待恢复"))
            {
                continue;
            }
            self.transfers.retain(|task| task.saved != record.id);
            let id = self.next_transfer_id;
            self.next_transfer_id += 1;
            let (direction, target, destination) = transfer_label(record);
            let status = match &record.state {
                JournalState::Complete => "已完成",
                JournalState::Cancelled => "已取消",
                JournalState::Paused(_) => "已暂停",
                JournalState::Pending => unreachable!(),
            };
            self.transfers.push(TransferTask {
                saved: record.id,
                direction,
                started: Instant::now(),
                destination,
                id,
                target,
                status: transfer_status(status, &record.progress),
                running: false,
                connection: self.connection_revision,
                bucket: record.bucket.clone(),
                items: Vec::new(),
                expanded: false,
                cancel: CancellationToken::new(),
            });
            if let JournalState::Paused(error) = &record.state {
                self.push_output(Some(id), true, error.clone());
            }
        }
        let mut waiting = false;
        for record in records {
            if !matches!(record.state, JournalState::Pending)
                || self
                    .transfers
                    .iter()
                    .any(|task| task.saved == record.id && task.running)
            {
                continue;
            }
            if !self.start_saved_transfer(record.clone(), StartMode::Automatic, cx) {
                waiting = true;
                if !self.transfers.iter().any(|task| task.saved == record.id) {
                    let id = self.next_transfer_id;
                    self.next_transfer_id += 1;
                    let (direction, target, destination) = transfer_label(&record);
                    self.transfers.push(TransferTask {
                        saved: record.id,
                        direction,
                        started: Instant::now(),
                        destination,
                        id,
                        target,
                        status: "等待恢复".into(),
                        running: false,
                        connection: self.connection_revision,
                        bucket: record.bucket,
                        items: Vec::new(),
                        expanded: false,
                        cancel: CancellationToken::new(),
                    });
                    self.show_tasks = true;
                }
            }
        }
        if waiting && !self.retrying_saved_transfers {
            self.retrying_saved_transfers = true;
            let executor = cx.background_executor().clone();
            cx.spawn(async move |view, cx| {
                executor.timer(Duration::from_secs(2)).await;
                let _ = view.update(cx, |this, cx| {
                    this.retrying_saved_transfers = false;
                    this.load_transfer_history(cx);
                });
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn resume_saved_transfer(&mut self, saved: uuid::Uuid, cx: &mut Context<Self>) {
        let Some(connection) = &self.journal_connection else {
            return;
        };
        match TransferJournal::records(connection) {
            Ok(records) => {
                if let Some(record) = records.into_iter().find(|record| record.id == saved) {
                    self.start_saved_transfer(record, StartMode::Manual, cx);
                } else {
                    self.show_notice("请连接到此任务所属的存储服务", cx);
                }
            }
            Err(error) => self.show_error(error, cx),
        }
    }

    fn start_saved_transfer(
        &mut self,
        record: TransferRecord,
        mode: StartMode,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy.is_some() || self.journal_connection.as_deref() != Some(&record.connection) {
            return false;
        }
        if self
            .transfers
            .iter()
            .any(|task| task.running && task.saved == record.id)
        {
            return false;
        }
        let Some(server) = self.server.as_ref() else {
            return false;
        };
        let store = Bucket::named(&record.bucket).open(server);
        let (direction, description, destination) = transfer_label(&record);
        if let Some(destination) = &destination
            && self
                .transfers
                .iter()
                .any(|task| task.running && task.destination.as_ref() == Some(destination))
        {
            if mode != StartMode::Automatic {
                self.show_notice("已有任务正在下载到此路径", cx);
            }
            return false;
        }
        let connection = self.connection_revision;
        let mut scope = TransferScopes {
            connection,
            folders: Vec::new(),
            paths: Vec::new(),
        };
        let mut removed = Vec::new();
        let mut target = store.clone();
        match &record.operation {
            TransferOperation::Upload { prefix, paths } => {
                scope.folders.push((record.bucket.clone(), prefix.clone()));
                for path in paths {
                    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                        scope
                            .paths
                            .push((record.bucket.clone(), format!("{prefix}{name}")));
                        scope
                            .paths
                            .push((record.bucket.clone(), format!("{prefix}{name}/")));
                    }
                }
            }
            TransferOperation::Remote {
                target_bucket,
                targets,
                copying,
                ..
            } => {
                target = Bucket::named(target_bucket).open(server);
                for (entry, destination) in targets {
                    scope
                        .folders
                        .push((record.bucket.clone(), parent_prefix(entry.key()).to_owned()));
                    scope
                        .folders
                        .push((target_bucket.clone(), parent_prefix(destination).to_owned()));
                    scope
                        .paths
                        .push((record.bucket.clone(), entry.key().to_owned()));
                    scope
                        .paths
                        .push((target_bucket.clone(), destination.clone()));
                    if !copying {
                        removed.push(entry.clone());
                    }
                }
            }
            TransferOperation::Download { .. } | TransferOperation::Clipboard { .. } => {}
        }
        let writing = !matches!(
            direction,
            TransferDirection::Download | TransferDirection::Clipboard
        );
        let can_write = if direction == TransferDirection::Upload {
            !self
                .write_scopes
                .values()
                .any(|active| scope.overlaps(active))
                && !self.upload_scopes.values().any(|active| {
                    active.connection == connection
                        && scope.paths.iter().any(|(bucket, key)| {
                            active.paths.iter().any(|(other_bucket, other)| {
                                bucket == other_bucket
                                    && (key == other
                                        || (key.ends_with('/') && other.starts_with(key))
                                        || (other.ends_with('/') && key.starts_with(other)))
                            })
                        })
                })
        } else {
            self.write_scope_available(&scope)
        };
        if writing && !can_write {
            if mode != StartMode::Automatic {
                self.show_write_locked(cx);
            }
            return false;
        }
        let journal = match TransferJournal::open(record) {
            Ok(journal) => journal,
            Err(error) => {
                if mode != StartMode::Automatic {
                    self.show_error(error, cx);
                }
                return false;
            }
        };
        let record = journal.record();
        if matches!(
            record.state,
            JournalState::Complete | JournalState::Cancelled
        ) {
            return false;
        }
        let saved = record.id;
        let publish_clipboard = mode == StartMode::New && direction == TransferDirection::Clipboard;
        let restoring = !publish_clipboard && direction == TransferDirection::Clipboard;
        let bucket = record.bucket.clone();
        let target_bucket = target.bucket().to_owned();
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        if direction == TransferDirection::Upload {
            self.upload_scopes.insert(id, scope);
        } else if writing {
            self.write_scopes.insert(id, scope);
        }
        let cancel = CancellationToken::new();
        let counter = Arc::new(AtomicU64::new(0));
        let mut items = Vec::new();
        if let TransferOperation::Download { entry, .. } = &record.operation {
            items.push(TransferItem {
                id: 0,
                directory: false,
                progress: TransferProgress {
                    key: entry.key().to_owned(),
                    size: entry.size().max(0) as u64,
                    counter: counter.clone(),
                    sampled: 0,
                    sample_time: Instant::now(),
                    speed: 0.0,
                },
                state: Arc::new(Mutex::new(TransferState::Sending)),
                cancel: cancel.clone(),
            });
        }
        self.transfers.retain(|task| task.saved != saved);
        self.transfers.push(TransferTask {
            saved,
            direction,
            started: Instant::now(),
            destination,
            id,
            target: description.clone(),
            status: "正在读取任务进度".into(),
            running: true,
            connection,
            bucket: bucket.clone(),
            items,
            expanded: false,
            cancel: cancel.clone(),
        });
        self.show_tasks = true;
        self.show_sync_tasks = false;
        self.show_transfer_history = false;
        self.tasks_maximized = false;
        self.push_output(
            Some(id),
            false,
            format!("传输 #{id}，{}，{description}", direction.label()),
        );

        let slots = self.transfer_slots.clone();
        let gate = self.sync.read(cx).gate();
        let progress = Arc::new(Mutex::new(String::new()));
        let latest = progress.clone();
        let upload_items = Arc::new(Mutex::new(None));
        let visible = upload_items.clone();
        let mut job = self.runtime.spawn(async move {
            let _guard = if writing {
                tokio::select! {
                    guard = gate.read() => Some(guard),
                    _ = cancel.cancelled() => None,
                }
            } else {
                None
            };
            journal
                .run(store, target, slots, cancel, counter, visible, |status| {
                    *latest.lock().unwrap() = status;
                })
                .await
        });
        #[cfg(windows)]
        let window = self.preview_window;
        #[cfg(not(windows))]
        let _ = publish_clipboard;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            let result = loop {
                tokio::select! {
                    result = &mut job => break result.map_err(anyhow::Error::from).and_then(|result| result),
                    _ = executor.timer(Duration::from_millis(150)) => {
                        let status = progress.lock().unwrap().clone();
                        let uploaded = upload_items.lock().unwrap().take();
                        let _ = view.update(cx, |this, cx| {
                            if let Some(task) = this.transfers.iter_mut().find(|task| task.id == id) {
                                if let Some(items) = uploaded { task.items = upload_rows(items); }
                                if !status.is_empty() { task.status = status; }
                                for item in &mut task.items { item.progress.sample(); }
                                cx.notify();
                            }
                        });
                    }
                }
            };
            let _ = view.update(cx, |this, cx| {
                let status = progress.lock().unwrap().clone();
                if let Some(task) = this.transfers.iter_mut().find(|task| task.id == id) {
                    task.status = status;
                    if let Some(items) = upload_items.lock().unwrap().take() { task.items = upload_rows(items); }
                    if result.is_ok() && direction == TransferDirection::Download {
                        for item in &task.items {
                            *item.state.lock().unwrap() = TransferState::Complete;
                        }
                    }
                }
                let on_source = this.store.as_ref().is_some_and(|store| store.bucket() == bucket);
                let on_target = this.store.as_ref().is_some_and(|store| store.bucket() == target_bucket);
                if this.connection_revision == connection {
                    if result.is_ok() && on_source {
                        for entry in &removed { this.after_removed_entry(entry, cx); }
                    }
                    if writing && (on_source || on_target) {
                        this.refresh_usage(cx);
                    }
                }
                if writing { this.finish_write(id, cx); }
                match result {
                    Ok(()) => {
                        this.finish_transfer(id, None, cx);
                        #[cfg(windows)]
                        if publish_clipboard {
                            let owner = cx.weak_entity();
                            cx.defer(move |cx| {
                                let _ = window.update(cx, |_, window, cx| {
                                    let _ = owner.update(cx, |this, cx| this.publish_saved_clipboard(saved, window, cx));
                                });
                            });
                        }
                        if restoring {
                            this.show_notice("剪贴板文件已准备好，可在任务记录中复制", cx);
                        } else {
                            this.show_notice(format!("{}完成", direction.label()), cx);
                        }
                    }
                    Err(error) if error.is::<TransferCancelled>() => {
                        this.finish_transfer(id, Some(error), cx);
                        this.show_notice("已取消，已完成的操作保留", cx);
                    }
                    Err(error) => {
                        this.finish_transfer(id, Some(anyhow::anyhow!("{error:#}")), cx);
                        this.show_error(error, cx);
                    }
                }
                this.load_transfer_history(cx);
            });
        }).detach();
        cx.notify();
        true
    }
}

fn transfer_label(record: &TransferRecord) -> (TransferDirection, String, Option<PathBuf>) {
    match &record.operation {
        TransferOperation::Upload { prefix, paths } => (
            TransferDirection::Upload,
            format!("{} 个本地来源 → {} /{prefix}", paths.len(), record.bucket),
            None,
        ),
        TransferOperation::Download { entry, destination } => (
            TransferDirection::Download,
            format!("/{} → {}", entry.key(), destination.display()),
            Some(destination.clone()),
        ),
        TransferOperation::Clipboard { entries } => (
            TransferDirection::Clipboard,
            format!("准备剪贴板文件，{} 项", entries.len()),
            None,
        ),
        TransferOperation::Remote {
            target_bucket,
            targets,
            copying,
            rename,
        } => {
            let direction = if *rename {
                TransferDirection::Rename
            } else if *copying {
                TransferDirection::Copy
            } else {
                TransferDirection::Move
            };
            let target = if let [(entry, key)] = targets.as_slice() {
                format!("/{} → {target_bucket} /{key}", entry.key())
            } else {
                format!("{} 项 → {target_bucket}", targets.len())
            };
            (direction, target, None)
        }
    }
}

pub(super) fn transfer_status(state: &str, progress: &str) -> String {
    if progress.is_empty() {
        state.to_owned()
    } else {
        format!("{state}，{progress}")
    }
}

fn upload_rows(items: Vec<SavedUploadItem>) -> Vec<TransferItem> {
    items
        .into_iter()
        .enumerate()
        .map(|(id, item)| TransferItem {
            id,
            directory: item.directory,
            progress: TransferProgress {
                key: item.key,
                size: item.size,
                counter: item.counter,
                sampled: 0,
                sample_time: Instant::now(),
                speed: 0.0,
            },
            state: item.state,
            cancel: item.cancel,
        })
        .collect()
}
