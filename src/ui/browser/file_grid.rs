use super::*;

#[cfg(windows)]
#[derive(Default)]
pub(super) struct Thumbnails {
    images: HashMap<String, Option<Arc<Image>>>,
    pending: HashSet<String>,
    order: VecDeque<String>,
}

impl Browser {
    #[cfg(windows)]
    pub(super) fn clear_thumbnail_memory(&mut self, cx: &mut Context<Self>) {
        self.thumbnails.order.clear();
        for image in self
            .thumbnails
            .images
            .drain()
            .filter_map(|(_, image)| image)
        {
            image.remove_asset(cx);
        }
    }

    pub(super) fn file_grid(
        &self,
        count: usize,
        uploading: HashMap<usize, UploadPlaceholder>,
        pending: Vec<UploadPlaceholder>,
        creating: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let owner = cx.weak_entity();
        let visible_count = count + pending.len() + usize::from(creating);
        let columns = self.grid_columns.clone();
        let scroll = self.grid_scroll.clone();
        canvas(
            move |bounds, window, cx| {
                let gap = rems(0.5);
                let gap_pixels = window.rem_size() * gap.0;
                let tile_width = window.rem_size() * 8.;
                let columns_count = ((f32::from(bounds.size.width + gap_pixels)
                    / f32::from(tile_width + gap_pixels))
                    as usize)
                    .max(1);
                columns.set(columns_count);
                let width = (bounds.size.width - gap_pixels * (columns_count - 1) as f32)
                    / columns_count as f32;
                let mut grid = uniform_list(
                    "file-grid",
                    visible_count.div_ceil(columns_count),
                    move |range, _, cx| {
                        owner
                            .update(cx, |this, cx| {
                                range
                                    .map(|row| {
                                        let start = row * columns_count;
                                        div()
                                            .flex()
                                            .h(rems(8.5))
                                            .gap(gap)
                                            .pb(gap)
                                            .children(
                                                (start..(start + columns_count).min(visible_count))
                                                    .map(|index| {
                                                        if creating && index == 0 {
                                                            return this
                                                                .object_create_draft()
                                                                .map_or_else(
                                                                    || {
                                                                        div()
                                                                            .w(width)
                                                                            .into_any_element()
                                                                    },
                                                                    |draft| {
                                                                        this.file_create_tile(
                                                                            draft, width, cx,
                                                                        )
                                                                    },
                                                                );
                                                        }
                                                        let index = index - usize::from(creating);
                                                        if let Some(upload) =
                                                            uploading.get(&index).or_else(|| {
                                                                index.checked_sub(count).and_then(
                                                                    |index| pending.get(index),
                                                                )
                                                            })
                                                        {
                                                            return this.pending_file_tile(
                                                                upload, width, cx,
                                                            );
                                                        }
                                                        this.file_tile(index, width, cx)
                                                    }),
                                            )
                                            .into_any_element()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    },
                )
                .track_scroll(&scroll)
                .size_full()
                .into_any_element();
                grid.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
                grid.prepaint_at(bounds.origin, window, cx);
                grid
            },
            |_, mut grid, window, cx| grid.paint(window, cx),
        )
        .size_full()
        .into_any_element()
    }

    fn file_create_tile(
        &self,
        draft: &CreateDraft,
        width: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        self.create_editor(
            div()
                .id("grid-create")
                .w(width)
                .h_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .p_2()
                .child(
                    div()
                        .w_full()
                        .h(rems(5.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(self.create_icon(draft, cx).size_10()),
                )
                .child(Self::inline_name_input(&draft.input)),
            draft,
            cx,
        )
    }

    fn file_tile(&mut self, index: usize, width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let entry = self.rows()[index].clone();
        #[cfg(windows)]
        let image = self.thumbnail(&entry, cx);
        #[cfg(not(windows))]
        let image: Option<Arc<Image>> = None;
        let theme = cx.theme();
        let selected = self.object_selected(entry.key());
        let row = div().w(width).h_full().flex_shrink_0().child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .p_2()
                .rounded(cx.theme().radius)
                .when(selected, |tile| tile.bg(theme.primary.opacity(0.16)))
                .when(!selected, |tile| tile.hover(|tile| tile.bg(theme.accent)))
                .child(
                    div()
                        .w_full()
                        .h(rems(5.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(if let Some(image) = image {
                            img(image)
                                .size_full()
                                .object_fit(ObjectFit::Contain)
                                .into_any_element()
                        } else {
                            Icon::new(if entry.directory() {
                                IconName::Folder
                            } else {
                                file_icon(entry.name())
                            })
                            .size_10()
                            .text_color(theme.muted_foreground)
                            .into_any_element()
                        }),
                )
                .child(self.rename_field(&entry, false, cx).unwrap_or_else(|| {
                    div()
                        .w_full()
                        .text_center()
                        .text_sm()
                        .truncate()
                        .child(entry.name().to_owned())
                        .into_any_element()
                })),
        );
        let row = if entry.directory() {
            self.drop_surface(row, entry.key().to_owned(), cx)
        } else {
            row
        };
        self.file_entry_surface(row, entry, cx).into_any_element()
    }

    fn pending_file_tile(
        &self,
        (id, file, queued): &UploadPlaceholder,
        width: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let name = file.key.rsplit('/').next().unwrap_or(&file.key);
        let percent = if *queued { 0.0 } else { file.percent() };
        div()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .p_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .h(rems(3.5))
                            .flex()
                            .items_center()
                            .child(Icon::new(file_icon(name)).size_8()),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_center()
                            .text_sm()
                            .child(name.to_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if *queued {
                                "排队中".to_owned()
                            } else {
                                format!("{percent:.0}%")
                            }),
                    )
                    .child(
                        Progress::new((
                            ElementId::from(("tile-upload-progress", id.0)),
                            id.1.to_string(),
                        ))
                        .xsmall()
                        .w_full()
                        .value(percent)
                        .accessibility_label(format!("上传 {name}")),
                    ),
            )
            .into_any_element()
    }

    #[cfg(windows)]
    fn thumbnail(&mut self, entry: &Entry, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        use crate::ui::file_viewer;
        let store = self.store.as_ref()?;
        let profile = self.connected_profile;
        let key = file_viewer::thumbnail_key(store, entry, profile)?;
        if let Some(image) = self.thumbnails.images.get(&key) {
            return image.clone();
        }
        if self.thumbnails.pending.len() >= 4 || !self.thumbnails.pending.insert(key.clone()) {
            return None;
        }
        let source = entry.clone();
        let slots = self.transfer_slots.clone();
        let store = store.clone();
        let cache_key = key.clone();
        let job = self.runtime.spawn(async move {
            file_viewer::thumbnail(store, source, profile, &cache_key, slots).await
        });
        cx.spawn(async move |view, cx| {
            let image = job.await.ok().and_then(Result::ok);
            let _ = view.update(cx, |this, cx| {
                let cache = &mut this.thumbnails;
                cache.pending.remove(&key);
                cache.order.push_back(key.clone());
                cache.images.insert(key, image);
                if cache.order.len() > 256
                    && let Some(old) = cache.order.pop_front()
                {
                    if let Some(Some(image)) = cache.images.remove(&old) {
                        image.remove_asset(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        None
    }
}
