use super::*;

impl Browser {
    pub(super) fn sync_task_list(&self, cx: &Context<Self>) -> impl IntoElement {
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
                [service.config_error(), service.history_error()]
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
                            .child("暂无同步任务。可在「设置 → 文件夹同步」添加任务。"),
                    )
                },
            )
            .children(
                service
                    .jobs()
                    .iter()
                    .map(|job| self.sync_task_row(job, service, cx)),
            )
    }

    #[inline(never)]
    fn sync_task_policy(job: &crate::services::sync::SyncJob, cx: &App) -> AnyElement {
        use crate::services::sync::Direction;
        let direction = match job.direction {
            Direction::Upload => IconName::Upload,
            Direction::Download => IconName::Download,
            Direction::Both => IconName::ArrowUpDown,
        };
        let schedule = if job.automatic {
            (IconName::Clock, "自动同步")
        } else {
            (IconName::MousePointer, "仅手动")
        };
        let deletion = if job.propagate_deletes {
            (IconName::Trash, "同步删除")
        } else {
            (IconName::ShieldCheck, "保留另一端文件")
        };
        let conflict = match job.conflict_policy {
            ConflictPolicy::PreserveBoth => IconName::Copy,
            ConflictPolicy::PreferLocal => IconName::HardDrive,
            ConflictPolicy::PreferRemote => IconName::Cloud,
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x_3()
            .gap_y_1()
            .text_color(cx.theme().muted_foreground)
            .children(
                [(direction, job.direction.label()), schedule, deletion]
                    .into_iter()
                    .chain(
                        (job.direction == Direction::Both)
                            .then_some((conflict, job.conflict_policy.label())),
                    )
                    .map(|(icon, label)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(Icon::new(icon).size_3p5())
                            .child(label)
                    }),
            )
            .into_any_element()
    }

    fn sync_task_row(
        &self,
        job: &crate::services::sync::SyncJob,
        service: &SyncService,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = job.id;
        let key = SharedString::from(id.to_string());
        let profile = self.profiles.get(job.profile);
        let running = service.running() == Some(id);
        let folder = job.folder.file_name().map_or_else(
            || job.folder.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let last_run = service.last_run(id);
        let has_conflict = last_run.is_some_and(|run| matches!(run.outcome(), Outcome::Conflict));
        let status = if profile.is_none() {
            "缺少连接凭证".to_owned()
        } else if running
            && service
                .cancellation()
                .is_some_and(CancellationToken::is_cancelled)
        {
            "正在取消".to_owned()
        } else if running {
            service.progress(id).map_or_else(
                || "运行中".to_owned(),
                |(_, _, progress)| {
                    if progress.total > 0 {
                        format!("运行中，{}/{}", progress.processed, progress.total)
                    } else {
                        progress.stage.to_string()
                    }
                },
            )
        } else if let Some(run) =
            last_run.filter(|run| !matches!(run.outcome(), Outcome::Completed))
        {
            format!("上次{}", run.outcome().label())
        } else if job.automatic {
            "等待自动同步".to_owned()
        } else {
            "手动同步".to_owned()
        };
        div()
            .id((ElementId::from("sync-task"), key.clone()))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .text_sm()
            .child(
                div()
                    .flex_1()
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
                                        && last_run.is_some_and(|run| run.outcome().has_issue()))
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
                Button::new((ElementId::from("sync-task-details"), key.clone()))
                    .ghost()
                    .h_10()
                    .min_w_12()
                    .flex_shrink_0()
                    .label(if has_conflict {
                        "处理冲突…"
                    } else {
                        "详情"
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if has_conflict {
                            this.open_sync_conflicts(id, window, cx);
                        } else {
                            this.open_sync_task_details(id, window, cx);
                        }
                    })),
            )
            .child(
                Button::new((ElementId::from("sync-task-run"), key.clone()))
                    .ghost()
                    .h_10()
                    .min_w_12()
                    .flex_shrink_0()
                    .label("立即同步")
                    .disabled(
                        profile.is_none() || service.running().is_some() || service.is_quitting(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sync
                            .update(cx, |service, cx| service.start(id, false, cx));
                    })),
            )
    }

    fn open_sync_conflicts(&self, id: uuid::Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let feedback = cx.new(|_| None::<String>);
        window.open_dialog(cx, move |dialog, _, cx| {
            let dialog = dialog.title("处理同步冲突").overlay_closable(false);
            let Some(owner) = owner.upgrade() else {
                return dialog;
            };
            let browser = owner.read(cx);
            let service = browser.sync.read(cx);
            let Some((job, run)) = service.jobs().iter().find(|job| job.id == id)
                .zip(service.last_run(id))
                .filter(|(_, run)| matches!(run.outcome(), Outcome::Conflict))
            else {
                return dialog.child("当前没有待处理的冲突记录。");
            };
            let both = job.direction == crate::services::sync::Direction::Both;
            let blocked = if service.is_quitting() {
                Some("应用正在退出。")
            } else if service.running().is_some() {
                Some("已有同步任务在运行，请等本轮结束。")
            } else if browser.profiles.get(job.profile).is_none() {
                Some("缺少连接凭证，请先检查任务配置。")
            } else {
                None
            };
            let finished = run.finished();
            let sync = browser.sync.clone();
            dialog
                .child(
                    div().id("sync-conflicts").max_h(rems(22.)).overflow_y_scroll()
                        .flex().flex_col().gap_2().text_sm()
                        .child(format!("上次同步，{}", browser.time_zone.format(finished)))
                        .child(run.message().to_owned())
                        .child(if both {
                            "以下操作会重新扫描整个任务，对本轮所有冲突使用所选端的文件覆盖或补传到另一端。所选端缺失的文件会保留为冲突。"
                        } else {
                            "单向任务需手动核对两端文件，保留所需版本后再同步。"
                        }),
                )
                .children(blocked.or_else(|| feedback.read(cx).as_deref())
                    .map(|message| div().text_sm().text_color(cx.theme().danger).child(message.to_owned())))
                .footer(
                    div().flex().justify_end().flex_wrap().gap_2()
                        .child(Button::new("close-sync-conflicts").ghost().label("关闭")
                            .on_click(|_, window, cx| window.close_dialog(cx)))
                        .when(both, |row| row.children([
                            ("sync-conflict-local", "用本地覆盖或补传到 S3", ConflictPolicy::PreferLocal),
                            ("sync-conflict-remote", "用 S3 覆盖或补回本地", ConflictPolicy::PreferRemote),
                        ].map(|(key, label, policy)| {
                            let sync = sync.clone();
                            let owner = owner.downgrade();
                            let feedback = feedback.clone();
                            Button::new(key).small().label(label).disabled(blocked.is_some())
                                .on_click(move |_, window, cx| {
                                    match sync.update(cx, |service, cx| service.resolve_conflicts(id, finished, policy, cx)) {
                                        Ok(()) => {
                                            let _ = owner.update(cx, |browser, cx| {
                                                let message = format!("处理冲突，{}，等待传输队列", policy.label());
                                                browser.log(false, message.clone());
                                                browser.show_notice(message, cx);
                                            });
                                            window.close_dialog(cx);
                                        }
                                        Err(error) => feedback.update(cx, |message, cx| {
                                            *message = Some(error.to_string());
                                            cx.notify();
                                        }),
                                    }
                                })
                        }))),
                )
        });
    }

    fn open_sync_task_details(&self, id: uuid::Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("同步任务详情")
                // A follow-up press from the opening touch can land on the backdrop.
                .overlay_closable(false)
                .child(owner.upgrade().map_or_else(
                    || div().child("任务已不存在").into_any_element(),
                    |owner| owner.read(cx).sync_task_details(id, cx),
                ))
        });
    }

    #[inline(never)]
    fn sync_task_details(&self, id: uuid::Uuid, cx: &App) -> AnyElement {
        let service = self.sync.read(cx);
        let Some(job) = service.jobs().iter().find(|job| job.id == id) else {
            return div().child("任务已不存在").into_any_element();
        };
        let theme = cx.theme();
        let profile = self.profiles.get(job.profile);
        let key = SharedString::from(id.to_string());
        div()
            .id("sync-task-detail-body")
            .max_h(rems(22.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .child(job.folder.display().to_string())
            .child(div().text_color(theme.muted_foreground).child(format!(
                "S3  {}/{}，{}",
                job.bucket,
                job.prefix,
                profile.map_or("凭证已删除", |profile| profile.name.as_str()),
            )))
            .child(Self::sync_task_policy(job, cx))
            .when_some(service.next_run(id), |row, time| {
                row.child(format!("下次同步 {}", self.time_zone.format(time)))
            })
            .when_some(
                service.progress(id),
                |row, (started, automatic, progress)| {
                    let elapsed = (chrono::Utc::now() - started).num_seconds().max(0);
                    row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Spinner::new().small())
                            .child(format!(
                                "{}，{}触发，开始于 {}，已用 {} 秒",
                                progress.stage,
                                if automatic { "自动" } else { "手动" },
                                self.time_zone.format(started),
                                elapsed,
                            )),
                    )
                    .when(!progress.current.is_empty(), |row| {
                        row.child(div().child(format!("当前路径  {}", progress.current)))
                    })
                    .when(progress.total > 0, |row| {
                        row.child(
                            Progress::new((ElementId::from("sync-progress"), key.clone()))
                                .value(100.0 * progress.processed as f32 / progress.total as f32),
                        )
                    })
                    .child(div().text_color(theme.muted_foreground).child(format!(
                        "已检查 {}/{} 个文件，本地 {} 个，远端 {} 个",
                        progress.processed,
                        progress.total,
                        progress.local_files,
                        progress.remote_files,
                    )))
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(progress.summary()),
                    )
                },
            )
            .when_some(service.last_run(id), |row, run| {
                let elapsed = (run.finished() - run.started()).num_seconds().max(0);
                row.child(
                    div()
                        .text_color(if run.outcome().has_issue() {
                            theme.danger
                        } else {
                            theme.foreground
                        })
                        .child(format!(
                            "上次{}，{}触发，{} 至 {}，用时 {} 秒",
                            run.outcome().label(),
                            if run.automatic() { "自动" } else { "手动" },
                            self.time_zone.format(run.started()),
                            self.time_zone.format(run.finished()),
                            elapsed,
                        )),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(run.message().to_owned()),
                )
            })
            .when(
                service.last_run(id).is_none() && service.running() != Some(id),
                |row| {
                    row.child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("暂无同步记录"),
                    )
                },
            )
            .into_any_element()
    }
}
