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
        uploading: HashSet<usize>,
        pending: Vec<String>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let owner = cx.weak_entity();
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
                    (count + pending.len()).div_ceil(columns_count),
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
                                                (start
                                                    ..(start + columns_count)
                                                        .min(count + pending.len()))
                                                    .map(|index| {
                                                        if let Some(key) = index
                                                            .checked_sub(count)
                                                            .and_then(|index| pending.get(index))
                                                        {
                                                            return this
                                                                .pending_file_tile(key, width, cx);
                                                        }
                                                        this.file_tile(
                                                            index,
                                                            width,
                                                            uploading.contains(&index),
                                                            cx,
                                                        )
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

    fn file_tile(
        &mut self,
        index: usize,
        width: Pixels,
        uploading: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = self.rows()[index].clone();
        #[cfg(windows)]
        let image = (!uploading).then(|| self.thumbnail(&entry, cx)).flatten();
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
                .when(selected, |tile| tile.bg(theme.accent))
                .hover(|tile| tile.bg(theme.accent))
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
        if uploading {
            return self.pending_file_tile(entry.key(), width, cx);
        }
        self.file_entry_surface(row, entry, cx).into_any_element()
    }

    fn pending_file_tile(&self, key: &str, width: Pixels, cx: &Context<Self>) -> AnyElement {
        let name = key.rsplit('/').next().unwrap_or(key);
        div()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .p_2()
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
                            .h(rems(4.))
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
                            .child("正在上传"),
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
