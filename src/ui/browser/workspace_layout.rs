use super::*;
use serde::{Deserialize, Serialize};

const SIDEBAR_MIN: f32 = 14.;
const MAIN_MIN: f32 = 20.;
const PANE_MIN: f32 = 0.16;
const PANE_MAX: f32 = 0.75;

fn sidebar_max(window: &Window) -> f32 {
    (f32::from(window.viewport_size().width) / f32::from(window.rem_size()) - MAIN_MIN)
        .max(SIDEBAR_MIN)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct WorkspaceSizes {
    sidebar: f32,
    tasks: f32,
    output: f32,
}

impl Default for WorkspaceSizes {
    fn default() -> Self {
        Self {
            sidebar: 17.,
            tasks: 0.37,
            output: 0.30,
        }
    }
}

impl WorkspaceSizes {
    pub(super) fn load() -> Self {
        let Ok(path) = ConnectionSettings::path() else {
            return Self::default();
        };
        std::fs::read(path.with_file_name("workspace-layout.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(self) -> Result<()> {
        write_settings(
            &ConnectionSettings::path()?.with_file_name("workspace-layout.json"),
            &self,
        )
    }

    fn resolved(self, sidebar_max: f32) -> Self {
        Self {
            sidebar: self
                .sidebar
                .clamp(SIDEBAR_MIN.min(sidebar_max), sidebar_max),
            tasks: self.tasks.clamp(PANE_MIN, PANE_MAX),
            output: self.output.clamp(PANE_MIN, PANE_MAX),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ResizeTarget {
    sidebar: bool,
    tasks: bool,
    output: bool,
}

impl ResizeTarget {
    const SIDEBAR: Self = Self {
        sidebar: true,
        tasks: false,
        output: false,
    };
    const TASKS: Self = Self {
        sidebar: false,
        tasks: true,
        output: false,
    };
    const OUTPUT: Self = Self {
        sidebar: false,
        tasks: false,
        output: true,
    };

    fn cursor(self) -> CursorStyle {
        match (self.sidebar, self.tasks || self.output) {
            (true, true) => CursorStyle::Crosshair,
            (true, false) => CursorStyle::ResizeLeftRight,
            _ => CursorStyle::ResizeUpDown,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct WorkspaceDrag {
    target: ResizeTarget,
    start: Point<Pixels>,
    initial: WorkspaceSizes,
    original: WorkspaceSizes,
    touch: bool,
}

impl WorkspaceDrag {
    fn sizes(
        self,
        position: Point<Pixels>,
        rem: Pixels,
        height: Pixels,
        sidebar_max: f32,
    ) -> WorkspaceSizes {
        let mut next = self.original;
        let delta = position - self.start;
        if self.target.sidebar {
            next.sidebar = (self.initial.sidebar + f32::from(delta.x) / f32::from(rem))
                .clamp(SIDEBAR_MIN.min(sidebar_max), sidebar_max);
        }
        let change = f32::from(delta.y) / f32::from(height);
        if self.target.tasks {
            next.tasks = (self.initial.tasks - change).clamp(PANE_MIN, PANE_MAX);
        }
        if self.target.output {
            next.output = (self.initial.output - change).clamp(PANE_MIN, PANE_MAX);
        }
        next
    }
}

struct WorkspaceDivider {
    target: ResizeTarget,
    hitbox: Hitbox,
    marks: [Option<Bounds<Pixels>>; 2],
}

fn workspace_dividers(
    bounds: Bounds<Pixels>,
    sidebar: Pixels,
    tasks: Option<f32>,
    output: Option<f32>,
    window: &mut Window,
) -> Vec<WorkspaceDivider> {
    let left = bounds.origin.x;
    let top = bounds.origin.y;
    let right = left + bounds.size.width;
    let bottom = top + bounds.size.height;
    let split = left + sidebar;
    let grab = window.rem_size() * 0.32;
    let junction = grab * 1.5;
    let line = px(2.);
    let rect = |x, y, w, h| Bounds::new(point(x, y), size(w, h));
    let mut dividers = Vec::with_capacity(5);
    let mut add = |target, area, marks| {
        dividers.push(WorkspaceDivider {
            target,
            hitbox: window.insert_hitbox(area, HitboxBehavior::BlockMouse),
            marks,
        });
    };
    let vertical = rect(split - line / 2., top, line, bounds.size.height);
    add(
        ResizeTarget::SIDEBAR,
        rect(split - grab, top, grab * 2., bounds.size.height),
        [Some(vertical), None],
    );
    let task_y = tasks.map(|height| bottom - bounds.size.height * height);
    let output_y = output.map(|height| bottom - bounds.size.height * height);
    let mut joints = Vec::with_capacity(2);
    for (y, x, width, target) in [
        (task_y, left, sidebar, ResizeTarget::TASKS),
        (output_y, split, right - split, ResizeTarget::OUTPUT),
    ] {
        if let Some(y) = y {
            let horizontal = rect(x, y - line / 2., width, line);
            add(
                target,
                rect(x, y - grab, width, grab * 2.),
                [Some(horizontal), None],
            );
            joints.push((y, target, horizontal));
        }
    }
    if let [(task_y, _, tasks), (output_y, _, output)] = joints.as_slice()
        && (*task_y - *output_y).abs() <= px(1.)
    {
        add(
            ResizeTarget {
                sidebar: true,
                tasks: true,
                output: true,
            },
            rect(
                split - junction,
                *task_y - junction,
                junction * 2.,
                junction * 2.,
            ),
            [Some(*tasks), Some(*output)],
        );
    } else {
        for (y, target, horizontal) in joints {
            // Give close, distinct intersections their own half of the sidebar edge.
            let (x, width) = if task_y
                .zip(output_y)
                .is_some_and(|(a, b)| (a - b).abs() < junction * 2.)
            {
                if target.tasks {
                    (split - junction, junction)
                } else {
                    (split, junction)
                }
            } else {
                (split - junction, junction * 2.)
            };
            add(
                ResizeTarget {
                    sidebar: true,
                    ..target
                },
                rect(x, y - junction, width, junction * 2.),
                [Some(vertical), Some(horizontal)],
            );
        }
    }
    dividers
}

impl Browser {
    pub(super) fn narrow_workspace_sidebar(&self, window: &Window) -> bool {
        self.workspace_sizes.resolved(sidebar_max(window)).sidebar < 16.
    }

    pub(super) fn split_workspace(
        &self,
        sidebar: AnyElement,
        main: AnyElement,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let rem = window.rem_size();
        let sidebar_max = sidebar_max(window);
        let sizes = self.workspace_sizes.resolved(sidebar_max);
        let tasks = (self.show_tasks && !self.tasks_maximized).then_some(sizes.tasks);
        let output = self.output_state.is_visible().then_some(sizes.output);
        let side_width = rems(sizes.sidebar);
        let owner = cx.weak_entity();
        let original = self.workspace_sizes;
        let active = self.workspace_drag.map(|drag| drag.target);
        let highlight = cx.theme().primary;
        div()
            .relative()
            .flex()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .w(side_width)
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(sidebar)
                    .when_some(tasks, |side, height| {
                        side.child(
                            div()
                                .h(relative(height))
                                .flex_shrink_0()
                                .min_h_0()
                                .overflow_hidden()
                                .child(self.task_panel(false, cx)),
                        )
                    }),
            )
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(main),
                    )
                    .when_some(output, |main, height| {
                        main.child(
                            div()
                                .h(relative(height))
                                .flex_shrink_0()
                                .min_h_0()
                                .overflow_hidden()
                                .child(self.output_panel(cx)),
                        )
                    }),
            )
            .child(
                canvas(
                    move |bounds, window, _| {
                        workspace_dividers(bounds, side_width.to_pixels(rem), tasks, output, window)
                    },
                    move |bounds, dividers, window, _| {
                        let hovered = dividers
                            .iter()
                            .rev()
                            .find(|item| item.hitbox.is_hovered(window))
                            .map(|item| item.target);
                        for item in &dividers {
                            window.set_cursor_style(item.target.cursor(), &item.hitbox);
                            let marked = active.or(hovered).is_some_and(|target| {
                                item.target == target
                                    || (target.sidebar && item.target == ResizeTarget::SIDEBAR)
                            });
                            if marked {
                                for mark in item.marks.iter().flatten() {
                                    window.paint_quad(fill(*mark, highlight));
                                }
                            }
                        }
                        if let Some(target) = active {
                            window.set_window_cursor_style(target.cursor());
                        }

                        let mouse_owner = owner.clone();
                        let mouse_dividers = dividers
                            .iter()
                            .map(|item| (item.target, item.hitbox.clone()))
                            .collect::<Vec<_>>();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                            if !phase.bubble()
                                || event.button != MouseButton::Left
                                || window.default_prevented()
                            {
                                return;
                            }
                            let Some((target, _)) = mouse_dividers
                                .iter()
                                .rev()
                                .find(|(_, hitbox)| hitbox.is_hovered(window))
                            else {
                                return;
                            };
                            let _ = mouse_owner.update(cx, |this, cx| {
                                this.workspace_drag = Some(WorkspaceDrag {
                                    target: *target,
                                    start: event.position,
                                    initial: sizes,
                                    original,
                                    touch: false,
                                });
                                cx.notify();
                            });
                            window.prevent_default();
                            cx.stop_propagation();
                        });
                        let mouse_owner = owner.clone();
                        let mouse_dividers = dividers
                            .iter()
                            .map(|item| (item.target, item.hitbox.clone()))
                            .collect::<Vec<_>>();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let handled = mouse_owner
                                .update(cx, |this, cx| {
                                    let Some(drag) = this.workspace_drag.filter(|drag| !drag.touch)
                                    else {
                                        return false;
                                    };
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.move_workspace_divider(
                                            drag,
                                            event.position,
                                            rem,
                                            bounds.size.height,
                                            sidebar_max,
                                            cx,
                                        );
                                    } else {
                                        this.finish_workspace_drag(cx);
                                    }
                                    true
                                })
                                .unwrap_or(false);
                            if handled {
                                cx.stop_propagation();
                            } else if hovered
                                != mouse_dividers
                                    .iter()
                                    .rev()
                                    .find(|(_, hitbox)| hitbox.is_hovered(window))
                                    .map(|(target, _)| *target)
                            {
                                window.refresh();
                            }
                        });
                        let mouse_owner = owner.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture && event.button == MouseButton::Left
                            {
                                let handled = mouse_owner
                                    .update(cx, |this, cx| {
                                        if let Some(drag) =
                                            this.workspace_drag.filter(|drag| !drag.touch)
                                        {
                                            this.move_workspace_divider(
                                                drag,
                                                event.position,
                                                rem,
                                                bounds.size.height,
                                                sidebar_max,
                                                cx,
                                            );
                                            this.finish_workspace_drag(cx);
                                            return true;
                                        }
                                        false
                                    })
                                    .unwrap_or(false);
                                if handled {
                                    cx.stop_propagation();
                                }
                            }
                        });
                        let owner = owner.clone();
                        window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                            if !phase.bubble() {
                                return;
                            }
                            if event.phase == TouchPhase::Started {
                                let Some(item) = dividers
                                    .iter()
                                    .rev()
                                    .find(|item| item.hitbox.is_hovered(window))
                                else {
                                    return;
                                };
                                if window.default_prevented() {
                                    return;
                                }
                                window.prevent_default();
                                let _ = owner.update(cx, |this, cx| {
                                    this.workspace_drag = Some(WorkspaceDrag {
                                        target: item.target,
                                        start: event.start_position,
                                        initial: sizes,
                                        original,
                                        touch: true,
                                    });
                                    cx.notify();
                                });
                                return;
                            }
                            let _ = owner.update(cx, |this, cx| {
                                let Some(drag) = this.workspace_drag.filter(|drag| drag.touch)
                                else {
                                    return;
                                };
                                if event.phase != TouchPhase::Cancelled {
                                    this.move_workspace_divider(
                                        drag,
                                        event.position,
                                        rem,
                                        bounds.size.height,
                                        sidebar_max,
                                        cx,
                                    );
                                }
                                if event.phase != TouchPhase::Moved {
                                    this.finish_workspace_drag(cx);
                                }
                            });
                        });
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }

    fn move_workspace_divider(
        &mut self,
        drag: WorkspaceDrag,
        position: Point<Pixels>,
        rem: Pixels,
        height: Pixels,
        sidebar_max: f32,
        cx: &mut Context<Self>,
    ) {
        if height <= px(0.) {
            return;
        }
        let next = drag.sizes(position, rem, height, sidebar_max);
        if self.workspace_sizes != next {
            self.workspace_sizes = next;
            cx.notify();
        }
    }

    fn finish_workspace_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.workspace_drag.take() {
            if self.workspace_sizes != drag.original
                && let Err(error) = self.workspace_sizes.save()
            {
                self.log(true, format!("无法保存面板布局：{error:#}"));
            }
            cx.notify();
        }
    }
}
