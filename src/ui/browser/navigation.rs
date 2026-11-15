use super::*;

impl Browser {
    pub(super) fn rows(&self) -> &[Entry] {
        &self.visible_rows
    }

    pub(super) fn selection(&self) -> Option<Entry> {
        let key = self.selected.as_deref()?;
        self.rows().iter().find(|item| item.key() == key).cloned()
    }

    pub(super) fn rebuild_tree(&mut self) {
        self.visible_rows = self
            .listings
            .get(&self.prefix)
            .into_iter()
            .flatten()
            .filter(|row| !self.hide_dot_entries || !is_dot_path(row.key()))
            .cloned()
            .collect();
        self.file_sort.apply(&mut self.visible_rows);
        let visible_objects: HashSet<_> = self.visible_rows.iter().map(Entry::key).collect();
        self.object_multi
            .retain(|key| visible_objects.contains(key.as_str()));
        if self
            .object_anchor
            .as_deref()
            .is_some_and(|key| !visible_objects.contains(key))
        {
            self.object_anchor = None;
        }
        if self
            .selected
            .as_deref()
            .is_some_and(|key| !visible_objects.contains(key))
        {
            self.selected = self.object_multi.iter().min().cloned();
        }
        self.tree.clear();
        let mut pending = vec![(0, Entry::folder(String::new()))];
        while let Some((depth, item)) = pending.pop() {
            if self.expanded.contains(item.key())
                && let Some(children) = self.listings.get(item.key())
            {
                pending.extend(
                    children
                        .iter()
                        .rev()
                        .filter(|row| !self.hide_dot_entries || !is_dot_path(row.key()))
                        .map(|row| (depth + 1, row.clone())),
                );
            }
            self.tree.push((depth, item));
        }
        if !self.tree_multi.is_empty() {
            let visible: HashSet<_> = self.tree.iter().map(|(_, entry)| entry.key()).collect();
            self.tree_multi.retain(|key| visible.contains(key.as_str()));
            if self
                .tree_selected
                .as_ref()
                .is_some_and(|key| !self.tree_multi.contains(key))
            {
                self.tree_selected = self.tree_multi.iter().min().cloned();
            }
        }
    }

    pub(super) fn tree_create_prefix(&self) -> String {
        self.tree
            .iter()
            .map(|(_, entry)| entry)
            .find(|entry| Some(entry.key()) == self.tree_selected.as_deref())
            .map_or_else(
                || self.prefix.clone(),
                |entry| {
                    if entry.directory() {
                        entry.key().to_owned()
                    } else {
                        parent_prefix(entry.key()).to_owned()
                    }
                },
            )
    }

    pub(super) fn tree_create_row_index(&self) -> Option<(usize, usize)> {
        let draft = self.tree_create.as_ref()?;
        let parent = self
            .tree
            .iter()
            .position(|(_, entry)| entry.key() == draft.prefix)?;
        let depth = self.tree[parent].0 + 1;
        let index = if draft.directory {
            parent + 1
        } else {
            // File rows follow the direct child folders and their expanded descendants.
            let folders = self.tree[parent + 1..].iter().take_while(|(level, entry)| {
                *level > depth || (*level == depth && entry.directory())
            });
            parent + 1 + folders.count()
        };
        Some((index, depth))
    }

