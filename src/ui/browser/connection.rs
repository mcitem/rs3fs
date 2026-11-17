use super::*;

pub(super) struct BucketTree {
    listings: HashMap<String, Vec<Entry>>,
    pub(super) expanded: HashSet<String>,
    pub(super) rows: Vec<(usize, Entry)>,
}

use gpui_kit::component::checkbox::Checkbox;

impl Browser {
    pub(super) fn connect(
        &mut self,
        save_settings: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || self.fetching_connection_buckets {
            return;
        }
        if self.confirm_preview_leave(cx, move |this, window, cx| {
            this.connect(save_settings, window, cx);
        }) {
            return;
        }
        let (settings, access, secret) = match self.connection_draft(cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.show_error(error, cx);
                return;
            }
        };
        let mut profiles = self.profiles.clone();
        let id = if save_settings {
            match self.save_profile_into(&mut profiles, settings.clone(), &access, &secret, cx) {
                Ok(id) => Some(id),
                Err(error) => {
                    self.show_error(error, cx);
                    return;
                }
            }
        } else {
            self.selected_profile
        };
        self.tree_focus.focus(window, cx);
        let slots = self.transfer_slots.clone();
        self.bucket_request += 1;
        self.opening_bucket = None;
        let journal_connection = {
            use sha2::{Digest, Sha256};
            let key = serde_json::to_vec(&(
                id,
                settings.endpoint(),
                settings.region(),
                settings.profile(),
                access.as_str(),
            ))
            .expect("serializing connection identity");
            crate::services::store::hex_digest(&Sha256::digest(key))
        };
        self.run(
            "正在连接",
            connection_log_target(&settings),
            async move {
                let server = Self::connect_server(&settings, access, secret)
                    .await?
                    .with_transfer_slots(slots);
                let buckets = Self::connection_buckets(&server, &settings).await?;
                let current = if settings.bucket_names().next().is_some()
                    && let [bucket] = buckets.as_slice()
                    && !settings
                        .hidden_buckets()
                        .iter()
                        .any(|name| name == bucket.name())
                {
                    let store = bucket.open(&server);
                    let rows = store.list("", true).await?;
                    Some((store, rows))
                } else {
                    None
                };
                Ok((server, buckets, current, settings))
            },
            move |this, result, cx| match result {
                Ok((server, buckets, current, settings)) => {
                    this.log(false, format!("连接返回 {} 个存储桶", buckets.len()));
                    this.server = Some(server);
                    this.available_buckets = buckets;
                    this.connection_settings = settings;
                    this.close_settings();
                    this.connected_profile = id;
                    this.sync_remote = None;
                    let saved = if save_settings {
                        profiles.active = id;
                        this.persist_profiles(profiles)
                            .map(|()| this.selected_profile = id)
                    } else {
                        Ok(())
                    };
                    this.connection_revision += 1;
                    this.journal_connection = Some(journal_connection);
                    this.clear_bucket();
                    this.apply_bucket_visibility();
                    if let Some((store, rows)) = current {
                        this.set_bucket(store, rows, cx);
                    }
                    this.load_transfer_history(cx);
                    if let Err(error) = saved {
                        this.show_error(error.context("已连接，但无法保存连接配置"), cx);
                    }
                }
                Err(error) => this.show_error(error, cx),
            },
            cx,
        );
    }

    async fn connect_server(
        settings: &ConnectionSettings,
        access: String,
        secret: String,
    ) -> Result<Server> {
        let credentials = (!access.is_empty())
            .then(|| aws_sdk_s3::config::Credentials::new(access, secret, None, None, "s3fs"));
        Server::connect(
            settings.region().to_owned(),
            settings.endpoint().to_owned(),
            settings.profile().to_owned(),
            credentials,
        )
        .await
    }

    fn fetch_connection_buckets(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.fetching_connection_buckets {
            return;
        }
        let (settings, access, secret) = match self.server_draft(cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.show_error(error, cx);
                return;
            }
        };
        self.fetching_connection_buckets = true;
        let profile = self.selected_profile;
        self.run_background(
            "正在拉取存储桶",
            connection_log_target(&settings),
            async move {
                let server = Self::connect_server(&settings, access, secret).await?;
                server.buckets().await
            },
            move |this, result, cx| {
                this.fetching_connection_buckets = false;
                if this.selected_profile != profile {
                    return;
                }
                match result {
                    Ok(buckets) => {
                        this.log(false, format!("服务端返回 {} 个存储桶", buckets.len()));
                        this.fetched_bucket_names = Some(
                            buckets
                                .iter()
                                .map(|bucket| bucket.name().to_owned())
                                .collect(),
                        )
                    }
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        );
    }

    async fn connection_buckets(
        server: &Server,
        settings: &ConnectionSettings,
    ) -> Result<Vec<Bucket>> {
        let mut names: Vec<_> = settings.bucket_names().collect();
        if names.is_empty() {
            return server.buckets().await;
        }
        names.sort_unstable();
        names.dedup();
        Ok(names.into_iter().map(Bucket::named).collect())
    }

    fn clear_bucket(&mut self) {
        self.bucket_revision += 1;
        self.bucket_request += 1;
        self.opening_bucket = None;
        self.refreshing_buckets = false;
        self.loading_folders.clear();
        self.refresh_requested = false;
        self.store = None;
        self.reset_usage();
        self.close_preview();
        self.trash = None;
        self.trash_reload_requested = false;
        self.create_draft = None;
        self.rename = None;
        self.tree.clear();
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        self.visible_rows.clear();
        self.visible_row_indices.clear();
        self.listings.clear();
        self.parked_buckets.clear();
        self.upload_refresh_pending = false;
    }

    fn apply_bucket_visibility(&mut self) {
        let hidden = self.connection_settings.hidden_buckets();
        let is_hidden = |name: &str| hidden.iter().any(|item| item == name);
        self.buckets = self
            .available_buckets
            .iter()
            .filter(|bucket| !is_hidden(bucket.name()))
            .cloned()
            .collect();
        if self
            .store
            .as_ref()
            .is_some_and(|store| is_hidden(store.bucket()))
        {
            self.clear_bucket();
        }
        if self
            .sync_remote
            .as_ref()
            .is_some_and(|(bucket, _)| self.connection_settings.hidden_buckets().contains(bucket))
        {
            self.sync_remote = None;
        }
    }

    fn set_hidden_buckets(&mut self, id: Option<uuid::Uuid>, names: Vec<String>) -> Result<()> {
        let mut profiles = self.profiles.clone();
        if let Some(profile) = profiles
            .profiles
            .iter_mut()
            .find(|profile| Some(profile.id) == id)
        {
            profile.settings.set_hidden_buckets(names.clone());
            self.persist_profiles(profiles)?;
        }
        if self.server.is_some() && self.connected_profile == id {
            self.connection_settings.set_hidden_buckets(names);
            self.apply_bucket_visibility();
        }
        Ok(())
    }

    pub(super) fn hide_bucket_item(
        bucket: &str,
        owner: &WeakEntity<Self>,
        revision: u64,
        busy: bool,
    ) -> PopupMenuItem {
        let owner = owner.clone();
        let bucket = bucket.to_owned();
        PopupMenuItem::new("隐藏此存储桶")
            .disabled(busy)
            .on_click(move |_, _, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this.connection_revision != revision
                        || this.busy.is_some()
                        || !this.buckets.iter().any(|item| item.name() == bucket)
                    {
                        return;
                    }
                    let next = bucket.clone();
                    if this
                        .store
                        .as_ref()
                        .is_some_and(|store| store.bucket() == bucket)
                        && this.confirm_preview_leave(cx, move |this, _, cx| {
                            this.hide_bucket(next, cx)
                        })
                    {
                        return;
                    }
                    this.hide_bucket(bucket.clone(), cx);
                });
            })
    }

    fn hide_bucket(&mut self, bucket: String, cx: &mut Context<Self>) {
        let mut hidden = self.connection_settings.hidden_buckets().to_vec();
        hidden.push(bucket);
        let result = self.set_hidden_buckets(self.connected_profile, hidden);
        self.settings_saved(result, "已隐藏存储桶，可在「连接与凭证」中恢复", cx);
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_preview_leave(cx, Self::disconnect) {
            return;
        }
        self.server = None;
        self.journal_connection = None;
        self.clear_bucket();
        self.connected_profile = None;
        self.settings_page = Some(SettingsPage::Credentials);
        self.connection_revision += 1;
        self.buckets.clear();
        self.available_buckets.clear();
        self.error = None;
        self.clear_notice();
        self.log(false, "已断开连接");
        self.endpoint.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn selected_hidden_buckets(&self) -> &[String] {
        if self.server.is_some() && self.connected_profile == self.selected_profile {
            self.connection_settings.hidden_buckets()
        } else {
            self.selected_profile
                .and_then(|id| self.profiles.get(id))
                .map_or(&[], |profile| profile.settings.hidden_buckets())
        }
    }

    fn set_bucket(&mut self, store: Store, rows: Vec<Entry>, cx: &mut Context<Self>) {
        self.create_draft = None;
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        self.bucket_revision += 1;
        self.loading_folders.clear();
        self.refresh_requested = false;
        let auto_capacity = store.supports_server_capacity();
        self.rename = None;
        let previous = self.store.as_ref().map(|store| store.bucket().to_owned());
        let saved = self.parked_buckets.remove(store.bucket());
        if let Some(previous) = previous.filter(|name| name != store.bucket()) {
            let root_was_open = self.expanded.insert(String::new());
            if root_was_open {
                self.rebuild_tree();
                self.expanded.remove("");
            }

            self.parked_buckets.insert(
                previous,
                BucketTree {
                    listings: std::mem::take(&mut self.listings),
                    expanded: std::mem::take(&mut self.expanded),
                    rows: std::mem::take(&mut self.tree),
                },
            );
        }
        self.log(
            false,
            format!(
                "已打开存储桶 {:?}，根目录 {} 项",
                store.bucket(),
                rows.len()
            ),
        );
        self.store = Some(store);
        self.next_trash_cleanup = Instant::now();
        self.reset_usage();
        self.close_preview();
        self.trash = None;
        self.trash_reload_requested = false;
        self.prefix.clear();
        self.listings = HashMap::from([(String::new(), rows)]);
        self.expanded = HashSet::from([String::new()]);
        if let Some(saved) = saved {
            let root = self.listings.remove("").unwrap_or_default();
            self.listings = saved.listings;
            self.listings.insert(String::new(), root);
            self.expanded = saved.expanded;
            self.expanded.insert(String::new());
        }
        self.selected = None;
        self.object_multi.clear();
        self.object_anchor = None;
        self.tree_selected = Some(String::new());
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.grid_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.rebuild_tree();
        if auto_capacity {
            self.measure_usage(false, cx);
        }
    }

    pub(super) fn open_bucket(&mut self, bucket: Bucket, cx: &mut Context<Self>) {
        self.submit_create(cx);
        if self.busy.is_some() || self.create_draft.is_some() {
            return;
        }
        self.activate_bucket(bucket, None, false, cx);
    }

    pub(super) fn activate_bucket(
        &mut self,
        bucket: Bucket,
        entry: Option<Entry>,
        disclose: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let next_bucket = bucket.clone();
        let next_entry = entry.clone();
        if self.confirm_preview_leave(cx, move |this, _, cx| {
            this.activate_bucket(next_bucket, next_entry, disclose, cx);
        }) {
            return;
        }
        let Some(server) = self.server.as_ref() else {
            return;
        };
        let store = bucket.open(server);
        if self.opening_bucket.as_deref() == Some(store.bucket()) {
            return;
        }
        self.bucket_request += 1;
        self.opening_bucket = None;
        if entry.is_some()
            && let Some(rows) = self
                .parked_buckets
                .get(bucket.name())
                .and_then(|tree| tree.listings.get(""))
                .cloned()
        {
            self.set_bucket(store, rows, cx);
            if let Some(entry) = entry {
                if disclose {
                    self.disclose(entry.key().to_owned(), cx);
                } else {
                    let target = if entry.directory() {
                        entry.key()
                    } else {
                        parent_prefix(entry.key())
                    };
                    self.navigate(target.to_owned(), cx);
                    if !entry.directory() {
                        self.selected = Some(entry.key().to_owned());
                        self.tree_selected = self.selected.clone();
                    }
                }
            }
            return;
        }
        let request = self.bucket_request;
        let epoch = self.listing_epoch;
        self.opening_bucket = Some(store.bucket().to_owned());
        self.run_background(
            "正在打开存储桶",
            format!("{:?}", store.bucket()),
            async move {
                let rows = store.list("", true).await?;
                Ok((store, rows))
            },
            move |this, result, cx| {
                if this.bucket_request != request {
                    return;
                }
                this.opening_bucket = None;
                if this.has_pending_text_preview(cx) {
                    this.show_notice("当前预览还有未保存的更改，请处理后再切换存储桶", cx);
                    return;
                }
                match result {
                    Ok((store, rows)) => {
                        let changed = this.listing_epoch != epoch;
                        if changed {
                            this.parked_buckets.remove(store.bucket());
                        }
                        this.set_bucket(store, rows, cx);
                        if changed {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        );
    }

    pub(super) fn refresh_buckets(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.server.clone() else {
            return;
        };
        if self.busy.is_some() || self.refreshing_buckets {
            return;
        }
        self.refreshing_buckets = true;
        let revision = self.connection_revision;
        let settings = self.connection_settings.clone();
        self.run_background(
            "正在读取存储桶",
            connection_log_target(&settings),
            async move { Self::connection_buckets(&server, &settings).await },
            move |this, result, cx| {
                if this.connection_revision != revision {
                    return;
                }
                this.refreshing_buckets = false;
                match result {
                    Ok(buckets) => {
                        let total = buckets.len();
                        this.available_buckets = buckets;
                        this.apply_bucket_visibility();
                        this.log(
                            false,
                            format!("读取到 {total} 个存储桶，显示 {} 个", this.buckets.len()),
                        );
                    }
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        );
    }

    pub(super) fn bucket_tree_offset(&self) -> usize {
        let active = self.store.as_ref().map(Store::bucket);
        self.buckets
            .iter()
            .take_while(|bucket| Some(bucket.name()) != active)
            .map(|bucket| {
                self.parked_buckets
                    .get(bucket.name())
                    .filter(|tree| tree.expanded.contains(""))
                    .map_or(1, |tree| tree.rows.len().max(1))
            })
            .sum()
    }

    fn connection_draft(&self, cx: &App) -> Result<(ConnectionSettings, String, String)> {
        let (mut settings, access, secret) = self.server_draft(cx)?;
        if self.limit_connection_buckets {
            ensure!(
                !self.selected_buckets.is_empty(),
                "请至少勾选一个存储桶，或切换为使用所有可访问的桶。"
            );
            settings.set_bucket_names(self.selected_buckets.iter().map(String::as_str));
        }
        settings.set_hidden_buckets(self.selected_hidden_buckets().to_vec());
        Ok((settings, access, secret))
    }

    fn server_draft(&self, cx: &App) -> Result<(ConnectionSettings, String, String)> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let access = value(&self.access_key);
        let secret = value(&self.secret_key);
        ensure!(
            access.is_empty() == secret.is_empty(),
            "请同时填写 Access Key 和 Secret Key。"
        );
        let settings = ConnectionSettings::new(
            String::new(),
            value(&self.region),
            value(&self.endpoint),
            value(&self.profile),
        );
        Ok((settings, access, secret))
    }

    fn save_profile_into(
        &self,
        profiles: &mut ConnectionProfiles,
        settings: ConnectionSettings,
        access: &str,
        secret: &str,
        cx: &App,
    ) -> Result<uuid::Uuid> {
        let name = self.profile_name.read(cx).value().trim().to_owned();
        ensure!(!name.is_empty(), "请输入凭证名称。");
        let id = self.selected_profile.unwrap_or_else(uuid::Uuid::new_v4);
        let profile = ConnectionProfile {
            id,
            name,
            settings: settings.with_keys(access, secret)?,
        };
        if let Some(existing) = profiles.profiles.iter_mut().find(|item| item.id == id) {
            *existing = profile;
        } else {
            profiles.profiles.push(profile);
        }
        Ok(id)
    }

    pub(super) fn persist_profiles(&mut self, profiles: ConnectionProfiles) -> Result<()> {
        let path = ConnectionSettings::path()?;
        ConnectionProfiles::load(&path)?;
        write_settings(&path, &profiles)?;
        self.profiles = profiles;
        Ok(())
    }

    fn save_profile(&mut self, cx: &mut Context<Self>) {
        let result = (|| {
            let (settings, access, secret) = self.connection_draft(cx)?;
            let mut profiles = self.profiles.clone();
            let id = self.save_profile_into(&mut profiles, settings, &access, &secret, cx)?;
            self.persist_profiles(profiles)?;
            self.selected_profile = Some(id);
            Ok(())
        })();
        self.settings_saved(result, "凭证已保存", cx);
    }

    fn select_profile(
        &mut self,
        id: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = id.and_then(|id| self.profiles.get(id));
        let keys = profile.map(|profile| profile.settings.keys()).transpose();
        let (access, secret) = match keys {
            Ok(keys) => keys.unwrap_or_default(),
            Err(error) => {
                self.show_error(error, cx);
                return;
            }
        };
        let empty = ConnectionSettings::default();
        let settings = profile.map_or(&empty, |profile| &profile.settings);
        for (input, value) in [
            (
                &self.profile_name,
                profile.map_or("", |profile| profile.name.as_str()),
            ),
            (&self.region, settings.region()),
            (&self.endpoint, settings.endpoint()),
            (&self.profile, settings.profile()),
            (&self.access_key, &access),
            (&self.secret_key, &secret),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.selected_buckets = settings.bucket_names().map(str::to_owned).collect();
        self.limit_connection_buckets = !self.selected_buckets.is_empty();
        self.fetched_bucket_names = None;
        self.selected_profile = id;
        self.error = None;
        self.clear_notice();
        cx.notify();
    }

    fn delete_profile(&self, id: uuid::Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(id) else {
            return;
        };
        let title = format!("删除凭证「{}」？", profile.name);
        let owner = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            dialog
                .confirm()
                .title(title.clone())
                .description("将从本机移除这套凭证。")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .cancel_text("取消")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, window, cx| {
                    owner
                        .update(cx, |this, cx| {
                            let mut profiles = this.profiles.clone();
                            profiles.profiles.retain(|profile| profile.id != id);
                            if profiles.active == Some(id) {
                                profiles.active = None;
                            }
                            match this.persist_profiles(profiles) {
                                Ok(()) => {
                                    this.select_profile(None, window, cx);
                                    this.show_notice("凭证已删除", cx);
                                    true
                                }
                                Err(error) => {
                                    this.show_error(error, cx);
                                    false
                                }
                            }
                        })
                        .unwrap_or(false)
                })
        });
    }

    fn bucket_selection(&self, cx: &Context<Self>) -> impl IntoElement {
        let busy = self.busy.is_some() || self.fetching_connection_buckets;
        let fetched = self.fetched_bucket_names.as_deref().unwrap_or_default();
        let mut names: Vec<_> = fetched.iter().chain(&self.selected_buckets).collect();
        names.sort_unstable();
        names.dedup();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("fetch-connection-buckets")
                            .small()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label(if self.fetching_connection_buckets {
                                "正在拉取存储桶"
                            } else {
                                "拉取存储桶"
                            })
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.fetch_connection_buckets(cx)),
                            ),
                    )
                    .when(!names.is_empty(), |row| {
                        row.child(
                            Checkbox::new("all-connection-buckets")
                                .label("全选")
                                .checked(
                                    !self.limit_connection_buckets
                                        || names
                                            .iter()
                                            .all(|name| self.selected_buckets.contains(*name)),
                                )
                                .disabled(busy)
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.limit_connection_buckets = !*checked;
                                    this.selected_buckets.clear();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(
                self.fetched_bucket_names
                    .as_ref()
                    .is_some_and(Vec::is_empty),
                |list| {
                    list.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("服务器未返回存储桶"),
                    )
                },
            )
            .children(names.into_iter().map(|name| {
                let name = name.clone();
                Checkbox::new((
                    ElementId::from("connection-bucket"),
                    SharedString::from(name.clone()),
                ))
                .label(name.clone())
                .checked(!self.limit_connection_buckets || self.selected_buckets.contains(&name))
                .disabled(busy)
                .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                    if !this.limit_connection_buckets {
                        this.selected_buckets = this
                            .fetched_bucket_names
                            .iter()
                            .flatten()
                            .cloned()
                            .collect();
                        this.limit_connection_buckets = true;
                    }
                    if *checked {
                        this.selected_buckets.insert(name.clone());
                    } else {
                        this.selected_buckets.remove(&name);
                    }
                    cx.notify();
                }))
            }))
    }

    #[inline(never)]
    pub(super) fn credentials_settings(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let busy = self.busy.is_some() || self.fetching_connection_buckets;
        let editing_name = self
            .selected_profile
            .and_then(|id| self.profiles.get(id))
            .map_or("新建凭证", |profile| profile.name.as_str());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("连接与凭证"),
                    )
                    .when(self.server.is_some(), |row| {
                        row.child(
                            Button::new("disconnect")
                                .small()
                                .outline()
                                .icon(IconName::LogOut)
                                .label("断开连接")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.disconnect(window, cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("连接配置"),
                    )
                    .children(self.profiles.profiles.iter().map(|profile| {
                        let id = profile.id;
                        Button::new(SharedString::from(id.to_string()))
                            .ghost()
                            .small()
                            .justify_start()
                            .w_full()
                            .selected(self.selected_profile == Some(id))
                            .disabled(busy)
                            .accessibility_label(profile.name.clone())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_left()
                                    .child(profile.name.clone()),
                            )
                            .when(self.connected_profile == Some(id), |button| {
                                button.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("当前连接"),
                                )
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_profile(Some(id), window, cx);
                            }))
                    }))
                    .child(
                        div().flex().justify_start().pt_1().child(
                            Button::new("new-credentials")
                                .ghost()
                                .small()
                                .icon(IconName::FilePlus)
                                .label("新建凭证")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.select_profile(None, window, cx);
                                    this.profile_name
                                        .read(cx)
                                        .focus_handle(cx)
                                        .focus(window, cx);
                                })),
                        ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_lg()
                    .p_4()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(editing_name.to_owned()),
                    )
                    .child(
                        Form::horizontal()
                            .small()
                            .child(
                                Field::new()
                                    .label("名称")
                                    .child(Input::new(&self.profile_name).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("自定义端点")
                                    .child(Input::new(&self.endpoint).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("区域")
                                    .child(Input::new(&self.region).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("Access Key")
                                    .child(Input::new(&self.access_key).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("Secret Key")
                                    .child(Input::new(&self.secret_key).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("AWS 配置文件")
                                    .child(Input::new(&self.profile).disabled(busy)),
                            )
                            .child(
                                Field::new()
                                    .label("存储桶")
                                    .child(self.bucket_selection(cx)),
                            )
                            .when(!self.selected_hidden_buckets().is_empty(), |form| {
                                form.child(Field::new().label("已隐藏的桶").child(
                                    div().flex().flex_col().gap_1().children(
                                        self.selected_hidden_buckets().iter().map(|name| {
                                            let profile = self.selected_profile;
                                            let name = name.clone();
                                            Button::new((
                                                ElementId::from("restore-bucket"),
                                                SharedString::from(name.clone()),
                                            ))
                                            .small()
                                            .ghost()
                                            .label(format!("恢复显示 {name}"))
                                            .disabled(busy)
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    if this.selected_profile == profile
                                                        && this.busy.is_none()
                                                    {
                                                        let names = this
                                                            .selected_hidden_buckets()
                                                            .iter()
                                                            .filter(|item| *item != &name)
                                                            .cloned()
                                                            .collect();
                                                        let result =
                                                            this.set_hidden_buckets(profile, names);
                                                        this.settings_saved(
                                                            result,
                                                            "已恢复显示存储桶",
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            )
                                        }),
                                    ),
                                ))
                            })
                            .footer(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("connect")
                                            .primary()
                                            .label("保存并连接")
                                            .loading(busy)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.connect(true, window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("save-credentials")
                                            .label("保存")
                                            .disabled(busy)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.save_profile(cx)),
                                            ),
                                    )
                                    .when_some(self.selected_profile, |row, id| {
                                        row.child(
                                            Button::new("delete-credentials")
                                                .ghost()
                                                .label("删除")
                                                .disabled(busy)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.delete_profile(id, window, cx)
                                                    },
                                                )),
                                        )
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn connection_log_target(settings: &ConnectionSettings) -> String {
    let endpoint = if settings.endpoint().is_empty() {
        "默认 AWS 端点".to_owned()
    } else {
        url::Url::parse(settings.endpoint())
            .ok()
            .filter(|url| url.has_host())
            .map_or_else(
                || "自定义端点".to_owned(),
                |url| url.origin().ascii_serialization(),
            )
    };
    let region = settings.region();
    let region = if region.is_empty() { "自动" } else { region };
    let buckets = settings.bucket_names().count();
    let discovery = if buckets == 0 {
        "从服务端列举存储桶".to_owned()
    } else {
        format!("使用配置中的 {buckets} 个存储桶")
    };
    format!("{endpoint}，区域 {region}，{discovery}")
}
