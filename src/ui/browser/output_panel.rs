use super::*;
use gpui_kit::component::menu::DropdownMenu;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum OutputTab {
    #[default]
    Log,
    Info,
    Workers,
}

#[derive(Default)]
pub(super) struct OutputPanelState {
    visible: bool,
    tab: OutputTab,
    failures_only: bool,
    transfer: Option<u64>,
    entries: VecDeque<LogEntry>,
}

impl OutputPanelState {
    pub(super) fn new(entries: VecDeque<LogEntry>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }

    pub(super) fn is_visible(&self) -> bool {
        self.visible
    }

    pub(super) fn has_transfer_failure(&self, id: u64) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.transfer_id == Some(id) && entry.failed)
    }
}

impl OutputTab {
    const BUTTONS: [(Self, &'static str, &'static str); 3] = [
        (Self::Log, "log-output-tab", "输出"),
        (Self::Info, "info-output-tab", "信息"),
        (Self::Workers, "worker-output-tab", "Worker"),
    ];
}

impl Browser {
    pub(super) fn show_error(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        let details = SharedString::from(format!("{error:#}"));
        self.log(true, details.clone());
        self.error = Some(details);
        cx.notify();
    }

    pub(super) fn log(&mut self, failed: bool, message: impl Into<SharedString>) {
        self.push_output(None, failed, message);
    }

    pub(super) fn push_output(
        &mut self,
        transfer_id: Option<u64>,
        failed: bool,
        message: impl Into<SharedString>,
    ) {
        if self.output_state.entries.len() == LOG_HISTORY_LIMIT {
            self.output_state.entries.pop_back();
        }
        self.output_state
            .entries
            .push_front(LogEntry::new(transfer_id, failed, message));
    }

    pub(super) fn show_transfer_output(&mut self, id: u64, cx: &mut Context<Self>) {
        self.output_state.visible = true;
        self.output_state.tab = OutputTab::Log;
        self.output_state.transfer = Some(id);
        self.output_state.failures_only = true;
        cx.notify();
    }

    pub(super) fn show_worker_output(&mut self, cx: &mut Context<Self>) {
        self.output_state.visible = true;
        self.output_state.tab = OutputTab::Workers;
        cx.notify();
    }

    fn filtered_output(&self) -> impl Iterator<Item = &LogEntry> {
        self.output_state.entries.iter().filter(|entry| {
            (!self.output_state.failures_only || entry.failed)
                && self
                    .output_state
                    .transfer
                    .is_none_or(|id| entry.transfer_id == Some(id))
        })
    }

    pub(super) fn toggle_output(&mut self, cx: &mut Context<Self>) {
        self.output_state.visible = !self.output_state.visible;
        cx.notify();
    }

