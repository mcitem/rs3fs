use super::*;

impl Browser {
    fn quota(&self) -> Option<i64> {
        self.quotas
            .get(self.connected_profile?, self.store.as_ref()?.bucket())
    }

    pub(super) fn capacity_label(&self) -> String {
        if let Some(capacity) = self.server_capacity {
            return if capacity.total > 0 {
                format!(
                    "{} / {}",
                    size_label(capacity.used),
                    size_label(capacity.total)
                )
            } else {
                format!("已用 {}，服务端未设配额", size_label(capacity.used))
            };
        }
        match (self.bucket_usage, self.quota()) {
            (Some(used), Some(quota)) => {
                format!("{} / {}（参考）", size_label(used), size_label(quota))
            }
            (Some(used), None) => format!("已用 {}", size_label(used)),
            (None, Some(quota)) => format!("参考上限 {}", size_label(quota)),
            (None, None) => "存储容量".to_owned(),
        }
    }

    pub(super) fn open_storage_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tasks_maximized = false;
        self.settings_page = Some(SettingsPage::Storage);
        self.settings_focus.focus(window, cx);
        let value = self
            .quota()
            .map(|bytes| (bytes / BYTES_PER_MB as i64).to_string())
            .unwrap_or_default();
        self.quota_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    pub(super) fn reset_usage(&mut self) {
        self.usage_scan = None;
        self.bucket_usage = None;
        self.server_capacity = None;
        self.quota_query_failed = false;
        self.usage_revision = self.usage_revision.wrapping_add(1);
    }

    pub(super) fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        let scan_objects = self.bucket_usage.is_some() && self.server_capacity.is_none()
            || self
                .usage_scan
                .as_ref()
                .is_some_and(|scan| scan.scan_objects);
        let refresh = scan_objects
            || self.server_capacity.is_some()
            || self.usage_scan.is_some()
            || self
                .store
                .as_ref()
                .is_some_and(Store::supports_server_capacity);
        if refresh {
            self.usage_revision = self.usage_revision.wrapping_add(1);
            self.usage_scan = None;
            self.measure_usage(scan_objects, cx);
        }
    }

    pub(super) fn measure_usage(&mut self, scan_objects: bool, cx: &mut Context<Self>) {
        if self.usage_scan.is_some() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let bucket_revision = self.bucket_revision;
        let revision = self.usage_revision;
        let scan = self.runtime.spawn(async move {
            let quota =
                tokio::time::timeout(Duration::from_secs(12), store.server_capacity()).await;
            match quota {
                Ok(Ok(Some(capacity))) => (Some(capacity), false, Ok(Some(capacity.used))),
                other => (
                    None,
                    !matches!(other, Ok(Ok(None))),
                    if scan_objects {
                        store.usage().await.map(Some)
                    } else {
                        Ok(None)
                    },
                ),
            }
        });
        let abort = scan.abort_handle();
        let completion = cx.spawn(async move |view, cx| {
            let result = scan.await;
            let _ = view.update(cx, |this, cx| {
                if bucket_revision == this.bucket_revision && revision == this.usage_revision {
                    this.usage_scan = None;
                    match result {
                        Ok((capacity, failed, usage)) => {
                            this.quota_query_failed = failed;
                            match usage {
                                Ok(Some(bytes)) => {
                                    this.server_capacity = capacity;
                                    this.bucket_usage = Some(bytes);
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    this.quota_query_failed = true;
                                    this.show_error(error, cx);
                                }
                            }
                        }
                        Err(error) => this.show_error(error.into(), cx),
                    }
                }
                cx.notify();
            });
        });
        self.usage_scan = Some(UsageScan {
            scan_objects,
            abort,
            _completion: completion,
        });
        cx.notify();
    }

    pub(super) fn save_quota(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let result = (|| {
            let profile = self.connected_profile.context("请先保存并连接凭证。")?;
            let bucket = self.store.as_ref().context("请先打开存储桶。")?.bucket();
            let input = self.quota_input.read(cx).value();
            let bytes = if input.trim().is_empty() {
                None
            } else {
                Some(
                    input
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|value| *value > 0)
                        .and_then(|value| value.checked_mul(BYTES_PER_MB as i64))
                        .context("请输入有效的正整数 MiB。")?,
                )
            };
            let bucket = bucket.to_owned();
            self.persist_profiles(self.profiles.clone())?;
            self.quotas.save(profile, &bucket, bytes)
        })();
        self.settings_saved(result, "配额已保存", cx);
    }

    pub(super) fn storage_settings(&self, cx: &Context<Self>) -> AnyElement {
        let busy = self.busy.is_some();
        div().flex().flex_col().gap_5()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("存储容量"))
            .when(self.store.is_none(), |view| view.child("打开存储桶后可统计用量。"))
            .when_some(self.store.as_ref(), |view, store| view.child(
                Form::horizontal().small()
                    .child(Field::new().label("当前存储桶").child(div().text_sm().child(store.bucket().to_owned())))
                    .child(Field::new().label("用量与配额")
                        .description(if self.server_capacity.is_some() && self.quota_query_failed {
                            "暂时无法更新 OSCA 配额，当前显示上一次结果。"
                        } else if self.server_capacity.is_some() {
                            "用量和配额来自 OSCA 服务端。"
                        } else if self.quota_query_failed {
                            "暂时无法读取 OSCA 配额。用量按当前可列出的对象统计。"
                        } else {
                            "统计当前可列出的对象，包含应用回收站。历史版本和未完成的分片上传不计入。"
                        })
                        .child(div().flex().items_center().gap_3()
                            .child(div().text_sm().child(self.capacity_label()))
                            .child(Button::new("measure-usage").label("统计用量")
                                .loading(self.usage_scan.is_some())
                                .disabled(self.usage_scan.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.measure_usage(true, cx))))))
                    .child(Field::new().label("本地参考上限")
                        .child(div().flex().items_center().gap_2()
                            .child(div().w_32().child(Input::new(&self.quota_input)
                                .aria_label("本地参考上限").disabled(busy)))
                            .child(div().text_sm().text_color(cx.theme().muted_foreground).child("MiB"))
                            .child(Button::new("save-quota").label("保存")
                                .disabled(busy || self.connected_profile.is_none())
                                .on_click(cx.listener(|this, _, _, cx| this.save_quota(cx))))))
            ))
            .into_any_element()
    }
}
