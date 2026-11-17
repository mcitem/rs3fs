use super::{sync_tasks::SyncDetailTab, *};

impl Browser {
    fn choose_sync_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = &self.server else { return };
        if self.connected_profile.is_none() || self.busy.is_some() {
            return;
        }
        let mut stores: Vec<_> = self
            .buckets
            .iter()
            .map(|bucket| bucket.open(server))
            .collect();
        if let Some(store) = &self.store
            && !stores
                .iter()
                .any(|candidate| candidate.bucket() == store.bucket())
        {
            stores.push(store.clone());
        }
        let location = self
            .sync_remote
            .as_ref()
            .and_then(|(bucket, prefix)| {
                stores
                    .iter()
                    .find(|store| store.bucket() == bucket)
                    .map(|store| (store.clone(), prefix.clone()))
            })
            .or_else(|| self.store.clone().map(|store| (store, self.prefix.clone())));
        let revision = self.connection_revision;
        self.open_remote_picker(
            stores,
            location,
            "选择 S3 目录".into(),
            "选择当前目录",
            |_, _, _| true,
            move |this, store, prefix, cx| {
                if this.connection_revision != revision {
                    return false;
                }
                this.sync_remote = Some((store.bucket().to_owned(), prefix));
                cx.notify();
                true
            },
            window,
            cx,
        );
    }

    fn choose_sync_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |view, cx| {
            let folder = rfd::AsyncFileDialog::new()
                .set_title("选择本地同步文件夹")
                .pick_folder()
                .await;
            let Some(folder) = folder else { return };
            let _ = view.update(cx, |this, cx| {
                this.sync_local = Some(folder.path().to_owned());
                cx.notify();
            });
        })
        .detach();
    }

    fn add_sync_job(&mut self, interval_minutes: u32, cx: &mut Context<Self>) -> Result<()> {
        let (Some(profile), Some((bucket, prefix)), Some(folder)) = (
            self.connected_profile,
            self.sync_remote.clone(),
            self.sync_local.clone(),
        ) else {
            anyhow::bail!("请连接 S3 并选择本地和远端目录。");
        };
        if self.busy.is_some() {
            anyhow::bail!("请等当前操作结束后再添加任务。");
        }
        (|| {
            let mut jobs = self.sync.read(cx).jobs().to_vec();
            ensure!(
                !jobs.iter().any(|job| job.profile == profile
                    && job.bucket == bucket
                    && job.prefix == prefix
                    && job.folder == folder),
                "此本地和远端目录已有同步任务。"
            );
            let bucket_region = self
                .buckets
                .iter()
                .find(|candidate| candidate.name() == bucket)
                .and_then(|candidate| candidate.region())
                .map(str::to_owned);
            jobs.push(crate::services::sync::SyncJob {
                id: uuid::Uuid::new_v4(),
                profile,
                bucket,
                bucket_region,
                prefix,
                folder,
                direction: self.sync_direction,
                automatic: self.new_sync_automatic,
                interval_minutes,
                watch_local: self.new_sync_watch_local,
            });
            self.persist_profiles(self.profiles.clone())?;
            self.sync
                .update(cx, |service, cx| service.save_jobs(jobs, cx))
        })()?;
        self.sync_remote = None;
        self.sync_local = None;
        self.settings_saved(Ok(()), "同步任务已添加", cx);
        Ok(())
    }

    pub(super) fn sync_changed(&mut self, cx: &mut Context<Self>) {
        let service = self.sync.read(cx);
        let active = service.execution();
        let started = active
            .filter(|run| Some(*run) != self.observed_sync_run)
            .and_then(|(id, _)| {
                let job = service.jobs().iter().find(|job| job.id == id)?;
                let (_, automatic, _) = service.progress(id)?;
                let trigger = if automatic { "自动" } else { "手动" };
                Some(format!(
                    "同步，{trigger}启动，{}，{:?} /{:?}，{}",
                    job.direction.label(),
                    job.bucket,
                    job.prefix,
                    job.folder.display(),
                ))
            });
        let finished = self
            .observed_sync_run
            .filter(|run| Some(*run) != active)
            .and_then(|(id, started)| {
                let job = service.jobs().iter().find(|job| job.id == id)?;
                let run = service.last_run(id)?;
                if run.started() != started {
                    return None;
                }
                Some((
                    job.profile,
                    job.bucket.clone(),
                    run.outcome().has_issue(),
                    format!(
                        "{}，{:.2} 秒，{}",
                        run.outcome().label(),
                        (run.finished() - run.started()).num_milliseconds().max(0) as f64 / 1000.0,
                        run.message(),
                    ),
                ))
            });
        self.observed_sync_run = active;
        if let Some((profile, bucket, failed, message)) = finished {
            self.log(failed, format!("同步，{bucket}，{message}"));
            if self.connected_profile == Some(profile)
                && self
                    .store
                    .as_ref()
                    .is_some_and(|store| store.bucket() == bucket)
            {
                self.refresh_usage(cx);
                if self.trash.is_none() {
                    self.upload_refresh_pending = true;
                    self.refresh_after_upload(cx);
                }
            }
        }
        if let Some(message) = started {
            self.log(false, message);
        }
        cx.notify();
    }

    pub(super) fn show_sync_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_tasks = true;
        self.tasks_maximized = false;
        self.show_sync_tasks = true;
        self.tasks_focus.focus(window, cx);
        cx.notify();
    }

    // Keep the form and individual task rows in separate debug-build stack frames.
    #[inline(never)]
    pub(super) fn sync_settings(&self, cx: &Context<Self>) -> AnyElement {
        let service = self.sync.read(cx);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("文件夹同步"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("new-sync-task-settings")
                            .h_10()
                            .min_w_12()
                            .primary()
                            .label("添加任务")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_sync_job(window, cx)
                            })),
                    )
                    .child(
                        Button::new("view-sync-tasks")
                            .small()
                            .ghost()
                            .label("查看同步状态")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.show_sync_tasks(window, cx)),
                            ),
                    ),
            )
            .children(
                [service.config_error(), service.pause_error()]
                    .into_iter()
                    .flatten()
                    .map(|error| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error.to_owned())
                    }),
            )
            .children(
                service
                    .jobs()
                    .iter()
                    .map(|job| self.sync_settings_job(job, cx)),
            )
            .into_any_element()
    }

    pub(super) fn open_new_sync_job(&self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let owner = cx.weak_entity();
        let error = cx.new(|_| None::<SharedString>);
        let interval = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(crate::services::sync::default_interval_minutes().to_string())
        });
        let repaint = cx.subscribe_in(&interval, window, |_, _, _: &InputEvent, window, _| {
            window.refresh();
        });
        window.open_dialog(cx, move |dialog, window, cx| {
            let _keep_subscription = &repaint;
            let dialog = dialog.title("添加同步任务").width(px(544.));
            let Some(owner) = owner.upgrade() else {
                return dialog;
            };
            dialog.child(
                owner
                    .read(cx)
                    .sync_settings_form(&owner, &error, &interval, window, cx),
            )
        });
    }

    #[inline(never)]
    fn sync_settings_form(
        &self,
        owner: &Entity<Self>,
        error: &Entity<Option<SharedString>>,
        interval: &Entity<InputState>,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        use crate::services::sync::Direction;
        let busy = self.busy.is_some();
        let theme = cx.theme();
        let can_browse =
            self.connected_profile.is_some() && (self.store.is_some() || !self.buckets.is_empty());
        let minutes = if self.new_sync_automatic {
            sync_interval_minutes(interval, cx)
        } else {
            Some(crate::services::sync::default_interval_minutes())
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("choose-sync-remote")
                            .small()
                            .label("选择 S3 目录")
                            .disabled(busy || !can_browse)
                            .on_click(window.listener_for(owner, |this, _, window, cx| {
                                this.choose_sync_remote(window, cx)
                            })),
                    )
                    .child(div().text_sm().min_w_0().truncate().child(
                        self.sync_remote.as_ref().map_or_else(
                            || {
                                if can_browse {
                                    "未选择 S3 目录"
                                } else {
                                    "请先连接 S3"
                                }
                                .to_owned()
                            },
                            |(bucket, prefix)| format!("{bucket} /{prefix}"),
                        ),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("choose-sync-local")
                            .small()
                            .label("选择本地文件夹")
                            .on_click(window.listener_for(owner, |this, _, window, cx| {
                                this.choose_sync_local(window, cx)
                            })),
                    )
                    .child(div().text_sm().min_w_0().truncate().child(
                        self.sync_local.as_ref().map_or_else(
                            || "未选择本地文件夹".to_owned(),
                            |folder| folder.display().to_string(),
                        ),
                    )),
            )
            .child(div().flex().items_center().gap_2().children(
                [Direction::Upload, Direction::Download, Direction::Both].map(|direction| {
                    Button::new(direction.label())
                        .small()
                        .ghost()
                        .label(direction.label())
                        .selected(direction == self.sync_direction)
                        .disabled(busy)
                        .on_click(window.listener_for(owner, move |this, _, _, cx| {
                            this.sync_direction = direction;
                            cx.notify();
                        }))
                }),
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(self.sync_direction.description()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_3()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("自动触发"))
                    .child(
                        Switch::new("new-sync-automatic")
                            .label("定时探测两端")
                            .checked(self.new_sync_automatic)
                            .on_click(window.listener_for(owner, |this, checked, _, cx| {
                                this.new_sync_automatic = *checked;
                                cx.notify();
                            })),
                    )
                    .when(self.new_sync_automatic, |form| {
                        form.child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child("每隔")
                                .child(div().w_24().child(Input::new(interval).small()))
                                .child("分钟")
                                .when(minutes.is_none(), |row| {
                                    row.child(
                                        div().text_color(theme.danger).child("请输入大于 0 的整数"),
                                    )
                                }),
                        )
                    })
                    .child(
                        Switch::new("new-sync-watch-local")
                            .label("本地目录变化时同步")
                            .checked(self.new_sync_watch_local)
                            .on_click(window.listener_for(owner, |this, checked, _, cx| {
                                this.new_sync_watch_local = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .when_some(error.read(cx).clone(), |form, message| {
                form.child(div().text_sm().text_color(theme.danger).child(message))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-new-sync")
                            .label("取消")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("add-sync-job")
                            .h_10()
                            .min_w_12()
                            .primary()
                            .label("添加任务")
                            .disabled(
                                busy || !can_browse
                                    || self.sync_remote.is_none()
                                    || self.sync_local.is_none()
                                    || minutes.is_none(),
                            )
                            .on_click(window.listener_for(owner, {
                                let error = error.clone();
                                move |this, _, window, cx| {
                                    if let Some(minutes) = minutes {
                                        match this.add_sync_job(minutes, cx) {
                                            Ok(()) => window.close_dialog(cx),
                                            Err(failure) => error.update(cx, |error, cx| {
                                                *error = Some(format!("{failure:#}").into());
                                                cx.notify();
                                            }),
                                        }
                                    }
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn sync_settings_job(
        &self,
        job: &crate::services::sync::SyncJob,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = job.id;
        let key = SharedString::from(id.to_string());
        let status = self.sync.read(cx).task_status(job);
        div()
            .id((ElementId::from("sync-job"), key.clone()))
            .flex()
            .items_center()
            .gap_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .pt_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(job.folder.display().to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} · {}", job.direction.label(), status)),
                    ),
            )
            .children(
                self.sync_conflicts_button(id, cx.weak_entity(), cx)
                    .map(|button| button.small()),
            )
            .child(self.sync_task_run_button(job, false, cx).small())
            .child(self.sync_job_pause_button(id, false, cx))
            .child(
                Button::new("sync-settings-details")
                    .small()
                    .outline()
                    .flex_shrink_0()
                    .label("同步策略")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_sync_task_details(id, SyncDetailTab::Policy, window, cx);
                    })),
            )
            .child(
                Button::new("remove-sync")
                    .small()
                    .ghost()
                    .flex_shrink_0()
                    .label("移除任务")
                    .disabled(self.busy.is_some() || self.sync.read(cx).running() == Some(id))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mut jobs = this.sync.read(cx).jobs().to_vec();
                        jobs.retain(|job| job.id != id);
                        let result = this
                            .sync
                            .update(cx, |service, cx| service.save_jobs(jobs, cx));
                        this.settings_saved(result, "同步任务已移除", cx);
                    })),
            )
            .into_any_element()
    }

    pub(super) fn sync_policy_controls(
        &self,
        job: &crate::services::sync::SyncJob,
        owner: &Entity<Self>,
        interval: &Entity<InputState>,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        let id = job.id;
        let disabled = self.busy.is_some()
            || self.sync.read(cx).is_quitting()
            || self.sync.read(cx).running() == Some(id);
        let minutes = sync_interval_minutes(interval, cx);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("自动触发"))
                    .child(
                        Switch::new("sync-timed")
                            .label("定时探测两端")
                            .checked(job.automatic)
                            .disabled(disabled)
                            .on_click(window.listener_for(owner, move |this, checked, _, cx| {
                                this.update_sync_trigger(id, |job| job.automatic = *checked, cx);
                            })),
                    )
                    .when(job.automatic, |form| {
                        form.child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child("每隔")
                                .child(
                                    div()
                                        .w_24()
                                        .child(Input::new(interval).small().disabled(disabled)),
                                )
                                .child("分钟")
                                .child(
                                    Button::new("save-sync-interval")
                                        .small()
                                        .outline()
                                        .label("保存间隔")
                                        .disabled(
                                            disabled
                                                || minutes.is_none()
                                                || minutes == Some(job.interval_minutes),
                                        )
                                        .on_click(window.listener_for(
                                            owner,
                                            move |this, _, _, cx| {
                                                if let Some(minutes) = minutes {
                                                    this.update_sync_trigger(
                                                        id,
                                                        |job| job.interval_minutes = minutes,
                                                        cx,
                                                    );
                                                }
                                            },
                                        )),
                                )
                                .when(minutes.is_none(), |row| {
                                    row.child(
                                        div()
                                            .text_color(cx.theme().danger)
                                            .child("请输入大于 0 的整数"),
                                    )
                                }),
                        )
                    })
                    .child(
                        Switch::new("sync-watch-local")
                            .label("本地目录变化时同步")
                            .checked(job.watch_local)
                            .disabled(disabled)
                            .on_click(window.listener_for(owner, move |this, checked, _, cx| {
                                this.update_sync_trigger(id, |job| job.watch_local = *checked, cx);
                            })),
                    )
                    .when(job.watch_local, |form| {
                        form.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child("包含子目录。S3 端的变化由定时探测发现。"),
                        )
                    })
                    .when_some(
                        self.sync.read(cx).directory_watch_error(id),
                        |form, error| {
                            form.child(div().text_color(cx.theme().danger).child(error.to_owned()))
                        },
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .pt_3()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("文件处理"))
                    .child(format!("同步方向  {}", job.direction.label()))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(job.direction.description()),
                    ),
            )
            .into_any_element()
    }

    fn update_sync_trigger(
        &mut self,
        id: uuid::Uuid,
        update: impl FnOnce(&mut crate::services::sync::SyncJob),
        cx: &mut Context<Self>,
    ) {
        let mut jobs = self.sync.read(cx).jobs().to_vec();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        update(job);
        let result = self
            .sync
            .update(cx, |service, cx| service.save_jobs(jobs, cx));
        self.settings_saved(result, "同步触发设置已保存", cx);
    }
}

fn sync_interval_minutes(interval: &Entity<InputState>, cx: &App) -> Option<u32> {
    interval
        .read(cx)
        .value()
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|minutes| *minutes > 0)
}
