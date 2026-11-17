use super::*;
use gpui_kit::component::{button::ButtonCustomVariant, scroll::ScrollableElement};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FileColumn {
    #[default]
    Name,
    Modified,
    Size,
}

impl FileColumn {
    const ALL: [Self; 3] = [Self::Name, Self::Modified, Self::Size];

    fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Modified => "修改时间",
            Self::Size => "大小",
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Self::Name => 6.,
            Self::Modified => 7.,
            Self::Size => 4.,
        }
    }
}

#[derive(Default)]
pub(super) struct FileSort {
    column: FileColumn,
    descending: bool,
}

impl FileSort {
    fn toggle(&mut self, column: FileColumn) {
        self.descending = if self.column == column {
            !self.descending
        } else {
            column == FileColumn::Modified
        };
        self.column = column;
    }

    pub(super) fn apply(&self, rows: &mut [Entry]) {
        rows.sort_unstable_by(|a, b| {
            b.directory().cmp(&a.directory()).then_with(|| {
                let order = match self.column {
                    FileColumn::Name => a.name().cmp(b.name()),
                    FileColumn::Modified => a.modified().cmp(&b.modified()),
                    FileColumn::Size => a.size().cmp(&b.size()),
                };
                let order = if self.descending {
                    order.reverse()
                } else {
                    order
                };
                order.then_with(|| a.key().cmp(b.key()))
            })
        });
    }
}

pub(super) struct ColumnResize {
    column: FileColumn,
    start_x: Pixels,
    width: Rems,
    touch: bool,
}

impl Browser {
    pub(super) fn file_column(&self, column: FileColumn) -> Div {
        div()
            .w(self.file_column_widths[column as usize])
            .flex_shrink_0()
            .min_w_0()
    }

    fn resize_file_column(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let Some(resize) = &self.file_column_resize else {
            return;
        };
        let delta = f32::from(x - resize.start_x) / f32::from(window.rem_size());
        let width = &mut self.file_column_widths[resize.column as usize];
        let next = (resize.width.0 + delta).clamp(resize.column.min_width(), 100.);
        if width.0 != next {
            *width = rems(next);
            cx.notify();
        }
    }

