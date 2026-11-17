use super::*;
use crate::ui::touch::PinchZoom;
use gpui_kit::component::scroll::Scrollbar;

const FIT_ZOOM: u16 = 100;
const MIN_ZOOM: u16 = 25;
const MAX_ZOOM: u16 = 800;
const ZOOM_STEP: u16 = 25;

pub(super) struct ImagePreview {
    image: Arc<RenderImage>,
    zoom: u16,
    pinch: PinchZoom,
    scroll: ScrollHandle,
}

impl ImagePreview {
    pub(super) fn new(image: Arc<RenderImage>, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| cx.drop_image(this.image.clone(), None))
            .detach();
        Self {
            image,
            zoom: FIT_ZOOM,
            pinch: PinchZoom::default(),
            scroll: ScrollHandle::new(),
        }
    }

    fn set_zoom(&mut self, zoom: u16, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if self.zoom != zoom {
            self.zoom = zoom;
            cx.notify();
        }
    }

    fn viewport(&self, cx: &Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        let scroll = self.scroll.clone();
        let zoom = self.zoom;
        let owner = cx.weak_entity();
        // Measure the pane after the sidebar, path bar and zoom controls have been laid out.
        canvas(
            move |bounds, window, cx| {
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                let fit = ObjectFit::ScaleDown.get_bounds(bounds, image.size(0)).size;
                let scale = f32::from(zoom) / f32::from(FIT_ZOOM);
                let width = fit.width * scale;
                let height = fit.height * scale;
                let mut surface = div()
                    .relative()
                    .size_full()
                    .child(
                        div()
                            .id("image-preview-scroll")
                            .size_full()
                            .overflow_scroll()
                            .track_scroll(&scroll)
                            .child(
                                div()
                                    .w(width.max(bounds.size.width))
                                    .h(height.max(bounds.size.height))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        img(image)
                                            .w(width)
                                            .h(height)
                                            .flex_shrink_0()
                                            .object_fit(ObjectFit::Contain),
                                    ),
                            ),
                    )
                    .when(height > bounds.size.height, |surface| {
                        surface.child(
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .bottom_0()
                                .w(Scrollbar::width())
                                .child(Scrollbar::vertical(&scroll).viewport_from_layout()),
                        )
                    })
                    .when(width > bounds.size.width, |surface| {
                        surface.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(Scrollbar::width())
                                .child(Scrollbar::horizontal(&scroll).viewport_from_layout()),
                        )
                    })
                    .into_any_element();
                surface.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
                surface.prepaint_at(bounds.origin, window, cx);
                (surface, hitbox)
            },
            move |_, (mut surface, hitbox), window, cx| {
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
                    let _ = owner.update(cx, |this, cx| {
                        let zoom = if delta > px(0.) {
                            this.zoom + ZOOM_STEP
                        } else {
                            this.zoom.saturating_sub(ZOOM_STEP)
                        };
                        this.set_zoom(zoom, cx);
                    });
                    cx.stop_propagation();
                });
                surface.paint(window, cx);
            },
        )
        .size_full()
    }
}

impl Render for ImagePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("image-zoom-out")
                            .small()
                            .ghost()
                            .label("缩小")
                            .disabled(self.zoom == MIN_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.zoom.saturating_sub(ZOOM_STEP), cx);
                            })),
                    )
                    .child(
                        div()
                            .min_w(rems(3.))
                            .text_center()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}%", self.zoom)),
                    )
                    .child(
                        Button::new("image-zoom-in")
                            .small()
                            .ghost()
                            .label("放大")
                            .disabled(self.zoom == MAX_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.zoom + ZOOM_STEP, cx);
                            })),
                    )
                    .child(
                        Button::new("image-zoom-reset")
                            .small()
                            .ghost()
                            .label("适应窗口")
                            .tooltip("Ctrl＋滚轮可缩放图片")
                            .on_click(cx.listener(|this, _, _, cx| this.set_zoom(FIT_ZOOM, cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                        if let Some(zoom) = this.pinch.update(event, this.zoom, MIN_ZOOM, MAX_ZOOM)
                        {
                            this.set_zoom(zoom, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .child(self.viewport(cx)),
            )
    }
}
