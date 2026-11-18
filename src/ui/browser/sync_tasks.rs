use super::*;
use crate::services::sync::{Direction, PlanItem, SyncJob};
use gpui_kit::component::{
    clipboard::Clipboard,
    notification::Notification,
    tab::{Tab, TabBar},
};
use std::collections::BTreeMap;

const SYNC_PREVIEW_PAGE_SIZE: usize = 20;

#[derive(Default)]
struct SyncPreviewState {
    page: usize,
    conflicts_only: bool,
}

#[derive(Default)]
struct SyncConflictDialog {
    page: usize,
    choices: BTreeMap<String, (PlanItem, ConflictChoice)>,
    opening: bool,
    open_error: Option<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncDetailTab {
    Overview,
    Policy,
    Result,
}

impl SyncDetailTab {
    const ALL: [(Self, &'static str); 3] = [
        (Self::Overview, "概况"),
        (Self::Policy, "同步策略"),
        (Self::Result, "最近结果"),
    ];
}

impl Browser {
    pub(super) fn sync_task_list(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        let service = self.sync.read(cx);
        let theme = cx.theme();
        div()
            .id("sync-task-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .when(service.is_quitting(), |list| {
                list.child(div().px_3().py_2().text_sm().child("本轮同步结束后退出"))
            })
            .children(
                [
                    service.config_error(),
                    service.history_error(),
                    service.pause_error(),
                ]
                .into_iter()
                .flatten()
                .map(|error| {
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error.to_owned())
                }),
            )
            .when(
                service.jobs().is_empty() && service.config_error().is_none(),
                |list| {
                    list.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("暂无同步任务。"),
                    )
                },
            )
            .children(
                service
                    .jobs()
                    .iter()
                    .map(|job| self.sync_task_row(job, service, compact, cx)),
            )
    }

    fn sync_task_row(
        &self,
        job: &crate::services::sync::SyncJob,
        service: &SyncService,
        compact: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = job.id;
        let key = SharedString::from(id.to_string());
        let profile = self.profiles.get(job.profile);
        let running = service.running() == Some(id);
        let folder = job
            .folder
            .file_name()
            .unwrap_or(job.folder.as_os_str())
            .to_string_lossy()
            .into_owned();
        let last_run = service.last_run(id);
        let status = if profile.is_none() {
            "缺少连接凭证".to_owned()
        } else {
            service.task_status(job)
        };
        div()
            .id((ElementId::from("sync-task"), key.clone()))
            .flex()
            .items_center()
            .when(compact, |row| row.flex_col().items_stretch())
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .text_sm()
            .child(
                div()
                    .when(!compact, |summary| summary.flex_1())
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(folder),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(
                                if profile.is_none()
                                    || (!running
                                        && last_run.is_some_and(|run| {
                                            run.outcome().has_issue()
                                                && !matches!(
                                                run.outcome(),
                                                crate::services::sync_service::Outcome::Conflict
                                            )
                                        }))
                                {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                },
                            )
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(compact, |actions| actions.flex_wrap())
                    .child(
                        Button::new((ElementId::from("sync-task-details"), key))
                            .ghost()
                            .h_10()
                            .min_w_12()
                            .flex_shrink_0()
                            .map(|button| {
                                if compact {
                                    button
                                        .icon(IconName::Info)
                                        .tooltip("详情")
                                        .accessibility_label("详情")
                                } else {
                                    button.label("详情")
                                }
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_sync_task_details(
                                    id,
                                    SyncDetailTab::Overview,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(self.sync_job_pause_button(id, compact, cx))
                    .children(
                        self.sync_conflicts_button(id, cx.weak_entity(), cx)
                            .map(|button| button.h_10()),
                    )
                    .child(
                        self.sync_task_run_button(job, compact, cx)
                            .h_10()
                            .min_w_12(),
                    ),
            )
    }

    pub(super) fn sync_task_run_button(
        &self,
        job: &crate::services::sync::SyncJob,
        compact: bool,
        cx: &Context<Self>,
    ) -> Button {
        let id = job.id;
        let service = self.sync.read(cx);
        let available = self.profiles.get(job.profile).is_some();
        let preview = service.pending_preview(id).is_some_and(|plan| plan.is_ok());
        let previewing = service.is_previewing(id);
        let (label, icon) = if previewing {
            ("查看进度", IconName::Clock)
        } else if preview {
            ("查看预览", IconName::List)
        } else {
            ("立即同步", IconName::RefreshCw)
        };
        Button::new((
            ElementId::from("sync-task-run"),
            SharedString::from(id.to_string()),
        ))
        .ghost()
        .flex_shrink_0()
        .map(|button| {
            if compact {
                button.icon(icon).tooltip(label).accessibility_label(label)
            } else {
                button.label(label)
            }
        })
        .disabled(
            !preview
                && !previewing
                && (!available
                    || service.running().is_some()
                    || service.is_quitting()
                    || service.is_paused()
                    || service.is_job_paused(id)),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            if previewing || preview {
                this.open_sync_preview(id, window, cx);
            } else {
                this.sync.update(cx, |service, cx| service.run_now(id, cx));
            }
        }))
    }

    pub(super) fn sync_conflicts_button(
        &self,
        id: uuid::Uuid,
        owner: WeakEntity<Self>,
        cx: &App,
    ) -> Option<Button> {
        let count = self.sync.read(cx).conflicts(id)?.len();
        (count > 0).then(|| {
            Button::new((
                ElementId::from("sync-conflicts"),
                SharedString::from(id.to_string()),
            ))
            .outline()
            .flex_shrink_0()
            .label(format!("{count} 项冲突"))
            .on_click(move |_, window, cx| {
                let _ = owner.update(cx, |browser, cx| {
                    browser.open_sync_conflicts(id, window, cx)
                });
            })
        })
    }

    fn open_sync_conflicts(&self, id: uuid::Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let sync = self.sync.clone();
        let state = cx.new(|_| SyncConflictDialog::default());
        window.open_dialog(cx, move |dialog, window, cx| {
            let service = sync.read(cx);
            let dialog = dialog
                .title("待处理冲突")
                .width(rems(44.).to_pixels(window.rem_size()));
            let Some(browser) = owner.upgrade() else {
                return dialog;
            };
            let Some(job) = service.jobs().iter().find(|job| job.id == id) else {
                return dialog.child("同步任务已删除。");
            };
            let Some(items) = service.conflicts(id).filter(|items| !items.is_empty()) else {
                return dialog.child("没有待处理的冲突。");
            };
            let current = state.read(cx);
            let page_count = items.len().div_ceil(SYNC_PREVIEW_PAGE_SIZE);
            let page = current.page.min(page_count - 1);
            let selected_count = current
                .choices
                .iter()
                .filter(|(path, (shown, _))| {
                    items
                        .get(*path)
                        .is_some_and(|item| shown.same_versions(item))
                })
                .count();
            let running = service.running().is_some();
            let blocked = running
                || service.is_quitting()
                || service.is_paused()
                || service.is_job_paused(id)
                || service.pending_preview(id).is_some_and(|plan| plan.is_ok())
                || browser.read(cx).profiles.get(job.profile).is_none();
            let bulk = [
                (ConflictChoice::Local, "全部使用本地"),
                (ConflictChoice::Remote, "全部使用 S3"),
            ]
            .map(|(choice, label)| {
                let state = state.clone();
                let sync = sync.clone();
                let disabled = !items
                    .values()
                    .any(|item| item.accepts(choice, Direction::Both));
                Button::new(label)
                    .small()
                    .outline()
                    .label(label)
                    .disabled(disabled)
                    .on_click(move |_, _, cx| {
                        let selected = sync
                            .read(cx)
                            .conflicts(id)
                            .into_iter()
                            .flat_map(|items| items.iter())
                            .filter(|(_, item)| item.accepts(choice, Direction::Both))
                            .map(|(path, item)| (path.clone(), (item.clone(), choice)))
                            .collect();
                        state.update(cx, |state, cx| {
                            state.choices = selected;
                            cx.notify();
                        });
                    })
            });
            let submit = {
                let state = state.clone();
                let sync = sync.clone();
                Button::new("apply-conflict-choices")
                    .primary()
                    .label(format!("应用选择（{selected_count}）"))
                    .disabled(blocked || selected_count == 0)
                    .on_click(move |_, _, cx| {
                        let Some(items) = sync.read(cx).conflicts(id) else {
                            return;
                        };
                        let choices = state
                            .read(cx)
                            .choices
                            .iter()
                            .filter(|(path, (shown, _))| {
                                items
                                    .get(*path)
                                    .is_some_and(|item| shown.same_versions(item))
                            })
                            .map(|(path, selected)| (path.clone(), selected.clone()))
                            .collect();
                        let _ = sync
                            .update(cx, |service, cx| service.resolve_conflicts(id, choices, cx));
                    })
            };
            dialog
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .text_sm()
                        .child(format!(
                            "{} 项冲突。覆盖前会保存另一端的旧版本。",
                            items.len()
                        ))
                        .child(div().flex().flex_wrap().gap_2().children(bulk))
                        .when_some(service.conflict_error(id), |body, error| {
                            body.child(div().text_color(cx.theme().danger).child(error.to_owned()))
                        })
                        .when_some(current.open_error.clone(), |body, error| {
                            body.child(div().text_color(cx.theme().danger).child(error))
                        })
                        .when(current.opening, |body| body.child("正在准备打开文件"))
                        .child(
                            div()
                                .id("sync-conflict-items")
                                .max_h(rems(22.))
                                .overflow_y_scroll()
                                .children(
                                    items
                                        .values()
                                        .skip(page * SYNC_PREVIEW_PAGE_SIZE)
                                        .take(SYNC_PREVIEW_PAGE_SIZE)
                                        .map(|item| {
                                            let selected = current
                                                .choices
                                                .get(item.path())
                                                .filter(|(shown, _)| shown.same_versions(item))
                                                .map(|(_, choice)| *choice);
                                            browser.read(cx).sync_conflict_row(
                                                &browser, job, item, selected, &state, window, cx,
                                            )
                                        }),
                                ),
                        )
                        .when(page_count > 1, |body| {
                            body.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .children(
                                        [
                                            ("上一页", page.checked_sub(1)),
                                            ("下一页", (page + 1 < page_count).then_some(page + 1)),
                                        ]
                                        .map(
                                            |(label, target)| {
                                                let state = state.clone();
                                                Button::new(label)
                                                    .small()
                                                    .outline()
                                                    .label(label)
                                                    .disabled(target.is_none())
                                                    .on_click(move |_, _, cx| {
                                                        if let Some(target) = target {
                                                            state.update(cx, |state, cx| {
                                                                state.page = target;
                                                                cx.notify();
                                                            });
                                                        }
                                                    })
                                            },
                                        ),
                                    )
                                    .child(format!("{}/{}", page + 1, page_count)),
                            )
                        }),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .items_center()
                        .gap_2()
                        .when(running, |footer| {
                            footer.child(div().text_sm().child("同步运行中"))
                        })
                        .child(submit),
                )
        });
    }

    #[inline(never)]
    fn sync_conflict_row(
        &self,
        owner: &Entity<Self>,
        job: &SyncJob,
        item: &PlanItem,
        selected: Option<ConflictChoice>,
        state: &Entity<SyncConflictDialog>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let buttons = [
            (ConflictChoice::Local, "本地", "打开本地", "使用本地"),
            (ConflictChoice::Remote, "S3", "打开 S3", "使用 S3"),
        ]
        .map(|(choice, side, open, keep)| {
            let available = item.accepts(choice, Direction::Both);
            let timestamp = item
                .modified(choice)
                .map(|time| self.time_zone.format(time).to_string())
                .unwrap_or_else(|| {
                    if available {
                        "修改时间未知"
                    } else {
                        "文件缺失"
                    }
                    .into()
                });
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(rems(8.))
                        .child(format!("{side}  {timestamp}")),
                )
                .when(cfg!(windows), |row| {
                    #[cfg(windows)]
                    let row = row.child({
                        let job = job.clone();
                        let item = item.clone();
                        let state = state.clone();
                        Button::new(open)
                            .small()
                            .ghost()
                            .label(open)
                            .disabled(
                                state.read(cx).opening
                                    || !available
                                    || (choice == ConflictChoice::Remote
                                        && self.profiles.get(job.profile).is_none()),
                            )
                            .on_click(window.listener_for(owner, move |this, _, _, cx| {
                                this.open_sync_conflict_version(
                                    job.clone(),
                                    item.clone(),
                                    choice,
                                    &state,
                                    cx,
                                );
                            }))
                    });
                    row
                })
                .child(conflict_choice_button(
                    item, choice, keep, selected, available, state,
                ))
        });
        div()
            .id((
                ElementId::from("sync-conflict"),
                SharedString::from(item.path().to_owned()),
            ))
            .flex()
            .flex_col()
            .gap_1()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(item.path().to_owned()),
            )
            .children(buttons)
            .when(item.both_missing(), |row| {
                row.child(conflict_choice_button(
                    item,
                    ConflictChoice::AcknowledgeMissing,
                    "确认两端已移除",
                    selected,
                    true,
                    state,
                ))
            })
    }

    #[cfg(windows)]
    fn open_sync_conflict_version(
        &mut self,
        job: SyncJob,
        item: PlanItem,
        choice: ConflictChoice,
        state: &Entity<SyncConflictDialog>,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::file_viewer;
        state.update(cx, |state, cx| {
            state.opening = true;
            state.open_error = None;
            cx.notify();
        });
        let state = state.downgrade();
        let slots = self.transfer_slots.clone();
        let settings = self
            .profiles
            .get(job.profile)
            .map(|profile| profile.settings.clone());
        let task = self.runtime.spawn(async move {
            let file = if choice == ConflictChoice::Local {
                tokio::task::spawn_blocking(move || {
                    file_viewer::prepare_local_sync_version(&job, &item)
                })
                .await??
            } else {
                let settings = settings.context("缺少连接凭证")?;
                let entry = item.remote_entry(&job).context("S3 版本已不存在")?;
                let store =
                    crate::services::sync_service::open_sync_store(&job, &settings, &slots).await?;
                file_viewer::prepare_external(
                    store,
                    entry,
                    Some(job.profile),
                    Arc::default(),
                    Arc::default(),
                    slots,
                )
                .await?
            };
            tokio::task::spawn_blocking(move || file.choose_application(None)).await?
        });
        cx.spawn(async move |_, cx| {
            let error = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .err();
            let _ = state.update(cx, |state, cx| {
                state.opening = false;
                state.open_error = error.map(|error| format!("{error:#}").into());
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn sync_job_pause_button(
        &self,
        id: uuid::Uuid,
        compact: bool,
        cx: &Context<Self>,
    ) -> Button {
        let service = self.sync.read(cx);
        let paused = service.is_job_paused(id);
        let label = if paused {
            "恢复任务"
        } else {
            "暂停任务"
        };
        let button = Button::new((
            ElementId::from("sync-job-pause"),
            SharedString::from(id.to_string()),
        ))
        .small()
        .ghost()
        .flex_shrink_0()
        .disabled(service.is_quitting())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.sync
                .update(cx, |service, cx| service.toggle_job_pause(id, cx));
        }));
        if compact {
            button
                .xsmall()
                .icon(if paused {
                    IconName::Play
                } else {
                    IconName::Pause
                })
                .tooltip(label)
                .accessibility_label(label)
        } else {
            button.label(label)
        }
    }

    pub(super) fn open_sync_preview(
        &self,
        id: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sync = self.sync.clone();
        sync.update(cx, |service, cx| service.preview(id, cx));
        let state = cx.new(|_| SyncPreviewState::default());
        window.open_dialog(cx, move |dialog, window, cx| {
            let service = sync.read(cx);
            let dialog = dialog
                .title("预览同步")
                .width(rems(40.).to_pixels(window.rem_size()))
                .overlay_closable(false);
            let abandon = sync.clone();
            let discard = Button::new("discard-sync-preview")
                .ghost()
                .label(if service.is_previewing(id) {
                    "取消生成"
                } else {
                    "丢弃预览"
                })
                .on_click(move |_, window, cx| {
                    abandon.update(cx, |service, cx| service.discard_preview(id, cx));
                    window.close_dialog(cx);
                });
            let plan = match service.pending_preview(id) {
                Some(Ok(plan)) => plan,
                Some(Err(error)) => {
                    let retry = sync.clone();
                    return dialog
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(error.to_owned()),
                        )
                        .footer(
                            div().flex().justify_end().gap_2().child(discard).child(
                                Button::new("retry-sync-preview")
                                    .primary()
                                    .label("重新生成")
                                    .disabled(
                                        service.running().is_some()
                                            || service.is_paused()
                                            || service.is_job_paused(id)
                                            || service.is_quitting(),
                                    )
                                    .on_click(move |_, _, cx| {
                                        retry.update(cx, |service, cx| {
                                            service.discard_preview(id, cx);
                                            service.preview(id, cx);
                                        })
                                    }),
                            ),
                        );
                }
                None if service.is_previewing(id) => {
                    let detail = service
                        .progress(id)
                        .map(|(_, _, progress)| {
                            if progress.total == 0 {
                                progress.stage.to_owned()
                            } else {
                                format!(
                                    "{} {}/{}",
                                    progress.stage, progress.processed, progress.total
                                )
                            }
                        })
                        .unwrap_or_else(|| "正在生成计划".into());
                    return dialog
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(Spinner::new().small())
                                .child(detail),
                        )
                        .footer(
                            div().flex().justify_end().gap_2().child(discard).child(
                                Button::new("sync-preview-background")
                                    .primary()
                                    .label("转到后台")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            ),
                        );
                }
                None => return dialog.child("预览已失效，请关闭后重新查看。"),
            };
            let current = state.read(cx);
            let conflict_count = plan
                .items()
                .iter()
                .filter(|item| item.is_conflict())
                .count();
            let visible_count = if current.conflicts_only {
                conflict_count
            } else {
                plan.items().len()
            };
            let page_count = visible_count.div_ceil(SYNC_PREVIEW_PAGE_SIZE).max(1);
            let page = current.page.min(page_count - 1);
            let saves_versions = plan.items().iter().any(PlanItem::keeps_recovery_copy);
            let body = div()
                .flex()
                .flex_col()
                .gap_2()
                .text_sm()
                .child(format!(
                    "计划 {} 项 · 冲突 {conflict_count} 项",
                    plan.items().len()
                ))
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("执行或丢弃预览后，这个任务才会继续自动同步。"),
                )
                .when(conflict_count > 0, |body| {
                    let state = state.clone();
                    body.child(
                        Button::new("sync-preview-conflicts")
                            .small()
                            .outline()
                            .label(if current.conflicts_only {
                                "显示全部项目"
                            } else {
                                "只看冲突"
                            })
                            .on_click(move |_, _, cx| {
                                state.update(cx, |state, cx| {
                                    state.conflicts_only = !state.conflicts_only;
                                    state.page = 0;
                                    cx.notify();
                                })
                            }),
                    )
                })
                .when(saves_versions, |body| {
                    body.child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("覆盖前，本地旧版本会归档，S3 旧版本会保存到存储桶回收站。"),
                    )
                })
                .when(plan.items().is_empty(), |body| {
                    body.child("两端没有待同步的文件。")
                })
                .child(
                    div()
                        .id("sync-preview-items")
                        .max_h(rems(22.))
                        .overflow_y_scroll()
                        .children(
                            plan.items()
                                .iter()
                                .filter(|item| !current.conflicts_only || item.is_conflict())
                                .skip(page * SYNC_PREVIEW_PAGE_SIZE)
                                .take(SYNC_PREVIEW_PAGE_SIZE)
                                .map(|item| {
                                    let path = SharedString::from(item.path().to_owned());
                                    div()
                                        .id((ElementId::from("sync-plan-item"), path.clone()))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .py_2()
                                        .border_b_1()
                                        .border_color(cx.theme().border)
                                        .child(path.clone())
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{} · {}",
                                                    item.label(),
                                                    item.reason()
                                                )),
                                        )
                                }),
                        ),
                )
                .when(page_count > 1, |body| {
                    body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(
                                [
                                    ("sync-preview-prev", "上一页", page.checked_sub(1)),
                                    (
                                        "sync-preview-next",
                                        "下一页",
                                        (page + 1 < page_count).then_some(page + 1),
                                    ),
                                ]
                                .map(|(key, label, target)| {
                                    let state = state.clone();
                                    Button::new(key)
                                        .small()
                                        .outline()
                                        .label(label)
                                        .disabled(target.is_none())
                                        .on_click(move |_, _, cx| {
                                            if let Some(target) = target {
                                                state.update(cx, |state, cx| {
                                                    state.page = target;
                                                    cx.notify();
                                                });
                                            }
                                        })
                                }),
                            )
                            .child(format!("{} / {page_count}", page + 1)),
                    )
                });
            let blocked = service.running().is_some()
                || service.is_quitting()
                || service.is_paused()
                || service.is_job_paused(id);
            let sync = sync.clone();

            dialog.child(body).footer(
                div()
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap_2()
                    .child(discard)
                    .child(
                        Button::new("close-sync-preview")
                            .ghost()
                            .label("稍后处理")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("apply-sync-preview")
                            .primary()
                            .label("执行同步")
                            .disabled(blocked)
                            .on_click(move |_, window, cx| {
                                if sync.update(cx, |service, cx| service.confirm_preview(id, cx)) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
        });
    }

    pub(super) fn open_sync_task_details(
        &self,
        id: uuid::Uuid,
        initial_tab: SyncDetailTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.sync.read(cx).jobs().iter().find(|job| job.id == id) else {
            return;
        };
        let minutes = job.interval_minutes.to_string();
        let interval = cx.new(|cx| InputState::new(window, cx).default_value(minutes));
        let repaint = cx.subscribe_in(&interval, window, |_, _, _: &InputEvent, window, _| {
            window.refresh();
        });
        let tab = cx.new(|_| initial_tab);
        let owner = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let _keep_subscription = &repaint;
            let dialog = dialog
                .width(rems(42.).to_pixels(window.rem_size()))
                .title("同步任务详情");
            let Some(owner) = owner.upgrade() else {
                return dialog.child("任务已不存在");
            };
            let browser = owner.read(cx);
            let service = browser.sync.read(cx);
            let Some(job) = service.jobs().iter().find(|job| job.id == id) else {
                return dialog.child("任务已不存在");
            };
            let active_tab = *tab.read(cx);
            let body = match active_tab {
                SyncDetailTab::Overview => browser.sync_overview(job, cx),
                SyncDetailTab::Policy => {
                    browser.sync_policy_controls(job, &owner, &interval, window, cx)
                }
                SyncDetailTab::Result => browser.sync_last_result(id, cx),
            };
            let dialog = if active_tab == SyncDetailTab::Overview {
                let blocked = service.running().is_some() && !service.is_previewing(id)
                    || service.is_paused()
                    || service.is_job_paused(id)
                    || service.is_quitting()
                    || browser.profiles.get(job.profile).is_none();
                let owner = owner.downgrade();
                dialog.footer(
                    div().flex().justify_end().child(
                        Button::new("preview-sync-from-details")
                            .ghost()
                            .label("预览同步")
                            .disabled(blocked)
                            .on_click(move |_, window, cx| {
                                owner
                                    .update(cx, |browser, cx| {
                                        browser.open_sync_preview(id, window, cx);
                                    })
                                    .ok();
                            }),
                    ),
                )
            } else if active_tab == SyncDetailTab::Result {
                let conflicts = browser.sync_conflicts_button(id, owner.downgrade(), cx);
                let has_retry = service.has_retry_targets(id);
                let retry_blocked = service.retry_blocked_reason(id).or_else(|| {
                    browser
                        .profiles
                        .get(job.profile)
                        .is_none()
                        .then_some("缺少连接凭证")
                });
                let sync = browser.sync.clone();
                if has_retry || conflicts.is_some() {
                    dialog.footer(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap_2()
                            .children(conflicts)
                            .when(has_retry, |footer| {
                                footer
                                    .when_some(retry_blocked, |footer, reason| {
                                        footer.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(reason),
                                        )
                                    })
                                    .child(
                                        Button::new("retry-sync-run")
                                            .primary()
                                            .label("重试待处理文件")
                                            .disabled(retry_blocked.is_some())
                                            .on_click(move |_, window, cx| {
                                                if sync
                                                    .update(cx, |service, cx| service.retry(id, cx))
                                                {
                                                    window.close_dialog(cx);
                                                }
                                            }),
                                    )
                            }),
                    )
                } else {
                    dialog
                }
            } else {
                dialog
            };
            dialog.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .pb_2()
                    .text_sm()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div().text_lg().font_weight(FontWeight::SEMIBOLD).child(
                                    job.folder
                                        .file_name()
                                        .unwrap_or(job.folder.as_os_str())
                                        .to_string_lossy()
                                        .into_owned(),
                                ),
                            )
                            .child(div().text_color(cx.theme().muted_foreground).child(format!(
                                "{} · {}",
                                job.direction.label(),
                                service.task_status(job)
                            ))),
                    )
                    .child(
                        TabBar::new((
                            ElementId::from("sync-detail-tabs"),
                            SharedString::from(id.to_string()),
                        ))
                        .small()
                        .pill()
                        .menu(true)
                        .children(SyncDetailTab::ALL.map(|(_, label)| Tab::new().label(label)))
                        .selected_index(
                            SyncDetailTab::ALL
                                .iter()
                                .position(|(target, _)| *target == active_tab)
                                .expect("all sync detail tabs are listed"),
                        )
                        .on_click({
                            let tab = tab.clone();
                            move |index, window, cx| {
                                tab.update(cx, |value, cx| {
                                    *value = SyncDetailTab::ALL[*index].0;
                                    cx.notify();
                                });
                                window.refresh();
                            }
                        }),
                    )
                    .child(body),
            )
        });
    }

    #[inline(never)]
    fn sync_overview(&self, job: &crate::services::sync::SyncJob, cx: &App) -> AnyElement {
        let service = self.sync.read(cx);
        let theme = cx.theme();
        let id = job.id;
        let profile = self.profiles.get(job.profile);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .when_some(
                service.progress(id),
                |body, (started, automatic, progress)| {
                    body.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("当前运行"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Spinner::new().small())
                                    .child(progress.stage),
                            )
                            .child(div().text_color(theme.muted_foreground).child(format!(
                                "{}触发 · {} 开始 · 已用 {} 秒",
                                if automatic { "自动" } else { "手动" },
                                self.time_zone.format(started),
                                (chrono::Utc::now() - started).num_seconds().max(0),
                            )))
                            .when(!progress.current.is_empty(), |run| {
                                run.child(sync_detail_value(
                                    "当前路径",
                                    progress.current.clone(),
                                    cx,
                                ))
                            })
                            .when(progress.total > 0, |run| {
                                run.child(Progress::new("sync-overview-progress").value(
                                    100.0 * progress.processed as f32 / progress.total as f32,
                                ))
                                .child(format!(
                                    "已处理 {} / {} 项",
                                    progress.processed, progress.total
                                ))
                            })
                            .child(div().text_color(theme.muted_foreground).child(format!(
                                "本地文件 {} · S3 文件 {}",
                                progress.local_files, progress.remote_files
                            )))
                            .when(service.executing() == Some(id), |run| {
                                run.child(format!(
                                    "已上传 {} 个 · 已下载 {} 个",
                                    progress.uploaded, progress.downloaded
                                ))
                                .when(
                                    progress.conflicts > 0
                                        || progress.busy > 0
                                        || progress.retry_files > 0,
                                    |run| {
                                        run.child(format!(
                                            "冲突 {} · 文件占用 {} · 其他待重试 {}",
                                            progress.conflicts, progress.busy, progress.retry_files,
                                        ))
                                    },
                                )
                            }),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("同步位置"))
                    .child(sync_detail_value(
                        "本地文件夹",
                        job.folder.display().to_string(),
                        cx,
                    ))
                    .child(sync_detail_value(
                        "S3 目录",
                        format!("s3://{}/{}", job.bucket, job.prefix),
                        cx,
                    ))
                    .child(sync_detail_value(
                        "连接凭证",
                        profile
                            .map_or("凭证已删除", |profile| profile.name.as_str())
                            .to_owned(),
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_3()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("同步触发"))
                    .when(job.automatic, |summary| {
                        summary.child(format!("每 {} 分钟探测两端", job.interval_minutes))
                    })
                    .when(job.watch_local, |summary| {
                        summary.child("本地目录变化时同步")
                    })
                    .when(!job.automatic && !job.watch_local, |summary| {
                        summary.child("手动同步")
                    })
                    .when_some(
                        service
                            .next_run(id)
                            .filter(|_| !service.is_paused() && !service.is_job_paused(id)),
                        |summary, time| {
                            summary.child(sync_detail_value(
                                "下次定时探测",
                                self.time_zone.format(time).to_string(),
                                cx,
                            ))
                        },
                    )
                    .when_some(service.directory_watch_error(id), |summary, error| {
                        summary.child(div().text_color(theme.danger).child(error.to_owned()))
                    }),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn sync_last_result(&self, id: uuid::Uuid, cx: &App) -> AnyElement {
        let service = self.sync.read(cx);
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .when_some(service.last_run(id), |body, run| {
                let elapsed = (run.finished() - run.started()).num_seconds().max(0);
                body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if run.outcome().has_issue() {
                                    theme.danger
                                } else {
                                    theme.foreground
                                })
                                .child(run.outcome().label()),
                        )
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} · 用时 {elapsed} 秒", run.trigger_label(),)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(sync_detail_value(
                            "开始时间",
                            self.time_zone.format(run.started()).to_string(),
                            cx,
                        ))
                        .child(sync_detail_value(
                            "结束时间",
                            self.time_zone.format(run.finished()).to_string(),
                            cx,
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .pt_3()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("处理结果"))
                        .children(
                            run.message()
                                .lines()
                                .map(|line| div().child(line.to_owned())),
                        ),
                )
            })
            .when(service.last_run(id).is_none(), |body| {
                body.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("暂无同步记录"),
                )
            })
            .when_some(
                crate::services::sync::local_recovery_folder().ok(),
                |body, folder| {
                    body.child(
                        div().child(
                            Clipboard::new("copy-sync-recovery-location")
                                .small()
                                .label("复制本地恢复目录")
                                .value(folder.display().to_string())
                                .on_copied(|_, window, cx| {
                                    window.push_notification(
                                        Notification::success("已复制恢复目录路径"),
                                        cx,
                                    );
                                }),
                        ),
                    )
                },
            )
            .into_any_element()
    }
}

fn conflict_choice_button(
    item: &PlanItem,
    choice: ConflictChoice,
    label: &'static str,
    selected: Option<ConflictChoice>,
    available: bool,
    state: &Entity<SyncConflictDialog>,
) -> Button {
    let state = state.clone();
    let item = item.clone();
    Button::new(label)
        .small()
        .outline()
        .label(label)
        .disabled(!available)
        .selected(selected == Some(choice))
        .on_click(move |_, _, cx| {
            state.update(cx, |state, cx| {
                if selected == Some(choice) {
                    state.choices.remove(item.path());
                } else {
                    state
                        .choices
                        .insert(item.path().to_owned(), (item.clone(), choice));
                }
                cx.notify();
            })
        })
}

fn sync_detail_value(label: &'static str, value: String, cx: &App) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().whitespace_normal().child(value))
}