    fn file_column_handle(&self, column: FileColumn, cx: &Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let width = self.file_column_widths[column as usize];
        div()
            .absolute()
            .right_0()
            .top_0()
            .bottom_0()
            .w_3()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::BlockMouse),
                    move |_, hitbox, window, _| {
                        window.set_cursor_style(CursorStyle::ResizeLeftRight, &hitbox);
                        let mouse_owner = owner.clone();
                        let mouse_hitbox = hitbox.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                            if phase.bubble()
                                && event.button == MouseButton::Left
                                && mouse_hitbox.is_hovered(window)
                            {
                                let _ = mouse_owner.update(cx, |this, _| {
                                    this.file_column_resize = Some(ColumnResize {
                                        column,
                                        start_x: event.position.x,
                                        width,
                                        touch: false,
                                    });
                                });
                                cx.stop_propagation();
                            }
                        });
                        let mouse_owner = owner.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase == DispatchPhase::Capture
                                && event.pressed_button == Some(MouseButton::Left)
                            {
                                let _ = mouse_owner.update(cx, |this, cx| {
                                    if this.file_column_resize.as_ref().is_some_and(|resize| {
                                        resize.column == column && !resize.touch
                                    }) {
                                        this.resize_file_column(event.position.x, window, cx);
                                    }
                                });
                            }
                        });
                        let mouse_owner = owner.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture && event.button == MouseButton::Left
                            {
                                let _ = mouse_owner.update(cx, |this, _| {
                                    if this.file_column_resize.as_ref().is_some_and(|resize| {
                                        resize.column == column && !resize.touch
                                    }) {
                                        this.file_column_resize = None;
                                    }
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            if event.phase == TouchPhase::Started {
                                if !hitbox.is_hovered(window) || window.default_prevented() {
                                    return;
                                }
                                window.prevent_default();
                                let _ = owner.update(cx, |this, _| {
                                    this.file_column_resize = Some(ColumnResize {
                                        column,
                                        start_x: event.start_position.x,
                                        width,
                                        touch: true,
                                    });
                                });
                                return;
                            }
                            let _ =
                                owner.update(cx, |this, cx| {
                                    if this.file_column_resize.as_ref().is_some_and(|resize| {
                                        resize.column == column && resize.touch
                                    }) {
                                        if event.phase != TouchPhase::Cancelled {
                                            this.resize_file_column(event.position.x, window, cx);
                                        }
                                        if event.phase != TouchPhase::Moved {
                                            this.file_column_resize = None;
                                        }
                                    }
                                });
                        });
                    },
                )
                .size_full(),
            )
    }

    fn file_headers(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .pr_4()
            .h_9()
            .border_x_1()
            .border_b_1()
            .border_color(theme.border)
            .children(FileColumn::ALL.map(|column| {
                let sorted = self.file_sort.column == column;
                let label = column.label();
                // The name header also covers the leading icon column.
                // Absorb the body gutters here so the hover reaches both dividers.
                let gutter = if column == FileColumn::Name { 2.5 } else { 0.5 };
                self.file_column(column)
                    .w(rems(self.file_column_widths[column as usize].0 + gutter))
                    .relative()
                    .h_full()
                    .hover(|cell| cell.bg(theme.accent))
                    .child(
                        Button::new(("file-sort", column as usize))
                            .custom(ButtonCustomVariant::new(cx))
                            .rounded(px(0.))
                            .small()
                            .w_full()
                            .h_full()
                            .justify_start()
                            .label(label)
                            .when(sorted, |button| {
                                button.icon(if self.file_sort.descending {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronUp
                                })
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.file_sort.toggle(column);
                                this.apply_file_sort(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(self.file_column_handle(column, cx))
            }))
    }

    fn apply_file_sort(&mut self, cx: &mut Context<Self>) {
        self.sort_visible_rows();
        let index = self
            .selected
            .as_deref()
            .and_then(|key| self.visible_row_indices.get(key).copied())
            .unwrap_or(0);
        if self.grid_layout {
            self.grid_scroll.scroll_to_item(
                index / self.grid_columns.get().max(1),
                ScrollStrategy::Nearest,
            );
        } else {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn file_sort_menu(&self, mut menu: PopupMenu, owner: &WeakEntity<Self>) -> PopupMenu {
        for column in FileColumn::ALL {
            let owner = owner.clone();
            menu = menu.item(
                PopupMenuItem::new(column.label())
                    .checked(self.file_sort.column == column)
                    .on_click(move |_, _, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if this.file_sort.column != column {
                                this.file_sort.toggle(column);
                                this.apply_file_sort(cx);
                            }
                        });
                    }),
            );
        }
        menu = menu.separator();
        for (label, descending) in [("升序", false), ("降序", true)] {
            let owner = owner.clone();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .checked(self.file_sort.descending == descending)
                    .on_click(move |_, _, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            this.file_sort.descending = descending;
                            this.apply_file_sort(cx);
                        });
                    }),
            );
        }
        menu
    }

    fn file_row(&self, index: usize, cx: &Context<Self>) -> impl IntoElement {
        let entry = self.rows()[index].clone();
        let key = entry.key().to_owned();
        let directory = entry.directory();
        let theme = cx.theme();
        let busy = self.busy.is_some();
        let write_locked = self.entry_write_locked(&entry);
        let actions = [
            ("下载", IconName::Download, None),
            (
                "重命名",
                IconName::Pencil,
                Some((|entry| Edit::Rename(entry, false)) as fn(Entry) -> Edit),
            ),
            ("移动", IconName::FolderInput, Some(Edit::Move)),
            ("复制", IconName::Copy, Some(Edit::Copy)),
        ];
        let selected = self.object_selected(&key);
        let row = div()
            .flex()
            .items_center()
            .h_7()
            .w_full()
            .px_4()
            .gap_2()
            .text_sm()
            .when(selected, |row| row.bg(theme.primary.opacity(0.16)))
            .when(!selected, |row| row.hover(|row| row.bg(theme.accent)))
            .child(
                div().w_4().flex_shrink_0().child(
                    Icon::new(if directory {
                        IconName::Folder
                    } else {
                        file_icon(entry.name())
                    })
                    .size_4()
                    .text_color(theme.muted_foreground),
                ),
            )
            .child(
                self.file_column(FileColumn::Name)
                    .line_height(FILE_NAME_LINE_HEIGHT)
                    .child(self.rename_field(&entry, false, cx).unwrap_or_else(|| {
                        div()
                            .truncate()
                            .child(entry.name().to_owned())
                            .into_any_element()
                    })),
            )
            .child(
                self.file_column(FileColumn::Modified)
                    .truncate()
                    .text_color(theme.muted_foreground)
                    .child(
                        entry
                            .modified()
                            .map_or_else(|| "—".to_owned(), |time| self.time_zone.format(time)),
                    ),
            )
            .child(
                self.file_column(FileColumn::Size)
                    .text_right()
                    .text_color(theme.muted_foreground)
                    .child(if directory {
                        "—".to_owned()
                    } else {
                        size_label(entry.size())
                    }),
            )
            .child(
                div()
                    .ml_auto()
                    .w(rems(7.))
                    .flex_shrink_0()
                    .flex()
                    .justify_start()
                    .invisible()
                    .group_hover("object-row", |style| style.visible())
                    .children(
                        actions
                            .into_iter()
                            .filter(|(_, _, edit)| !directory || edit.is_some())
                            .map(|(label, icon, edit)| {
                                let entry = entry.clone();
                                Button::new(label)
                                    .ghost()
                                    .xsmall()
                                    .icon(icon)
                                    .tooltip(label)
                                    .accessibility_label(label)
                                    .disabled(busy || (edit.is_some() && write_locked))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        if let Some(edit) = edit {
                                            this.edit(edit(entry.clone()), window, cx);
                                        } else {
                                            this.download(entry.clone(), window, cx);
                                        }
                                    }))
                            }),
                    ),
            );
        let row = if directory {
            self.drop_surface(row, key.clone(), cx)
        } else {
            row
        };
        self.file_entry_surface(row, entry, cx)
    }

    pub(super) fn object_create_draft(&self) -> Option<&CreateDraft> {
        self.create_draft
            .as_ref()
            .filter(|draft| !draft.tree && draft.prefix == self.prefix)
    }

    fn file_create_row(&self, draft: &CreateDraft, cx: &Context<Self>) -> AnyElement {
        self.create_editor(
            div()
                .id("file-create")
                .flex()
                .items_center()
                .h_7()
                .w_full()
                .px_4()
                .gap_2()
                .child(self.create_icon(draft, cx).size_4())
                .child(
                    self.file_column(FileColumn::Name)
                        .child(Self::inline_name_input(&draft.input)),
                ),
            draft,
            cx,
        )
    }

    pub(super) fn file_entry_surface(
        &self,
        row: Div,
        entry: Entry,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = entry.key().to_owned();
        let menu_entry = entry.clone();
        let owner = cx.weak_entity();
        row.id((ElementId::from("object"), SharedString::from(key)))
            .group("object-row")
            .on_click(cx.listener(move |this, click: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.submit_create(cx);
                this.list_focus.focus(window, cx);
                if this.busy.is_some() || this.create_draft.is_some() {
                    return;
                }
                this.tree_multi.clear();
                this.tree_range_anchor = None;
                let modifiers = click.modifiers();
                this.select_object(entry.key(), modifiers.control, modifiers.shift);
                if !modifiers.control
                    && !modifiers.shift
                    && (window.last_input_was_touch() || click.click_count() >= 2)
                {
                    this.open_entry(entry.clone(), window, cx);
                }
                cx.notify();
            }))
            .context_menu(move |menu, _window, _cx| {
                let batch = owner
                    .update(_cx, |this, cx| {
                        this.select_context_entry(&menu_entry, false, cx)
                    })
                    .unwrap_or(false);
                Self::file_entry_menu(menu, &owner, &menu_entry, batch, _window, _cx)
            })
    }

    fn file_entry_menu(
        mut menu: PopupMenu,
        owner: &WeakEntity<Self>,
        menu_entry: &Entry,
        batch: bool,
        _window: &mut Window,
        _cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        if batch {
            return Self::batch_entry_menu(menu, owner, false, _window, _cx);
        }
        let Some(view) = owner.upgrade() else {
            return menu;
        };
        let view = view.read(_cx);
        let directory = menu_entry.directory();
        let busy = view.busy.is_some();
        let locked = view.entry_write_locked(menu_entry);
        let revision = view.bucket_revision;
        let bucket = view
            .store
            .as_ref()
            .map(Store::bucket)
            .unwrap_or_default()
            .to_owned();
        #[cfg(windows)]
        if !directory {
            menu = Self::open_with_submenu(menu, menu_entry, owner, revision, _window, _cx);
        }
        let show_details = {
            let owner = owner.clone();
            let entry = menu_entry.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |this, cx| {
                    if this.bucket_revision == revision {
                        this.show_metadata(entry.clone(), window, cx);
                    }
                });
            }
        };
        if directory {
            menu = menu.item(
                PopupMenuItem::new("详细信息")
                    .disabled(busy)
                    .on_click(show_details),
            );
            menu = Self::folder_search_menu(menu, menu_entry.key(), owner, busy);
        } else {
            let details_owner = owner.clone();
            let details_entry = menu_entry.clone();
            let details = PopupMenu::build(_window, _cx, |mut submenu, _, _| {
                submenu = submenu.item(PopupMenuItem::new("查看详细信息").on_click(show_details));
                for (label, compare) in [("查看 ETag", false), ("与本地文件比对 ETag", true)]
                {
                    let owner = details_owner.clone();
                    let entry = details_entry.clone();
                    submenu =
                        submenu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            let _ = owner.update(cx, |this, cx| {
                                if this.bucket_revision == revision {
                                    this.inspect_etag(entry.clone(), compare, window, cx);
                                }
                            });
                        }));
                }
                submenu
            });
            menu = menu.item(PopupMenuItem::submenu("详细信息", details).disabled(busy));

            menu = Self::file_download_menu(
                menu,
                owner,
                Arc::from([menu_entry.clone()]),
                false,
                _window,
                _cx,
            );
        }
        Self::entry_edit_menu(menu, menu_entry, &bucket, owner, false, busy || locked)
    }

    fn object_actions(&self, file: Option<&Entry>, cx: &Context<Self>) -> impl IntoElement {
        enum Action {
            Rename,
            Move,
            Copy,
            Trash,
        }

        let batch = if file.is_some() {
            None
        } else if self.tree_multi.len() > 1 {
            Some(true)
        } else if self.object_multi.len() > 1 {
            Some(false)
        } else {
            None
        };
        let single_disabled = self.busy.is_some() || self.tree_multi.len() > 1 || batch.is_some();
        let batch_disabled = batch.is_some_and(|tree| self.batch_write_locked(tree));
        let revision = self.bucket_revision;
        let selected = file.cloned().or_else(|| self.selection());
        let download = selected
            .as_ref()
            .filter(|entry| !entry.directory())
            .cloned();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .child(
                Button::new("download")
                    .small()
                    .ghost()
                    .accessibility_label("下载")
                    .child(div().flex_shrink_0().child("下载"))
                    .disabled(single_disabled || download.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(entry) = download.clone() {
                            this.download(entry, window, cx);
                        }
                    })),
            )
            .children(
                [
                    ("rename", "重命名", Action::Rename),
                    ("move", "移动", Action::Move),
                    ("copy", "复制", Action::Copy),
                    ("delete", "移入回收站", Action::Trash),
                ]
                .map(|(id, label, action)| {
                    let selection = selected.clone();
                    let disabled = if batch.is_some() && !matches!(action, Action::Rename) {
                        self.busy.is_some() || batch_disabled
                    } else {
                        single_disabled
                            || selection
                                .as_ref()
                                .is_none_or(|entry| self.entry_write_locked(entry))
                    };
                    Button::new(id)
                        .small()
                        .ghost()
                        .accessibility_label(label)
                        .child(div().flex_shrink_0().child(label))
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.bucket_revision != revision {
                                return;
                            }
                            if let Some(tree) = batch {
                                match action {
                                    Action::Move | Action::Copy => this
                                        .choose_batch_transfer_folder(
                                            tree,
                                            matches!(action, Action::Copy),
                                            window,
                                            cx,
                                        ),
                                    Action::Trash => {
                                        this.confirm_batch_delete(tree, false, window, cx);
                                    }
                                    Action::Rename => {}
                                }
                            } else if let Some(entry) = selection.clone() {
                                let edit = match action {
                                    Action::Rename => Edit::Rename(entry, false),
                                    Action::Move => Edit::Move(entry),
                                    Action::Copy => Edit::Copy(entry),
                                    Action::Trash => Edit::Delete(entry),
                                };
                                this.edit(edit, window, cx);
                            }
                        }))
                }),
            )
    }

    pub(super) fn file_workspace(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let tree_view = cx.weak_entity();
        let list_view = cx.weak_entity();
        let count = self.rows().len();
        let uploads = self
            .transfers
            .iter()
            .filter(|task| {
                task.running
                    && task.direction == TransferDirection::Upload
                    && task.connection == self.connection_revision
                    && self
                        .store
                        .as_ref()
                        .is_some_and(|store| store.bucket() == task.bucket)
            })
            .flat_map(|task| {
                task.items.iter().filter_map(move |item| {
                    if item.directory
                        || parent_prefix(&item.progress.key) != self.prefix
                        || (self.hide_dot_entries && is_dot_path(&item.progress.key))
                    {
                        return None;
                    }
                    let state = item.state.lock().unwrap();
                    state.active().then(|| {
                        (
                            (task.id, item.id),
                            item.progress.clone(),
                            matches!(*state, TransferState::Queued),
                        )
                    })
                })
            });
        let mut existing = HashMap::new();
        let mut pending = Vec::new();
        for upload in uploads {
            if let Some(&index) = self.visible_row_indices.get(upload.1.key.as_str()) {
                existing.insert(index, upload);
            } else {
                pending.push(upload);
            }
        }
        let creating = self.object_create_draft().is_some();
        let visible_count = count + pending.len() + usize::from(creating);
        let split = self.bucket_tree_offset();
        let draft_row = self
            .tree_create_row_index()
            .map(|(index, depth)| (split + index, depth));
        let active_bucket = self.store.as_ref().map(Store::bucket);
        let mut bucket_ranges = Vec::with_capacity(self.buckets.len());
        let mut row_count = 0;
        for (bucket_index, bucket) in self.buckets.iter().enumerate() {
            let active = Some(bucket.name()) == active_bucket;
            let count = if active {
                self.tree.len()
            } else {
                self.parked_buckets
                    .get(bucket.name())
                    .filter(|tree| tree.expanded.contains(""))
                    .map_or(1, |tree| tree.rows.len().max(1))
            };
            let end = row_count + count;
            bucket_ranges.push((bucket_index, row_count, end, active));
            row_count = end;
        }
        let row_count = row_count + usize::from(draft_row.is_some());
        let tree = uniform_list("tree-rows", row_count, move |range, _, cx| {
            tree_view
                .update(cx, |this, cx| {
                    range
                        .map(|mut index| {
                            if let Some((draft_index, depth)) = draft_row {
                                if index == draft_index
                                    && let Some(draft) = &this.create_draft
                                {
                                    return this.tree_create_row(draft, depth, cx);
                                }
                                index -= usize::from(index > draft_index);
                            }
                            let span =
                                bucket_ranges.partition_point(|&(_, _, end, _)| end <= index);
                            let (bucket_index, start, _, active) = bucket_ranges[span];
                            let row = index - start;
                            if active {
                                return this.tree_row(row, cx).into_any_element();
                            }
                            let bucket = &this.buckets[bucket_index];
                            let parked = this.parked_buckets.get(bucket.name());
                            if row == 0 {
                                let root = Entry::folder(String::new());
                                let expanded =
                                    parked.is_some_and(|tree| tree.expanded.contains(""));
                                return this.parked_tree_row(bucket, 0, &root, expanded, cx);
                            }
                            let tree = parked.expect("expanded parked bucket");
                            let (depth, entry) = &tree.rows[row];
                            this.parked_tree_row(
                                bucket,
                                *depth,
                                entry,
                                tree.expanded.contains(entry.key()),
                                cx,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .track_scroll(&self.tree_scroll)
        .size_full();
        let contents = if self.grid_layout && visible_count > 0 {
            self.file_grid(count, existing, pending, creating, cx)
        } else if visible_count > 0 {
            uniform_list("file-rows", visible_count, move |range, _, cx| {
                list_view
                    .update(cx, |this, cx| {
                        range
                            .map(|index| {
                                if creating && index == 0 {
                                    return this.object_create_draft().map_or_else(
                                        || div().h_7().into_any_element(),
                                        |draft| this.file_create_row(draft, cx),
                                    );
                                }
                                let index = index - usize::from(creating);
                                if let Some((id, file, queued)) =
                                    existing.get(&index).or_else(|| {
                                        index
                                            .checked_sub(count)
                                            .and_then(|index| pending.get(index))
                                    })
                                {
                                    this.upload_row(*id, file, *queued, cx).into_any_element()
                                } else {
                                    this.file_row(index, cx).into_any_element()
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .track_scroll(&self.list_scroll)
            .size_full()
            .into_any_element()
        } else {
            let loading = self.loading_folders.contains(&self.prefix);
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .when(loading, |body| body.child(Spinner::new().small()))
                .when(!loading, |body| {
                    body.child(Icon::new(IconName::FolderOpen).size_6())
                })
                .child(if loading {
                    "正在读取目录"
                } else if !self.listings.contains_key(&self.prefix) {
                    "目录尚未读取，按 F5 重试"
                } else {
                    "此目录为空"
                })
                .when(!loading, |body| {
                    body.child(div().text_sm().child("拖放文件或文件夹到这里上传"))
                })
                .into_any_element()
        };
        let sidebar = self.bucket_sidebar(
            tree.into_any_element(),
            self.narrow_workspace_sidebar(window),
            cx,
        );
        let main = if self.show_tasks && self.tasks_maximized {
            self.task_panel(true, cx)
        } else if let Some(preview) = self.preview.as_ref().filter(|_| self.show_preview) {
            self.preview_pane(preview, cx)
        } else if self.store.is_some() && self.trash.is_some() {
            self.trash_pane(cx)
        } else if self.store.is_some() {
            self.object_pane(contents, self.grid_layout, cx)
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(if self.buckets.is_empty() {
                    if self.available_buckets.is_empty() {
                        "服务器未返回存储桶"
                    } else {
                        "存储桶已全部隐藏，可在「连接与凭证」中恢复"
                    }
                } else {
                    "从左侧选择存储桶"
                })
                .into_any_element()
        };
        self.split_workspace(sidebar, main, window, cx)
    }

    fn directory_breadcrumb(
        &self,
        prefix: &str,
        file: Option<&Entry>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let bucket = self.store.as_ref().map_or("", Store::bucket);
        let mut ancestors = Vec::new();
        let mut current = String::new();
        let mut segments = std::iter::once((bucket, ""))
            .chain(prefix.split_inclusive('/').scan(0, |end, segment| {
                *end += segment.len();
                let label = segment.strip_suffix('/').unwrap_or(segment);
                Some((if label.is_empty() { "/" } else { label }, &prefix[..*end]))
            }))
            .chain(file.map(|entry| (entry.name(), prefix)))
            .peekable();
        while let Some((label, target)) = segments.next() {
            if segments.peek().is_none() {
                current = label.to_owned();
            } else {
                let target = target.to_owned();
                ancestors.push(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            Button::new(SharedString::from(target.clone()))
                                .small()
                                .ghost()
                                .label(label.to_owned())
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.list_focus.focus(window, cx);
                                    this.navigate(target.clone(), cx);
                                })),
                        )
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size_3p5()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .into_any_element(),
                );
            }
        }
        let input = file.and_then(|entry| self.rename_field(entry, false, cx));
        div()
            .id("directory-breadcrumb")
            .flex_1()
            .min_w_0()
            .h_full()
            .text_sm()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        if bounds.size.width <= px(0.) {
                            return Vec::new();
                        }
                        let mut current = div()
                            .max_w(bounds.size.width)
                            .px_2()
                            .line_height(FILE_NAME_LINE_HEIGHT)
                            .child(
                                div()
                                    .relative()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .truncate()
                                            .when(input.is_some(), |label| label.invisible())
                                            .child(current),
                                    )
                                    .when_some(input, |label, input| {
                                        label.child(div().absolute().inset_0().child(input))
                                    }),
                            )
                            .into_any_element();
                        let space = size(AvailableSpace::MaxContent, AvailableSpace::MaxContent);
                        let current_size = current.layout_as_root(space, window, cx);
                        let mut width = current_size.width;
                        let mut visible = vec![(current, current_size)];

                        // Fit whole crumbs from the current location back toward the root.
                        for mut ancestor in ancestors.into_iter().rev() {
                            let size = ancestor.layout_as_root(space, window, cx);
                            if width + size.width > bounds.size.width {
                                break;
                            }
                            width += size.width;
                            visible.push((ancestor, size));
                        }

                        visible.reverse();
                        let mut x = bounds.left();
                        for (crumb, size) in &mut visible {
                            let y = bounds.top() + (bounds.size.height - size.height) / 2.;
                            crumb.prepaint_at(point(x, y), window, cx);
                            x += size.width;
                        }
                        visible
                    },
                    |_, visible, window, cx| {
                        for (mut crumb, _) in visible {
                            crumb.paint(window, cx);
                        }
                    },
                )
                .size_full(),
            )
    }

    pub(super) fn path_bar(&self, file: Option<&Entry>, cx: &Context<Self>) -> AnyElement {
        let prefix = file.map_or(self.prefix.as_str(), |entry| parent_prefix(entry.key()));
        let parent = if file.is_some() {
            prefix
        } else {
            parent_prefix(prefix)
        }
        .to_owned();
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h_10()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("parent")
                    .small()
                    .ghost()
                    .icon(IconName::ArrowUp)
                    .tooltip("上一级")
                    .accessibility_label("上一级")
                    .disabled((file.is_none() && prefix.is_empty()) || self.busy.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.list_focus.focus(window, cx);
                        this.navigate(parent.clone(), cx);
                    })),
            )
            .child(self.directory_breadcrumb(prefix, file, cx))
            .child(
                Button::new("object-search")
                    .small()
                    .ghost()
                    .icon(IconName::Search)
                    .accessibility_label("搜索文件")
                    .tooltip("搜索文件（Ctrl+F）")
                    .disabled(self.busy.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.open_search("", window, cx))),
            )
            .when(file.is_none() && self.trash.is_none(), |bar| {
                let label = if self.grid_layout {
                    "切换到列表"
                } else {
                    "切换到宫格"
                };
                bar.child(
                    Button::new("file-layout")
                        .small()
                        .ghost()
                        .icon(if self.grid_layout {
                            IconName::List
                        } else {
                            IconName::LayoutGrid
                        })
                        .tooltip(label)
                        .accessibility_label(label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.grid_layout = !this.grid_layout;
                            this.list_focus.focus(window, cx);
                            if let Err(error) =
                                crate::services::settings::GridLayout::save(this.grid_layout)
                            {
                                this.show_error(error, cx);
                            }
                            cx.notify();
                        })),
                )
            })
            .child(self.object_actions(file, cx).into_any_element())
            .when_some(
                file.and(self.preview.as_ref())
                    .and_then(|preview| preview.can_save(cx)),
                |bar, enabled| {
                    bar.child(
                        Button::new("save-file-preview")
                            .small()
                            .primary()
                            .label("保存")
                            .tooltip("保存（Ctrl+S）")
                            .disabled(!enabled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_text_preview(window, cx);
                            })),
                    )
                },
            )
            .when(file.is_some(), |bar| {
                let dirty = self
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.is_dirty(cx));
                let label = if dirty {
                    "关闭预览（有未保存的更改）"
                } else {
                    "关闭预览"
                };
                bar.child(
                    Button::new("close-file-preview")
                        .small()
                        .ghost()
                        .size_6()
                        .p_0()
                        .group("close-file-preview")
                        .child(
                            div()
                                .relative()
                                .size_4()
                                .child(
                                    div()
                                        .when(dirty, |icon| {
                                            icon.invisible()
                                                .group_hover("close-file-preview", |icon| {
                                                    icon.visible()
                                                })
                                        })
                                        .child(Icon::new(IconName::X).size_4()),
                                )
                                .when(dirty, |icon| {
                                    icon.child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .group_hover("close-file-preview", |dot| {
                                                dot.invisible()
                                            })
                                            .child(
                                                div()
                                                    .size_2()
                                                    .rounded_full()
                                                    .bg(cx.theme().warning),
                                            ),
                                    )
                                }),
                        )
                        .tooltip(label)
                        .accessibility_label(label)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.cancel_preview(window, cx)),
                        ),
                )
            })
            .into_any_element()
    }

    #[inline(never)]
    fn object_pane(&self, contents: AnyElement, grid: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let owner = cx.weak_entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.path_bar(None, cx))
            .child({
                let body = self
                    .drop_surface(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .border_1()
                            .border_color(theme.background)
                            .focus_visible(|style| style.border_color(theme.ring))
                            .track_focus(&self.list_focus)
                            .on_key_down(cx.listener(|this, event, window, cx| {
                                if this.list_focus.is_focused(window) {
                                    this.keyboard(event, false, window, cx);
                                }
                            }))
                            .child(contents),
                        self.prefix.clone(),
                        cx,
                    )
                    .id("object-list-surface")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_create(cx);
                        this.list_focus.focus(window, cx);
                        this.object_multi.clear();
                        this.object_anchor = None;
                        if this.selected.take().is_some() {
                            this.tree_selected = Some(this.prefix.clone());
                            cx.notify();
                        }
                    }))
                    .context_menu(move |menu, window, cx| {
                        let Some(view) = owner.upgrade() else {
                            return menu;
                        };
                        let sort_owner = owner.clone();
                        let menu = menu.submenu("排序方式", window, cx, move |menu, _, cx| {
                            if let Some(view) = sort_owner.upgrade() {
                                view.read(cx).file_sort_menu(menu, &sort_owner)
                            } else {
                                menu
                            }
                        });
                        let view = view.read(cx);
                        let blocked = view.busy.is_some() || view.folder_write_locked(&view.prefix);
                        #[cfg(windows)]
                        let menu = if super::file_clipboard::has_files() {
                            let paste_owner = owner.clone();
                            menu.separator().item(
                                PopupMenuItem::new("粘贴").disabled(blocked).on_click(
                                    move |_, window, cx| {
                                        let _ = paste_owner.update(cx, |this, cx| {
                                            this.paste_clipboard_files(window, cx);
                                        });
                                    },
                                ),
                            )
                        } else {
                            menu
                        };
                        Self::create_menu(menu.separator(), &view.prefix, &owner, blocked, false)
                    });
                if grid {
                    body.into_any_element()
                } else {
                    let width = self
                        .file_column_widths
                        .iter()
                        .map(|width| width.0)
                        .sum::<f32>();
                    div()
                        .id("file-table-scroll")
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .h_full()
                                .min_w(rems(width + 13.))
                                .child(self.file_headers(cx))
                                .child(body),
                        )
                        .overflow_x_scrollbar()
                        .into_any_element()
                }
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, FileColumn, FileSort};

    #[test]
    fn file_sort_toggles_direction_and_keeps_folders_first() {
        let mut rows = [
            Entry::file("a.txt".into(), 200, String::new()),
            Entry::file("z.txt".into(), 10, String::new()),
            Entry::folder("folder/".into()),
        ];
        let mut sort = FileSort::default();
        sort.apply(&mut rows);
        assert_eq!(
            rows.each_ref().map(Entry::key),
            ["folder/", "a.txt", "z.txt"]
        );

        sort.toggle(FileColumn::Size);
        sort.apply(&mut rows);
        assert_eq!(
            rows.each_ref().map(Entry::key),
            ["folder/", "z.txt", "a.txt"]
        );

        sort.toggle(FileColumn::Size);
        sort.apply(&mut rows);
        assert_eq!(
            rows.each_ref().map(Entry::key),
            ["folder/", "a.txt", "z.txt"]
        );
    }
}

