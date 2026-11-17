use super::*;
use crate::ui::office_document::{OfficeDocument, Paragraph, Presentation, Slide, WordBlock};
use crate::ui::touch::PinchZoom;
use gpui_kit::{
    base::{VirtualListScrollHandle, v_virtual_list},
    component::scroll::Scrollbar,
};
use std::{cell::Cell, rc::Rc};

const MIN_SLIDE_ZOOM: u16 = 50;
const MAX_SLIDE_ZOOM: u16 = 250;

pub(super) fn extension(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1;
    match extension.to_ascii_lowercase().as_str() {
        "doc" => Some(".doc"),
        "docx" => Some(".docx"),
        "ppt" => Some(".ppt"),
        "pptx" => Some(".pptx"),
        _ => None,
    }
}

pub(super) struct OfficePreview {
    content: Content,
    focus: FocusHandle,
}

enum Content {
    Word {
        blocks: Vec<WordBlock>,
        scroll: ListState,
        zoom: u16,
        pinch: PinchZoom,
    },
    Slides(Slides),
}

struct Slides {
    deck: Arc<Presentation>,
    current: usize,
    pending: HashSet<usize>,
    reader: VirtualListScrollHandle,
    navigator: UniformListScrollHandle,
    show_navigator: bool,
    zoom: u16,
    pinch: PinchZoom,
    row_height: Rc<Cell<Pixels>>,
    reader_y: Rc<Cell<Pixels>>,
    jump: Rc<Cell<Option<usize>>>,
    zoom_position: Rc<Cell<Option<(f32, Pixels)>>>,
    cache: VecDeque<(usize, Result<Arc<Slide>, SharedString>)>,
}

