use super::*;
use crate::services::transfer_journal::TransferOperation;

impl Browser {
    pub(super) fn after_removed_entry(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        let key = entry.key();
        let removed =
            |candidate: &str| candidate == key || (entry.directory() && candidate.starts_with(key));
        self.tree_multi.retain(|candidate| !removed(candidate));
        self.object_multi.retain(|candidate| !removed(candidate));
        if self.object_anchor.as_deref().is_some_and(removed) {
            self.object_anchor = None;
        }
        if self.selected.as_deref().is_some_and(removed) {
            self.selected = self.object_multi.iter().min().cloned();
        }
        if !self.has_pending_text_preview(cx)
            && self
                .preview
                .as_ref()
                .is_some_and(|preview| removed(preview.entry.key()))
        {
            self.close_preview();
        }
        if entry.directory() {
            self.expanded.retain(|candidate| !removed(candidate));
            if self.prefix.starts_with(key) {
                self.set_location(parent_prefix(key).to_owned(), cx);
            }
        }
        if self.tree_selected.as_deref().is_some_and(removed) {
            self.tree_selected = Some(self.prefix.clone());
        }
    }

    fn change(
        &mut self,
        scope: TransferScopes,
        label: &'static str,
        detail: String,
        removed_entry: Option<Entry>,
        future: impl Future<Output = Result<Option<String>>> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        self.run_write(
            scope,
            label,
            detail,
            future,
            move |this, result, cx| {
                let result = match result {
                    Ok(notice) => {
                        if let Some(message) = &notice {
                            this.log(false, message.clone());
                        }
                        if let Some(entry) = removed_entry {
                            this.after_removed_entry(&entry, cx);
                        }
                        Ok(notice)
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(Some(message)) => this.show_notice(message, cx),
                    Ok(None) => {}
                    Err(error) => this.show_error(error, cx),
                }
            },
            cx,
        )
    }

    pub(super) fn confirm_batch_delete(
        &self,
        tree: bool,
        permanent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || window.has_active_dialog(cx) {
            return;
        }
        let entries = selected_roots(self.batch_entries(tree).into_iter());
        if entries.is_empty() || entries.iter().any(|entry| self.entry_write_locked(entry)) {
            return;
        }
        let count = entries.len();
        let permanent = std::rc::Rc::new(std::cell::Cell::new(permanent));
        let revision = self.bucket_revision;
        let owner = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let mode = permanent.get();
            let toggle = permanent.clone();
            let label = if mode {
                "永久删除"
            } else {
                "移入回收站"
            };
            let entries = entries.clone();
            let owner = owner.clone();
            dialog
                .confirm()
                .title(format!("{label}这 {count} 项？"))
                .description(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(if mode {
                            "选中的项目会被永久删除。若包含文件夹，其内容也会被删除。"
                        } else {
                            "选中的项目会移入回收站。若包含文件夹，其内容也会一并移入。"
                        })
                        .child(
                            Switch::new("batch-permanent-delete")
                                .label("永久删除")
                                .checked(mode)
                                .on_change(move |checked, window, _| {
                                    toggle.set(*checked);
                                    window.refresh();
                                }),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(label)
                        .cancel_text("取消")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, _, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if this.bucket_revision == revision {
                            this.delete_batch_entries(entries.clone(), mode, cx);
                        }
                    });
                    true
                })
        });
    }

    fn delete_batch_entries(
        &mut self,
        entries: Vec<Entry>,
        permanent: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.clone() else {
            return;
        };
        if entries.iter().any(|entry| self.entry_write_locked(entry)) {
            return self.show_write_locked(cx);
        }
        let scope = self.write_scope(&store, entries.iter().map(|entry| entry.key().to_owned()));
        self.run_write(
            scope,
            if permanent {
                "正在批量永久删除"
            } else {
                "正在批量移入回收站"
            },
            format!("{:?}，{} 项", store.bucket(), entries.len()),
            async move {
                let mut removed = Vec::new();
                let mut failures = Vec::new();
                for entry in entries {
                    let result = if permanent {
                        store.delete_permanently(entry.clone()).await
                    } else {
                        store.trash(entry.clone()).await
                    };
                    match result {
                        Ok(()) => removed.push(entry),
                        Err(error) => failures.push(format!("/{}：{error:#}", entry.key())),
                    }
                }
                Ok((removed, failures))
            },
            move |this, result, cx| match result {
                Ok((removed, failures)) => {
                    let count = removed.len();
                    for entry in &removed {
                        this.after_removed_entry(entry, cx);
                    }
                    let action = if permanent {
                        "永久删除"
                    } else {
                        "移入回收站"
                    };
                    if failures.is_empty() {
                        this.show_notice(format!("已{action} {count} 项"), cx);
                    } else {
                        this.show_error(
                            anyhow::anyhow!(
                                "已{action} {count} 项，失败 {} 项：\n{}",
                                failures.len(),
                                failures.join("\n"),
                            ),
                            cx,
                        );
                    }
                }
                Err(error) => {
                    this.show_error(error, cx);
                }
            },
            cx,
        );
    }

    pub(super) fn confirm_delete(
        &self,
        entry: Entry,
        permanent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() || entry.key().is_empty() || window.has_active_dialog(cx) {
            return;
        }
        if self.entry_write_locked(&entry) {
            return;
        }
        let permanent = std::rc::Rc::new(std::cell::Cell::new(permanent));
        let revision = self.bucket_revision;
        let owner = cx.weak_entity();
        let validation = cx.new(|_| EditMessage(None));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let mode = permanent.get();
            let toggle = permanent.clone();
            let entry = entry.clone();
            let owner = owner.clone();
            let validation = validation.clone();
            let label = if mode {
                "永久删除"
            } else {
                "移入回收站"
            };
            let description = if mode {
                "此操作会跳过应用回收站，无法从中还原。"
            } else {
                "可以在当前存储桶的回收站中还原此项。"
            };
            dialog
                .confirm()
                .title(format!("{label}「{}」？", entry.name()))
                .description(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(description)
                        .when(entry.directory(), |body| {
                            body.child("文件夹及其内容会一并处理。")
                        })
                        .child(
                            Switch::new("permanent-delete")
                                .label("永久删除")
                                .checked(mode)
                                .on_change(move |checked, window, _| {
                                    toggle.set(*checked);
                                    window.refresh();
                                }),
                        )
                        .child(validation.clone()),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(label)
                        .cancel_text("取消")
                        .ok_variant(ButtonVariant::Danger),
                )
                .on_ok(move |_, _, cx| {
                    let entry = entry.clone();
                    let result = owner
                        .update(cx, |this, cx| {
                            ensure!(
                                this.bucket_revision == revision,
                                "存储桶已切换，请重新打开操作。"
                            );
                            ensure!(this.busy.is_none(), "请等待当前操作完成。");
                            ensure!(
                                !this.entry_write_locked(&entry),
                                "该目录还有操作正在进行，请稍后重试"
                            );
                            let store = this.store.clone().context("存储桶已断开连接。")?;
                            let scope = this.write_scope(&store, [entry.key().to_owned()]);
                            let started = this.change(
                                scope,
                                if mode {
                                    "正在永久删除"
                                } else {
                                    "正在移入回收站"
                                },
                                format!("{:?} /{:?}", store.bucket(), entry.key()),
                                Some(entry.clone()),
                                async move {
                                    if mode {
                                        store.delete_permanently(entry).await?;
                                    } else {
                                        store.trash(entry).await?;
                                    }
                                    Ok(Some(
                                        if mode {
                                            "已永久删除"
                                        } else {
                                            "已移入回收站"
                                        }
                                        .into(),
                                    ))
                                },
                                cx,
                            );
                            ensure!(started, "该目录还有操作正在进行，请稍后重试");
                            Ok(())
                        })
                        .and_then(|result| result);
                    match result {
                        Ok(()) => true,
                        Err(error) => {
                            validation.update(cx, |message, cx| {
                                message.0 = Some(error.to_string().into());
                                cx.notify();
                            });
                            false
                        }
                    }
                })
        });
    }

    pub(super) fn edit(&mut self, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let entry = match &edit {
            Edit::Rename(entry, _)
            | Edit::Delete(entry)
            | Edit::Move(entry)
            | Edit::Copy(entry) => entry,
        };
        if self.entry_write_locked(entry) {
            return self.show_write_locked(cx);
        }
        match edit {
            Edit::Rename(entry, tree) => self.start_rename(entry, tree, window, cx),
            Edit::Delete(entry) => self.confirm_delete(entry, false, window, cx),
            Edit::Move(entry) => self.choose_transfer_folder(vec![entry], false, false, window, cx),
            Edit::Copy(entry) => self.choose_transfer_folder(vec![entry], true, false, window, cx),
        }
    }

    fn start_rename(
        &mut self,
        entry: Entry,
        tree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entry.key().is_empty() {
            return;
        }
        self.tree_create = None;
        let name = entry.name().to_owned();
        let caret = if entry.directory() {
            name.len()
        } else {
            name.rfind('.')
                .filter(|&dot| dot > 0 && dot + 1 < name.len())
                .unwrap_or(name.len())
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_rename(cx);
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.submit_rename(cx));
        if tree {
            if let Some(index) = self
                .tree
                .iter()
                .position(|(_, item)| item.key() == entry.key())
            {
                self.tree_scroll
                    .scroll_to_item(self.bucket_tree_offset() + index, ScrollStrategy::Nearest);
            }
        } else if let Some(index) = self
            .rows()
            .iter()
            .position(|item| item.key() == entry.key())
        {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        self.rename = Some(InlineRename {
            entry,
            tree,
            input: input.clone(),
            _subscription: subscription,
            _blur: blur,
        });
        window.defer(cx, move |window, cx| {
            focus.focus(window, cx);
            input.update(cx, |input, cx| input.set_selected_range(caret..caret, cx));
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let (Some(draft), Some(store)) = (&self.rename, self.store.clone()) else {
            return;
        };
        let entry = draft.entry.clone();
        let name = draft.input.read(cx).value();
        if name.trim().is_empty() || name.as_ref() == entry.name() {
            self.rename = None;
            cx.notify();
            return;
        }
        let target = match child_key(parent_prefix(entry.key()), &name, entry.directory()) {
            Ok(target) => target,
            Err(error) => return self.show_error(error, cx),
        };
        if self.entry_write_locked(&entry) || self.write_locked(&target, entry.directory()) {
            return self.show_write_locked(cx);
        }
        if self.queue_transfer(
            store.bucket().to_owned(),
            TransferOperation::Remote {
                target_bucket: store.bucket().to_owned(),
                targets: vec![(entry, target)],
                copying: false,
                rename: true,
            },
            cx,
        ) {
            self.rename = None;
        }
    }

    pub(super) fn rename_field(
        &self,
        entry: &Entry,
        tree: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let draft = self
            .rename
            .as_ref()
            .filter(|draft| draft.tree == tree && draft.entry.key() == entry.key())?;
        Some(
            div()
                .id("inline-rename-field")
                .relative()
                .w_full()
                .min_w_0()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.submit_rename(cx)))
                .on_click(|_, _, cx| cx.stop_propagation())
                .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.rename = None;
                        if tree {
                            &this.tree_focus
                        } else {
                            &this.list_focus
                        }
                        .focus(window, cx);
                        cx.notify();
                    }
                }))
                .child(Self::inline_name_input(&draft.input).aria_label("新名称"))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .rounded_sm()
                        .border_color(cx.theme().ring.opacity(0.65)),
                )
                .into_any_element(),
        )
    }

    pub(super) fn batch_write_locked(&self, tree: bool) -> bool {
        let selected = if tree {
            &self.tree_multi
        } else {
            &self.object_multi
        };
        selected
            .iter()
            .any(|key| self.write_locked(key, key.ends_with('/')))
    }

    pub(super) fn batch_entries(&self, tree: bool) -> Vec<&Entry> {
        if tree {
            self.tree
                .iter()
                .map(|(_, entry)| entry)
                .filter(|entry| self.tree_multi.contains(entry.key()))
                .collect()
        } else {
            self.rows()
                .iter()
                .filter(|entry| self.object_multi.contains(entry.key()))
                .collect()
        }
    }

    pub(super) fn choose_batch_transfer_folder(
        &mut self,
        tree: bool,
        copying: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = selected_roots(self.batch_entries(tree).into_iter());
        self.choose_transfer_folder(entries, copying, true, window, cx);
    }

    fn choose_transfer_folder(
        &mut self,
        entries: Vec<Entry>,
        copying: bool,
        batch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(store), Some(first)) = (self.store.clone(), entries.first()) else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        if entries.iter().any(|entry| self.entry_write_locked(entry)) {
            return self.show_write_locked(cx);
        }
        let mut names = HashSet::new();
        if entries
            .iter()
            .any(|entry| !names.insert(transfer_name(entry)))
        {
            self.show_error(
                anyhow::anyhow!("选中项中有同名对象，无法放入同一目标目录"),
                cx,
            );
            return;
        }
        let stores = self
            .server
            .as_ref()
            .map(|server| {
                self.buckets
                    .iter()
                    .map(|bucket| bucket.open(server))
                    .collect()
            })
            .unwrap_or_else(|| vec![store.clone()]);
        let source_store = store.clone();
        let source_bucket = store.bucket().to_owned();
        let prefix = parent_prefix(first.key()).to_owned();
        let label = match (copying, batch) {
            (true, true) => "批量复制",
            (false, true) => "批量移动",
            (true, false) => "复制",
            (false, false) => "移动",
        };
        let title = if batch {
            format!("{label} {} 项", entries.len())
        } else {
            format!("{label}「{}」", first.name())
        };
        let revision = self.bucket_revision;
        let sources = entries.clone();
        let connection = self.connection_revision;
        let source_name = source_bucket.clone();
        let scope_sources = entries.clone();
        let transfer_scope = move |target: &Store, prefix: &str| {
            let target_bucket = target.bucket().to_owned();
            let mut folders = vec![(target_bucket.clone(), prefix.to_owned())];
            let mut paths = Vec::new();
            for entry in &scope_sources {
                folders.push((source_bucket.clone(), parent_prefix(entry.key()).to_owned()));
                paths.push((source_bucket.clone(), entry.key().to_owned()));
                paths.push((
                    target_bucket.clone(),
                    format!("{prefix}{}", transfer_name(entry)),
                ));
            }
            TransferScopes {
                connection,
                folders,
                paths,
            }
        };
        let confirm_scope = transfer_scope.clone();
        self.open_remote_picker(
            stores,
            Some((store, prefix)),
            title,
            label,
            move |this, target, prefix| {
                this.write_scope_available(&transfer_scope(target, prefix))
                    && sources.iter().all(|source| {
                        (target.bucket() != source_name
                            || (prefix != parent_prefix(source.key())
                                && (!source.directory() || !prefix.starts_with(source.key()))))
                            && prefix.len() + transfer_name(source).len() <= 1024
                    })
            },
            move |this, store, prefix, cx| {
                if this.bucket_revision != revision || this.busy.is_some() {
                    return false;
                }
                let entries = entries.clone();
                let source_store = source_store.clone();
                let scope = confirm_scope(&store, &prefix);
                if !this.write_scope_available(&scope) {
                    this.show_write_locked(cx);
                    return false;
                }
                let targets = entries
                    .iter()
                    .cloned()
                    .map(|entry| {
                        let name = format!("{prefix}{}", transfer_name(&entry));
                        (entry, name)
                    })
                    .collect();
                this.queue_transfer(
                    source_store.bucket().to_owned(),
                    TransferOperation::Remote {
                        target_bucket: store.bucket().to_owned(),
                        targets,
                        copying,
                        rename: false,
                    },
                    cx,
                )
            },
            window,
            cx,
        );
    }

    pub(super) fn write_scope_available(&self, scope: &TransferScopes) -> bool {
        self.busy.is_none()
            && self.connection_revision == scope.connection
            && !self
                .write_scopes
                .values()
                .chain(self.upload_scopes.values())
                .any(|active| active.overlaps(scope))
            && !self.saving_in_scope(scope)
    }

    pub(super) fn show_metadata(
        &mut self,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let details = cx.new(|_| EditMessage(None));
        let view = details.clone();
        let key = entry.key().to_owned();
        let heading = entry.name().to_owned();
        let directory = entry.directory();
        let initial = if directory {
            "正在统计目录内容"
        } else {
            "正在读取对象元数据"
        };
        let path = format!("{}/{}", store.bucket(), entry.key());
        let time_zone = self.time_zone;
        let task = self.runtime.spawn(async move {
            let mut fields = Vec::new();
            if directory {
                let rows = store.list(&key, false).await?;
                let files = rows.iter().filter(|row| !row.key().ends_with('/'));
                let size = files
                    .clone()
                    .fold(0i64, |size, row| size.saturating_add(row.size()));
                fields.push(format!("文件数：{}", files.clone().count()));
                fields.push(format!("大小：{}", size_label(size)));
                if let Some(modified) = files.filter_map(Entry::modified).max() {
                    fields.push(format!("最近文件修改：{}", time_zone.format(modified)));
                }
            } else {
                fields.push(format!("大小：{}", size_label(entry.size())));
                if let Some(modified) = entry.modified() {
                    fields.push(format!("修改时间：{}", time_zone.format(modified)));
                }
                if let Some(class) = entry.storage_class() {
                    fields.push(format!("存储类型：{class}"));
                }
                if let Some(etag) = entry.etag() {
                    fields.push(format!("ETag：{etag}"));
                }
                match store.object_metadata(&key).await {
                    Ok(metadata) => fields.extend(metadata),
                    Err(_) => fields.push("无法读取更多对象元数据".to_owned()),
                }
            }
            Ok::<_, anyhow::Error>(fields.join("\n"))
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            let value = view.read(cx).0.clone();
            dialog.title(heading.clone()).child(
                div()
                    .id("object-details")
                    .max_h(rems(24.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_sm()
                    .child(path.clone())
                    .children(
                        value
                            .as_deref()
                            .unwrap_or(initial)
                            .lines()
                            .map(|line| div().child(line.to_owned())),
                    ),
            )
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = cx.update(|_, cx| {
                details.update(cx, |details, cx| {
                    details.0 = Some(result.unwrap_or_else(|error| format!("{error:#}")).into());
                    cx.notify();
                });
                let _ = view.update(cx, |_, cx| cx.notify());
            });
        })
        .detach();
    }
}

pub(super) struct EditMessage(pub(super) Option<SharedString>);

impl Render for EditMessage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .text_sm()
            .text_color(cx.theme().danger)
            .children(self.0.clone())
    }
}

