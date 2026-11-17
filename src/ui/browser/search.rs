use super::*;

impl Browser {
    pub(super) fn open_search(
        &mut self,
        initial_query: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefix = self
            .preview
            .as_ref()
            .filter(|_| self.show_preview)
            .map_or(self.prefix.as_str(), |preview| {
                parent_prefix(preview.entry.key())
            })
            .to_owned();
        self.open_search_at(initial_query, prefix, false, window, cx);
    }

    pub(super) fn open_search_at(
        &mut self,
        initial_query: &'static str,
        prefix: String,
        folder_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || window.has_active_dialog(cx) {
            return;
        }
        let store = self.store.clone();
        let placeholder = if folder_only {
            format!("在 /{prefix} 中查找文件")
        } else {
            "搜索文件，输入 > 搜索设置".to_owned()
        };
        let state = cx.new(|cx| {
            let mut state = CommandState::new(window, cx);
            state.set_query(initial_query, window, cx);
            state
        });
        let rows = cx.new(|_| Vec::<Entry>::new());
        let load_error = cx.new(|_| EditMessage(None));
        let hide_dot_entries = self.hide_dot_entries;
        if let Some(store) = store {
            let target = format!("{:?} /{prefix:?}", store.bucket());
            let started = Instant::now();
            self.log(false, format!("正在读取搜索范围，{target}"));
            state.update(cx, |state, cx| state.set_loading(true, window, cx));
            let task = self.runtime.spawn({
                let prefix = prefix.clone();
                async move { store.list(&prefix, false).await }
            });
            let job_state = state.clone();
            let job_rows = rows.clone();
            let job_error = load_error.clone();
            cx.spawn_in(window, async move |view, cx| {
                let result = task
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                let _ = cx.update(|window, cx| {
                    let elapsed = started.elapsed().as_secs_f64();
                    let (failed, message) = match result {
                        Ok(mut entries) => {
                            entries.retain(|entry| {
                                !entry.key().ends_with('/')
                                    && (!hide_dot_entries || !is_dot_path(entry.key()))
                            });
                            entries.sort_unstable_by(|a, b| a.key().cmp(b.key()));
                            let count = entries.len();
                            job_rows.update(cx, |rows, cx| {
                                *rows = entries;
                                cx.notify();
                            });
                            (false, format!("搜索范围读取完成，{target}，{count} 个可搜索文件，{elapsed:.2} 秒"))
                        }
                        Err(error) => {
                            job_error.update(cx, |message, cx| {
                                message.0 = Some(format!("{error:#}").into());
                                cx.notify();
                            });
                            (true, format!("搜索范围读取失败，{target}，{elapsed:.2} 秒，{error:#}"))
                        }
                    };
                    job_state.update(cx, |state, cx| state.set_loading(false, window, cx));
                    let _ = view.update(cx, |this, cx| {
                        this.log(failed, message);
                        cx.notify();
                    });
                });
            })
            .detach();
        }
        let owner = cx.weak_entity();
        let revision = self.bucket_revision;
        let focus = state.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let query = state.read(cx).query(cx);
            let query = query.trim_start();
            let settings_query = if folder_only {
                None
            } else {
                query.strip_prefix('>')
            };
            let mut pages = Vec::new();
            let mut files = Vec::new();
            let mut items = Vec::new();
            if let Some(query) = settings_query {
                let query = query.trim().to_lowercase();
                for (page, label, keywords) in [
                    (
                        SettingsPage::Credentials,
                        "连接与凭证",
                        "账号 密钥 s3 credentials",
                    ),
                    (
                        SettingsPage::Appearance,
                        "外观",
                        "深色 浅色 主题 跟随系统 theme",
                    ),
                    (SettingsPage::Preview, "文件预览", "文本 大小上限 preview"),
                    (
                        SettingsPage::Transfer,
                        "传输",
                        "上传 下载 并行 并发 分片 transfer upload download",
                    ),
                    (
                        SettingsPage::DateTime,
                        "日期与时间",
                        "时区 修改时间 timezone",
                    ),
                    (SettingsPage::Storage, "存储容量", "配额 用量 quota"),
                    (
                        SettingsPage::Trash,
                        "回收站",
                        "自动清理 自动删除 保留 trash",
                    ),
                    (SettingsPage::Startup, "启动", "开机 自启 登录 startup"),
                    (SettingsPage::Sync, "文件夹同步", "定时 删除 sync"),
                ] {
                    if label.contains(&query)
                        || keywords.contains(&query)
                        || "设置".contains(&query)
                    {
                        pages.push(page);
                        items.push(
                            CommandItem::new()
                                .label(format!("设置，{label}"))
                                .icon(Icon::new(IconName::Settings)),
                        );
                    }
                }
            } else {
                let query = query.trim().to_lowercase();
                for (index, entry) in rows.read(cx).iter().enumerate() {
                    let label = entry.key().strip_prefix(&prefix).unwrap_or(entry.key());
                    if label.to_lowercase().contains(&query) {
                        files.push(index);
                        items.push(
                            CommandItem::new()
                                .label(label.to_owned())
                                .icon(Icon::new(file_icon(entry.name()))),
                        );
                    }
                }
            }
            let selection = rows.clone();
            let query_owner = owner.clone();
            let owner = owner.clone();
            dialog
                .margin_top(window.rem_size() * 0.5)
                .width(
                    (window.rem_size() * 42.)
                        .min(window.viewport_size().width - window.rem_size() * 2.),
                )
                .overlay(true)
                .overlay_closable(true)
                .close_button(false)
                .p_1()
                .when(settings_query.is_none(), |dialog| {
                    dialog.child(load_error.clone())
                })
                .child(
                    Command::new(&state)
                        .placeholder(placeholder.clone())
                        .max_h(rems(22.))
                        .filterable(false)
                        .items(items)
                        .on_query(move |_, _, cx| {
                            let _ = query_owner.update(cx, |_, cx| cx.notify());
                        })
                        .on_confirm(move |index, window, cx| {
                            if let Some(page) = pages.get(index.row).copied() {
                                window.close_dialog(cx);
                                let _ = owner.update(cx, |this, cx| {
                                    this.tasks_maximized = false;
                                    if page == SettingsPage::Storage {
                                        this.open_storage_settings(window, cx);
                                    } else {
                                        this.open_settings_page(page, cx);
                                    }
                                });
                            } else if let Some(entry) = files
                                .get(index.row)
                                .and_then(|index| selection.read(cx).get(*index))
                                .cloned()
                            {
                                window.close_dialog(cx);
                                let _ = owner.update(cx, |this, cx| {
                                    if this.bucket_revision == revision {
                                        this.close_settings();
                                        this.open_entry(entry, window, cx);
                                    }
                                });
                            }
                        }),
                )
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |state, cx| state.focus(window, cx))
        });
    }
}
