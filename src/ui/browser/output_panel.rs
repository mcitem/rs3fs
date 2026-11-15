use super::*;
use gpui_kit::component::{
    menu::DropdownMenu,
    tab::{Tab, TabBar},
};

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
        if self.output.len() == LOG_HISTORY_LIMIT {
            self.output.pop_back();
        }
        self.output
            .push_front(LogEntry::new(transfer_id, failed, message));
    }

    pub(super) fn show_transfer_output(&mut self, id: u64, cx: &mut Context<Self>) {
        self.show_output = true;
        self.show_output_info = false;
        self.output_transfer = Some(id);
        self.output_failures_only = true;
        cx.notify();
    }

    fn filtered_output(&self) -> impl Iterator<Item = &LogEntry> {
        self.output.iter().filter(|entry| {
            (!self.output_failures_only || entry.failed)
                && self
                    .output_transfer
                    .is_none_or(|id| entry.transfer_id == Some(id))
        })
    }

    pub(super) fn toggle_output(&mut self, cx: &mut Context<Self>) {
        self.show_output = !self.show_output;
        cx.notify();
    }

    pub(super) fn with_output(&self, main: AnyElement, cx: &Context<Self>) -> AnyElement {
        if !self.show_output {
            return main;
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(div().flex().flex_1().min_h_0().min_w_0().child(main))
            .child(self.output_panel(cx))
            .into_any_element()
    }

    #[inline(never)]
    fn output_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let empty = self.filtered_output().next().is_none();
        div()
            .flex()
            .flex_col()
            .h(relative(0.3))
            .min_h(rems(5.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        TabBar::new("output-tabs")
                            .small()
                            .underline()
                            .selected_index(usize::from(self.show_output_info))
                            .child(Tab::new().label("输出"))
                            .child(Tab::new().label("信息"))
                            .on_click(cx.listener(|this, index, _, cx| {
                                this.show_output_info = *index == 1;
                                cx.notify();
                            })),
                    )
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
            .when(!self.show_output_info, |panel| {
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
                                .selected(self.output_failures_only)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.output_failures_only = !this.output_failures_only;
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
            .child(if self.show_output_info {
                self.output_info(cx)
            } else {
                div()
                    .id(SharedString::from(format!(
                        "output-log:{:?}:{}",
                        self.output_transfer, self.output_failures_only
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
                            if self.output_transfer.is_some() || self.output_failures_only {
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
                    .into_any_element()
            })
            .into_any_element()
    }

    fn output_transfer_filter(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.output_transfer;
        let mut ids: Vec<_> = self
            .output
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
                                    this.output_transfer = id;
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
                let running = sync.running() == Some(job.id);
                let schedule = if !job.automatic {
                    "仅手动".into()
                } else if sync.is_quitting() {
                    "自动同步已停止".into()
                } else if running {
                    "运行结束后安排下次同步".into()
                } else if let Some(next) = sync
                    .next_run(job.id)
                    .filter(|next| *next > chrono::Utc::now())
                {
                    format!("下次 {}", self.time_zone.format(next))
                } else {
                    "等待调度".into()
                };
                let last = sync.last_run(job.id);
                div()
                    .mt_1()
                    .child(format!(
                        "{} /{}，{}，{}{}",
                        job.bucket,
                        job.prefix,
                        job.direction.label(),
                        job.folder.display(),
                        if running { "，正在同步" } else { "" },
                    ))
                    .child(div().text_color(theme.muted_foreground).child(schedule))
                    .when_some(last, |row, run| {
                        row.child(div().text_color(theme.muted_foreground).child(format!(
                            "上次 {}，{}，{}",
                            self.time_zone.format(run.finished()),
                            run.outcome().label(),
                            run.message(),
                        )))
                    })
            }))
            .into_any_element()
    }
}
