use super::file_list::FileColumn;
use super::*;

impl Browser {
    #[inline(never)]
    pub(super) fn task_panel(&self, maximized: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut groups = Vec::new();
        let history = self.show_transfer_history;
        let preview = (!history)
            .then(|| self.active_preview_download())
            .flatten()
            .map(|(entry, received)| (entry.name().to_owned(), entry.size(), received));
        let preview_count = usize::from(preview.is_some());
        let mut count = preview_count;
        let is_active = |task: &TransferTask| {
            task.running || task.status.starts_with("已暂停") || task.status.starts_with("等待恢复")
        };
        let history_count = self
            .transfers
            .iter()
            .filter(|task| !is_active(task))
            .count();
        for (index, task) in self.transfers.iter().enumerate().rev() {
            if is_active(task) == history {
                continue;
            }
            let items: Vec<_> = if task.expanded {
                task.items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| history || item.state.lock().unwrap().active())
                    .map(|(index, _)| index)
                    .collect()
            } else {
                Vec::new()
            };
            count += 1 + items.len();
            groups.push((count, index, items));
        }
        let view = cx.weak_entity();
        let list = uniform_list("task-rows", count, move |range, _, cx| {
            view.update(cx, |this, cx| {
                range
                    .map(|row| {
                        if row == 0
                            && let Some((name, total, received)) = &preview
                        {
                            return this.preview_download_row(name, *total, *received, cx);
                        }
                        let group = groups.partition_point(|(end, _, _)| *end <= row);
                        let (_, index, items) = &groups[group];
                        let start = group
                            .checked_sub(1)
                            .map_or(preview_count, |previous| groups[previous].0);
                        let task = &this.transfers[*index];
                        match row - start {
                            0 => this.transfer_group_row(task, cx),
                            item => this.transfer_item_row(task, &task.items[items[item - 1]], cx),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
        .track_scroll(if maximized {
            &self.expanded_task_scroll
        } else {
            &self.task_scroll
        })
        .size_full();
        let expand_label = if maximized {
            "还原任务面板"
        } else {
            "最大化任务面板"
        };
        div()
            .flex()
            .flex_col()
            .min_h_0()
            .when(maximized, |panel| panel.flex_1().min_w_0())
            .when(!maximized, |panel| {
                panel
                    .h_56()
                    .max_h(relative(0.55))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.border)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h_7()
                    .flex_shrink_0()
                    .px_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child("任务")
                            .child(
                                Button::new("transfer-task-tab")
                                    .ghost()
                                    .xsmall()
                                    .label("传输")
                                    .selected(!self.show_sync_tasks && !history)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_sync_tasks = false;
                                        this.show_transfer_history = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("history-task-tab")
                                    .ghost()
                                    .xsmall()
                                    .label("历史")
                                    .selected(!self.show_sync_tasks && history)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_sync_tasks = false;
                                        this.show_transfer_history = true;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("sync-task-tab")
                                    .ghost()
                                    .xsmall()
                                    .label("同步")
                                    .selected(self.show_sync_tasks)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_sync_tasks = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .when(maximized && !self.show_sync_tasks, |row| {
                                row.child(
                                    Button::new("transfer-workers")
                                        .ghost()
                                        .xsmall()
                                        .label("Worker")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_worker_details(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("task-settings")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Settings)
                                    .tooltip("任务配置")
                                    .accessibility_label("打开任务配置")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.tasks_maximized = false;
                                        this.settings_page = Some(if this.show_sync_tasks {
                                            SettingsPage::Sync
                                        } else {
                                            SettingsPage::Transfer
                                        });
                                        this.settings_focus.focus(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("maximize-tasks")
                                    .ghost()
                                    .xsmall()
                                    .icon(if maximized {
                                        IconName::Minimize
                                    } else {
                                        IconName::Maximize
                                    })
                                    .tooltip(expand_label)
                                    .accessibility_label(expand_label)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.tasks_maximized = !this.tasks_maximized;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("close-tasks")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::X)
                                    .tooltip("关闭任务面板")
                                    .accessibility_label("关闭任务面板")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_tasks = false;
                                        this.tasks_maximized = false;
                                        this.tasks_focus.focus(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(self.show_sync_tasks, |panel| {
                panel.child(self.sync_task_list(cx))
            })
            .when(!self.show_sync_tasks, |panel| {
                panel.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .when(history && history_count > 0, |pane| {
                            pane.child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py_1()
                                    .child(
                                        Button::new("clear-transfer-history")
                                            .ghost()
                                            .xsmall()
                                            .label("清空记录")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.transfers.retain(|task| task.running);
                                                cx.notify();
                                            })),
                                    ),
                            )
                        })
                        .when(count == 0, |pane| {
                            pane.child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(if history {
                                        "暂无传输历史"
                                    } else {
                                        "暂无进行中或排队任务"
                                    }),
                            )
                        })
                        .when(count > 0, |pane| {
                            pane.child(div().flex_1().min_h_0().child(list))
                        }),
                )
            })
            .into_any_element()
    }

    fn preview_download_row(
        &self,
        name: &str,
        total: i64,
        received: i64,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .h_10()
            .px_2()
            .gap_2()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(name.to_owned()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if total > 0 {
                                format!(
                                    "预览下载，{} / {}",
                                    size_label(received),
                                    size_label(total)
                                )
                            } else {
                                format!("预览下载，{}", size_label(received))
                            }),
                    ),
            )
            .when(total > 0, |row| {
                row.child(
                    Progress::new("preview-task-progress")
                        .xsmall()
                        .w_16()
                        .flex_shrink_0()
                        .value((100. * received as f32 / total as f32).min(100.))
                        .accessibility_label("预览下载进度"),
                )
            })
            .child(
                Button::new("cancel-preview-download-task")
                    .ghost()
                    .xsmall()
                    .label("取消")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.active_preview_download().is_some() {
                            this.cancel_preview(window, cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn open_worker_details(&self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let mut rows = div()
                .id("worker-details")
                .max_h(rems(22.))
                .overflow_y_scroll();
            if let Some(owner) = owner.upgrade() {
                let this = owner.read(cx);
                let (limit, active) = this.transfer_slots.workers();
                rows = rows
                    .child(Self::worker_row(
                        &WorkerRow::Summary(active.len(), limit),
                        cx,
                    ))
                    .children(
                        active
                            .into_iter()
                            .map(|(id, label)| Self::worker_row(&WorkerRow::Active(id, label), cx)),
                    );
            }
            dialog.title("传输 Worker").child(rows)
        });
    }

    fn worker_row(worker: &WorkerRow, cx: &App) -> AnyElement {
        let row = div()
            .w_full()
            .h_10()
            .px_3()
            .gap_3()
            .flex()
            .items_center()
            .text_sm();
        match worker {
            WorkerRow::Summary(active, limit) => row
                .id("worker-summary")
                .bg(cx.theme().muted)
                .font_weight(FontWeight::SEMIBOLD)
                .child("传输 Worker")
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{active}/{limit} 活跃")),
                ),
            WorkerRow::Active(id, label) => row
                .id(("transfer-worker", *id))
                .child(div().w_20().flex_shrink_0().child(format!("Worker {id}")))
                .child(div().flex_1().min_w_0().truncate().child(label.clone())),
        }
        .into_any_element()
    }

    fn transfer_group_row(&self, task: &TransferTask, cx: &Context<Self>) -> AnyElement {
        let id = task.id;
        let summary = if task.running && task.cancel.is_cancelled() {
            format!("正在取消，{}", task.status)
        } else if task.running
            && task.direction == TransferDirection::Download
            && let Some(item) = task.items.first()
        {
            let bytes = item
                .progress
                .counter
                .load(Ordering::Relaxed)
                .min(item.progress.size);
            format!(
                "{} / {}",
                size_label(bytes as i64),
                size_label(item.progress.size as i64)
            )
        } else {
            task.status.clone()
        };
        div()
            .id(("upload-task", id))
            .h_10()
            .px_1()
            .flex()
            .items_center()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_sm().child(task.target.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}，{summary}", task.direction.label())),
                    ),
            )
            .when(!task.items.is_empty(), |row| {
                row.child(
                    Button::new(("toggle-upload-group", id))
                        .ghost()
                        .xsmall()
                        .label(if task.expanded { "收起" } else { "详情" })
                        .selected(task.expanded)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(task) = this.transfers.iter_mut().find(|task| task.id == id)
                            {
                                task.expanded = !task.expanded;
                                cx.notify();
                            }
                        })),
                )
            })
            .when_some(
                Some(task.saved).filter(|_| {
                    cfg!(windows)
                        && task.direction == TransferDirection::Clipboard
                        && !task.running
                        && task.status.starts_with("已完成")
                }),
                |row, saved| {
                    row.child(
                        Button::new(("publish-clipboard", id))
                            .ghost()
                            .xsmall()
                            .label("复制到剪贴板")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                #[cfg(windows)]
                                this.publish_saved_clipboard(saved, window, cx);
                                #[cfg(not(windows))]
                                let _ = (this, window, cx, saved);
                            })),
                    )
                },
            )
            .when_some(
                Some(task.saved).filter(|_| {
                    !task.running
                        && (task.status.starts_with("已暂停")
                            || task.status.starts_with("等待恢复"))
                }),
                |row, saved| {
                    row.child(
                        Button::new(("resume-transfer", id))
                            .ghost()
                            .xsmall()
                            .label("继续")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resume_saved_transfer(saved, cx)
                            })),
                    )
                },
            )
            .when(
                !task.running
                    && self
                        .output
                        .iter()
                        .any(|entry| entry.transfer_id == Some(id) && entry.failed),
                |row| {
                    row.child(
                        Button::new(("transfer-log", id))
                            .ghost()
                            .xsmall()
                            .label("查看日志")
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.show_transfer_output(id, cx)
                                }),
                            ),
                    )
                },
            )
            .when(task.running, |row| {
                row.child(Self::cancel_transfer_button(
                    ElementId::from(("cancel-upload-group", id)),
                    &task.cancel,
                    "取消整组传输",
                    cx,
                ))
            })
            .into_any_element()
    }

    fn transfer_item_row(
        &self,
        task: &TransferTask,
        item: &TransferItem,
        cx: &Context<Self>,
    ) -> AnyElement {
        let group = task.id;
        let theme = cx.theme();
        let state = item.state.lock().unwrap().clone();
        let file = &item.progress;
        let name = file
            .key
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&file.key);
        let status = match &state {
            TransferState::Queued | TransferState::Sending if item.cancel.is_cancelled() => {
                "正在取消".to_owned()
            }
            TransferState::Queued => "排队中".to_owned(),
            TransferState::Sending if file.sampled >= file.size => "正在确认".to_owned(),
            TransferState::Sending => format!("{}/s", size_label(file.speed as i64)),
            TransferState::Complete => "已完成".to_owned(),
            TransferState::Cancelled => "已取消".to_owned(),
            TransferState::Failed(_) => format!("{}失败", task.direction.label()),
        };
        let detail = if item.directory {
            status.clone()
        } else if matches!(state, TransferState::Sending) {
            format!(
                "{} / {}，{status}",
                size_label(file.sampled as i64),
                size_label(file.size as i64)
            )
        } else {
            format!("{}，{status}", size_label(file.size as i64))
        };
        let tooltip = match &state {
            TransferState::Failed(error) => format!("/{}\n{error}", file.key),
            _ => format!("/{}\n{detail}", file.key),
        };
        div()
            .id((ElementId::from(("upload-item", group)), item.id.to_string()))
            .h_10()
            .flex()
            .items_center()
            .pl_5()
            .pr_1()
            .gap_1()
            .child(
                Icon::new(if item.directory {
                    IconName::Folder
                } else {
                    file_icon(name)
                })
                .size_4()
                .text_color(theme.muted_foreground),
            )
            .child(
                Button::new((
                    ElementId::from(("upload-item-info", group)),
                    item.id.to_string(),
                ))
                .ghost()
                .small()
                .h_9()
                .flex_1()
                .min_w_0()
                .justify_start()
                .tooltip(tooltip)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_start()
                        .child(div().w_full().truncate().child(name.to_owned()))
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_xs()
                                .text_color(if matches!(state, TransferState::Failed(_)) {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                })
                                .child(detail),
                        ),
                ),
            )
            .when(state.active(), |row| {
                row.child(Self::cancel_transfer_button(
                    ElementId::from((
                        ElementId::from(("cancel-upload-item", group)),
                        item.id.to_string(),
                    )),
                    &item.cancel,
                    "取消此项传输",
                    cx,
                ))
            })
            .into_any_element()
    }

    fn cancel_transfer_button(
        id: ElementId,
        token: &CancellationToken,
        label: &'static str,
        cx: &Context<Self>,
    ) -> Button {
        let cancel = token.clone();
        Button::new(id)
            .ghost()
            .xsmall()
            .icon(IconName::X)
            .tooltip(label)
            .accessibility_label(label)
            .disabled(cancel.is_cancelled())
            .on_click(cx.listener(move |_, _, _, cx| {
                cancel.cancel();
                cx.notify();
            }))
    }

    pub(super) fn upload_row(
        &self,
        id: (u64, usize),
        file: &TransferProgress,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let name = file.key.rsplit('/').next().unwrap_or(&file.key);
        let awaiting_server = file.sampled >= file.size;
        let percentage = if awaiting_server {
            100.0
        } else {
            ((file.sampled as f64 / file.size as f64) * 100.0).min(99.0)
        };
        let detail = if awaiting_server {
            "正在确认远端文件".into()
        } else if file.speed >= 1.0 {
            let seconds = ((file.size - file.sampled) as f64 / file.speed).ceil() as u64;
            let eta = if seconds >= 3600 {
                format!("{} 小时 {} 分", seconds / 3600, (seconds % 3600) / 60)
            } else if seconds >= 60 {
                format!("{} 分 {} 秒", seconds / 60, seconds % 60)
            } else {
                format!("{seconds} 秒")
            };
            format!("{}/s，约剩 {eta}", size_label(file.speed as i64))
        } else {
            "正在上传，正在估算".into()
        };
        div()
            .id((ElementId::from(("upload-file", id.0)), id.1.to_string()))
            .on_click(|_, _, cx| cx.stop_propagation())
            .h_7()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .text_sm()
            .bg(theme.muted)
            .child(
                div().w_4().flex_shrink_0().child(
                    Icon::new(file_icon(name))
                        .size_4()
                        .text_color(theme.muted_foreground),
                ),
            )
            .child(
                self.file_column(FileColumn::Name)
                    .truncate()
                    .child(name.to_owned()),
            )
            .child(
                self.file_column(FileColumn::Modified)
                    .text_color(theme.muted_foreground)
                    .child("—"),
            )
            .child(
                self.file_column(FileColumn::Size)
                    .text_right()
                    .text_color(theme.muted_foreground)
                    .child(size_label(file.size.min(i64::MAX as u64) as i64)),
            )
            .child(
                Progress::new((ElementId::from(("upload-progress", id.0)), id.1.to_string()))
                    .xsmall()
                    .w_16()
                    .flex_shrink_0()
                    .value(percentage as f32)
                    .accessibility_label(format!("上传 {name}")),
            )
            .child(div().w_10().text_right().child(format!("{percentage:.0}%")))
            .child(
                div()
                    .w_48()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
    }
}
