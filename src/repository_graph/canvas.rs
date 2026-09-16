//! Screen-space geometry, painting, and gestures; no persistence or domain rules.
use gpui_kit::component::{ActiveTheme as _, StyledExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, DispatchPhase, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, PathBuilder, Styled, Window, canvas, div, point,
    px,
};

use super::layout::{NODE_HEIGHT, NODE_WIDTH, Point, Viewport};
use super::{Draft, GraphPage};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Curve {
    pub points: [Point; 4],
}
impl Curve {
    pub fn between(from: Point, to: Point) -> Self {
        let start = Point::new(from.x + NODE_WIDTH / 2., from.y + NODE_HEIGHT);
        let end = Point::new(to.x + NODE_WIDTH / 2., to.y);
        Self::endpoints(start, end)
    }
    /// Bypass intervening nodes (e.g. api → cli past backend). Drawing and hit
    /// testing use exactly the same route, including the description label.
    pub fn routed(
        from: Point,
        to: Point,
        positions: &std::collections::BTreeMap<String, Point>,
    ) -> Self {
        let curve = Self::between(from, to);
        let obstacles: Vec<_> = positions
            .values()
            .copied()
            .filter(|p| *p != from && *p != to)
            .collect();
        let blocked = |curve: Self| {
            obstacles.iter().any(|p| {
                (1..48).any(|i| {
                    let q = curve.at(i as f32 / 48.);
                    q.x > p.x - 24.
                        && q.x < p.x + NODE_WIDTH + 24.
                        && q.y > p.y - 16.
                        && q.y < p.y + NODE_HEIGHT + 16.
                })
            })
        };
        if !blocked(curve) {
            return curve;
        }
        let [start, _, _, end] = curve.points;
        let side = if end.x > start.x { 1. } else { -1. };
        // Try successively wider lanes. This stays bounded even when users
        // deliberately overlap nodes; automatic layout does not overlap them.
        for distance in [240., 400., 640., 960.] {
            let lane = if side > 0. {
                start.x.max(end.x) + distance
            } else {
                start.x.min(end.x) - distance
            };
            let routed = Self {
                points: [
                    start,
                    Point::new(lane, start.y + 40.),
                    Point::new(lane, end.y - 40.),
                    end,
                ],
            };
            if !blocked(routed) {
                return routed;
            }
        }
        curve
    }
    pub fn endpoints(start: Point, end: Point) -> Self {
        let (a, b) = if end.y > start.y + 25. {
            let middle = (start.y + end.y) / 2.;
            (Point::new(start.x, middle), Point::new(end.x, middle))
        } else {
            // Return edges route outside the nodes. Reversing endpoints takes
            // the other side, keeping two-node cycles independently selectable.
            let side = if end.x >= start.x { 1. } else { -1. };
            (
                Point::new(start.x + side * 150., start.y + 150.),
                Point::new(end.x + side * 150., end.y - 150.),
            )
        };
        Self {
            points: [start, a, b, end],
        }
    }
    pub fn at(self, t: f32) -> Point {
        let u = 1. - t;
        let weights = [u * u * u, 3. * u * u * t, 3. * u * t * t, t * t * t];
        self.points
            .iter()
            .zip(weights)
            .fold(Point::default(), |p, (q, w)| {
                Point::new(p.x + q.x * w, p.y + q.y * w)
            })
    }
    pub fn screen(self, view: Viewport) -> Self {
        Self {
            points: self.points.map(|p| view.screen(p)),
        }
    }
    pub fn hit(self, point: Point) -> bool {
        (0..64).any(|i| {
            segment_distance(
                point,
                self.at(i as f32 / 64.),
                self.at((i + 1) as f32 / 64.),
            ) <= 9.
        })
    }
}
fn segment_distance(p: Point, a: Point, b: Point) -> f32 {
    let (x, y) = (b.x - a.x, b.y - a.y);
    let t = (((p.x - a.x) * x + (p.y - a.y) * y) / (x * x + y * y).max(0.0001)).clamp(0., 1.);
    p.distance(Point::new(a.x + t * x, a.y + t * y))
}

#[derive(Clone, Debug)]
pub(super) enum Gesture {
    Node {
        key: String,
        pointer: Point,
        initial: Point,
    },
    Pan {
        pointer: Point,
        initial: Point,
    },
    Connect {
        from: String,
        to: Option<String>,
        pointer: Point,
    },
    Rewire {
        id: String,
        provider: bool,
        from: String,
        to: String,
        pointer: Point,
    },
}

pub(super) fn local(page: &GraphPage, p: gpui_kit::Point<gpui_kit::Pixels>) -> Point {
    let bounds = page.bounds.get();
    Point::new(
        f32::from(p.x - bounds.origin.x),
        f32::from(p.y - bounds.origin.y),
    )
}

