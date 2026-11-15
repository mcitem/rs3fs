use super::*;

pub(super) struct EtagCheck {
    pub(super) label: String,
    pub(super) percent: Arc<AtomicU64>,
    pub(super) cancel: CancellationToken,
}

impl Browser {
    pub(super) fn select_context_entry(
        &mut self,
        entry: &Entry,
        tree: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = entry.key();
        let selection = if tree {
            &self.tree_multi
        } else {
            &self.object_multi
        };
        if selection.len() > 1 && selection.contains(key) {
            return true;
        }
        if tree {
            self.tree_multi.clear();
            self.tree_range_anchor = None;
            self.tree_selected = Some(key.to_owned());
        } else {
            self.tree_multi.clear();
            self.tree_range_anchor = None;
            self.select_object(key, false, false);
        }
        cx.notify();
        false
    }

    pub(super) fn folder_search_menu(
        menu: PopupMenu,
        prefix: &str,
        owner: &WeakEntity<Self>,
        busy: bool,
    ) -> PopupMenu {
        let prefix = prefix.to_owned();
        let owner = owner.clone();
        menu.item(
            PopupMenuItem::new("在文件夹中查找")
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.open_search_at("", prefix.clone(), true, window, cx);
                    });
                }),
        )
    }

    pub(super) fn tree_create_menu(
        mut menu: PopupMenu,
        prefix: &str,
        owner: &WeakEntity<Self>,
        disabled: bool,
    ) -> PopupMenu {
        for (label, directory) in [("新建文件", false), ("新建文件夹", true)] {
            let prefix = prefix.to_owned();
            let owner = owner.clone();
            menu = menu.item(PopupMenuItem::new(label).disabled(disabled).on_click(
                move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.start_tree_create(prefix.clone(), directory, window, cx);
                    });
                },
            ));
        }
        menu
    }

    pub(super) fn copy_path_menu(menu: PopupMenu, bucket: &str, key: &str) -> PopupMenu {
        let path = format!("/{bucket}/{key}");
        menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        }))
    }

    pub(super) fn entry_edit_menu(
        menu: PopupMenu,
        entry: &Entry,
        bucket: &str,
        owner: &WeakEntity<Self>,
        tree: bool,
        busy: bool,
    ) -> PopupMenu {
        let mut menu = Self::copy_path_menu(menu, bucket, entry.key());
        let rename_owner = owner.clone();
        let rename_entry = entry.clone();
        menu = menu.item(PopupMenuItem::new("重命名").disabled(busy).on_click(
            move |_, window, cx| {
                let _ = rename_owner.update(cx, |this, cx| {
                    this.edit(Edit::Rename(rename_entry.clone(), tree), window, cx)
                });
            },
        ));
        for (label, operation) in [
            ("移动", Edit::Move as fn(Entry) -> Edit),
            ("复制", Edit::Copy),
            ("移入回收站", Edit::Delete),
        ] {
            let owner = owner.clone();
            let entry = entry.clone();
            menu = menu.item(PopupMenuItem::new(label).disabled(busy).on_click(
                move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.edit(operation(entry.clone()), window, cx);
                    });
                },
            ));
        }
        menu
    }

    pub(super) fn batch_entry_menu(
        mut menu: PopupMenu,
        owner: &WeakEntity<Self>,
        tree: bool,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(view) = owner.upgrade() else {
            return menu;
        };
        let view = view.read(cx);
        let busy = view.busy.is_some();
        let revision = view.bucket_revision;
        let locked = view.batch_write_locked(tree);
        let entries = view.batch_entries(tree);
        let files: Arc<[Entry]> = entries
            .iter()
            .copied()
            .filter(|entry| !entry.directory())
            .cloned()
            .collect();
        let bucket = view.store.as_ref().map(Store::bucket).unwrap_or_default();
        let paths = entries
            .iter()
            .map(|entry| format!("/{bucket}/{}", entry.key()))
            .collect::<Vec<_>>()
            .join("\n");
        if !files.is_empty() {
            menu = Self::file_download_menu(menu, owner, files, true, window, cx);
        }
        menu = menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(paths.clone()));
        }));
        for (label, copying) in [("移动", false), ("复制", true)] {
            let owner = owner.clone();
            menu = menu.item(PopupMenuItem::new(label).disabled(busy || locked).on_click(
                move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.bucket_revision == revision {
                            this.choose_batch_transfer_folder(tree, copying, window, cx);
                        }
                    });
                },
            ));
        }
        let owner = owner.clone();
        menu.item(
            PopupMenuItem::new("移入回收站")
                .disabled(busy || locked)
                .on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.bucket_revision == revision {
                            this.confirm_batch_delete(tree, false, window, cx);
                        }
                    });
                }),
        )
    }

    pub(super) fn file_download_menu(
        menu: PopupMenu,
        owner: &WeakEntity<Self>,
        files: Arc<[Entry]>,
        batch: bool,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(view) = owner.upgrade() else {
            return menu;
        };
        let view = view.read(cx);
        let busy = view.busy.is_some();
        let revision = view.bucket_revision;
        let sharing_link = view.sharing_link;
        let download = PopupMenu::build(window, cx, |submenu, _, _| {
            let download_owner = owner.clone();
            let download_files = files.clone();
            let label = "下载到本地…";
            let submenu = submenu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = download_owner.update(cx, |this, cx| {
                    if this.bucket_revision != revision {
                        return;
                    }
                    if batch {
                        this.download_batch(download_files.clone(), window, cx);
                    } else if let Some(entry) = download_files.first() {
                        this.download(entry.clone(), window, cx);
                    }
                });
            }));
            Self::add_download_links(submenu, owner, files, revision, sharing_link)
        });
        menu.item(PopupMenuItem::submenu("下载", download).disabled(busy))
    }

    fn add_download_links(
        mut menu: PopupMenu,
        owner: &WeakEntity<Self>,
        files: Arc<[Entry]>,
        revision: u64,
        disabled: bool,
    ) -> PopupMenu {
        for (duration, minutes) in [
            ("15 分钟", 15),
            ("1 小时", 60),
            ("1 天", 24 * 60),
            ("3 天", 3 * 24 * 60),
            ("7 天", 7 * 24 * 60),
        ] {
            let label = format!("复制下载链接（{duration}）");
            let owner = owner.clone();
            let files = files.clone();
            menu = menu.item(PopupMenuItem::new(label).disabled(disabled).on_click(
                move |_, _, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.bucket_revision == revision {
                            this.share_files(files.clone(), duration, minutes, cx);
                        }
                    });
                },
            ));
        }
        menu
    }

    pub(super) fn inspect_etag(
        &mut self,
        entry: Entry,
        compare: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || entry.directory() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let runtime = self.runtime.clone();
        let slots = self.transfer_slots.clone();
        if compare && self.etag_check.is_some() {
            self.show_notice("已有 ETag 比对任务", cx);
            return;
        }
        let percent = Arc::new(AtomicU64::new(0));
        let cancel = CancellationToken::new();
        if compare {
            self.etag_check = Some(EtagCheck {
                label: entry.name().to_owned(),
                percent: percent.clone(),
                cancel: cancel.clone(),
            });
        }
        self.error = None;
        self.clear_notice();
        cx.spawn_in(window, async move |view, cx| {
            let path = if compare {
                rfd::AsyncFileDialog::new()
                    .set_title("选择要比对的本地文件")
                    .pick_file()
                    .await
                    .map(|file| file.path().to_owned())
            } else {
                None
            };
            let key = entry.key().to_owned();
            let executor = cx.background_executor().clone();
            let progress = percent.clone();
            let token = cancel.clone();
            let mut job = runtime.spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => Ok(None),
                    result = async {
                        if compare && path.is_none() { return Ok(None); }
                        let tag = store.head(&key).await?;
                        let comparison = match path {
                            Some(path) => {
                                let _permit = slots.acquire_worker(&token, format!("ETag 比对，/{key}")).await?;
                                Some(store.compare_etag_progress(&key, &tag, &path, |value| progress.store(value, Ordering::Relaxed)).await?)
                            }
                            None => None,
                        };
                        Ok(Some((tag, comparison)))
                    } => result,
                }
            });
            let result = loop {
                tokio::select! {
                    result = &mut job => break result.map_err(anyhow::Error::from).and_then(|result| result),
                    _ = executor.timer(Duration::from_millis(120)), if compare => {
                        let _ = view.update(cx, |_, cx| cx.notify());
                    }
                }
            };
            let _ = cx.update(|window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if compare { this.etag_check = None; }
                    match result {
                        Ok(Some((tag, comparison))) => {
                            let value = tag.value().to_owned();
                            let title = format!("ETag，{}", entry.name());
                            window.open_dialog(cx, move |dialog, _, _| {
                                let copy = value.clone();
                                dialog.title(title.clone()).child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_3()
                                        .child(div().child(value.clone()))
                                        .children(comparison.clone())
                                        .child(
                                            Button::new("copy-etag").label("复制 ETag").on_click(
                                                move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(copy.clone()),
                                                    )
                                                },
                                            ),
                                        ),
                                )
                            });
                        }
                        Ok(None) => {}
                        Err(error) => this.show_error(error, cx),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    fn share_files(
        &mut self,
        files: Arc<[Entry]>,
        duration_label: &'static str,
        minutes: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.sharing_link || self.busy.is_some() {
            return;
        }
        self.sharing_link = true;
        let count = files.len();
        let revision = self.bucket_revision;
        self.run_background(
            "正在生成下载链接",
            format!(
                "{:?}，{count} 个文件，有效期 {duration_label}",
                store.bucket()
            ),
            async move {
                let mut urls = Vec::with_capacity(count);
                for entry in files.iter() {
                    urls.push(store.share_url(entry.key().to_owned(), minutes).await?);
                }
                Ok(urls.join("\n"))
            },
            move |this, result, cx| {
                this.sharing_link = false;
                if this.bucket_revision != revision {
                    return;
                }
                match result {
                    Ok(url) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(url));
                        this.show_notice(
                            format!("已复制 {count} 条下载链接，有效期 {duration_label}"),
                            cx,
                        );
                    }
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        );
    }
}