    #[inline(never)]
    pub(super) fn output_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let empty = self.filtered_output().next().is_none();
        div()
            .flex()
            .flex_col()
            .h_full()
            .min_h_0()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .h_7()
                    .px_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(OutputTab::BUTTONS.map(|(tab, id, label)| {
                        Button::new(id)
                            .ghost()
                            .xsmall()
                            .label(label)
                            .selected(self.output_state.tab == tab)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.output_state.tab = tab;
                                cx.notify();
                            }))
                    }))
                    .child(div().flex_1())
                    .child(
                        Button::new("close-output")
                            .ghost()
                            .xsmall()
                            .icon(IconName::X)
                            .tooltip("关闭输出")
                            .accessibility_label("关闭输出")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_output(cx))),
                    ),
            )
            .when(self.output_state.tab == OutputTab::Log, |panel| {
                panel.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py_1()
                        .child(self.output_transfer_filter(cx))
                        .child(
                            Button::new("output-failures")
                                .ghost()
                                .xsmall()
                                .label("仅看失败")
                                .selected(self.output_state.failures_only)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.output_state.failures_only =
                                        !this.output_state.failures_only;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("copy-output")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Copy)
                                .tooltip("复制当前显示的日志")
                                .accessibility_label("复制当前显示的日志")
                                .disabled(empty)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let text = this
                                        .filtered_output()
                                        .map(|entry| entry.line(this.time_zone))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                })),
                        ),
                )
            })
            .child(match self.output_state.tab {
                OutputTab::Info => self.output_info(cx),
                OutputTab::Workers => self.output_workers(cx),
                OutputTab::Log => div()
                    .id(SharedString::from(format!(
                        "output-log:{:?}:{}",
                        self.output_state.transfer, self.output_state.failures_only
                    )))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .when(empty, |list| {
                        list.child(div().text_color(theme.muted_foreground).child(
                            if self.output_state.transfer.is_some()
                                || self.output_state.failures_only
                            {
                                "没有符合筛选条件的日志"
                            } else {
                                "暂无日志"
                            },
                        ))
                    })
                    .children(self.filtered_output().map(|entry| {
                        div()
                            .py_1()
                            .when(entry.failed, |row| row.text_color(theme.danger))
                            .child(entry.line(self.time_zone))
                    }))
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn output_workers(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (_, workers) = self.transfer_slots.workers();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .text_sm()
            .child(
                div()
                    .id("output-workers")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(workers.is_empty(), |list| {
                        list.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_color(theme.muted_foreground)
                                .child("暂无活动 Worker"),
                        )
                    })
                    .children(workers.into_iter().map(|worker| {
                        let speed = worker.speed.map_or_else(
                            || "—".to_owned(),
                            |speed| format!("{}/s", size_label(speed as i64)),
                        );
                        let (detail, percent) = match worker.progress {
                            Some((transferred, Some(total))) => {
                                let percent = if total == 0 {
                                    100.0
                                } else {
                                    100.0 * transferred.min(total) as f32 / total as f32
                                };
                                (
                                    format!(
                                        "{} / {}",
                                        size_label(transferred as i64),
                                        size_label(total as i64)
                                    ),
                                    Some(percent),
                                )
                            }
                            Some((transferred, None)) => {
                                (format!("已传输 {}", size_label(transferred as i64)), None)
                            }
                            None => ("处理中".into(), None),
                        };
                        div()
                            .id(("transfer-worker", worker.id))
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(format!("Worker {}", worker.id)),
                                    )
                                    .child(div().flex_1().min_w_0().child(worker.label))
                                    .child(div().flex_shrink_0().text_right().child(speed)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .mt_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .when_some(percent, |row, percent| {
                                        row.child(
                                            Progress::new(("worker-progress", worker.id))
                                                .xsmall()
                                                .w_20()
                                                .flex_shrink_0()
                                                .value(percent)
                                                .accessibility_label(format!(
                                                    "Worker {} 子任务进度",
                                                    worker.id
                                                )),
                                        )
                                    })
                                    .child(detail),
                            )
                    })),
            )
            .into_any_element()
    }

    fn output_transfer_filter(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.output_state.transfer;
        let mut ids: Vec<_> = self
            .output_state
            .entries
            .iter()
            .filter_map(|entry| entry.transfer_id)
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.dedup();
        let view = cx.weak_entity();
        Button::new("output-transfer-filter")
            .ghost()
            .xsmall()
            .label(selected.map_or_else(|| "全部日志".into(), |id| format!("传输 #{id}")))
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                std::iter::once(None)
                    .chain(ids.iter().copied().map(Some))
                    .fold(menu.scrollable(true), |menu, id| {
                        let view = view.clone();
                        menu.item(
                            PopupMenuItem::new(
                                id.map_or_else(|| "全部日志".into(), |id| format!("传输 #{id}")),
                            )
                            .checked(selected == id)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.output_state.transfer = id;
                                    cx.notify();
                                });
                            }),
                        )
                    })
            })
    }

    fn output_info(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let sync = self.sync.read(cx);
        let endpoint = self.connection_settings.endpoint();
        div()
            .id("output-info")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .text_sm()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("当前连接"))
            .when(self.server.is_none(), |section| section.child("未连接"))
            .when_some(self.server.as_ref(), |section, server| {
                section
                    .child(format!(
                        "Endpoint，{}",
                        if endpoint.is_empty() {
                            "SDK 自动解析"
                        } else {
                            endpoint
                        }
                    ))
                    .child(format!(
                        "Bucket，{}",
                        self.store.as_ref().map_or("未选择", Store::bucket)
                    ))
                    .child(format!("Region，{}", server.region().unwrap_or("未知")))
                    .child(if endpoint.is_empty() {
                        "寻址方式，SDK 默认"
                    } else {
                        "寻址方式，路径式"
                    })
            })
            .child(
                div()
                    .mt_3()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("后台同步"),
            )
            .when(sync.is_quitting(), |section| section.child("正在退出"))
            .when(
                sync.jobs().is_empty() && sync.config_error().is_none(),
                |section| section.child("暂无同步任务"),
            )
            .children(
                sync.config_error()
                    .into_iter()
                    .chain(sync.history_error())
                    .map(|error| div().text_color(theme.danger).child(error.to_owned())),
            )
            .children(sync.jobs().iter().map(|job| {
                let next = sync.next_run(job.id).filter(|next| {
                    *next > chrono::Utc::now()
                        && !sync.is_paused()
                        && !sync.is_job_paused(job.id)
                        && !sync.is_quitting()
                });
                div()
                    .mt_1()
                    .child(format!(
                        "{} /{}，{}，{}",
                        job.bucket,
                        job.prefix,
                        job.direction.label(),
                        job.folder.display(),
                    ))
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(sync.task_status(job)),
                    )
                    .when_some(next, |row, next| {
                        row.child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("下次定时探测 {}", self.time_zone.format(next),)),
                        )
                    })
                    .when_some(sync.last_run(job.id), |row, run| {
                        row.child(div().text_color(theme.muted_foreground).child(format!(
                            "上次 {}，{}",
                            self.time_zone.format(run.finished()),
                            run.outcome().label(),
                        )))
                    })
            }))
            .into_any_element()
    }
}