fn paint_curve(
    curve: Curve,
    color: gpui_kit::Hsla,
    selected: bool,
    origin: gpui_kit::Point<gpui_kit::Pixels>,
    window: &mut Window,
) {
    let [start, a, b, end] = curve.points.map(|p| origin + point(px(p.x), px(p.y)));
    let mut path = PathBuilder::stroke(px(if selected { 3. } else { 1.75 }));
    path.move_to(start);
    path.cubic_bezier_to(end, a, b);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
    let tangent = curve.at(0.98);
    let end_p = curve.points[3];
    let angle = (end_p.y - tangent.y).atan2(end_p.x - tangent.x);
    let mut arrow = PathBuilder::stroke(px(2.));
    for offset in [-0.5f32, 0.5] {
        arrow.move_to(end);
        arrow.line_to(
            end - point(
                px(10. * (angle + offset).cos()),
                px(10. * (angle + offset).sin()),
            ),
        );
    }
    if let Ok(path) = arrow.build() {
        window.paint_path(path, color);
    }
}

pub(super) fn render(page: &GraphPage, cx: &mut Context<GraphPage>) -> impl IntoElement {
    let nodes = page.visible_nodes(cx);
    let keys: std::collections::BTreeSet<_> = nodes.iter().map(|n| n.key().to_owned()).collect();
    let view = page.layout().viewport;
    let positions = &page.layout().positions;
    let mut curves = Vec::new();
    let mut labels = Vec::new();
    if let Some(graph) = &page.graph {
        for edge in &graph.relationships {
            if !keys.contains(&edge.from) || !keys.contains(&edge.to) {
                continue;
            }
            let (Some(from), Some(to)) = (positions.get(&edge.from), positions.get(&edge.to))
            else {
                continue;
            };
            let selected =
                matches!(&page.draft, Some(Draft::Edge { id: Some(id), .. }) if id == &edge.id);
            let curve = Curve::routed(*from, *to, positions).screen(view);
            curves.push((curve, selected, false));
            labels.push((
                edge.id.clone(),
                edge.description.clone(),
                curve.at(0.5),
                selected,
            ));
        }
    }
    // Inspector endpoint edits and pointer gestures preview without a write.
    if let Some(curve) = page.preview_curve(cx) {
        curves.push((curve.screen(view), true, true));
    }
    let bounds_cell = page.bounds.clone();
    let weak = cx.entity().downgrade();
    let accent = cx.theme().primary;
    let line = cx.theme().muted_foreground;
    let mut content = div()
        .id("repository-graph-canvas")
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(cx.theme().background)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|page, event: &MouseDownEvent, window, cx| {
                page.focus_handle.focus(window, cx);
                let p = local(page, event.position);
                if let Some(id) = page.hit_edge(p, cx) {
                    page.select_edge(&id, None, window, cx);
                } else {
                    page.gesture = Some(Gesture::Pan {
                        pointer: p,
                        initial: page.layout().viewport.offset,
                    });
                }
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|page, event: &MouseDownEvent, _, cx| {
                page.gesture = Some(Gesture::Pan {
                    pointer: local(page, event.position),
                    initial: page.layout().viewport.offset,
                });
                cx.notify();
            }),
        )
        .on_scroll_wheel(
            cx.listener(|page, event: &gpui_kit::ScrollWheelEvent, _, cx| {
                if page.gesture.is_some() {
                    return;
                }
                let p = local(page, event.position);
                let delta = event.delta.pixel_delta(px(20.));
                page.layout_mut()
                    .viewport
                    .zoom_at(p, (-f32::from(delta.y) * 0.002).exp());
                page.schedule_layout_save(cx);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .child(
            canvas(
                move |bounds, _, _| bounds_cell.set(bounds),
                move |bounds, _, window, _| {
                    for (curve, selected, preview) in curves {
                        paint_curve(
                            curve,
                            if selected || preview {
                                accent
                            } else {
                                line.opacity(0.65)
                            },
                            selected,
                            bounds.origin,
                            window,
                        );
                    }
                    let weak_move = weak.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ = weak_move
                                .update(cx, |page, cx| page.pointer_move(event, window, cx));
                        }
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ = weak.update(cx, |page, cx| page.pointer_up(event, window, cx));
                        }
                    });
                },
            )
            .absolute()
            .size_full(),
        );
    for (id, description, p, selected) in labels {
        let key_id = id.clone();
        content = content.child(
            div()
                .id(format!("edge-{id}"))
                .absolute()
                .left(px(p.x - 75.))
                .top(px(p.y - 10.))
                .w(px(150.))
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if selected { accent } else { cx.theme().border })
                .bg(cx.theme().background)
                .text_color(cx.theme().muted_foreground)
                .text_size(px(11.))
                .truncate()
                .cursor_pointer()
                .tab_index(0)
                .focus(|style| style.border_color(accent))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |page, _, window, cx| {
                        page.select_edge(&id, None, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_key_down(cx.listener(
                    move |page, event: &gpui_kit::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "enter" {
                            page.select_edge(&key_id, None, window, cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .child(description),
        );
    }
    for node in nodes {
        let Some(pos) = positions.get(node.key()).copied() else {
            continue;
        };
        let pos = view.screen(pos);
        let key = node.key().to_owned();
        let click_key = key.clone();
        let keyboard_key = key.clone();
        let selected =
            matches!(&page.draft, Some(Draft::Node { key: selected, .. }) if selected == &key);
        let target = page.target_key(cx).as_deref() == Some(&key);
        let hidden = page.hidden_count(&key, &keys);
        let preview = node
            .repository
            .description
            .as_deref()
            .unwrap_or("Add a repository description");
        let preview: String = preview.chars().take(95).collect();
        let mut body = v_flex()
            .id(format!("graph-node-{key}"))
            .absolute()
            .left(px(pos.x))
            .top(px(pos.y))
            .w(px(NODE_WIDTH * view.zoom))
            .h(px(NODE_HEIGHT * view.zoom))
            .p(px(12. * view.zoom))
            .gap(px(5. * view.zoom))
            .rounded_md()
            .border_2()
            .border_color(if selected || target {
                accent
            } else {
                cx.theme().border
            })
            .bg(cx.theme().secondary)
            .cursor_pointer()
            .tab_index(0)
            .focus(|style| style.border_color(accent))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |page, event: &MouseDownEvent, window, cx| {
                    page.select_node(&click_key, window, cx);
                    page.focus_handle.focus(window, cx);
                    let pointer = page.layout().viewport.world(local(page, event.position));
                    if let Some(initial) = page.layout().positions.get(&click_key).copied() {
                        page.gesture = Some(Gesture::Node {
                            key: click_key.clone(),
                            pointer,
                            initial,
                        });
                    }
                    cx.stop_propagation();
                }),
            )
            .on_key_down(
                cx.listener(move |page, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "enter" {
                        page.select_node(&keyboard_key, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .font_semibold()
                    .text_size(px((14. * view.zoom).max(10.)))
                    .truncate()
                    .child(node.label().to_owned()),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px((11. * view.zoom).max(9.)))
                    .overflow_hidden()
                    .flex_1()
                    .child(preview),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px((10. * view.zoom).max(8.)))
                    .truncate()
                    .child(if node.unresolved {
                        "Missing repository · cleanup available".into()
                    } else if hidden > 0 {
                        format!("{hidden} hidden connections · Show in All")
                    } else if node.repository.checkout_missing {
                        "Checkout missing".into()
                    } else if !node.repository.is_linked() {
                        "Not linked on this device".into()
                    } else {
                        node.repository.group.clone().unwrap_or_default()
                    }),
            );
        for output in [false, true] {
            let handle_key = key.clone();
            body = body.child(
                div()
                    .id(format!(
                        "{}-{}",
                        if output { "output" } else { "input" },
                        key
                    ))
                    .absolute()
                    .left(px(NODE_WIDTH * view.zoom / 2. - 8.))
                    .w(px(16.))
                    .h(px(16.))
                    .rounded_full()
                    .bg(if target {
                        accent
                    } else {
                        cx.theme().background
                    })
                    .border_2()
                    .border_color(accent)
                    .when(output, |el| el.bottom(px(-8.)))
                    .when(!output, |el| el.top(px(-8.)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |page, event: &MouseDownEvent, window, cx| {
                            page.start_handle(
                                &handle_key,
                                output,
                                local(page, event.position),
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        content = content.child(body);
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_dependencies_bypass_intermediate_nodes() {
        let positions = [
            ("api".into(), Point::new(0., 0.)),
            ("backend".into(), Point::new(0., 240.)),
            ("cli".into(), Point::new(0., 480.)),
        ]
        .into_iter()
        .collect();
        let curve = Curve::routed(Point::new(0., 0.), Point::new(0., 480.), &positions);
        for i in 1..100 {
            let p = curve.at(i as f32 / 100.);
            assert!(!(p.x > 0. && p.x < NODE_WIDTH && p.y > 240. && p.y < 240. + NODE_HEIGHT));
        }
        assert!(curve.at(0.5).x < -24.);
    }

    #[test]
    fn hit_targets_remain_screen_sized_and_cycles_have_distinct_paths() {
        let a = Point::new(0., 0.);
        let b = Point::new(300., 240.);
        let curve = Curve::between(a, b);
        for zoom in [0.25, 1., 2.5] {
            let curve = curve.screen(Viewport {
                zoom,
                ..Default::default()
            });
            let p = curve.at(0.5);
            assert!(curve.hit(Point::new(p.x + 5., p.y)));
            assert!(!curve.hit(Point::new(-500., -500.)));
        }
        let reverse = Curve::between(b, a);
        assert!(curve.at(0.5).distance(reverse.at(0.5)) > 20.);
    }
}