fn transfer_name(entry: &Entry) -> &str {
    &entry.key()[parent_prefix(entry.key()).len()..]
}

fn selected_roots<'a>(entries: impl Iterator<Item = &'a Entry>) -> Vec<Entry> {
    let mut entries: Vec<_> = entries
        .filter(|entry| !entry.key().is_empty())
        .cloned()
        .collect();
    entries.sort_unstable_by(|a, b| a.key().cmp(b.key()));
    let mut roots: Vec<Entry> = Vec::new();
    for entry in entries {
        if !roots.last().is_some_and(|parent| {
            parent.key() == entry.key()
                || (parent.directory() && entry.key().starts_with(parent.key()))
        }) {
            roots.push(entry);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::{Entry, TransferScopes, selected_roots};

    #[test]
    fn root_transfer_preserves_writes_in_unrelated_subfolders() {
        let scope = TransferScopes {
            connection: 1,
            folders: vec![
                ("source".into(), "".into()),
                ("target".into(), "done/".into()),
            ],
            paths: vec![
                ("source".into(), "report.txt".into()),
                ("target".into(), "done/report.txt".into()),
            ],
        };
        assert!(scope.contains_entry("source", "draft.txt", false));
        assert!(scope.contains_folder("source", ""));
        assert!(!scope.contains_folder("source", "unrelated/"));
        assert!(!scope.contains_entry("source", "unrelated/draft.txt", false));
        assert!(scope.contains_entry("target", "done/other.txt", false));
        assert!(!scope.contains_entry("target", "done-next/other.txt", false));
        assert!(!scope.contains_entry("source", "done/report.txt", false));
        assert!(scope.affects_listing("target", ""));
        assert!(!scope.contains_folder("target", ""));
    }

    #[test]
    fn directory_transfers_conflict_with_ancestor_and_descendant_writes() {
        let scope = TransferScopes {
            connection: 1,
            folders: vec![("bucket".into(), "a/".into())],
            paths: vec![("bucket".into(), "a/b/".into())],
        };
        assert!(scope.contains_folder("bucket", "a/b/deep/"));
        assert!(scope.contains_entry("bucket", "a/", true));
        assert!(!scope.contains_entry("bucket", "a2/", true));
        let unrelated = TransferScopes {
            connection: 1,
            folders: vec![("bucket".into(), "a/b-next/".into())],
            paths: vec![("bucket".into(), "a/b-next/note.txt".into())],
        };
        assert!(!scope.overlaps(&unrelated));
        let mut nested = TransferScopes {
            connection: 1,
            folders: vec![("bucket".into(), "a/b/deep/".into())],
            paths: vec![],
        };
        assert!(scope.overlaps(&nested));
        assert!(nested.overlaps(&scope));
        nested.connection += 1;
        assert!(!scope.overlaps(&nested));
    }

    #[test]
    fn batch_delete_skips_descendants_but_preserves_other_branches() {
        let entries =
            ["a/b/", "ab/", "a/", "", "a/b/c/", "a-/"].map(|key| Entry::folder(key.to_owned()));
        let roots = selected_roots(entries.iter());
        assert_eq!(
            roots.iter().map(Entry::key).collect::<Vec<_>>(),
            ["a-/", "a/", "ab/"],
        );
    }
}
