use super::*;

impl Browser {
    pub(super) fn trash_settings(&self, cx: &Context<Self>) -> AnyElement {
        div().flex().flex_col().gap_3()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("回收站"))
            .child("设置回收记录的保留天数。输入 0 或留空后保存，可关闭自动清理。")
            .child(div().flex().items_center().gap_2()
                .child("保留天数")
                .child(div().w_40().child(Input::new(&self.trash_days_input).small().aria_label("保留天数")))
                .child(Button::new("save-trash-retention").small().label("保存")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let result = (|| {
                            let value = this.trash_days_input.read(cx).value();
                            let days = match value.trim() {
                                "" => None,
                                value => Some(value.parse::<u32>().context("请输入整数天数")?),
                            };
                            let policy = TrashRetention::new(days)?;
                            policy.save()?;
                            this.trash_retention = policy;
                            this.next_trash_cleanup = Instant::now();
                            Ok(())
                        })();
                        this.settings_saved(result, "回收站设置已保存", cx);
                    }))))
            .child(div().text_sm().text_color(cx.theme().muted_foreground)
                .child("应用窗口打开时，每小时检查当前打开的存储桶。到期项目将永久删除，未完成的回收记录会保留。"))
            .into_any_element()
    }

    pub(super) fn trash_busy(&self) -> bool {
        self.store.as_ref().is_some_and(|store| {
            self.trash_jobs
                .contains(&(self.connection_revision, store.bucket().to_owned()))
        })
    }

    fn run_trash<T: Send + 'static>(
        &mut self,
        bucket: String,
        write: Option<u64>,
        label: &'static str,
        detail: String,
        future: impl Future<Output = Result<T>> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let key = (self.connection_revision, bucket);
        debug_assert!(!self.trash_jobs.contains(&key));
        self.trash_jobs.insert(key.clone());
        self.run_background(
            label,
            detail,
            future,
            move |this, result, cx| {
                this.trash_jobs.remove(&key);
                if let Some(id) = write {
                    this.finish_write(id, cx);
                }
                if this.connection_revision == key.0
                    && this
                        .store
                        .as_ref()
                        .is_some_and(|store| store.bucket() == key.1)
                {
                    finish(this, result, cx);
                    if this.trash.is_some() && std::mem::take(&mut this.trash_reload_requested) {
                        this.open_trash(cx);
                    }
                }
            },
            cx,
        );
    }

    pub(super) fn clean_expired_trash(&mut self, cx: &mut Context<Self>) {
        let Some(days) = self.trash_retention.days() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.busy.is_some() || self.trash_busy() || Instant::now() < self.next_trash_cleanup {
            return;
        }
        self.next_trash_cleanup = Instant::now() + Duration::from_secs(3600);
        let cutoff = chrono::Utc::now().timestamp() - i64::from(days) * 86_400;
        self.purge_trash_entries(store, Some(cutoff), None, cx);
    }

    fn confirm_empty_trash(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.busy.is_some() || self.trash_busy() || window.has_active_dialog(cx) {
            return;
        }
        let ids: HashSet<_> = self
            .trash
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|entry| !entry.pending() && !entry.interrupted())
            .map(TrashEntry::id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let revision = self.bucket_revision;
        let owner = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            let store = store.clone();
            let ids = ids.clone();
            dialog
                .confirm()
                .title(format!("清空「{}」的回收站？", store.bucket()))
                .description(format!(
                    "将永久删除当前的 {} 项，无法从回收站还原。未完成的回收记录会保留。",
                    ids.len()
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("清空回收站")
                        .cancel_text("取消")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, _, cx| {
                    owner
                        .update(cx, |this, cx| {
                            if this.bucket_revision != revision {
                                return true;
                            }
                            if this.busy.is_some() || this.trash_busy() {
                                return false;
                            }
                            this.purge_trash_entries(store.clone(), None, Some(ids.clone()), cx);
                            true
                        })
                        .unwrap_or(true)
                })
        });
    }

    fn purge_trash_entries(
        &mut self,
        store: Store,
        cutoff: Option<i64>,
        ids: Option<HashSet<uuid::Uuid>>,
        cx: &mut Context<Self>,
    ) {
        let scheduler = self.sync.read(cx).scheduler();
        self.run_trash(
            store.bucket().to_owned(),
            None,
            if cutoff.is_some() {
                "正在自动清理回收站"
            } else {
                "正在清空回收站"
            },
            format!("{:?}", store.bucket()),
            async move {
                let mut failures = Vec::new();
                let mut removed = 0;
                let entries = store.list_trash().await?;
                let mut remaining = Vec::new();
                for entry in entries {
                    if entry.pending()
                        || entry.interrupted()
                        || cutoff.is_some_and(|date| !entry.expired(date))
                        || ids.as_ref().is_some_and(|ids| !ids.contains(&entry.id()))
                    {
                        remaining.push(entry);
                    } else {
                        let scope = Scope::trash_record(
                            store.endpoint(),
                            store.bucket(),
                            &entry.manifest(),
                        );
                        let _reservation = scheduler.acquire(vec![scope]).await;
                        if let Err(error) = store.purge_trash(&entry).await {
                            failures.push(format!("/{}：{error:#}", entry.original()));
                            remaining.push(entry);
                        } else {
                            removed += 1;
                        }
                    }
                }
                Ok((remaining, removed, failures))
            },
            move |this, result, cx| match result {
                Ok((remaining, removed, failures)) => {
                    this.log(
                        false,
                        format!(
                            "回收站，删除 {removed} 项，失败 {} 项，剩余 {} 项",
                            failures.len(),
                            remaining.len()
                        ),
                    );
                    if this.trash.is_some() {
                        this.trash = Some(remaining);
                    }
                    if removed > 0 {
                        this.refresh_usage(cx);
                    }
                    if !failures.is_empty() {
                        this.show_error(
                            anyhow::anyhow!(
                                "已删除 {removed} 项，失败 {} 项：\n{}",
                                failures.len(),
                                failures.join("\n")
                            ),
                            cx,
                        );
                    } else if cutoff.is_none() {
                        this.show_notice(format!("已从回收站删除 {removed} 项"), cx);
                    }
                }
                Err(error) => this.show_error(error, cx),
            },
            cx,
        );
    }

    pub(super) fn open_trash(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        self.trash.get_or_insert_with(Vec::new);
        if self.trash_busy() {
            return;
        }
        self.run_trash(
            store.bucket().to_owned(),
            None,
            "正在读取回收站",
            format!("{:?}", store.bucket()),
            async move { store.list_trash().await },
            |this, result, cx| match result {
                Ok(entries) => {
                    this.log(false, format!("回收站返回 {} 项", entries.len()));
                    if this.trash.is_some() {
                        this.trash = Some(entries);
                    }
                }
                Err(error) => this.show_error(error, cx),
            },
            cx,
        );
    }

    fn trash_action(&mut self, entry: TrashEntry, purge: bool, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if self.busy.is_some() || self.trash_busy() {
            return;
        }
        let (write, mut scopes) = if purge {
            (None, Vec::new())
        } else {
            let scope = self.write_scope(&store, [entry.original().to_owned()]);
            let scopes = scope.remote_writes(&store);
            let Some(id) = self.reserve_write(scope, cx) else {
                return;
            };
            (Some(id), scopes)
        };
        scopes.push(Scope::trash_record(
            store.endpoint(),
            store.bucket(),
            &entry.manifest(),
        ));
        let label = if purge {
            "正在从回收站删除"
        } else {
            "正在还原"
        };
        let scheduler = self.sync.read(cx).scheduler();
        self.run_trash(
            store.bucket().to_owned(),
            write,
            label,
            format!("{:?} /{:?}", store.bucket(), entry.original()),
            async move {
                let result = {
                    let _reservation = scheduler.acquire(scopes).await;
                    if purge {
                        store.purge_trash(&entry).await
                    } else {
                        store.restore_trash(&entry).await
                    }
                };
                Ok((result, store.list_trash().await))
            },
            move |this, result, cx| match result {
                Ok((operation, entries)) => {
                    this.refresh_usage(cx);
                    match entries {
                        Ok(entries) => {
                            if this.trash.is_some() {
                                this.trash = Some(entries);
                            }
                        }
                        Err(error) => this.show_error(error, cx),
                    }
                    if let Err(error) = operation {
                        this.show_error(error, cx);
                    }
                }
                Err(error) => this.show_error(error, cx),
            },
            cx,
        );
    }

    fn confirm_purge(&self, entry: TrashEntry, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let revision = self.bucket_revision;
        let title = format!("从回收站删除「/{}」？", entry.original());
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            let entry = entry.clone();
            dialog
                .confirm()
                .title(title.clone())
                .description("删除后无法通过此回收站还原。")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("从回收站删除")
                        .cancel_text("取消")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, _, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.bucket_revision == revision {
                            this.trash_action(entry.clone(), true, cx);
                        }
                    });
                    true
                })
        });
    }

    #[inline(never)]
    pub(super) fn trash_pane(&self, cx: &Context<Self>) -> AnyElement {
        let entries = self.trash.as_deref().unwrap_or_default();
        let busy = self.busy.is_some() || self.trash_busy();
        let theme = cx.theme();
        let view = cx.weak_entity();
        let rows = uniform_list("trash-rows", entries.len(), move |range, _, cx| {
            view.update(cx, |this, cx| {
                let entries = this.trash.as_deref().unwrap_or_default();
                range
                    .filter_map(|index| entries.get(index))
                    .map(|entry| this.trash_row(entry, cx))
                    .collect()
            })
            .unwrap_or_default()
        })
        .size_full();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_11()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("leave-trash")
                            .small()
                            .ghost()
                            .label("返回文件")
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trash = None;
                                this.trash_reload_requested = false;
                                this.list_focus.focus(window, cx);
                            })),
                    )
                    .child(div().flex_1().child(format!(
                        "{} / 回收站",
                        self.store.as_ref().map_or("", Store::bucket)
                    )))
                    .child(
                        Button::new("empty-trash")
                            .small()
                            .ghost()
                            .label("清空")
                            .disabled(
                                busy || !entries
                                    .iter()
                                    .any(|entry| !entry.pending() && !entry.interrupted()),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_empty_trash(window, cx)
                            })),
                    )
                    .child(
                        Button::new("refresh-trash")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .tooltip("刷新回收站")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.open_trash(cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(if entries.is_empty() {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted_foreground)
                            .child(if busy {
                                "正在处理回收站"
                            } else if self.error.is_some() {
                                "可刷新回收站重试"
                            } else {
                                "回收站为空"
                            })
                            .into_any_element()
                    } else {
                        rows.into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn trash_row(&self, entry: &TrashEntry, cx: &Context<Self>) -> AnyElement {
        let restore = entry.clone();
        let purge = entry.clone();
        let id = entry.id();
        let busy = self.busy.is_some() || self.trash_busy();
        div()
            .flex()
            .items_center()
            .gap_2()
            .h_12()
            .w_full()
            .px_4()
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().truncate().child(format!("/{}", entry.original())))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if entry.pending() {
                                format!("{}，删除操作未确认", entry.deleted_at())
                            } else if entry.interrupted() {
                                format!("{}，移入未完成", entry.deleted_at())
                            } else {
                                entry.deleted_at()
                            }),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("restore-{id}")))
                    .small()
                    .ghost()
                    .label("还原")
                    .disabled(
                        busy || !entry.can_restore()
                            || self.write_locked(entry.original(), entry.original().ends_with('/')),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.trash_action(restore.clone(), false, cx)
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("purge-{id}")))
                    .small()
                    .ghost()
                    .label("删除")
                    .disabled(busy || entry.pending())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_purge(purge.clone(), window, cx)
                    })),
            )
            .into_any_element()
    }
}
