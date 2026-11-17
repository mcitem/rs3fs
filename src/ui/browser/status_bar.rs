use super::*;

impl Browser {
    fn transfer_cancel_menu(&self, mut menu: PopupMenu, cx: &Context<Self>) -> PopupMenu {
        for task in self.transfers.iter().rev().filter(|task| task.running) {
            let cancel = task.cancel.clone();
            menu = menu.item(
                PopupMenuItem::new(format!("取消 {}", task.target))
                    .disabled(cancel.is_cancelled())
                    .on_click(move |_, _, _| cancel.cancel()),
            );
        }
        if self.active_preview_download().is_some() {
            let owner = cx.weak_entity();
            menu = menu.item(
                PopupMenuItem::new("取消预览下载").on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.active_preview_download().is_some() {
                            this.cancel_preview(window, cx);
                        }
                    });
                }),
            );
        }
        if let Some(cancel) = self.sync.read(cx).cancellation().cloned() {
            menu = menu.item(
                PopupMenuItem::new("取消文件夹同步")
                    .disabled(cancel.is_cancelled())
                    .on_click(move |_, _, _| cancel.cancel()),
            );
        }
        if let Some(check) = &self.etag_check {
            let cancel = check.cancel.clone();
            menu = menu.item(
                PopupMenuItem::new("取消 ETag 比对")
                    .disabled(cancel.is_cancelled())
                    .on_click(move |_, _, _| cancel.cancel()),
            );
        }
        menu
    }

    pub(super) fn status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let tasks_maximized = self.show_tasks && self.tasks_maximized;
        let storage_visible = !tasks_maximized && self.settings_page == Some(SettingsPage::Storage);
        let trash_visible = !tasks_maximized
            && self.trash.is_some()
            && !self.show_preview
            && self.settings_page.is_none();
        let settings_visible =
            !tasks_maximized && (self.settings_page.is_some() || self.server.is_none());
        let active = self.transfers.iter().filter(|task| task.running).count()
            + usize::from(self.active_preview_download().is_some())
            + usize::from(self.busy.is_some())
            + usize::from(self.sync.read(cx).running().is_some());
        let workspace_activity = if self.opening_bucket.is_some() {
            Some("正在打开存储桶")
        } else if self.loading_folders.contains(&self.prefix) {
            Some("正在读取目录")
        } else if self.trash.is_some() && self.trash_busy() {
            Some("正在处理回收站")
        } else if self.refreshing_listings {
            Some("正在刷新目录")
        } else if self.refreshing_buckets {
            Some("正在刷新存储桶")
        } else if self
            .write_scopes
            .values()
            .any(|scope| scope.connection == self.connection_revision)
        {
            Some("正在处理文件")
        } else {
            None
        };
        let owner = cx.weak_entity();
        let cancel_menu = move |menu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            if let Some(owner) = owner.upgrade() {
                // Build from the task snapshot at the time of the right click.
                owner.update(cx, |this, cx| this.transfer_cancel_menu(menu, cx))
            } else {
                menu
            }
        };
        let cancellable = self.transfers.iter().any(|task| task.running)
            || self.active_preview_download().is_some()
            || self.sync.read(cx).running().is_some()
            || self.etag_check.is_some();
        let task = self.transfers.iter().rev().find(|task| task.running);
        let download = self.transfers.iter().rev().find(|task| {
            task.running
                && task.direction == TransferDirection::Download
                && !task.cancel.is_cancelled()
        });
        let download_progress = download
            .and_then(|task| task.items.first())
            .map(|item| &item.progress);
        let (upload_speed, download_speed) = self.transfer_slots.direction_speeds();
        let ordinary_speed = |direction| {
            self.transfers
                .iter()
                .any(|task| task.running && task.direction == direction)
                .then_some(0)
        };
        let upload_speed = upload_speed.or_else(|| ordinary_speed(TransferDirection::Upload));
        let download_speed = download_speed.or_else(|| ordinary_speed(TransferDirection::Download));
        let message = self
            .busy
            .clone()
            .or_else(|| {
                let service = self.sync.read(cx);
                service.running().map(|_| {
                    if service.executing().is_some() {
                        "正在同步文件夹".into()
                    } else {
                        "正在预览同步".into()
                    }
                })
            })
            .or_else(|| {
                let task = task?;
                if task.cancel.is_cancelled() {
                    Some("正在取消传输".into())
                } else if task.direction == TransferDirection::Download
                    && download_progress.is_some()
                {
                    None
                } else {
                    Some(task.status.clone().into())
                }
            })
            .or_else(|| {
                self.active_preview_download()
                    .map(|_| "正在下载预览".into())
            })
            .or_else(|| self.etag_check.as_ref().map(|_| "正在比对 ETag".into()))
            .or_else(|| workspace_activity.map(Into::into))
            .or_else(|| self.notice.clone());
        StatusBar::new()
            .flex_shrink_0()
            .left(
                // The status bar clips its regions. Leave room for the button's outer focus ring.
                div().p(px(3.)).child(
                    Button::new("tasks")
                        .ghost()
                        .xsmall()
                        .icon(IconName::List)
                        .label(if active == 0 {
                            "任务".to_owned()
                        } else {
                            format!("任务 {active}")
                        })
                        .selected(self.show_tasks)
                        .track_focus(&self.tasks_focus)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_tasks = !this.show_tasks;
                            this.tasks_maximized = false;
                            cx.notify();
                        })),
                ),
            )
            .left(
                div().p(px(3.)).child(
                    Button::new("toggle-output")
                        .ghost()
                        .xsmall()
                        .icon(IconName::FileTerminal)
                        .selected(self.output_state.is_visible())
                        .tooltip("输出（Ctrl+`）")
                        .accessibility_label("切换输出面板")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_output(cx))),
                ),
            )
            .when_some(
                upload_speed,
                |bar, speed| {
                    bar.left(
                        div()
                            .id("status-upload")
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(Spinner::new().small())
                            .child(format!("上传 {}/s", size_label(speed)))
                            .context_menu(cancel_menu.clone()),
                    )
                },
            )
            .when_some(
                download_speed,
                |bar, speed| {
                    bar.left(
                        div()
                            .id("status-download")
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(Spinner::new().small())
                            .child(format!("下载 {}/s", size_label(speed)))
                    .when_some(download_progress, |row, file| {
                        let bytes = format!(
                            "{} / {}",
                            size_label(file.sampled as i64),
                            size_label(file.size as i64)
                        );
                        let percent = if file.size == 0 { 0.0 } else { file.percent() };
                        row.child(
                            Progress::new("status-download-progress")
                                .xsmall()
                                .w_16()
                                .flex_shrink_0()
                                .value(percent)
                                .accessibility_label(format!("下载进度 {bytes}")),
                        )
                        .child(div().truncate().child(bytes))
                    })
                            .context_menu(cancel_menu.clone()),
                    )
                },
            )
            .child(
                div()
                    .id("status-message")
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .when(message.is_some() && (self.busy.is_some() || cancellable || workspace_activity.is_some()), |row| {
                        row.child(Spinner::new().small())
                    })
                    .children(message)
                    .map(|row| {
                        if cancellable {
                            row.context_menu(cancel_menu).into_any_element()
                        } else {
                            row.into_any_element()
                        }
                    }),
            )
            .right(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p(px(3.))
                    .when_some(self.etag_check.as_ref(), |row, check| {
                        row.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .max_w_32()
                                        .truncate()
                                        .child(format!("比对 {}", check.label)),
                                )
                                .child(
                                    Progress::new("etag-progress")
                                        .xsmall()
                                        .w_16()
                                        .value(check.percent.load(Ordering::Relaxed) as f32),
                                )
                                .child(
                                    Button::new("cancel-etag")
                                        .ghost()
                                        .xsmall()
                                        .label("取消")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(check) = &this.etag_check {
                                                check.cancel.cancel();
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when(
                        self.store.is_some()
                            && !tasks_maximized
                            && !settings_visible
                            && !trash_visible
                            && !self.show_preview,
                        |row| {
                            let count = self.rows().len();
                            let selected = if self.object_multi.is_empty() {
                                usize::from(self.selection().is_some())
                            } else {
                                self.object_multi.len()
                            };
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if selected == 0 {
                                        format!("{count}项")
                                    } else {
                                        format!("{count}项，已选{selected}项")
                                    }),
                            )
                        },
                    )
                    .when(
                        self.store.is_some()
                            && (self.bucket_usage.is_some() || self.server_capacity.is_some()),
                        |row| {
                            row.child(
                                Button::new("bucket-capacity")
                                    .ghost()
                                    .xsmall()
                                    .label(self.capacity_label())
                                    .selected(storage_visible)
                                    .when(self.usage_scan.is_some(), |button| {
                                        button.child(Spinner::new().small())
                                    })
                                    .tooltip(if self.usage_scan.is_some() {
                                        "正在更新用量，当前显示上一次结果"
                                    } else if self.quota_query_failed {
                                        "更新失败，当前显示上一次结果"
                                    } else {
                                        "查看当前桶的用量和配额"
                                    })
                                    .disabled(self.busy.is_some())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_storage_settings(window, cx);
                                    })),
                            )
                        },
                    )
                    .child(
                        Button::new("open-trash")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Trash)
                            .label("回收站")
                            .selected(trash_visible)
                            .disabled(self.store.is_none() || self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let toggle = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                                    this.close_settings();
                                    this.tasks_maximized = false;
                                    this.close_preview();
                                    if trash_visible {
                                        this.trash = None;
                                        this.list_focus.focus(window, cx);
                                        this.refresh(cx);
                                    } else if this.trash.is_none() {
                                        this.open_trash(cx);
                                    } else {
                                        cx.notify();
                                    }
                                };
                                if !this.confirm_preview_leave(cx, toggle) {
                                    toggle(this, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("settings")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings)
                            .tooltip("设置")
                            .accessibility_label("设置")
                            .selected(settings_visible)
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.tasks_maximized = false;
                                if settings_visible && this.server.is_some() {
                                    this.close_settings();
                                    this.tree_focus.focus(window, cx);
                                } else {
                                    let page =
                                        this.settings_page.get_or_insert(SettingsPage::Credentials);
                                    if *page == SettingsPage::Credentials {
                                        this.profile_name
                                            .read(cx)
                                            .focus_handle(cx)
                                            .focus(window, cx);
                                    } else {
                                        this.settings_focus.focus(window, cx);
                                    }
                                }
                                cx.notify();
                            })),
                    ),
            )
    }
}