    pub(super) fn start_tree_create(
        &mut self,
        prefix: String,
        directory: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_tree_create(cx);
        if self.busy.is_some() || self.trash.is_some() || self.tree_create.is_some() {
            return;
        }
        if self.store.is_none() {
            return;
        }
        if self.folder_write_locked(&prefix) {
            return self.show_write_locked(cx);
        }
        self.error = None;
        self.clear_notice();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if directory {
                "文件夹名称"
            } else {
                "文件名"
            })
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_tree_create(cx);
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.submit_tree_create(cx));
        self.tree_create = Some(TreeCreate {
            prefix: prefix.clone(),
            directory,
            input: input.clone(),
            _subscription: subscription,
            _blur: blur,
        });
        let mut ancestor = prefix.as_str();
        loop {
            self.expanded.insert(ancestor.to_owned());
            if ancestor.is_empty() {
                break;
            }
            ancestor = parent_prefix(ancestor);
        }
        self.rebuild_tree();
        if let Some((index, _)) = self.tree_create_row_index() {
            self.tree_scroll
                .scroll_to_item(self.bucket_tree_offset() + index, ScrollStrategy::Nearest);
        }
        window.defer(cx, move |window, cx| {
            input.read(cx).focus_handle(cx).focus(window, cx)
        });
        if !self.listings.contains_key(&prefix) {
            self.load_folder(prefix, cx);
        }
        cx.notify();
    }

    pub(super) fn submit_tree_create(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let (Some(draft), Some(store)) = (&self.tree_create, self.store.clone()) else {
            return;
        };
        let directory = draft.directory;
        let prefix = draft.prefix.clone();
        let name = draft.input.read(cx).value();
        if name.trim().is_empty() {
            self.tree_create = None;
            cx.notify();
            return;
        }
        let target = match child_key(&prefix, &name, directory) {
            Ok(target) => target,
            Err(error) => return self.show_error(error, cx),
        };
        if self.write_locked(&target, directory) {
            return self.show_write_locked(cx);
        }
        let scope = self.write_scope(&store, [target.clone()]);
        let detail = format!("{:?} /{target:?}", store.bucket());
        let selected = self.tree_selected.clone();
        let visited = self.prefix.clone();
        let created = target.clone();
        if self.run_write(
            scope,
            if directory {
                "正在新建文件夹"
            } else {
                "正在新建文件"
            },
            detail,
            async move {
                if directory {
                    store.mkdir(target).await
                } else {
                    store.create_file(target).await
                }
            },
            move |this, result, cx| match result {
                Ok(()) => {
                    if this.prefix == visited
                        && this.tree_selected == selected
                        && this.tree_multi.is_empty()
                    {
                        this.tree_selected = Some(created);
                    }
                    this.show_notice("已创建", cx);
                }
                Err(error) => this.show_error(error, cx),
            },
            cx,
        ) {
            self.tree_create = None;
        }
    }

    pub(super) fn navigate(&mut self, target: String, cx: &mut Context<Self>) {
        let next = target.clone();
        if self.confirm_preview_leave(cx, move |this, _, cx| this.navigate(next, cx)) {
            return;
        }
        self.submit_tree_create(cx);
        if self.busy.is_some() || self.tree_create.is_some() {
            return;
        }
        self.tree_create = None;
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        if self
            .preview
            .as_ref()
            .is_some_and(|preview| matches!(preview.content, PreviewContent::Loading(_)))
        {
            self.close_preview();
        }
        #[cfg(windows)]
        if self.preview.as_ref().is_some_and(|preview| {
            matches!(
                preview.content,
                PreviewContent::Image(_) | PreviewContent::Pdf(_)
            )
        }) {
            self.close_preview();
        }
        self.show_preview = false;
        self.trash = None;
        self.trash_reload_requested = false;
        let needs_listing = !self.listings.contains_key(&target);
        self.set_location(target.clone(), cx);
        if needs_listing {
            self.load_folder(target, cx);
        }
    }

    fn load_folder(&mut self, prefix: String, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if !self.loading_folders.insert(prefix.clone()) {
            return;
        }
        let revision = self.bucket_revision;
        let epoch = self.listing_epoch;
        let key = prefix.clone();
        self.run_background(
            "正在读取目录",
            format!("{:?} /{prefix:?}", store.bucket()),
            async move { store.list(&key, true).await },
            move |this, result, cx| {
                if this.bucket_revision != revision {
                    return;
                }
                this.loading_folders.remove(&prefix);
                if this.listing_epoch != epoch {
                    if this.prefix == prefix || this.expanded.contains(&prefix) {
                        this.load_folder(prefix, cx);
                    }
                    return;
                }
                match result {
                    Ok(rows) => {
                        this.listings.insert(prefix, rows);
                        this.rebuild_tree();
                    }
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        );
    }

    pub(super) fn set_location(&mut self, target: String, cx: &mut Context<Self>) {
        self.tree_selected = Some(target.clone());
        self.expanded.insert(target.clone());
        let mut ancestor = target.as_str();
        while !ancestor.is_empty() {
            ancestor = parent_prefix(ancestor);
            self.expanded.insert(ancestor.to_owned());
        }
        if self.prefix != target {
            self.prefix = target;
            self.selected = None;
            self.object_multi.clear();
            self.object_anchor = None;
            self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.grid_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.rebuild_tree();
        cx.notify();
    }

    pub(super) fn disclose(&mut self, key: String, cx: &mut Context<Self>) {
        self.submit_tree_create(cx);
        if self.busy.is_some() || self.tree_create.is_some() {
            return;
        }
        self.tree_create = None;
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        self.tree_selected = Some(key.clone());
        if !self.expanded.remove(&key) {
            self.expanded.insert(key.clone());
            if !self.listings.contains_key(&key) {
                self.load_folder(key, cx);
            }
        }
        self.rebuild_tree();
        cx.notify();
    }

    pub(super) fn collapse_folders(&mut self, cx: &mut Context<Self>) {
        self.submit_tree_create(cx);
        if self.busy.is_some() || self.tree_create.is_some() {
            return;
        }
        self.tree_create = None;
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        self.expanded.retain(String::is_empty);
        self.tree_selected = Some(String::new());
        self.rebuild_tree();
        self.tree_scroll
            .scroll_to_item(self.bucket_tree_offset(), ScrollStrategy::Top);
        cx.notify();
    }

    pub(super) fn refresh_directory_listings(&mut self, cx: &mut Context<Self>) {
        if self.refreshing_listings
            || self.busy.is_some()
            || (self.pending_listing_refreshes.is_empty() && !self.refresh_requested)
        {
            return;
        }
        let pending = std::mem::take(&mut self.pending_listing_refreshes);
        let full = std::mem::take(&mut self.refresh_requested);
        let Some(store) = self.store.clone() else {
            return;
        };
        let connection = self.connection_revision;
        let pending: Vec<_> = pending
            .into_iter()
            .filter(|scope| scope.connection == connection)
            .collect();
        for scope in &pending {
            for (bucket, _) in &scope.folders {
                if bucket != store.bucket() {
                    self.parked_buckets.remove(bucket);
                }
            }
        }
        let mut prefixes: HashSet<_> = self
            .listings
            .keys()
            .filter(|prefix| {
                pending
                    .iter()
                    .any(|scope| scope.affects_listing(store.bucket(), prefix))
            })
            .cloned()
            .collect();
        if full {
            prefixes.extend(self.expanded.iter().cloned());
            let mut prefix = self.prefix.as_str();
            loop {
                prefixes.insert(prefix.to_owned());
                if prefix.is_empty() {
                    break;
                }
                prefix = parent_prefix(prefix);
            }
        }
        if prefixes.is_empty() {
            return;
        }
        let epoch = self.listing_epoch;
        let revision = self.bucket_revision;
        self.refreshing_listings = true;
        let job = self.runtime.spawn(async move {
            let mut listings = HashMap::new();
            let mut errors = Vec::new();
            for prefix in prefixes {
                match store.list(&prefix, true).await {
                    Ok(rows) => {
                        listings.insert(prefix, rows);
                    }
                    Err(error) => errors.push(format!("/{prefix}：{error:#}")),
                }
            }
            (listings, errors)
        });
        cx.spawn(async move |view, cx| {
            let result = job.await;
            let _ = view.update(cx, |this, cx| {
                this.refreshing_listings = false;
                if this.connection_revision == connection {
                    if this.listing_epoch != epoch || this.bucket_revision != revision {
                        // A foreground request may have landed newer listings.
                        this.pending_listing_refreshes.extend(pending);
                        if this.bucket_revision == revision {
                            this.refresh_requested |= full;
                        }
                    } else {
                        match result {
                            Ok((listings, errors)) => {
                                this.listings.extend(listings);
                                this.rebuild_tree();
                                if !errors.is_empty() {
                                    let message = format!("刷新目录失败：{}", errors.join("；"));
                                    this.log(true, message.clone());
                                    if full {
                                        this.show_error(anyhow::anyhow!(message), cx);
                                    }
                                }
                            }
                            Err(error) => this.log(true, format!("传输后刷新目录失败：{error}")),
                        }
                    }
                }
                this.refresh_directory_listings(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.store.is_some() {
            self.refresh_requested = true;
            self.refresh_directory_listings(cx);
            cx.notify();
        }
    }

    pub(super) fn toggle_tree_selection(&mut self, key: String, cx: &mut Context<Self>) {
        if key.is_empty() || self.busy.is_some() {
            return;
        }
        self.tree_create = None;
        self.tree_range_anchor = None;
        if self.tree_multi.is_empty()
            && let Some(previous) = self.tree_selected.as_ref()
            && !previous.is_empty()
        {
            self.tree_multi.insert(previous.clone());
        }
        if !self.tree_multi.remove(&key) {
            self.tree_multi.insert(key.clone());
        }
        self.tree_selected = if self.tree_multi.contains(&key) {
            Some(key)
        } else {
            self.tree
                .iter()
                .find(|(_, entry)| self.tree_multi.contains(entry.key()))
                .map(|(_, entry)| entry.key().to_owned())
        };
        cx.notify();
    }

    pub(super) fn select_tree_range(&mut self, index: usize, cx: &mut Context<Self>) {
        let key = self.tree[index].1.key();
        if key.is_empty() {
            return;
        }
        let anchor = self
            .tree_range_anchor
            .as_ref()
            .or(self.tree_selected.as_ref())
            .and_then(|anchor| {
                self.tree
                    .iter()
                    .position(|(_, entry)| entry.key() == anchor)
            })
            .unwrap_or(index);
        self.tree_range_anchor = Some(self.tree[anchor].1.key().to_owned());
        self.tree_multi = self.tree[index.min(anchor)..=index.max(anchor)]
            .iter()
            .map(|(_, entry)| entry.key())
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect();
        self.tree_selected = Some(key.to_owned());
        cx.notify();
    }
}

pub(super) fn is_dot_path(key: &str) -> bool {
    key.split('/').any(|segment| segment.starts_with('.'))
}