pub(super) fn file_icon(name: &str) -> IconName {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        ".gitignore" | ".gitattributes" | ".gitmodules" => return IconName::GitBranch,
        "dockerfile" | "containerfile" => return IconName::Container,
        "makefile" | "cmakelists.txt" | ".editorconfig" | ".env" => return IconName::FileCog,
        "cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" => {
            return IconName::FileLock;
        }
        _ => {}
    }
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    match extension {
        "rs" => IconName::Cog,
        "py" | "pyw" | "go" | "java" | "kt" | "c" | "cpp" | "h" | "hpp" | "cs" | "rb" | "php"
        | "swift" => IconName::FileCode,
        "js" | "mjs" | "cjs" | "ts" | "jsx" | "tsx" => IconName::Braces,
        "html" | "htm" | "vue" | "svelte" => IconName::CodeXml,
        "css" | "scss" | "less" => IconName::Palette,
        "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => IconName::FileTerminal,
        "json" | "jsonc" | "xml" => IconName::FileBraces,
        "toml" | "ini" | "cfg" | "conf" | "yaml" | "yml" => IconName::FileCog,
        "md" | "markdown" => IconName::BookOpen,
        "txt" | "log" | "rtf" | "doc" | "docx" | "odt" => IconName::FileText,
        "pdf" => IconName::FileType,
        "csv" | "tsv" | "xls" | "xlsx" | "ods" => IconName::FileSpreadsheet,
        "ppt" | "pptx" | "odp" => IconName::Presentation,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tiff" | "avif"
        | "heic" => IconName::FileImage,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" => IconName::FileMusic,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => IconName::FileVideoCamera,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" => IconName::FileArchive,
        "sql" | "sqlite" | "sqlite3" | "db" => IconName::Database,
        "exe" | "msi" | "apk" | "dmg" | "deb" => IconName::Package,
        "dll" | "so" | "dylib" | "bin" | "pdb" => IconName::FileDigit,
        "woff" | "woff2" | "ttf" | "otf" => IconName::Type,
        "pem" | "cer" | "crt" | "key" | "pfx" => IconName::FileKey,
        _ => IconName::File,
    }
}
