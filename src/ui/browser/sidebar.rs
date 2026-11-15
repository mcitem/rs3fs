use super::*;
use gpui_kit::component::button::ButtonCustomVariant;

impl Browser {
    fn tree_disclosure(id: impl Into<ElementId>, expanded: bool, busy: bool, cx: &App) -> Button {
        let label = if expanded { "折叠" } else { "展开" };
        Button::new(id)
            .custom(ButtonCustomVariant::new(cx).foreground(cx.theme().secondary_foreground))
            .xsmall()
            .w_4()
            .flex_shrink_0()
            .icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .tooltip(label)
            .accessibility_label(label)
            .disabled(busy)
    }

    fn tree_indent(depth: usize, cx: &Context<Self>) -> impl IntoElement {
        let color = cx.theme().sidebar_foreground.opacity(0.2);
        div()
            .flex()
            .h_full()
            .flex_shrink_0()
            .children((0..depth.min(10)).map(move |_| {
                div()
                    .flex()
                    .justify_center()
                    .w_4()
                    .h_full()
                    .child(div().h_full().border_l_1().border_color(color))
            }))
    }

    pub(super) fn inline_name_input(input: &Entity<InputState>) -> Input {
        Input::new(input)
            .xsmall()
            .appearance(false)
            .p_0()
            .h(FILE_NAME_LINE_HEIGHT)
            .line_height(FILE_NAME_LINE_HEIGHT)
            .text_sm()
    }

