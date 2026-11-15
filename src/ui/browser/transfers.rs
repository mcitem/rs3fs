use super::*;
use crate::services::transfer_journal::TransferOperation;

impl Browser {
    pub(super) fn upload(&mut self, prefix: String, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !paths.is_empty()
            && let Some(store) = &self.store
        {
            self.queue_transfer(
                store.bucket().to_owned(),
                TransferOperation::Upload { prefix, paths },
                cx,
            );
        }
    }

    pub(super) fn finish_transfer(
        &mut self,
        id: u64,
        error: Option<anyhow::Error>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.transfers.iter_mut().find(|task| task.id == id) else {
            return;
        };
        let cancelled = error
            .as_ref()
            .is_some_and(|error| error.is::<crate::services::store::TransferCancelled>());
        let state = if cancelled {
            "已取消"
        } else if error.is_some() {
            "已暂停"
        } else {
            "已完成"
        };
        task.running = false;
        task.status = super::durable_transfer::transfer_status(state, &task.status);
        for item in &task.items {
            let mut state = item.state.lock().unwrap();
            if state.active() {
                *state = match &error {
                    Some(error) if !cancelled => TransferState::Failed(format!("{error:#}")),
                    _ => TransferState::Cancelled,
                };
            }
        }
        let mut summary = format!(
            "传输 #{id}，{}，{}，{}，{:.2} 秒",
            task.direction.label(),
            task.target,
            task.status,
            task.started.elapsed().as_secs_f64(),
        );
        if let Some(error) = error.as_ref().filter(|_| !cancelled) {
            summary.push_str(&format!("\n{error:#}"));
        }
        self.push_output(Some(id), error.is_some() && !cancelled, summary);
        if self
            .transfers
            .iter()
            .filter(|task| {
                !task.running
                    && !task.status.starts_with("已暂停")
                    && !task.status.starts_with("等待恢复")
            })
            .count()
            > 20
            && let Some(index) = self.transfers.iter().position(|task| {
                !task.running
                    && !task.status.starts_with("已暂停")
                    && !task.status.starts_with("等待恢复")
            })
        {
            self.transfers.remove(index);
        }
        cx.notify();
    }

    pub(super) fn refresh_after_upload(&mut self, cx: &mut Context<Self>) {
        if self.upload_refresh_pending && self.busy.is_none() {
            self.upload_refresh_pending = false;
            self.listing_epoch += 1;
            self.refresh(cx);
        }
    }

    pub(super) fn pick_upload(
        &mut self,
        directories: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.folder_write_locked(&self.prefix) {
            return self.show_write_locked(cx);
        }
        let picker = rfd::AsyncFileDialog::new().set_title(if directories {
            "选择上传的文件夹"
        } else {
            "选择上传的文件"
        });
        let prefix = self.prefix.clone();
        let revision = self.bucket_revision;
        cx.spawn_in(window, async move |view, cx| {
            let files = if directories {
                picker.pick_folders().await
            } else {
                picker.pick_files().await
            };
            if let Some(files) = files {
                let _ = view.update(cx, |this, cx| {
                    if this.bucket_revision != revision {
                        return;
                    }
                    this.upload(
                        prefix,
                        files
                            .into_iter()
                            .map(|file| file.path().to_owned())
                            .collect(),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(super) fn download(&mut self, entry: Entry, window: &Window, cx: &mut Context<Self>) {
        if entry.directory() || self.busy.is_some() {
            return;
        }
        let name = entry.name().to_owned();
        let revision = self.bucket_revision;
        cx.spawn_in(window, async move |view, cx| {
            let mut dialog = rfd::AsyncFileDialog::new()
                .set_title("保存文件")
                .set_file_name(name);
            if let Some(directory) =
                std::env::var_os("S3FS_DOWNLOAD_DIR").filter(|path| !path.is_empty())
            {
                dialog = dialog.set_directory(std::path::PathBuf::from(directory));
            }
            let file = dialog.save_file().await;
            if let Some(file) = file {
                let path = file.path().to_owned();
                let _ = view.update(cx, |this, cx| {
                    if this.bucket_revision != revision {
                        return;
                    }
                    let Some(store) = this.store.clone() else {
                        return;
                    };
                    this.start_download(store, entry, path, cx);
                });
            }
        })
        .detach();
    }

    fn start_download(
        &mut self,
        store: Store,
        entry: Entry,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.queue_transfer(
            store.bucket().to_owned(),
            TransferOperation::Download {
                entry,
                destination: path,
            },
            cx,
        );
    }

    pub(super) fn download_batch(
        &mut self,
        files: Arc<[Entry]>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || files.is_empty() {
            return;
        }
        let revision = self.bucket_revision;
        cx.spawn_in(window, async move |view, cx| {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("选择批量下载目录");
            if let Some(directory) =
                std::env::var_os("S3FS_DOWNLOAD_DIR").filter(|path| !path.is_empty())
            {
                dialog = dialog.set_directory(PathBuf::from(directory));
            }
            let Some(folder) = dialog.pick_folder().await else {
                return;
            };
            let root = folder.path().to_owned();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut names = HashSet::new();
                    let mut destinations = Vec::with_capacity(files.len());
                    for entry in files.iter() {
                        let name = entry.name();
                        anyhow::ensure!(name != "/", "对象名称无法保存为本地文件：{}", entry.key());
                        let path = crate::services::store::local_path(&root, name)?;
                        anyhow::ensure!(
                            names.insert(name.to_uppercase()) && !path.try_exists()?,
                            "批量下载目标存在同名文件：{}",
                            path.display()
                        );
                        destinations.push((entry.clone(), path));
                    }
                    Ok(destinations)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.bucket_revision != revision {
                    return;
                }
                let Some(store) = this.store.clone() else {
                    return;
                };
                match result {
                    Ok(destinations) => {
                        for (entry, path) in destinations {
                            this.start_download(store.clone(), entry, path, cx);
                        }
                    }
                    Err(error) => this.show_error(error, cx),
                }
            });
        })
        .detach();
    }
}
