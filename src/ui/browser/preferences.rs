use super::*;

impl Browser {
    pub(super) fn open_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.settings_page = Some(page);
        #[cfg(windows)]
        if page == SettingsPage::Preview {
            self.update_preview_cache(false, cx);
        }
        cx.notify();
    }

    #[cfg(windows)]
    fn update_preview_cache(&mut self, clear: bool, cx: &mut Context<Self>) {
        if self.preview_cache_busy {
            return;
        }
        self.preview_cache_busy = true;
        let job = self
            .runtime
            .spawn_blocking(move || crate::ui::file_viewer::cache_usage(clear));
        cx.spawn(async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = view.update(cx, |this, cx| {
                this.preview_cache_busy = false;
                match result {
                    Ok(usage) => {
                        this.preview_cache_usage = Some(usage.total);
                        if clear {
                            this.clear_thumbnail_memory(cx);
                            let status = if usage.failed {
                                "部分缓存暂时无法清理"
                            } else if usage.active {
                                "正在使用的预览已保留"
                            } else {
                                "预览缓存已清理"
                            };
                            this.show_notice(
                                format!("{status}，释放 {}", size_label(usage.removed as i64)),
                                cx,
                            );
                        }
                    }
                    Err(error) => this.show_error(error.context("无法读取或清理预览缓存"), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_preview_settings(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            let limit: u32 = self
                .preview_limit_input
                .read(cx)
                .value()
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("请输入大于 0 的整数。"))?;
            ensure!(limit > 0, "请输入大于 0 的整数。");
            PreviewSettings::save(limit)?;
            self.preview_limit_mb = limit;
            Ok(())
        })();
        self.settings_saved(result, "预览设置已保存", cx);
    }

    pub(super) fn close_settings(&mut self) {
        if self.settings_page.take().is_some() {
            self.clear_notice();
        }
    }

    pub(super) fn clear_notice(&mut self) {
        self.notice = None;
        self.notice_timeout = None;
    }

    pub(super) fn show_notice(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some(message.into());
        let executor = cx.background_executor().clone();
        self.notice_timeout = Some(cx.spawn(async move |view, cx| {
            executor.timer(Duration::from_secs(4)).await;
            let _ = view.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn settings_saved(
        &mut self,
        result: Result<()>,
        notice: &'static str,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.error = None;
                self.log(false, notice);
                self.show_notice(notice, cx);
            }
            Err(error) => self.show_error(error, cx),
        }
    }

    #[inline(never)]
    pub(super) fn settings_workspace(&self, cx: &Context<Self>) -> AnyElement {
        let page = self.settings_page.unwrap_or(SettingsPage::Credentials);
        let theme = cx.theme();
        let busy = self.busy.is_some();
        let body = match page {
            SettingsPage::Credentials => self.credentials_settings(cx),
            SettingsPage::Appearance => self.appearance_settings(cx),
            SettingsPage::Preview => self.preview_settings(cx),
            SettingsPage::Transfer => self.transfer_settings(cx),
            SettingsPage::Storage => self.storage_settings(cx),
            SettingsPage::Trash => self.trash_settings(cx),
            SettingsPage::Startup => self.startup_settings(cx),
            SettingsPage::Sync => self.sync_settings(cx),
            SettingsPage::DateTime => self.date_time_settings(cx),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.settings_focus)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h_10()
                    .flex_shrink_0()
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(IconName::Settings).size_4())
                            .child("设置"),
                    )
                    .when(self.server.is_some(), |header| {
                        header.child(
                            Button::new("close-settings")
                                .ghost()
                                .xsmall()
                                .icon(IconName::X)
                                .accessibility_label("关闭设置")
                                .tooltip("关闭设置")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_settings();
                                    this.tree_focus.focus(window, cx);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(rems(17.))
                            .min_h_0()
                            .flex_shrink_0()
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .id("settings-sidebar")
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p_2()
                                    .gap_1()
                                    .children(
                                        [
                                            (
                                                SettingsPage::Credentials,
                                                "settings-credentials",
                                                "连接与凭证",
                                            ),
                                            (
                                                SettingsPage::Appearance,
                                                "settings-appearance",
                                                "外观",
                                            ),
                                            (SettingsPage::Preview, "settings-preview", "文件预览"),
                                            (SettingsPage::Transfer, "settings-transfer", "传输"),
                                            (
                                                SettingsPage::DateTime,
                                                "settings-time-zone",
                                                "日期与时间",
                                            ),
                                            (SettingsPage::Storage, "settings-storage", "存储容量"),
                                            (SettingsPage::Trash, "settings-trash", "回收站"),
                                            (SettingsPage::Startup, "settings-startup", "启动"),
                                            (SettingsPage::Sync, "settings-sync", "文件夹同步"),
                                        ]
                                        .map(
                                            |(item, id, label)| {
                                                Button::new(id)
                                                    .ghost()
                                                    .small()
                                                    .w_full()
                                                    .justify_start()
                                                    .accessibility_label(label)
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .truncate()
                                                            .text_left()
                                                            .child(label),
                                                    )
                                                    .selected(item == page)
                                                    .disabled(busy)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.tasks_maximized = false;
                                                            if item == SettingsPage::Storage {
                                                                this.open_storage_settings(
                                                                    window, cx,
                                                                );
                                                                return;
                                                            }
                                                            this.open_settings_page(item, cx);
                                                        },
                                                    ))
                                            },
                                        ),
                                    ),
                            )
                            .when(self.show_tasks && !self.tasks_maximized, |sidebar| {
                                sidebar.child(self.task_panel(false, cx))
                            }),
                    )
                    .child(self.with_output(
                        if self.show_tasks && self.tasks_maximized {
                            self.task_panel(true, cx)
                        } else {
                            div()
                                .id(("settings-body", page as usize))
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .overflow_y_scroll()
                                .px_6()
                                .py_5()
                                .child(div().w_full().max_w(rems(42.)).child(body))
                                .into_any_element()
                        },
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn appearance_settings(&self, cx: &Context<Self>) -> AnyElement {
        use crate::ui::appearance::Appearance;
        let current = *cx.global::<Appearance>();
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("外观"),
            )
            .child(
                Form::horizontal()
                    .small()
                    .child(
                        Field::new().label("主题颜色").child(
                            div().flex().items_center().gap_1().children(
                                [
                                    (Appearance::Light, "light", "浅色"),
                                    (Appearance::Dark, "dark", "深色"),
                                    (Appearance::System, "system", "跟随系统"),
                                ]
                                .map(|(mode, id, label)| {
                                    Button::new(id)
                                        .small()
                                        .label(label)
                                        .outline()
                                        .when(current == mode, |button| button.primary())
                                        .selected(current == mode)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let result =
                                                crate::ui::appearance::set(mode, window, cx);
                                            this.settings_saved(result, "主题已保存", cx);
                                        }))
                                }),
                            ),
                        ),
                    )
                    .child(
                        Field::new()
                            .label("隐藏点文件和点目录")
                            .description("在文件树、文件列表和搜索中隐藏以点开头的文件与目录。")
                            .child(
                                Switch::new("hide-dot-entries")
                                    .accessibility_label("隐藏点文件和点目录")
                                    .checked(self.hide_dot_entries)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        let result = DotEntrySettings::save(*checked);
                                        if result.is_ok() {
                                            this.hide_dot_entries = *checked;
                                            if *checked {
                                                if !this.has_pending_text_preview(cx)
                                                    && this.preview.as_ref().is_some_and(
                                                        |preview| is_dot_path(preview.entry.key()),
                                                    )
                                                {
                                                    this.close_preview();
                                                }
                                                if this
                                                    .tree_create
                                                    .as_ref()
                                                    .is_some_and(|draft| is_dot_path(&draft.prefix))
                                                {
                                                    this.tree_create = None;
                                                }
                                            }
                                            let mut prefix = this.prefix.as_str();
                                            while *checked && is_dot_path(prefix) {
                                                prefix = parent_prefix(prefix);
                                            }
                                            if prefix != this.prefix {
                                                this.set_location(prefix.to_owned(), cx);
                                            } else {
                                                this.rebuild_tree();
                                                if this
                                                    .tree_selected
                                                    .as_deref()
                                                    .is_some_and(|key| *checked && is_dot_path(key))
                                                {
                                                    this.tree_selected = Some(this.prefix.clone());
                                                }
                                                if this.selection().is_none() {
                                                    this.selected = None;
                                                }
                                            }
                                        }
                                        this.settings_saved(result, "外观设置已保存", cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn date_time_settings(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("日期与时间"),
            )
            .child(
                Form::horizontal().small().child(
                    Field::new()
                        .label("显示时区")
                        .description("留空跟随系统。可输入 Asia/Shanghai、Asia/Tokyo 或 UTC。")
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().w_64().min_w_0().child(
                                    Input::new(&self.time_zone_input).aria_label("显示时区"),
                                ))
                                .child(Button::new("save-time-zone").label("保存").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        let result = (|| {
                                            let zone = DisplayTimeZone::parse(
                                                &this.time_zone_input.read(cx).value(),
                                            )?;
                                            zone.save()?;
                                            this.time_zone = zone;
                                            Ok(())
                                        })();
                                        this.settings_saved(result, "显示时区已保存", cx);
                                    }),
                                )),
                        ),
                ),
            )
            .into_any_element()
    }

    fn preview_settings(&self, cx: &Context<Self>) -> AnyElement {
        let view = div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("文件预览"),
            )
            .child(
                Form::horizontal().small().child(
                    Field::new()
                        .label("文本预览上限")
                        .description("PDF 和图片最多 100 MB。")
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div().w_24().child(
                                        Input::new(&self.preview_limit_input)
                                            .aria_label("文本预览大小上限（MB）"),
                                    ),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("MB"),
                                )
                                .child(
                                    Button::new("save-preview-settings").label("保存").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.save_preview_settings(cx)
                                        }),
                                    ),
                                ),
                        ),
                ),
            );
        #[cfg(windows)]
        let view = view.child(
            Form::horizontal().small().child(
                Field::new()
                    .label("预览磁盘缓存")
                    .description("清理闲置缓存，正在使用的预览会保留。")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(if self.preview_cache_busy {
                                "正在检查…".to_owned()
                            } else {
                                self.preview_cache_usage.map_or_else(
                                    || "占用未知".into(),
                                    |bytes| size_label(bytes as i64),
                                )
                            })
                            .child(
                                Button::new("clear-preview-cache")
                                    .small()
                                    .label("清理缓存")
                                    .disabled(self.preview_cache_busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.update_preview_cache(true, cx)
                                    })),
                            ),
                    ),
            ),
        );
        view.into_any_element()
    }

    fn transfer_settings(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("传输"))
            .child(
                Form::horizontal().small()
                    .child(
                        Field::new()
                            .label("传输并发数")
                            .description("上传、下载、预览和同步共享此上限。")
                            .child(div().w_24().child(Input::new(&self.transfer_parallelism_input)
                                .aria_label("传输并发数"))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("save-transfer-settings").label("保存").on_click(
                        cx.listener(|this, _, _, cx| this.save_transfer_settings(cx)),
                    ))
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                        format!("可设为 1–{MAX_TRANSFER_PARALLELISM}。降低上限时，进行中的请求会先完成。"),
                    )),
            )
            .into_any_element()
    }

    fn save_transfer_settings(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            let limit = self
                .transfer_parallelism_input
                .read(cx)
                .value()
                .trim()
                .parse()
                .map_err(|_| {
                    anyhow::anyhow!("传输并发数请输入 1 到 {MAX_TRANSFER_PARALLELISM} 之间的整数。")
                })?;
            TransferSettings::save(limit)?;
            self.transfer_slots.set_limit(limit);
            Ok(())
        })();
        self.settings_saved(result, "传输设置已保存", cx);
    }

    fn startup_settings(&self, cx: &Context<Self>) -> AnyElement {
        use crate::services::startup::Registration;
        let current = self.startup.as_ref().ok() == Some(&Registration::Current);
        let different = self.startup.as_ref().ok() == Some(&Registration::DifferentCommand);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("启动"),
            )
            .child(
                Form::horizontal().small().child(
                    Field::new()
                        .label("登录启动")
                        .description("登录 Windows 后在托盘运行同步。点击托盘图标打开主窗口。")
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_2()
                                .when(different, |view| {
                                    view.child("已注册的启动命令与当前程序不同。")
                                })
                                .children(self.startup.as_ref().err().cloned())
                                .child(
                                    div().child(
                                        Button::new("toggle-startup")
                                            .label(if different {
                                                "更新登录启动路径"
                                            } else if current {
                                                "关闭登录启动"
                                            } else {
                                                "开启登录启动"
                                            })
                                            .disabled(!cfg!(windows))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let result =
                                                    crate::services::startup::set(!current);
                                                this.startup =
                                                    crate::services::startup::registered()
                                                        .map_err(|error| error.to_string().into());
                                                this.settings_saved(result, "启动设置已更新", cx);
                                            })),
                                    ),
                                )
                                .when(different, |view| {
                                    view.child(
                                        div().child(
                                            Button::new("disable-stale-startup")
                                                .ghost()
                                                .label("关闭登录启动")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    let result =
                                                        crate::services::startup::set(false);
                                                    this.startup =
                                                        crate::services::startup::registered()
                                                            .map_err(|error| {
                                                                error.to_string().into()
                                                            });
                                                    this.settings_saved(
                                                        result,
                                                        "启动设置已更新",
                                                        cx,
                                                    );
                                                })),
                                        ),
                                    )
                                }),
                        ),
                ),
            )
            .into_any_element()
    }
}