    pub(super) fn tree_create_row(
        &self,
        draft: &TreeCreate,
        depth: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        div()
            .id("tree-create")
            .flex()
            .items_center()
            .h_6()
            .w_full()
            .px_2()
            .border_l_2()
            .border_color(cx.theme().sidebar)
            .gap_1()
            .on_click(|_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.tree_create = None;
                    this.error = None;
                    this.tree_focus.focus(window, cx);
                    cx.notify();
                }
            }))
            .child(Self::tree_indent(depth, cx))
            .child(div().w_4().flex_shrink_0())
            .child(
                Icon::new(if draft.directory {
                    IconName::Folder
                } else {
                    IconName::File
                })
                .size_4()
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.submit_tree_create(cx)))
                    .child(Self::inline_name_input(&draft.input)),
            )
            .into_any_element()
    }

    pub(super) fn parked_tree_row(
        &self,
        bucket: &Bucket,
        depth: usize,
        entry: &Entry,
        expanded: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let root = entry.key().is_empty();
        let directory = root || entry.directory();
        let label = if root { bucket.name() } else { entry.name() };
        let icon = if root {
            IconName::Database
        } else if !directory {
            file_icon(entry.name())
        } else if expanded {
            IconName::FolderOpen
        } else {
            IconName::Folder
        };
        let theme = cx.theme();
        let object = entry.clone();
        let target = bucket.clone();
        let branch = entry.clone();
        let branch_bucket = bucket.clone();
        let bucket_name = bucket.name().to_owned();
        let owner = cx.weak_entity();
        let revision = self.connection_revision;
        let busy = self.busy.is_some();
        let row = div()
            .id((
                ElementId::from(bucket_name.clone()),
                SharedString::from(entry.key().to_owned()),
            ))
            .flex()
            .items_center()
            .h_6()
            .w_full()
            .text_sm()
            .px_2()
            .gap_1()
            .border_l_2()
            .border_color(theme.sidebar)
            .hover(|row| row.bg(theme.accent))
            .when(depth > 0, |row| row.child(Self::tree_indent(depth, cx)))
            .when(!directory, |row| row.child(div().w_4().flex_shrink_0()))
            .when(directory, |row| {
                row.child(
                    Self::tree_disclosure("disclose-parked", expanded, busy, cx).on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if root {
                                if let Some(tree) =
                                    this.parked_buckets.get_mut(branch_bucket.name())
                                {
                                    if !tree.expanded.remove("") {
                                        tree.expanded.insert(String::new());
                                    }
                                    cx.notify();
                                } else {
                                    this.open_bucket(branch_bucket.clone(), cx);
                                }
                            } else {
                                this.activate_bucket(
                                    branch_bucket.clone(),
                                    Some(branch.clone()),
                                    true,
                                    cx,
                                );
                            }
                        }),
                    ),
                )
            })
            .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .line_height(FILE_NAME_LINE_HEIGHT)
                    .truncate()
                    .child(label.to_owned()),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if root {
                    this.open_bucket(target.clone(), cx);
                } else {
                    this.activate_bucket(target.clone(), Some(object.clone()), false, cx);
                    if !directory
                        && this
                            .store
                            .as_ref()
                            .is_some_and(|store| store.bucket() == target.name())
                    {
                        this.open_entry(object.clone(), window, cx);
                    }
                }
            }));
        if root {
            row.context_menu(move |menu, _, _| {
                Self::copy_path_menu(menu, &bucket_name, "")
                    .separator()
                    .item(Self::hide_bucket_item(&bucket_name, &owner, revision, busy))
            })
            .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    pub(super) fn tree_row(
        &self,
        index: usize,
        tree_focused: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (depth, entry) = &self.tree[index];
        let key = entry.key().to_owned();
        let directory = entry.directory();
        let opened = directory && self.expanded.contains(&key);
        let current = key == self.prefix && !self.show_preview;
        let selected = if self.tree_multi.is_empty() {
            self.tree_selected.as_ref() == Some(&key)
        } else {
            self.tree_multi.contains(&key)
        };
        let label = if key.is_empty() {
            self.store.as_ref().map_or("", Store::bucket)
        } else {
            entry.name()
        };
        let icon = if !directory {
            file_icon(entry.name())
        } else if key.is_empty() {
            IconName::Database
        } else if opened {
            IconName::FolderOpen
        } else {
            IconName::Folder
        };
        let theme = cx.theme();
        let menu_entry = entry.clone();
        let owner = cx.weak_entity();
        let bucket = self
            .store
            .as_ref()
            .map(Store::bucket)
            .unwrap_or_default()
            .to_owned();
        let busy = self.busy.is_some();
        let connection_revision = self.connection_revision;
        #[cfg(windows)]
        let bucket_revision = self.bucket_revision;
        let row = div()
            .flex()
            .items_center()
            .h_6()
            .w_full()
            .text_sm()
            .px_2()
            .gap_1()
            .border_l_2()
            .border_color(if selected {
                theme.sidebar_primary.opacity(0.65)
            } else {
                theme.sidebar
            })
            .when(selected, |row| row.bg(theme.sidebar_primary.opacity(0.11)))
            .when(!selected, |row| row.hover(|row| row.bg(theme.accent)))
            .when(*depth > 0, |row| row.child(Self::tree_indent(*depth, cx)))
            .when(!directory, |row| row.child(div().w_4().flex_shrink_0()))
            .when(directory, |row| {
                row.child(
                    Self::tree_disclosure(
                        (ElementId::from("expand"), SharedString::from(key.clone())),
                        opened,
                        busy,
                        cx,
                    )
                    .on_click(cx.listener({
                        let key = key.clone();
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.tree_focus.focus(window, cx);
                            this.disclose(key.clone(), cx);
                        }
                    })),
                )
            })
            .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .line_height(FILE_NAME_LINE_HEIGHT)
                    .when(current && tree_focused, |row| {
                        row.font_weight(FontWeight::SEMIBOLD)
                    })
                    .child(self.rename_field(entry, true, cx).unwrap_or_else(|| {
                        div().truncate().child(label.to_owned()).into_any_element()
                    })),
            );
        let row = if directory {
            self.drop_surface(row, key.clone(), cx)
        } else {
            row
        };
        let click_entry = entry.clone();
        row.id((ElementId::from("tree"), SharedString::from(key.clone())))
            .on_click(cx.listener(move |this, click: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.submit_tree_create(cx);
                this.tree_focus.focus(window, cx);
                if this.busy.is_some() || this.tree_create.is_some() {
                    return;
                }
                if click.modifiers().shift {
                    if let Some(index) = this.tree.iter().position(|(_, entry)| entry.key() == key)
                    {
                        this.select_tree_range(index, cx);
                    }
                    return;
                }
                if click.modifiers().control && !key.is_empty() {
                    this.toggle_tree_selection(key.clone(), cx);
                    return;
                }
                if !directory {
                    this.open_entry(click_entry.clone(), window, cx);
                } else if this.trash.is_none()
                    && !this.show_preview
                    && this.prefix == key
                    && this.expanded.contains(&key)
                {
                    this.disclose(key.clone(), cx);
                } else {
                    this.navigate(key.clone(), cx);
                }
            }))
            .context_menu(move |mut menu, _window, _cx| {
                let batch = owner
                    .update(_cx, |this, cx| {
                        this.select_context_entry(&menu_entry, true, cx)
                    })
                    .unwrap_or(false);
                if batch {
                    return Self::batch_entry_menu(menu, &owner, true, _window, _cx);
                }
                let (write_locked, create_locked) =
                    owner.upgrade().map_or((false, false), |owner| {
                        let view = owner.read(_cx);
                        (
                            view.entry_write_locked(&menu_entry),
                            view.folder_write_locked(menu_entry.key()),
                        )
                    });
                #[cfg(windows)]
                if !directory {
                    menu = Self::open_with_submenu(
                        menu,
                        &menu_entry,
                        &owner,
                        bucket_revision,
                        _window,
                        _cx,
                    );
                }
                if !directory {
                    let download_owner = owner.clone();
                    let entry = menu_entry.clone();
                    let menu = menu.item(PopupMenuItem::new("下载").disabled(busy).on_click(
                        move |_, window, cx| {
                            let _ = download_owner.update(cx, |this, cx| {
                                this.download(entry.clone(), window, cx);
                            });
                        },
                    ));
                    return Self::entry_edit_menu(
                        menu,
                        &menu_entry,
                        &bucket,
                        &owner,
                        true,
                        busy || write_locked,
                    );
                }
                menu = Self::folder_search_menu(menu, menu_entry.key(), &owner, busy);
                menu =
                    Self::tree_create_menu(menu, menu_entry.key(), &owner, busy || create_locked);
                if menu_entry.key().is_empty() {
                    Self::copy_path_menu(menu, &bucket, menu_entry.key())
                        .separator()
                        .item(Self::hide_bucket_item(
                            &bucket,
                            &owner,
                            connection_revision,
                            busy,
                        ))
                } else {
                    Self::entry_edit_menu(
                        menu,
                        &menu_entry,
                        &bucket,
                        &owner,
                        true,
                        busy || write_locked,
                    )
                }
            })
    }

    #[inline(never)]
    pub(super) fn bucket_sidebar(&self, tree: AnyElement, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let busy = self.busy.is_some();
        let has_bucket = self.store.is_some();
        let owner = cx.weak_entity();
        let prefix = self.prefix.clone();
        let create_disabled = busy || self.trash.is_some() || self.tree_multi.len() > 1;
        let refresh_disabled = busy
            || if self.trash.is_some() {
                self.trash_busy()
            } else if has_bucket {
                self.refreshing_listings
            } else {
                self.refreshing_buckets
            };
        let upload_locked = self.folder_write_locked(&prefix);
        let create_locked = self.folder_write_locked(&self.tree_create_prefix());
        div()
            .flex()
            .flex_col()
            .w(rems(17.))
            .min_h_0()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .h_10()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .children(
                                [
                                    ("upload", "上传", IconName::Upload, false),
                                    ("upload-folder", "上传文件夹", IconName::FolderUp, true),
                                ]
                                .map(
                                    |(id, label, icon, directories)| {
                                        Button::new(id)
                                            .ghost()
                                            .xsmall()
                                            .icon(icon)
                                            .label(label)
                                            .tooltip(if directories {
                                                "上传文件夹"
                                            } else {
                                                "上传文件"
                                            })
                                            .disabled(
                                                busy || upload_locked
                                                    || !has_bucket
                                                    || self.show_preview
                                                    || self.trash.is_some(),
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.pick_upload(directories, window, cx);
                                            }))
                                    },
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .children(
                                [
                                    (
                                        "sidebar-new-file",
                                        "新建文件…",
                                        gpui_kit::assets::IconName::FilePlus,
                                        false,
                                    ),
                                    (
                                        "sidebar-new-folder",
                                        "新建文件夹…",
                                        gpui_kit::assets::IconName::FolderPlus,
                                        true,
                                    ),
                                ]
                                .map(
                                    |(id, label, icon, directory)| {
                                        Button::new(id)
                                            .ghost()
                                            .xsmall()
                                            .icon(icon)
                                            .tooltip(label)
                                            .accessibility_label(label)
                                            .disabled(
                                                create_disabled || create_locked || !has_bucket,
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                let prefix = this.tree_create_prefix();
                                                this.start_tree_create(
                                                    prefix, directory, window, cx,
                                                );
                                            }))
                                    },
                                ),
                            )
                            .child(
                                Button::new("sidebar-refresh")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::RefreshCw)
                                    .accessibility_label("刷新")
                                    .tooltip(if has_bucket {
                                        "刷新（F5）"
                                    } else {
                                        "刷新存储桶"
                                    })
                                    .disabled(refresh_disabled)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.store.is_some() {
                                            if this.trash.is_some() {
                                                this.open_trash(cx);
                                            } else {
                                                this.refresh(cx);
                                            }
                                        } else {
                                            this.refresh_buckets(cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new("sidebar-collapse")
                                    .ghost()
                                    .xsmall()
                                    .icon(gpui_kit::assets::IconName::SquareMinus)
                                    .tooltip("折叠所有文件夹")
                                    .accessibility_label("折叠所有文件夹")
                                    .disabled(busy || !has_bucket)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.collapse_folders(cx)),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("sidebar-tree-surface")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.sidebar)
                    .focus_visible(|style| style.border_color(theme.ring))
                    .track_focus(&self.tree_focus)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_tree_create(cx);
                        this.tree_selected = None;
                        this.tree_multi.clear();
                        this.tree_range_anchor = None;
                        window.blur(cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if this.tree_focus.is_focused(window) {
                            this.keyboard(event, true, window, cx);
                        }
                    }))
                    .child(tree)
                    .map(|surface| {
                        if has_bucket {
                            surface
                                .context_menu(move |menu, _, _| {
                                    Self::tree_create_menu(
                                        menu,
                                        &prefix,
                                        &owner,
                                        create_disabled || upload_locked,
                                    )
                                })
                                .into_any_element()
                        } else {
                            surface.into_any_element()
                        }
                    }),
            )
            .when(self.tree_multi.len() > 1, |sidebar| {
                sidebar.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .border_t_1()
                        .border_color(theme.border)
                        .px_3()
                        .h_10()
                        .flex_shrink_0()
                        .text_sm()
                        .child(
                            Button::new("trash-tree-selection")
                                .small()
                                .ghost()
                                .label("移入回收站")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_batch_delete(true, false, window, cx);
                                })),
                        ),
                )
            })
            .when(self.show_tasks && !self.tasks_maximized, |sidebar| {
                sidebar.child(self.task_panel(false, cx))
            })
            .into_any_element()
    }
}
