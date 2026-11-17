use super::*;
use crate::ui::{file_viewer::PdfFile, touch::PinchZoom};
use gpui_kit::{
    base::{VirtualListScrollHandle, v_virtual_list},
    component::scroll::Scrollbar,
};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

const MIN_ZOOM: u16 = 25;
const MAX_ZOOM: u16 = 300;
const ZOOM_STEP: u16 = 25;
const WHEEL_ZOOM_STEP: u16 = 10;

#[derive(Clone, Copy)]
enum ViewTarget {
    Page(usize),
    Anchor {
        page: usize,
        within_page: Point<f32>,
        viewport: Point<f32>,
    },
}

struct PageLayout {
    bounds: Bounds<Pixels>,
    sizes: Rc<Vec<Size<Pixels>>>,
    ends: Vec<Pixels>,
    inset: Pixels,
    row_width: Pixels,
}

impl PageLayout {
    fn page_at(&self, y: Pixels) -> usize {
        self.ends
            .partition_point(|end| *end <= y)
            .min(self.ends.len() - 1)
    }

    fn page_top(&self, page: usize) -> Pixels {
        page.checked_sub(1)
            .map_or(px(0.), |previous| self.ends[previous])
    }

    fn page_origin(&self, page: usize) -> Point<Pixels> {
        point(
            (self.row_width - self.sizes[page].width) / 2.,
            self.page_top(page) + self.inset / 2.,
        )
    }

    fn anchor(&self, viewport: Point<f32>, offset: Point<Pixels>) -> ViewTarget {
        let content = self.viewport_point(viewport) - offset;
        let page = self.page_at(content.y);
        let within = content - self.page_origin(page);
        let size = self.sizes[page];
        ViewTarget::Anchor {
            page,
            within_page: point(
                f32::from(within.x) / f32::from(size.width.max(px(1.))),
                f32::from(within.y) / f32::from(size.height - self.inset),
            ),
            viewport,
        }
    }

    fn viewport_point(&self, viewport: Point<f32>) -> Point<Pixels> {
        point(
            self.bounds.size.width * viewport.x,
            self.bounds.size.height * viewport.y,
        )
    }

