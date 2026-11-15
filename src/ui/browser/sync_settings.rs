use super::*;

impl Browser {
    fn choose_sync_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = &self.server else { return };
        if self.connected_profile.is_none() || window.has_active_dialog(cx) {
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

    fn add_sync_job(&mut self, cx: &mut Context<Self>) {
        let (Some(profile), Some((bucket, prefix)), Some(folder)) = (
            self.connected_profile,
            self.sync_remote.clone(),
            self.sync_local.clone(),
        ) else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let result = (|| {
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
                conflict_policy: self.sync_conflict_policy,
                automatic: self.new_sync_automatic,
                propagate_deletes: false,
            });
            self.persist_profiles(self.profiles.clone())?;
            self.sync
                .update(cx, |service, cx| service.save_jobs(jobs, cx))
        })();
        if result.is_ok() {
            self.sync_remote = None;
            self.sync_local = None;
        }
        self.settings_saved(result, "同步任务已添加", cx);
    }

    fn set_sync_conflict_policy(
        &mut self,
        id: uuid::Uuid,
        policy: ConflictPolicy,
        cx: &mut Context<Self>,
    ) {
        let service = self.sync.read(cx);
        if service.running() == Some(id) {
            return;
        }
        let mut jobs = service.jobs().to_vec();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        job.conflict_policy = policy;
        let result = self
            .sync
            .update(cx, |service, cx| service.save_jobs(jobs, cx));
        self.settings_saved(result, "冲突策略已保存", cx);
    }

    fn set_sync_deletes(&mut self, id: uuid::Uuid, enabled: bool, cx: &mut Context<Self>) -> bool {
        if self.sync.read(cx).running() == Some(id) {
            self.show_error(anyhow::anyhow!("请等待本次同步结束。"), cx);
            return false;
        }
        let mut jobs = self.sync.read(cx).jobs().to_vec();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return false;
        };
        job.propagate_deletes = enabled;
        let result = self
            .sync
            .update(cx, |service, cx| service.save_jobs(jobs, cx));
        let saved = result.is_ok();
        self.settings_saved(result, "删除同步设置已保存", cx);
        saved
    }

    pub(super) fn sync_changed(&mut self, cx: &mut Context<Self>) {
        let service = self.sync.read(cx);
        let active = service.running();
        let started = active
            .filter(|id| Some(*id) != self.observed_sync_run)
            .and_then(|id| {
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
            .filter(|id| Some(*id) != active)
            .and_then(|id| {
                let job = service.jobs().iter().find(|job| job.id == id)?;
                let run = service.last_run(id)?;
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
        let form = self.sync_settings_form(cx);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(form)
            .when_some(service.config_error(), |view, error| {
                view.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.to_owned()),
                )
            })
            .children(
                service
                    .jobs()
                    .iter()
                    .map(|job| self.sync_settings_job(job, cx)),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn sync_settings_form(&self, cx: &Context<Self>) -> AnyElement {
        use crate::services::sync::Direction;
        let busy = self.busy.is_some();
        let theme = cx.theme();
        let can_browse =
            self.connected_profile.is_some() && (self.store.is_some() || !self.buckets.is_empty());
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
                    .text_sm()
                    .child("双向任务可设置同路径文件的冲突策略。删除同步可按任务开启。"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("choose-sync-remote")
                            .small()
                            .label("选择 S3 目录…")
                            .disabled(busy || !can_browse)
                            .on_click(cx.listener(|this, _, window, cx| {
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
                            .label("选择本地文件夹…")
                            .on_click(cx.listener(|this, _, window, cx| {
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(
                        [Direction::Upload, Direction::Download, Direction::Both].map(
                            |direction| {
                                Button::new(direction.label())
                                    .small()
                                    .ghost()
                                    .label(direction.label())
                                    .selected(direction == self.sync_direction)
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.sync_direction = direction;
                                        cx.notify();
                                    }))
                            },
                        ),
                    )
                    .child(
                        Button::new("add-sync-job")
                            .small()
                            .label("添加任务")
                            .disabled(
                                busy || !can_browse
                                    || self.sync_remote.is_none()
                                    || self.sync_local.is_none(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.add_sync_job(cx))),
                    ),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                match self.sync_direction {
                    Direction::Upload => "仅将本地新增文件和允许传播的修改上传到 S3。",
                    Direction::Download => "仅将 S3 新增文件和允许传播的修改下载到本地。",
                    Direction::Both => "任一端新增或单独修改文件时同步到另一端。",
                },
            ))
            .when(self.sync_direction == Direction::Both, |view| {
                view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child("冲突策略")
                        .children(ConflictPolicy::ALL.map(|policy| {
                            Button::new((
                                ElementId::from("new-sync-policy"),
                                SharedString::from(policy.label()),
                            ))
                            .small()
                            .ghost()
                            .label(policy.label())
                            .tooltip(policy.description())
                            .selected(self.sync_conflict_policy == policy)
                            .disabled(busy)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.sync_conflict_policy = policy;
                                    cx.notify();
                                },
                            ))
                        })),
                )
                .child(
                    div().text_sm().text_color(theme.muted_foreground).child(
                        "首次遇到同路径文件或两端都修改同一文件时，优先策略允许覆盖另一端。",
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Switch::new("new-sync-automatic")
                            .label("新任务启用自动同步")
                            .checked(self.new_sync_automatic)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.new_sync_automatic = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("view-sync-tasks")
                            .small()
                            .ghost()
                            .label("查看同步状态")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_sync_tasks(window, cx);
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
        use crate::services::sync::Direction;
        let service = self.sync.read(cx);
        let theme = cx.theme();
        let id = job.id;
        let running = self.sync.read(cx).running() == Some(id);
        let policy = job.conflict_policy;
        let remote = format!("{} /{}，{}", job.bucket, job.prefix, job.direction.label());
        div()
            .id((
                ElementId::from("sync-job"),
                SharedString::from(id.to_string()),
            ))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .pt_3()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(job.folder.display().to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(remote),
            )
            .when(job.direction == Direction::Both, |row| {
                row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child("冲突策略")
                        .children(ConflictPolicy::ALL.map(|choice| {
                            Button::new((
                                ElementId::from("sync-policy"),
                                SharedString::from(choice.label()),
                            ))
                            .small()
                            .ghost()
                            .label(choice.label())
                            .tooltip(choice.description())
                            .selected(policy == choice)
                            .disabled(running || policy == choice)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.set_sync_conflict_policy(id, choice, cx);
                                },
                            ))
                        })),
                )
            })
            .when_some(service.last_run(id), |row, run| {
                row.child(
                    div()
                        .text_sm()
                        .text_color(if !run.outcome().has_issue() {
                            theme.muted_foreground
                        } else {
                            theme.danger
                        })
                        .child(format!("{}，{}", run.outcome().label(), run.message())),
                )
            })
            .child(self.sync_settings_job_actions(job, cx))
            .into_any_element()
    }

    #[inline(never)]
    fn sync_settings_job_actions(
        &self,
        job: &crate::services::sync::SyncJob,
        cx: &Context<Self>,
    ) -> AnyElement {
        let service = self.sync.read(cx);
        let id = job.id;
        let busy = self.busy.is_some();
        let running = service.running() == Some(id);
        let available = self.profiles.get(job.profile).is_some();
        let automatic = job.automatic;
        let propagate_deletes = job.propagate_deletes;
        let mut actions = div().flex().gap_2();
        actions = actions.child(
            Button::new((
                ElementId::from("run-sync"),
                SharedString::from(id.to_string()),
            ))
            .small()
            .label(if running {
                "正在同步"
            } else {
                "立即同步"
            })
            .disabled(service.running().is_some() || service.is_quitting() || !available)
            .tooltip(if available {
                "立即同步"
            } else {
                "任务所用的凭证已删除"
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.sync
                    .update(cx, |service, cx| service.start(id, false, cx))
            })),
        );
        actions = actions.child(
            Button::new((
                ElementId::from("auto-sync"),
                SharedString::from(id.to_string()),
            ))
            .small()
            .ghost()
            .label(if automatic {
                "关闭自动同步"
            } else {
                "开启自动同步"
            })
            .selected(automatic)
            .disabled(busy)
            .tooltip("应用在托盘运行时，每约 5 分钟检查一次")
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut jobs = this.sync.read(cx).jobs().to_vec();
                if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                    job.automatic = !automatic;
                }
                let result = this
                    .sync
                    .update(cx, |service, cx| service.save_jobs(jobs, cx));
                this.settings_saved(result, "自动同步设置已保存", cx);
            })),
        );
        actions = actions.child(Button::new((ElementId::from("sync-deletes"), SharedString::from(id.to_string())))
                                    .small().ghost()
                                    .label(if propagate_deletes { "关闭删除同步" } else { "开启删除同步" })
                                    .selected(propagate_deletes)
                                    .disabled(busy || running)
                                    .tooltip("按同步方向删除另一端未改动的文件")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if propagate_deletes {
                                            this.set_sync_deletes(id, false, cx);
                                        } else {
                                            let owner = cx.weak_entity();
                                            window.open_alert_dialog(cx, move |dialog, _, _| {
                                                let owner = owner.clone();
                                                dialog.confirm().title("开启删除同步？")
                                                    .description("按任务方向删除另一端未改动的文件。本地文件会直接删除，远端删除遵循存储桶的版本策略。")
                                                    .button_props(DialogButtonProps::default().ok_text("开启").cancel_text("取消"))
                                                    .on_ok(move |_, _, cx| owner.update(cx, |this, cx| {
                                                        this.set_sync_deletes(id, true, cx)
                                                    }).unwrap_or(false))
                                            });
                                        }
                                    })),
                            );
        actions = actions.child(
            Button::new((
                ElementId::from("remove-sync"),
                SharedString::from(id.to_string()),
            ))
            .small()
            .ghost()
            .label("移除任务")
            .disabled(busy || running)
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut jobs = this.sync.read(cx).jobs().to_vec();
                jobs.retain(|job| job.id != id);
                let result = this
                    .sync
                    .update(cx, |service, cx| service.save_jobs(jobs, cx));
                this.settings_saved(result, "同步任务已移除", cx);
            })),
        );
        actions.into_any_element()
    }
}