impl OfficePreview {
    pub(super) fn new(document: OfficeDocument, cx: &mut Context<Browser>) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            cx.on_release(|this, cx| match &this.content {
                Content::Word { blocks, .. } => {
                    for block in blocks {
                        if let WordBlock::Image { image, .. } = block {
                            image.clone().remove_asset(cx);
                        }
                    }
                }
                Content::Slides(slides) => {
                    for (_, slide) in &slides.cache {
                        if let Ok(slide) = slide {
                            remove_slide_assets(slide, cx);
                        }
                    }
                }
            })
            .detach();
            let content = match document {
                OfficeDocument::Word(blocks) => Content::Word {
                    scroll: ListState::new(blocks.len(), ListAlignment::Top, px(240.)),
                    blocks,
                    zoom: 100,
                    pinch: PinchZoom::default(),
                },
                OfficeDocument::Slides(deck, first) => Content::Slides(Slides {
                    deck: Arc::new(deck),
                    current: 0,
                    pending: HashSet::new(),
                    reader: VirtualListScrollHandle::new(),
                    navigator: UniformListScrollHandle::new(),
                    show_navigator: true,
                    zoom: 100,
                    pinch: PinchZoom::default(),
                    row_height: Rc::new(Cell::new(px(0.))),
                    reader_y: Rc::new(Cell::new(px(0.))),
                    jump: Rc::new(Cell::new(None)),
                    zoom_position: Rc::new(Cell::new(None)),
                    cache: VecDeque::from([(0, Ok(Arc::new(first)))]),
                }),
            };
            Self {
                content,
                focus: cx.focus_handle(),
            }
        })
    }

    fn jump_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let Content::Slides(slides) = &mut self.content else {
            return;
        };
        slides.current = index.min(slides.deck.len() - 1);
        slides.jump.set(Some(slides.current));
        slides
            .navigator
            .scroll_to_item(slides.current, ScrollStrategy::Nearest);
        self.request_slide(index, false, cx);
        cx.notify();
    }

    fn request_slide(&mut self, index: usize, thumbnail: bool, cx: &mut Context<Self>) {
        let Content::Slides(slides) = &mut self.content else {
            return;
        };
        if index >= slides.deck.len() {
            return;
        }
        if let Some(pos) = slides.cache.iter().position(|(cached, _)| *cached == index) {
            if let Some(entry) = slides.cache.remove(pos) {
                slides.cache.push_back(entry);
            }
            return;
        }
        if slides.pending.len() >= if thumbnail { 1 } else { 3 } || !slides.pending.insert(index) {
            return;
        }
        let deck = slides.deck.clone();
        let job = cx.background_spawn(async move { deck.slide(index) });
        cx.spawn(async move |owner, cx| {
            let slide = job
                .await
                .map(Arc::new)
                .map_err(|error| format!("无法读取第 {} 张幻灯片：{error:#}", index + 1).into());
            let _ = owner.update(cx, |this, cx| {
                let Content::Slides(slides) = &mut this.content else {
                    return;
                };
                slides.pending.remove(&index);
                slides.cache.push_back((index, slide));
                if slides.cache.len() > 18
                    && let Some(oldest) = slides
                        .cache
                        .iter()
                        .position(|(cached, _)| *cached != slides.current)
                    && let Some((_, Ok(slide))) = slides.cache.remove(oldest)
                {
                    remove_slide_assets(&slide, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_slide_zoom(&mut self, zoom: u16, y: Pixels, cx: &mut Context<Self>) {
        let Content::Slides(slides) = &mut self.content else {
            return;
        };
        let zoom = zoom.clamp(MIN_SLIDE_ZOOM, MAX_SLIDE_ZOOM);
        if slides.zoom == zoom {
            return;
        }
        let height = slides.row_height.get();
        if height > px(0.) && slides.zoom_position.get().is_none() {
            let page_position = f32::from(y - slides.reader.offset().y) / f32::from(height);
            slides.zoom_position.set(Some((page_position, y)));
        }
        slides.zoom = zoom;
        cx.notify();
    }

    fn slide_tile(&mut self, index: usize, thumb: bool, cx: &mut Context<Self>) -> AnyElement {
        self.request_slide(index, thumb, cx);
        let Content::Slides(slides) = &self.content else {
            return div().into_any_element();
        };
        let body = match slides.cache.iter().find(|(cached, _)| *cached == index) {
            Some((_, Ok(slide))) => {
                slide_view(slide.clone(), slides.deck.width, slides.deck.height)
            }
            Some((_, Err(error))) if !thumb => div().p_4().child(error.clone()).into_any_element(),
            Some((_, Err(_))) => div().into_any_element(),
            None if thumb => div()
                .size_full()
                .bg(cx.theme().background)
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new().small())
                .into_any_element(),
        };
        if !thumb {
            return body;
        }
        div()
            .id(("slide-thumbnail", index))
            .w_full()
            .h(rems(7.))
            .flex()
            .flex_col()
            .items_center()
            .px_2()
            .py_1()
            .gap_1()
            .cursor_pointer()
            .rounded_sm()
            .when(slides.current == index, |row| {
                row.bg(cx.theme().primary.opacity(0.16))
            })
            .when(slides.current != index, |row| {
                row.hover(|row| row.bg(cx.theme().accent))
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
            .child(div().text_sm().child((index + 1).to_string()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.jump_to(index, cx);
            }))
            .into_any_element()
    }

    fn set_word_zoom(&mut self, zoom: u16, cx: &mut Context<Self>) {
        if let Content::Word {
            scroll,
            zoom: current,
            ..
        } = &mut self.content
        {
            let zoom = zoom.clamp(50, 250);
            if *current != zoom {
                *current = zoom;
                scroll.remeasure();
                cx.notify();
            }
        }
    }

    fn word_pane(&self, scroll: &ListState, zoom: u16, cx: &Context<Self>) -> AnyElement {
        let owner = cx.weak_entity();
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().muted)
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                if let Content::Word { zoom, pinch, .. } = &mut this.content {
                    if let Some(zoom) = pinch.update(event, *zoom, 50, 250) {
                        this.set_word_zoom(zoom, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .bg(cx.theme().background)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(
                        [
                            (
                                "word-zoom-out",
                                "缩小".to_owned(),
                                zoom.saturating_sub(25),
                                zoom == 50,
                            ),
                            ("word-zoom-reset", format!("{zoom}%"), 100, zoom == 100),
                            ("word-zoom-in", "放大".to_owned(), zoom + 25, zoom == 250),
                        ]
                        .into_iter()
                        .map(|(id, label, target, disabled)| {
                            Button::new(id)
                                .small()
                                .ghost()
                                .label(label)
                                .disabled(disabled)
                                .when(id == "word-zoom-reset", |button| {
                                    button.tooltip("恢复 100%")
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.set_word_zoom(target, cx)
                                    }),
                                )
                        }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(scroll.clone(), move |index, _, cx| {
                            owner
                                .read_with(cx, |this, cx| {
                                    let Content::Word { blocks, .. } = &this.content else {
                                        return div().into_any_element();
                                    };
                                    word_row(
                                        &blocks[index],
                                        index == 0,
                                        index + 1 == blocks.len(),
                                        f32::from(zoom) / 100.,
                                        cx,
                                    )
                                })
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full(),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .w(Scrollbar::width())
                            .child(Scrollbar::vertical(scroll).viewport_from_layout()),
                    ),
            )
            .into_any_element()
    }

    fn slide_reader(&self, slides: &Slides, cx: &Context<Self>) -> AnyElement {
        let owner = cx.entity();
        let scroll = slides.reader.clone();
        let row_height = slides.row_height.clone();
        let reader_y = slides.reader_y.clone();
        let jump = slides.jump.clone();
        let zoom_position = slides.zoom_position.clone();
        let count = slides.deck.len();
        let width = slides.deck.width;
        let height = slides.deck.height;
        let zoom = slides.zoom;
        canvas(
            move |bounds, window, cx| {
                let gap = window.rem_size();
                let fit = (f32::from((bounds.size.width - gap * 2.).max(px(1.))) / width)
                    .min(f32::from((bounds.size.height - gap * 2.).max(px(1.))) / height);
                let scale = fit * f32::from(zoom) / 100.;
                let slide_width = px(width * scale);
                let slide_height = px(height * scale);
                let row = slide_height + gap;
                let row_width = bounds.size.width.max(slide_width + gap * 2.);
                let previous_height = row_height.replace(row);
                let mut offset = scroll.offset();
                let requested = jump.take();
                if let Some(index) = requested {
                    offset.y = -row * index as f32;
                    zoom_position.take();
                } else if let Some((position, y)) = zoom_position.take() {
                    offset.y = y - row * position;
                } else if previous_height > px(0.) && previous_height != row {
                    offset.y *= f32::from(row) / f32::from(previous_height);
                }
                scroll.set_offset(offset);
                let sizes = Rc::new(vec![size(row_width, row); count]);
                let observer = owner.downgrade();
                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                let mut surface = div()
                    .relative()
                    .size_full()
                    .bg(cx.theme().muted)
                    .child(
                        v_virtual_list(
                            owner.clone(),
                            "office-slide-pages",
                            sizes,
                            move |this, range, _, cx| {
                                range
                                    .map(|index| {
                                        div()
                                            .w(row_width)
                                            .h(row)
                                            .pt(gap / 2.)
                                            .flex()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .w(slide_width)
                                                    .h(slide_height)
                                                    .child(this.slide_tile(index, false, cx)),
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
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(Scrollbar::width())
                            .child(Scrollbar::vertical(&scroll).viewport_from_layout()),
                    )
                    .when(row_width > bounds.size.width, |surface| {
                        surface.child(
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
                surface.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
                surface.prepaint_at(bounds.origin, window, cx);
                let y = -scroll.offset().y;
                let current = (f32::from(y + row.min(bounds.size.height) * 0.35) / f32::from(row))
                    .floor()
                    .max(0.) as usize;
                let current = if y > px(0.) && y + bounds.size.height >= row * count as f32 - px(1.)
                {
                    count - 1
                } else {
                    current.min(count - 1)
                };
                let previous_y = reader_y.replace(y);
                if requested.is_none()
                    && previous_y != y
                    && owner.read(cx).selected_slide() != Some(current)
                {
                    let observer = observer.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = observer.update(cx, |this, cx| {
                            let Content::Slides(slides) = &mut this.content else {
                                return;
                            };
                            if slides.current != current
                                && slides.jump.get().is_none()
                                && -slides.reader.offset().y == y
                            {
                                slides.current = current;
                                if slides.show_navigator {
                                    slides
                                        .navigator
                                        .scroll_to_item(current, ScrollStrategy::Nearest);
                                }
                                cx.notify();
                            }
                        });
                    });
                }
                (surface, hitbox, observer)
            },
            |bounds, (mut surface, hitbox, owner), window, cx| {
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
                            let Content::Slides(slides) = &mut this.content else {
                                return false;
                            };
                            if event.phase != TouchPhase::Started && !slides.pinch.is_active() {
                                return false;
                            }
                            if let Some(zoom) = slides.pinch.update(
                                event,
                                slides.zoom,
                                MIN_SLIDE_ZOOM,
                                MAX_SLIDE_ZOOM,
                            ) {
                                this.set_slide_zoom(zoom, event.position.y - bounds.origin.y, cx);
                            }
                            true
                        })
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation()
                    }
                });
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
                        if let Content::Slides(slides) = &this.content {
                            let zoom = if delta > px(0.) {
                                slides.zoom + 10
                            } else {
                                slides.zoom.saturating_sub(10)
                            };
                            this.set_slide_zoom(zoom, event.position.y - bounds.origin.y, cx);
                        }
                    });
                    cx.stop_propagation();
                });
                surface.paint(window, cx);
            },
        )
        .size_full()
        .into_any_element()
    }

    fn selected_slide(&self) -> Option<usize> {
        match &self.content {
            Content::Slides(slides) => Some(slides.current),
            _ => None,
        }
    }

    fn zoom_slide_by(&mut self, change: i16, cx: &mut Context<Self>) {
        if let Content::Slides(slides) = &self.content {
            let zoom = if change == 0 {
                100
            } else {
                slides.zoom.saturating_add_signed(change)
            };
            let middle = slides.reader.bounds().size.height / 2.;
            self.set_slide_zoom(zoom, middle, cx);
        }
    }

    fn slides_pane(&self, slides: &Slides, cx: &Context<Self>) -> AnyElement {
        let owner = cx.weak_entity();
        let theme = cx.theme();
        let count = slides.deck.len();
        div()
            .id("office-slides")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let Some(current) = this.selected_slide() else {
                    return;
                };
                match event.keystroke.key.as_str() {
                    "pageup" => this.jump_to(current.saturating_sub(1), cx),
                    "pagedown" => this.jump_to(current + 1, cx),
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
                    .flex_shrink_0()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("slide-toggle-navigation")
                            .small()
                            .ghost()
                            .label(if slides.show_navigator {
                                "隐藏缩略图"
                            } else {
                                "显示缩略图"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Content::Slides(slides) = &mut this.content {
                                    slides.show_navigator = !slides.show_navigator;
                                    if slides.show_navigator {
                                        slides.navigator.scroll_to_item(
                                            slides.current,
                                            ScrollStrategy::Nearest,
                                        );
                                    }
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("slide-previous")
                            .small()
                            .ghost()
                            .label("上一页")
                            .disabled(slides.current == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(current) = this.selected_slide() {
                                    this.jump_to(current.saturating_sub(1), cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} / {count}", slides.current + 1)),
                    )
                    .child(
                        Button::new("slide-next")
                            .small()
                            .ghost()
                            .label("下一页")
                            .disabled(slides.current + 1 >= count)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(current) = this.selected_slide() {
                                    this.jump_to(current + 1, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("slide-zoom-out")
                            .small()
                            .ghost()
                            .label("缩小")
                            .disabled(slides.zoom <= MIN_SLIDE_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_slide_by(-25, cx))),
                    )
                    .child(
                        Button::new("slide-reset-zoom")
                            .small()
                            .ghost()
                            .label(format!("{}%", slides.zoom))
                            .tooltip("恢复 100%")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_slide_by(0, cx))),
                    )
                    .child(
                        Button::new("slide-zoom-in")
                            .small()
                            .ghost()
                            .label("放大")
                            .disabled(slides.zoom >= MAX_SLIDE_ZOOM)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_slide_by(25, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .when(slides.show_navigator, |body| {
                        body.child(
                            div()
                                .relative()
                                .h_full()
                                .w(rems(8.5))
                                .flex_shrink_0()
                                .border_r_1()
                                .border_color(theme.border)
                                .child(
                                    uniform_list("slide-navigation", count, move |range, _, cx| {
                                        owner
                                            .update(cx, |this, cx| {
                                                range
                                                    .map(|index| this.slide_tile(index, true, cx))
                                                    .collect()
                                            })
                                            .unwrap_or_default()
                                    })
                                    .track_scroll(&slides.navigator)
                                    .size_full(),
                                )
                                .child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .right_0()
                                        .w(Scrollbar::width())
                                        .child(
                                            Scrollbar::vertical(&slides.navigator)
                                                .viewport_from_layout(),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.slide_reader(slides, cx)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for OfficePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.content {
            Content::Word { blocks, .. } if blocks.is_empty() => {
                div().p_4().child("文档没有可预览的正文").into_any_element()
            }
            Content::Word { scroll, zoom, .. } => self.word_pane(scroll, *zoom, cx),
            Content::Slides(slides) => self.slides_pane(slides, cx),
        }
    }
}

fn word_row(block: &WordBlock, first: bool, last: bool, scale: f32, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let content = match block {
        WordBlock::Paragraph(Paragraph {
            text,
            emphasis,
            size,
            alignment,
            bullet,
            heading,
        }) => {
            let highlights = emphasis.iter().map(|(range, bold, italic)| {
                (
                    range.clone(),
                    HighlightStyle {
                        font_weight: bold.then_some(FontWeight::BOLD),
                        font_style: italic.then_some(FontStyle::Italic),
                        ..Default::default()
                    },
                )
            });
            div()
                .flex()
                .items_start()
                .gap_2()
                .when(*bullet, |row| row.pl_3().child("•"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h(px(*size * scale * 1.4))
                        .text_size(px(*size * scale))
                        .line_height(px(*size * scale * 1.4))
                        .when(*heading, |p| p.font_weight(FontWeight::SEMIBOLD))
                        .when(*alignment == "center", |p| p.text_center())
                        .when(*alignment == "right", |p| p.text_right())
                        .child(StyledText::new(text.clone()).with_highlights(highlights)),
                )
                .into_any_element()
        }
        WordBlock::TableRow(row) => div()
            .text_size(px(16. * scale))
            .flex()
            .border_t_1()
            .border_l_1()
            .border_color(theme.border)
            .children(row.iter().map(|cell| {
                div()
                    .flex_1()
                    .min_w_0()
                    .p_2()
                    .border_r_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(cell.clone())
            }))
            .into_any_element(),
        WordBlock::Image {
            image,
            width,
            ratio,
        } => div()
            .w_full()
            .child(
                img(image.clone())
                    .w(px(*width * scale))
                    .max_w_full()
                    .aspect_ratio(*ratio)
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element(),
        WordBlock::Break => div()
            .my_4()
            .border_b_1()
            .border_dashed()
            .border_color(theme.border)
            .into_any_element(),
    };
    div()
        .w_full()
        .flex()
        .justify_center()
        .px_3()
        .child(
            div()
                .w_full()
                .max_w(rems(55.))
                .px_6()
                .py_1()
                .bg(theme.background)
                .when(first, |row| row.pt_6())
                .when(last, |row| row.pb_6())
                .child(content),
        )
        .into_any_element()
}

fn remove_slide_assets(slide: &Slide, cx: &mut App) {
    for shape in &slide.shapes {
        if let Some(image) = &shape.image {
            image.clone().remove_asset(cx);
        }
    }
}

fn slide_view(slide: Arc<Slide>, width: f32, height: f32) -> AnyElement {
    canvas(
        move |bounds, window, cx| {
            let margin = px(0.);
            let scale = (f32::from((bounds.size.width - margin).max(px(1.))) / width)
                .min(f32::from((bounds.size.height - margin).max(px(1.))) / height);
            let size = size(px(width * scale), px(height * scale));
            let origin = bounds.origin
                + point(
                    (bounds.size.width - size.width) / 2.,
                    (bounds.size.height - size.height) / 2.,
                );
            let mut frame = div()
                .relative()
                .w(size.width)
                .h(size.height)
                .overflow_hidden()
                .bg(rgb(slide.background))
                .children(slide.shapes.iter().map(|shape| {
                    let [x, y, w, h] = shape.geometry;
                    let font_size = (shape.font_size * 12_700. * scale).clamp(5., 200.);
                    div()
                        .absolute()
                        .left(px(x * scale))
                        .top(px(y * scale))
                        .w(px(w * scale))
                        .h(px(h * scale))
                        .overflow_hidden()
                        .when_some(shape.fill, |shape, color| shape.bg(rgb(color)))
                        .when_some(shape.image.clone(), |shape, image| {
                            shape.child(img(image).size_full().object_fit(ObjectFit::Contain))
                        })
                        .when(!shape.text.is_empty(), |view| {
                            view.px(px(70_000. * scale))
                                .py(px(35_000. * scale))
                                .text_color(rgb(shape.text_color))
                                .text_size(px(font_size))
                                .line_height(px(font_size * 1.18))
                                .child(shape.text.clone())
                        })
                }))
                .into_any_element();
            frame.layout_as_root(size.map(AvailableSpace::Definite), window, cx);
            frame.prepaint_at(origin, window, cx);
            frame
        },
        |_, mut frame, window, cx| frame.paint(window, cx),
    )
    .size_full()
    .into_any_element()
}