    fn offset_for(&self, target: ViewTarget, offset: Point<Pixels>) -> Point<Pixels> {
        match target {
            ViewTarget::Page(page) => point(offset.x, -self.page_top(page)),
            ViewTarget::Anchor {
                page,
                within_page,
                viewport,
            } => {
                let size = self.sizes[page];
                self.viewport_point(viewport)
                    - self.page_origin(page)
                    - point(
                        size.width * within_page.x,
                        (size.height - self.inset) * within_page.y,
                    )
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PageImage {
    index: usize,
    thumbnail: bool,
}

pub(super) struct PdfPreview {
    document: Arc<PdfFile>,
    zoom: u16,
    pinch: PinchZoom,
    current: usize,
    pages: VirtualListScrollHandle,
    thumbnails: UniformListScrollHandle,
    show_thumbnails: bool,
    layout: Rc<RefCell<Option<Rc<PageLayout>>>>,
    requested_view: Rc<Cell<Option<ViewTarget>>>,
    last_offset: Rc<Cell<Pixels>>,
    visible_pages: Rc<Cell<usize>>,
    images: HashMap<PageImage, Option<Arc<Image>>>,
    pending: HashSet<PageImage>,
    image_order: VecDeque<PageImage>,
    focus: FocusHandle,
}

impl PdfPreview {
    pub(super) fn new(
        document: Arc<PdfFile>,
        window: &mut Window,
        cx: &mut Context<Browser>,
    ) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            cx.on_release(|this, cx| {
                for image in this.images.drain().filter_map(|(_, image)| image) {
                    image.remove_asset(cx);
                }
            })
            .detach();
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            Self {
                document,
                zoom: 100,
                pinch: PinchZoom::default(),
                current: 0,
                pages: VirtualListScrollHandle::new(),
                thumbnails: UniformListScrollHandle::new(),
                show_thumbnails: true,
                layout: Rc::new(RefCell::new(None)),
                requested_view: Rc::new(Cell::new(None)),
                last_offset: Rc::new(Cell::new(px(0.))),
                visible_pages: Rc::new(Cell::new(1)),
                images: HashMap::new(),
                pending: HashSet::new(),
                image_order: VecDeque::new(),
                focus,
            }
        })
    }

    fn page_count(&self) -> usize {
        self.document.dimensions().len()
    }

    fn page_image(&mut self, key: PageImage, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if let Some(image) = self.images.get(&key) {
            if let Some(index) = self.image_order.iter().position(|page| *page == key) {
                self.image_order.remove(index);
                self.image_order.push_back(key);
            }
            return match image {
                Some(image) => img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                None => div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("无法渲染此页")
                    .into_any_element(),
            };
        }

        let active = self
            .pending
            .iter()
            .filter(|page| page.thumbnail == key.thumbnail)
            .count();
        if active < 2 && self.pending.insert(key) {
            let document = self.document.clone();
            let resolution = if key.thumbnail { 240 } else { 2200 };
            let job = cx
                .background_spawn(async move { document.render_page(key.index, resolution).ok() });
            cx.spawn(async move |view, cx| {
                let image = job.await;
                let _ = view.update(cx, |this, cx| {
                    this.pending.remove(&key);
                    this.images.insert(key, image);
                    this.image_order.push_back(key);
                    let limit = if key.thumbnail {
                        64
                    } else {
                        12.max(this.visible_pages.get() + 2)
                    };
                    if this
                        .image_order
                        .iter()
                        .filter(|page| page.thumbnail == key.thumbnail)
                        .count()
                        > limit
                        && let Some(index) = this
                            .image_order
                            .iter()
                            .position(|page| page.thumbnail == key.thumbnail)
                        && let Some(old) = this.image_order.remove(index)
                        && let Some(Some(image)) = this.images.remove(&old)
                    {
                        image.remove_asset(cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }

        Spinner::new().small().into_any_element()
    }

    fn go_to(&mut self, index: usize, cx: &mut Context<Self>) {
        self.current = index.min(self.page_count() - 1);
        self.requested_view
            .set(Some(ViewTarget::Page(self.current)));
        self.thumbnails
            .scroll_to_item(self.current, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn anchor_view(&self, viewport: Point<f32>) {
        if self.requested_view.get().is_none()
            && let Some(layout) = self.layout.borrow().as_ref()
        {
            self.requested_view
                .set(Some(layout.anchor(viewport, self.pages.offset())));
        }
    }

    fn zoom_at(&mut self, zoom: u16, viewport: Point<f32>, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if self.zoom != zoom {
            self.anchor_view(viewport);
            self.zoom = zoom;
            cx.notify();
        }
    }

    fn set_zoom(&mut self, zoom: u16, cx: &mut Context<Self>) {
        self.zoom_at(zoom, point(0.5, 0.5), cx);
    }

    fn toggle_thumbnails(&mut self, cx: &mut Context<Self>) {
        self.anchor_view(point(0.5, 0.5));
        self.show_thumbnails = !self.show_thumbnails;
        if self.show_thumbnails {
            self.thumbnails
                .scroll_to_item(self.current, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn thumbnail(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let image = self.page_image(
            PageImage {
                index,
                thumbnail: true,
            },
            cx,
        );
        let theme = cx.theme();
        let selected = index == self.current;
        div()
            .id(("pdf-thumbnail", index))
            .w_full()
            .h(rems(8.))
            .px_2()
            .py_1()
            .cursor_pointer()
            .rounded_sm()
            .when(selected, |row| row.bg(theme.primary.opacity(0.16)))
            .when(!selected, |row| row.hover(|row| row.bg(theme.accent)))
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(image),
            )
            .child(div().text_sm().child((index + 1).to_string()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.go_to(index, cx);
            }))
            .into_any_element()
    }

    fn reader(&self, cx: &Context<Self>) -> AnyElement {
        let owner = cx.entity();
        let document = self.document.clone();
        let scroll = self.pages.clone();
        let requested_view = self.requested_view.clone();
        let last_layout = self.layout.clone();
        let last_offset = self.last_offset.clone();
        let visible_pages = self.visible_pages.clone();
        let zoom = self.zoom;
        canvas(
            move |bounds, window, cx| {
                let inset = window.rem_size();
                let viewport_width = bounds.size.width;
                let widest = document
                    .dimensions()
                    .iter()
                    .map(|(width, _)| *width)
                    .fold(0., f32::max);
                let usable_width = (viewport_width - inset * 2.).max(px(1.)).min(inset * 46.);
                let usable_height = (bounds.size.height - inset * 2.).max(px(1.));
                let fit = (f32::from(usable_width) / widest)
                    .min(f32::from(usable_height) / document.dimensions()[0].1);
                let scale = fit * f32::from(zoom) / 100.;
                let row_width = viewport_width.max(px(widest * scale) + inset * 2.);
                let sizes: Rc<Vec<_>> = Rc::new(
                    document
                        .dimensions()
                        .iter()
                        .map(|&(width, height)| {
                            size(px(width * scale), px(height * scale).max(px(1.)) + inset)
                        })
                        .collect(),
                );
                let mut offset = px(0.);
                let page_ends: Vec<_> = sizes
                    .iter()
                    .map(|page| {
                        offset += page.height;
                        offset
                    })
                    .collect();
                let layout = Rc::new(PageLayout {
                    bounds,
                    sizes: sizes.clone(),
                    ends: page_ends,
                    inset,
                    row_width,
                });
                let requested = requested_view.take();
                if let Some(target) = requested {
                    scroll.set_offset(layout.offset_for(target, scroll.offset()));
                }
                last_layout.replace(Some(layout.clone()));
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

                let row_sizes = sizes.clone();
                let observer = owner.downgrade();
                let mut reader = div()
                    .relative()
                    .size_full()
                    .bg(cx.theme().muted)
                    .child(
                        v_virtual_list(
                            owner,
                            "pdf-pages",
                            sizes,
                            move |this, range: Range<usize>, _, cx| {
                                range
                                    .map(|index| {
                                        let page_size = row_sizes[index];
                                        let image = this.page_image(
                                            PageImage {
                                                index,
                                                thumbnail: false,
                                            },
                                            cx,
                                        );
                                        div()
                                            .w(row_width)
                                            .h(page_size.height)
                                            .pt(inset / 2.)
                                            .flex()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .w(page_size.width.max(px(1.)))
                                                    .h(page_size.height - inset)
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .bg(cx.theme().background)
                                                    .child(image),
                                            )
                                    })
                                    .collect()
                            },
                        )
                        .track_scroll(&scroll)
                        .size_full(),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(Scrollbar::width())
                            .child(Scrollbar::vertical(&scroll).viewport_from_layout()),
                    )
                    .when(row_width > viewport_width, |reader| {
                        reader.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(Scrollbar::width())
                                .child(Scrollbar::horizontal(&scroll).viewport_from_layout()),
                        )
                    })
                    .into_any_element();
                reader.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
                reader.prepaint_at(bounds.origin, window, cx);

                let y = -scroll.offset().y;
                let first = layout.page_at(y);
                let end = layout
                    .ends
                    .partition_point(|end| *end < y + bounds.size.height);
                visible_pages.set(end.saturating_sub(first) + 1);
                let previous_y = last_offset.replace(y);
                if !matches!(requested, Some(ViewTarget::Page(_))) && y != previous_y {
                    let last_page = layout.ends.len() - 1;
                    let current = if y > px(0.)
                        && y + bounds.size.height >= layout.ends[last_page] - px(1.)
                    {
                        last_page
                    } else {
                        first
                    };
                    let observer = observer.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = observer.update(cx, |this, cx| {
                            if this.requested_view.get().is_none()
                                && -this.pages.offset().y == y
                                && this.current != current
                            {
                                this.current = current;
                                this.thumbnails
                                    .scroll_to_item(current, ScrollStrategy::Nearest);
                                cx.notify();
                            }
                        });
                    });
                }
                (reader, observer, hitbox)
            },
            |bounds, (mut reader, owner, hitbox), window, cx| {
                let pinch_owner = owner.clone();
                let pinch_hitbox = hitbox.clone();
                window.on_mouse_event(move |event: &PinchEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || (event.phase == TouchPhase::Started && !pinch_hitbox.is_hovered(window))
                    {
                        return;
                    }
                    let handled = pinch_owner
                        .update(cx, |this, cx| {
                            if event.phase == TouchPhase::Started || this.pinch.is_active() {
                                if let Some(zoom) =
                                    this.pinch.update(event, this.zoom, MIN_ZOOM, MAX_ZOOM)
                                {
                                    let cursor = event.position - bounds.origin;
                                    this.zoom_at(
                                        zoom,
                                        point(
                                            f32::from(cursor.x)
                                                / f32::from(bounds.size.width.max(px(1.))),
                                            f32::from(cursor.y)
                                                / f32::from(bounds.size.height.max(px(1.))),
                                        ),
                                        cx,
                                    );
                                }
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                });
                // Handle Ctrl before the virtual list consumes the same wheel event.
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || !event.modifiers.control
                        || !hitbox.should_handle_scroll(window)
                    {
                        return;
                    }
                    let delta = event.delta.pixel_delta(px(1.)).y;
                    if delta == px(0.) {
                        return;
                    }

                    let cursor = event.position - bounds.origin;
                    let viewport = point(
                        f32::from(cursor.x) / f32::from(bounds.size.width.max(px(1.))),
                        f32::from(cursor.y) / f32::from(bounds.size.height.max(px(1.))),
                    );
                    let _ = owner.update(cx, |this, cx| {
                        let zoom = if delta > px(0.) {
                            this.zoom + WHEEL_ZOOM_STEP
                        } else {
                            this.zoom.saturating_sub(WHEEL_ZOOM_STEP)
                        };
                        this.zoom_at(zoom, viewport, cx);
                    });
                    cx.stop_propagation();
                });
                reader.paint(window, cx);
            },
        )
        .size_full()
        .into_any_element()
    }
}

impl Render for PdfPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let theme = cx.theme();
        let count = self.page_count();
        let thumbnails = uniform_list("pdf-thumbnails", count, move |range, _, cx| {
            owner
                .update(cx, |this, cx| {
                    range.map(|index| this.thumbnail(index, cx)).collect()
                })
                .unwrap_or_default()
        })
        .track_scroll(&self.thumbnails)
        .size_full();

        div()
            .id("pdf-preview")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = &event.keystroke;
                match key.key.as_str() {
                    "pageup" => this.go_to(this.current.saturating_sub(1), cx),
                    "pagedown" => this.go_to(this.current + 1, cx),
                    "-" if !key.modifiers.control => {
                        this.set_zoom(this.zoom.saturating_sub(ZOOM_STEP), cx)
                    }
                    "+" | "=" if !key.modifiers.control => this.set_zoom(this.zoom + ZOOM_STEP, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("pdf-toggle-thumbnails")
                            .small()
                            .ghost()
                            .label(if self.show_thumbnails {
                                "隐藏缩略图"
                            } else {
                                "显示缩略图"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_thumbnails(cx))),
                    )
                    .child(
                        Button::new("pdf-prev")
                            .small()
                            .ghost()
                            .label("上一页")
                            .disabled(self.current == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.go_to(this.current.saturating_sub(1), cx)
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} / {count}", self.current + 1)),
                    )
                    .child(
                        Button::new("pdf-next")
                            .small()
                            .ghost()
                            .label("下一页")
                            .disabled(self.current + 1 >= count)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.go_to(this.current + 1, cx)),
                            ),
                    )
                    .child(
                        Button::new("pdf-zoom-out")
                            .small()
                            .ghost()
                            .label("缩小")
                            .disabled(self.zoom <= MIN_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.zoom.saturating_sub(ZOOM_STEP), cx)
                            })),
                    )
                    .child(
                        Button::new("pdf-zoom-reset")
                            .small()
                            .ghost()
                            .label(format!("{}%", self.zoom))
                            .tooltip("恢复 100% · Ctrl＋滚轮缩放")
                            .on_click(cx.listener(|this, _, _, cx| this.set_zoom(100, cx))),
                    )
                    .child(
                        Button::new("pdf-zoom-in")
                            .small()
                            .ghost()
                            .label("放大")
                            .disabled(self.zoom >= MAX_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.zoom + ZOOM_STEP, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .when(self.show_thumbnails, |body| {
                        body.child(
                            div()
                                .relative()
                                .h_full()
                                .w(rems(8.5))
                                .flex_shrink_0()
                                .border_r_1()
                                .border_color(theme.border)
                                .child(thumbnails)
                                .child(
                                    div()
                                        .absolute()
                                        .right_0()
                                        .top_0()
                                        .bottom_0()
                                        .w(Scrollbar::width())
                                        .child(
                                            Scrollbar::vertical(&self.thumbnails)
                                                .viewport_from_layout(),
                                        ),
                                ),
                        )
                    })
                    .child(div().flex_1().min_w_0().h_full().child(self.reader(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{PageLayout, ViewTarget};
    use gpui_kit::{Bounds, point, px, size};
    use std::rc::Rc;

    #[test]
    fn zoom_keeps_the_same_pdf_point_under_the_cursor() {
        let layout = |factor: f32| PageLayout {
            bounds: Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(800.), px(600.)),
            },
            sizes: Rc::new(vec![
                size(px(500. * factor), px(700. * factor + 16.)),
                size(px(700. * factor), px(500. * factor + 16.)),
            ]),
            ends: vec![px(700. * factor + 16.), px(1200. * factor + 32.)],
            inset: px(16.),
            row_width: px((700. * factor + 32.).max(800.)),
        };
        let before = layout(1.);
        let after = layout(1.5);
        let cursor = point(0.625, 0.5);
        let anchor = before.anchor(cursor, point(px(0.), px(-700.)));
        let offset = after.offset_for(anchor, point(px(0.), px(0.)));
        let ViewTarget::Anchor {
            page, within_page, ..
        } = after.anchor(cursor, offset)
        else {
            panic!("expected a page anchor");
        };
        assert_eq!(page, 1);
        assert!((within_page.x - 450. / 700.).abs() < 1e-5);
        assert!((within_page.y - 276. / 500.).abs() < 1e-5);
    }
}
