//! Global native repository map. Drafts retain their read revision through polling
//! and failed saves. All filesystem work runs on the background executor.
mod canvas;
mod layout;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use std::{cell::Cell, collections::BTreeSet, rc::Rc, time::Duration};

use crate::data::{
    DataRoot,
    relationships::{self, Graph, Mutation, Node, Query},
};
use canvas::{Curve, Gesture};
use layout::{GroupLayout, Layouts, NODE_HEIGHT, NODE_WIDTH, Point};

#[derive(Clone, Debug)]
enum Draft {
    Node {
        key: String,
        revision: String,
    },
    Edge {
        id: Option<String>,
        revision: String,
        anchor: Option<Point>,
    },
}

pub(crate) enum GraphEvent {
    AddRepository,
    OpenRepository { key: String, label: String },
    MetadataChanged,
}

pub(crate) struct GraphPage {
    pub(crate) focus_handle: FocusHandle,
    root: Option<DataRoot>,
    active: bool,
    graph: Option<Graph>,
    layouts: Layouts,
    layouts_loaded: bool,
    loading: bool,
    reload_pending: bool,
    generation: u64,
    saving: bool,
    layout_saving: bool,
    layout_dirty: bool,
    layout_debounce: u64,
    error: Option<String>,
    load_error: Option<String>,
    draft: Option<Draft>,
    search: Entity<InputState>,
    from: Entity<InputState>,
    to: Entity<InputState>,
    description: Entity<TextareaState>,
    group: Entity<InputState>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    gesture: Option<Gesture>,
}
impl EventEmitter<GraphEvent> for GraphPage {}

impl GraphPage {
    pub(crate) fn new(root: Option<DataRoot>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search repositories…"));
        let from = cx.new(|cx| InputState::new(window, cx).placeholder("Provider repository key"));
        let to = cx.new(|cx| InputState::new(window, cx).placeholder("Consumer repository key"));
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .placeholder("Describe what the consumer uses from the provider")
        });
        let group = cx
            .new(|cx| InputState::new(window, cx).placeholder("Personal, Work, or a custom group"));
        for input in [&search, &from, &to] {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }
        let focus_handle = cx.focus_handle();
        cx.on_focus_out(&focus_handle, window, |page, _, _, cx| {
            page.cancel_gesture(cx)
        })
        .detach();
        cx.observe_window_activation(window, |page, window, cx| {
            if !window.is_window_active() {
                page.cancel_gesture(cx);
            }
        })
        .detach();
        cx.spawn(async move |page, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if page
                    .update(cx, |page, cx| {
                        if page.active {
                            page.refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut layouts = Layouts::default();
        layouts.groups.insert("All".into(), GroupLayout::default());
        Self {
            focus_handle,
            root,
            active: false,
            graph: None,
            layouts,
            layouts_loaded: false,
            loading: false,
            reload_pending: false,
            generation: 0,
            saving: false,
            layout_saving: false,
            layout_dirty: false,
            layout_debounce: 0,
            error: None,
            load_error: None,
            draft: None,
            search,
            from,
            to,
            description,
            group,
            bounds: Rc::new(Cell::new(Bounds::default())),
            gesture: None,
        }
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.refresh(cx);
        } else {
            self.cancel_gesture(cx);
        }
        cx.notify();
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.saving || self.gesture.is_some() {
            self.reload_pending = true;
            return;
        }
        let Some(root) = self.root.clone() else {
            self.load_error = Some("Portable data is unavailable".into());
            return;
        };
        self.loading = true;
        self.reload_pending = false;
        self.generation += 1;
        let generation = self.generation;
        let load_layouts = !self.layouts_loaded;
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_spawn(async move {
                    let graph = relationships::load(&root, Query::default())
                        .map_err(|e| format!("{e:#}"))?;
                    let layouts =
                        load_layouts.then(|| Layouts::load(&root).map_err(|e| format!("{e:#}")));
                    Ok::<_, String>((graph, layouts))
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                page.loading = false;
                if page.generation == generation {
                    match result {
                        Ok((graph, layouts)) => {
                            page.graph = Some(graph);
                            page.load_error = None;
                            if let Some(layouts) = layouts {
                                page.layouts_loaded = true;
                                match layouts {
                                    Ok(layouts) => page.layouts = layouts,
                                    Err(error) => page.error = Some(error),
                                }
                            }
                            page.ensure_positions();
                        }
                        Err(error) => page.load_error = Some(error),
                    }
                }
                if page.reload_pending && !page.saving {
                    page.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn layout(&self) -> &GroupLayout {
        &self.layouts.groups[&self.layouts.last_group]
    }
    fn layout_mut(&mut self) -> &mut GroupLayout {
        self.layouts
            .groups
            .entry(self.layouts.last_group.clone())
            .or_default()
    }
    fn ensure_positions(&mut self) {
        let keys: Vec<_> = self
            .graph
            .as_ref()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter(|n| {
                        relationships::group_matches(
                            n.repository.group.as_deref(),
                            Some(&self.layouts.last_group),
                        )
                    })
                    .map(|n| n.key().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let edges = self
            .graph
            .as_ref()
            .map(|g| g.relationships.clone())
            .unwrap_or_default();
        self.layout_mut().include_new(&keys, &edges);
    }
    fn visible_nodes(&self, cx: &gpui_kit::App) -> Vec<Node> {
        let search = self.search.read(cx).value().to_lowercase();
        self.graph
            .as_ref()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter(|node| {
                        relationships::group_matches(
                            node.repository.group.as_deref(),
                            Some(&self.layouts.last_group),
                        ) && (search.is_empty()
                            || format!(
                                "{} {} {}",
                                node.key(),
                                node.label(),
                                node.repository.description.as_deref().unwrap_or("")
                            )
                            .to_lowercase()
                            .contains(&search))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    fn set_group(&mut self, group: String, cx: &mut Context<Self>) {
        self.cancel_gesture(cx);
        self.layouts.last_group = group;
        self.ensure_positions();
        self.persist_layout(cx);
        cx.notify();
    }
    fn groups(&self) -> Vec<String> {
        let mut groups = BTreeSet::new();
        if let Some(graph) = &self.graph {
            for node in &graph.nodes {
                if let Some(group) = &node.repository.group
                    && !group.eq_ignore_ascii_case("Personal")
                    && !group.eq_ignore_ascii_case("Work")
                {
                    groups.insert(group.clone());
                }
            }
        }
        let mut choices = vec!["All".into(), "Personal".into(), "Work".into()];
        choices.extend(
            groups
                .into_iter()
                .filter(|g| g != "All" && g != "Ungrouped"),
        );
        choices.push("Ungrouped".into());
        choices
    }
    fn hidden_count(&self, key: &str, visible: &BTreeSet<String>) -> usize {
        self.graph
            .as_ref()
            .map(|graph| {
                graph
                    .relationships
                    .iter()
                    .filter(|e| {
                        (e.from == key && !visible.contains(&e.to))
                            || (e.to == key && !visible.contains(&e.from))
                    })
                    .count()
            })
            .unwrap_or(0)
    }
    fn fit(&mut self, cx: &mut Context<Self>) {
        let mut points: Vec<_> = self
            .visible_nodes(cx)
            .iter()
            .filter_map(|node| self.layout().positions.get(node.key()).copied())
            .collect();
        let visible: BTreeSet<_> = self
            .visible_nodes(cx)
            .iter()
            .map(|n| n.key().to_owned())
            .collect();
        if let Some(graph) = &self.graph {
            for edge in &graph.relationships {
                if visible.contains(&edge.from) && visible.contains(&edge.to) {
                    let positions = &self.layout().positions;
                    if let (Some(from), Some(to)) =
                        (positions.get(&edge.from), positions.get(&edge.to))
                    {
                        let curve = Curve::routed(*from, *to, positions);
                        points.extend((0..=16).map(|i| curve.at(i as f32 / 16.)));
                    }
                }
            }
        }
        let size = self.bounds.get().size;
        self.layout_mut().viewport.fit(
            points.into_iter(),
            Point::new(size.width.into(), size.height.into()),
        );
        self.persist_layout(cx);
        cx.notify();
    }
    fn arrange(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<_> = self
            .graph
            .as_ref()
            .into_iter()
            .flat_map(|g| &g.nodes)
            .filter(|n| {
                relationships::group_matches(
                    n.repository.group.as_deref(),
                    Some(&self.layouts.last_group),
                )
            })
            .map(|n| n.key().to_owned())
            .collect();
        let edges = self
            .graph
            .as_ref()
            .map(|g| g.relationships.as_slice())
            .unwrap_or_default();
        let positions = layout::arrange(&keys, edges);
        self.layout_mut().positions = positions;
        self.fit(cx);
    }
    fn schedule_layout_save(&mut self, cx: &mut Context<Self>) {
        self.layout_debounce += 1;
        let generation = self.layout_debounce;
        cx.spawn(async move |page, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = page.update(cx, |page, cx| {
                if page.layout_debounce == generation {
                    page.persist_layout(cx);
                }
            });
        })
        .detach();
    }
    fn persist_layout(&mut self, cx: &mut Context<Self>) {
        if self.layout_saving || self.gesture.is_some() {
            self.layout_dirty = true;
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let layouts = self.layouts.clone();
        self.layout_saving = true;
        self.layout_dirty = false;
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_spawn(async move { layouts.save(&root) })
                .await;
            let _ = page.update(cx, |page, cx| {
                page.layout_saving = false;
                if let Err(error) = result {
                    page.error = Some(format!("Could not save local layout: {error:#}"));
                    cx.notify();
                }
                if page.layout_dirty {
                    page.persist_layout(cx);
                }
            });
        })
        .detach();
    }

    fn select_node(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if matches!(&self.draft, Some(Draft::Node { key: current, .. }) if current == key) {
            return;
        }
        let Some(node) = self
            .graph
            .as_ref()
            .and_then(|g| g.nodes.iter().find(|n| n.key() == key))
            .cloned()
        else {
            return;
        };
        self.description.update(cx, |input, cx| {
            input.set_value(
                node.repository.description.clone().unwrap_or_default(),
                window,
                cx,
            )
        });
        self.group.update(cx, |input, cx| {
            input.set_value(
                node.repository.group.clone().unwrap_or_default(),
                window,
                cx,
            )
        });
        self.draft = Some(Draft::Node {
            key: key.into(),
            revision: node.repository.revision,
        });
        self.error = None;
        cx.notify();
    }
    fn select_edge(
        &mut self,
        id: &str,
        anchor: Option<Point>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        if matches!(&self.draft, Some(Draft::Edge { id: Some(current), .. }) if current == id) {
            return;
        }
        let Some((edge, revision)) = self.graph.as_ref().and_then(|g| {
            g.relationships
                .iter()
                .find(|e| e.id == id)
                .map(|e| (e.clone(), g.revision.clone()))
        }) else {
            return;
        };
        self.from
            .update(cx, |input, cx| input.set_value(edge.from, window, cx));
        self.to
            .update(cx, |input, cx| input.set_value(edge.to, window, cx));
        self.description.update(cx, |input, cx| {
            input.set_value(edge.description, window, cx)
        });
        self.draft = Some(Draft::Edge {
            id: Some(id.into()),
            revision,
            anchor,
        });
        self.error = None;
        cx.notify();
    }
    fn new_edge(
        &mut self,
        from: String,
        to: String,
        anchor: Option<Point>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        if let Some(id) = self
            .graph
            .as_ref()
            .and_then(|g| {
                g.relationships
                    .iter()
                    .find(|e| e.from == from && e.to == to)
            })
            .map(|e| e.id.clone())
        {
            self.select_edge(&id, anchor, window, cx);
            self.error = Some(
                "This connection already exists. Edit its description or endpoints below.".into(),
            );
            return;
        }
        let Some(graph) = &self.graph else {
            return;
        };
        self.draft = Some(Draft::Edge {
            id: None,
            revision: graph.revision.clone(),
            anchor,
        });
        self.from
            .update(cx, |input, cx| input.set_value(from, window, cx));
        self.to
            .update(cx, |input, cx| input.set_value(to, window, cx));
        self.description
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        cx.notify();
    }
    fn reload_draft_revision(&mut self, cx: &mut Context<Self>) {
        if let Some(graph) = &self.graph {
            match &mut self.draft {
                Some(Draft::Node { key, revision }) => {
                    if let Some(node) = graph.nodes.iter().find(|n| n.key() == key) {
                        *revision = node.repository.revision.clone();
                    }
                }
                Some(Draft::Edge { revision, .. }) => *revision = graph.revision.clone(),
                None => {}
            }
        }
        self.error =
            Some("Draft kept. Review the current stored value below before saving over it.".into());
        cx.notify();
    }
    fn save(&mut self, delete: bool, cx: &mut Context<Self>) {
        if self.saving || self.load_error.is_some() {
            return;
        }
        let (Some(root), Some(draft)) = (self.root.clone(), self.draft.clone()) else {
            return;
        };
        let description = self.description.read(cx).value().to_string();
        let group = self.group.read(cx).value().to_string();
        let from = self.from.read(cx).value().trim().to_owned();
        let to = self.to.read(cx).value().trim().to_owned();
        let node_edit = matches!(draft, Draft::Node { .. });
        self.saving = true;
        self.generation += 1; // A read begun before this mutation cannot win later.
        self.error = None;
        cx.notify();
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_spawn(async move {
                    match draft {
                        Draft::Node { key, revision } => {
                            crate::data::patch_repository_purpose(
                                &root,
                                &key,
                                description,
                                group,
                                &revision,
                            )?;
                        }
                        Draft::Edge { id, revision, .. } => {
                            let mutation = match (id, delete) {
                                (Some(id), true) => Mutation::Delete { id },
                                (Some(id), false) => Mutation::Update {
                                    id,
                                    from: Some(from),
                                    to: Some(to),
                                    description: Some(description),
                                },
                                (None, _) => Mutation::Create {
                                    from,
                                    to,
                                    description,
                                },
                            };
                            relationships::mutate(&root, &revision, mutation)?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                page.saving = false;
                match result {
                    Ok(()) => {
                        page.draft = None;
                        if node_edit {
                            cx.emit(GraphEvent::MetadataChanged);
                        }
                    }
                    Err(error) => page.error = Some(format!("{error:#}")),
                }
                page.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn hit_edge(&self, p: Point, cx: &gpui_kit::App) -> Option<String> {
        let visible: BTreeSet<_> = self
            .visible_nodes(cx)
            .iter()
            .map(|n| n.key().to_owned())
            .collect();
        self.graph
            .as_ref()?
            .relationships
            .iter()
            .rev()
            .find(|edge| {
                if !visible.contains(&edge.from) || !visible.contains(&edge.to) {
                    return false;
                }
                let positions = &self.layout().positions;
                let (Some(from), Some(to)) = (positions.get(&edge.from), positions.get(&edge.to))
                else {
                    return false;
                };
                Curve::routed(*from, *to, positions)
                    .screen(self.layout().viewport)
                    .hit(p)
            })
            .map(|e| e.id.clone())
    }
    fn hit_handle(&self, p: Point, output: bool, cx: &gpui_kit::App) -> Option<String> {
        let view = self.layout().viewport;
        self.visible_nodes(cx).iter().find_map(|node| {
            let pos = self.layout().positions.get(node.key())?;
            let handle = view.screen(Point::new(
                pos.x + NODE_WIDTH / 2.,
                pos.y + if output { NODE_HEIGHT } else { 0. },
            ));
            (handle.distance(p) <= 22.).then(|| node.key().to_owned())
        })
    }
    fn target_key(&self, cx: &gpui_kit::App) -> Option<String> {
        match &self.gesture {
            Some(Gesture::Connect { to, .. }) => to.clone(),
            Some(Gesture::Rewire {
                pointer, provider, ..
            }) => self.hit_handle(*pointer, *provider, cx),
            _ => None,
        }
    }
    fn start_handle(
        &mut self,
        key: &str,
        output: bool,
        p: Point,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.focus_handle.focus(window, cx);
        if let Some(Draft::Edge { id: Some(id), .. }) = &self.draft {
            let from = self.from.read(cx).value().to_string();
            let to = self.to.read(cx).value().to_string();
            if (output && from == key) || (!output && to == key) {
                self.gesture = Some(Gesture::Rewire {
                    id: id.clone(),
                    provider: output,
                    from,
                    to,
                    pointer: p,
                });
                cx.notify();
                return;
            }
        }
        if output {
            self.gesture = Some(Gesture::Connect {
                from: key.into(),
                to: None,
                pointer: p,
            });
        } else {
            self.select_node(key, window, cx);
        }
        cx.notify();
    }
    fn preview_curve(&self, cx: &gpui_kit::App) -> Option<Curve> {
        let positions = &self.layout().positions;
        let endpoint = |key: &str, output| {
            positions.get(key).map(|p| {
                Point::new(
                    p.x + NODE_WIDTH / 2.,
                    p.y + if output { NODE_HEIGHT } else { 0. },
                )
            })
        };
        match &self.gesture {
            Some(Gesture::Connect { from, pointer, .. }) => Some(Curve::endpoints(
                endpoint(from, true)?,
                self.layout().viewport.world(*pointer),
            )),
            Some(Gesture::Rewire {
                provider,
                from,
                to,
                pointer,
                ..
            }) => {
                let p = self.layout().viewport.world(*pointer);
                Some(if *provider {
                    Curve::endpoints(p, endpoint(to, false)?)
                } else {
                    Curve::endpoints(endpoint(from, true)?, p)
                })
            }
            _ if matches!(self.draft, Some(Draft::Edge { .. })) => Some(Curve::routed(
                *positions.get(self.from.read(cx).value().trim())?,
                *positions.get(self.to.read(cx).value().trim())?,
                positions,
            )),
            _ => None,
        }
    }
    fn pointer_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture.clone() else {
            return;
        };
        if event.pressed_button.is_none() {
            self.cancel_gesture(cx);
            return;
        }
        let p = canvas::local(self, event.position);
        match gesture {
            Gesture::Node {
                key,
                pointer,
                initial,
            } => {
                let now = self.layout().viewport.world(p);
                self.layout_mut().positions.insert(
                    key,
                    Point::new(initial.x + now.x - pointer.x, initial.y + now.y - pointer.y),
                );
            }
            Gesture::Pan { pointer, initial } => {
                self.layout_mut().viewport.offset =
                    Point::new(initial.x + p.x - pointer.x, initial.y + p.y - pointer.y);
            }
            Gesture::Connect { from, .. } => {
                let to = self.hit_handle(p, false, cx).filter(|to| to != &from);
                self.gesture = Some(Gesture::Connect {
                    from,
                    to,
                    pointer: p,
                });
            }
            Gesture::Rewire {
                id,
                provider,
                from,
                to,
                ..
            } => {
                self.gesture = Some(Gesture::Rewire {
                    id,
                    provider,
                    from,
                    to,
                    pointer: p,
                });
            }
        }
        cx.notify();
    }
    fn pointer_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(event.button, MouseButton::Left | MouseButton::Middle) {
            return;
        }
        if !self.bounds.get().contains(&event.position) {
            self.cancel_gesture(cx);
            return;
        }
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        let p = canvas::local(self, event.position);
        match gesture {
            Gesture::Node { .. } | Gesture::Pan { .. } => self.persist_layout(cx),
            Gesture::Connect { from, .. } => {
                if let Some(to) = self.hit_handle(p, false, cx).filter(|to| to != &from) {
                    self.new_edge(from, to, Some(p), window, cx);
                }
            }
            Gesture::Rewire {
                provider, from, to, ..
            } => {
                if let Some(key) = self
                    .hit_handle(p, provider, cx)
                    .filter(|key| if provider { key != &to } else { key != &from })
                {
                    let input = if provider { &self.from } else { &self.to };
                    input.update(cx, |input, cx| input.set_value(key, window, cx));
                }
            }
        }
        if self.reload_pending {
            self.refresh(cx);
        }
        cx.notify();
    }
    fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
        match self.gesture.take() {
            Some(Gesture::Node { key, initial, .. }) => {
                self.layout_mut().positions.insert(key, initial);
            }
            Some(Gesture::Pan { initial, .. }) => self.layout_mut().viewport.offset = initial,
            _ => {}
        }
        if self.layout_dirty {
            self.persist_layout(cx);
        }
        cx.notify();
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(draft) = &self.draft else {
            return div().into_any_element();
        };
        let mut panel = v_flex()
            .id("relationship-inspector")
            .occlude()
            .absolute()
            .top(px(12.))
            .right(px(12.))
            .w(px(330.))
            .max_h_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_lg()
            .text_sm();
        if let Draft::Edge {
            anchor: Some(anchor),
            ..
        } = draft
        {
            let size = self.bounds.get().size;
            panel = panel
                .left(px(
                    (anchor.x + 20.).clamp(12., (f32::from(size.width) - 350.).max(12.))
                ))
                .top(px(anchor
                    .y
                    .clamp(12., (f32::from(size.height) - 480.).max(12.))));
        }
        panel = panel.child(
            h_flex()
                .justify_between()
                .items_center()
                .child(div().font_semibold().child(match draft {
                    Draft::Node { key, .. } => key.clone(),
                    Draft::Edge { id: Some(_), .. } => "Edit relationship".into(),
                    _ => "Add relationship".into(),
                }))
                .child(
                    Button::new("close-inspector")
                        .ghost()
                        .label("×")
                        .disabled(self.saving)
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.draft = None;
                            page.error = None;
                            cx.notify();
                        })),
                ),
        );
        match draft {
            Draft::Node { key, .. } => {
                let node = self
                    .graph
                    .as_ref()
                    .and_then(|g| g.nodes.iter().find(|n| n.key() == key));
                if node.is_some_and(|n| n.unresolved) {
                    panel = panel.child("This repository record is missing or unreadable. Its connections are preserved; select one below to rewire or delete it.");
                } else {
                    panel = panel
                        .child("Purpose")
                        .child(
                            div()
                                .id("graph-description")
                                .child(Textarea::new(&self.description).h(px(125.))),
                        )
                        .child("Group")
                        .child(Input::new(&self.group))
                        .child(
                            Button::new("save-node")
                                .primary()
                                .label("Save repository")
                                .disabled(self.saving || self.load_error.is_some())
                                .on_click(cx.listener(|page, _, _, cx| page.save(false, cx))),
                        );
                    if let Some(node) = node.filter(|n| n.repository.is_linked()) {
                        let (key, label) = (key.clone(), node.label().to_owned());
                        panel = panel.child(
                            Button::new("open-graph-repository")
                                .ghost()
                                .label("Open repository")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(GraphEvent::OpenRepository {
                                        key: key.clone(),
                                        label: label.clone(),
                                    })
                                })),
                        );
                    }
                }
                panel = panel.child(
                    div()
                        .font_semibold()
                        .child("Connections · provider → consumer"),
                );
                if let Some(graph) = &self.graph {
                    for edge in graph
                        .relationships
                        .iter()
                        .filter(|e| e.from == *key || e.to == *key)
                    {
                        let id = edge.id.clone();
                        panel = panel.child(
                            Button::new(format!("inspect-{}", edge.id))
                                .ghost()
                                .label(format!("{} → {}", edge.from, edge.to))
                                .on_click(cx.listener(move |page, _, window, cx| {
                                    page.select_edge(&id, None, window, cx)
                                })),
                        );
                    }
                }
                panel = panel.child(
                    Button::new("show-endpoints-all")
                        .ghost()
                        .label("Show connections in All")
                        .on_click(cx.listener(|page, _, window, cx| {
                            page.search
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            page.set_group("All".into(), cx);
                            page.fit(cx);
                        })),
                );
            }
            Draft::Edge { id, .. } => {
                panel = panel
                    .child("Provider")
                    .child(self.endpoint_input(true, cx))
                    .child("Consumer")
                    .child(self.endpoint_input(false, cx))
                    .child(div().text_color(cx.theme().muted_foreground).child(format!(
                        "{} depends on {}",
                        self.to.read(cx).value(),
                        self.from.read(cx).value()
                    )))
                    .child("Description")
                    .child(
                        div()
                            .id("graph-description")
                            .child(Textarea::new(&self.description).h(px(125.))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("save-relationship")
                                    .primary()
                                    .label(if self.saving { "Saving…" } else { "Save" })
                                    .disabled(self.saving || self.load_error.is_some())
                                    .on_click(cx.listener(|page, _, _, cx| page.save(false, cx))),
                            )
                            .child(
                                Button::new("cancel-relationship")
                                    .ghost()
                                    .label("Cancel")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|page, _, _, cx| {
                                        page.draft = None;
                                        page.error = None;
                                        cx.notify();
                                    })),
                            ),
                    );
                if id.is_some() {
                    panel = panel.child(Button::new("delete-relationship").ghost().label("Delete relationship").disabled(self.saving || self.load_error.is_some()).on_click(cx.listener(|page, _, _, cx| page.save(true, cx))))
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Drag the selected provider/output or consumer/input handle to rewire. Changes apply on Save."));
                }
            }
        }
        if let Some(error) = &self.error {
            panel = panel
                .child(div().text_color(cx.theme().danger).child(error.clone()))
                .child(
                    Button::new("graph-rebase-draft")
                        .ghost()
                        .label("Use latest revision (keep draft)")
                        .disabled(self.loading || self.saving)
                        .on_click(cx.listener(|page, _, _, cx| page.reload_draft_revision(cx))),
                );
            let current = match draft {
                Draft::Node { key, .. } => self
                    .graph
                    .as_ref()
                    .and_then(|g| g.nodes.iter().find(|n| n.key() == key))
                    .map(|n| {
                        format!(
                            "Current stored purpose: {}\nGroup: {}",
                            n.repository.description.as_deref().unwrap_or(""),
                            n.repository.group.as_deref().unwrap_or("")
                        )
                    }),
                Draft::Edge { id: Some(id), .. } => self
                    .graph
                    .as_ref()
                    .and_then(|g| g.relationships.iter().find(|e| &e.id == id))
                    .map(|e| {
                        format!(
                            "Current stored connection: {} → {}\n{}",
                            e.from, e.to, e.description
                        )
                    }),
                _ => None,
            };
            if let Some(current) = current {
                panel = panel.child(div().text_xs().child(current));
            }
        }
        panel.into_any_element()
    }
    fn endpoint_input(&self, provider: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let choices: Vec<_> = self
            .graph
            .as_ref()
            .into_iter()
            .flat_map(|g| &g.nodes)
            .filter(|n| !n.unresolved)
            .map(|n| (n.key().to_owned(), n.label().to_owned()))
            .collect();
        h_flex()
            .gap_1()
            .child(Input::new(if provider { &self.from } else { &self.to }).flex_1())
            .child(
                Button::new(if provider {
                    "choose-provider"
                } else {
                    "choose-consumer"
                })
                .ghost()
                .label("⌄")
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu;
                    for (key, label) in &choices {
                        let (key, weak) = (key.clone(), weak.clone());
                        menu = menu.item(PopupMenuItem::new(format!("{label} · {key}")).on_click(
                            move |_, window, cx| {
                                let _ = weak.update(cx, |page, cx| {
                                    let input = if provider { &page.from } else { &page.to };
                                    input.update(cx, |input, cx| {
                                        input.set_value(key.clone(), window, cx)
                                    });
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                }),
            )
    }
}

impl Render for GraphPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let groups = self.groups();
        let toolbar = h_flex()
            .flex_none()
            .items_center()
            .flex_wrap()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("relationship-groups")
                    .ghost()
                    .label(format!("Group: {} ⌄", self.layouts.last_group))
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for group in &groups {
                            let (group, weak) = (group.clone(), weak.clone());
                            menu = menu.item(PopupMenuItem::new(group.clone()).on_click(
                                move |_, _, cx| {
                                    let _ = weak
                                        .update(cx, |page, cx| page.set_group(group.clone(), cx));
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(Input::new(&self.search).w(px(210.)))
            .child(
                Button::new("add-relationship")
                    .primary()
                    .label("Add relationship")
                    .disabled(self.graph.is_none() || self.saving)
                    .on_click(cx.listener(|page, _, window, cx| {
                        page.new_edge(String::new(), String::new(), None, window, cx)
                    })),
            )
            .child(
                Button::new("add-graph-repository")
                    .ghost()
                    .label("Add repository")
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.set_group("All".into(), cx);
                        cx.emit(GraphEvent::AddRepository);
                    })),
            )
            .child(
                Button::new("graph-fit")
                    .ghost()
                    .label("Fit view")
                    .on_click(cx.listener(|page, _, _, cx| page.fit(cx))),
            )
            .child(
                Button::new("graph-arrange")
                    .ghost()
                    .label("Auto arrange")
                    .on_click(cx.listener(|page, _, _, cx| page.arrange(cx))),
            )
            .child(
                Button::new("graph-refresh")
                    .ghost()
                    .label("Refresh")
                    .disabled(self.loading)
                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))),
            );
        let count = self.visible_nodes(cx).len();
        let mut body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(canvas::render(self, cx));
        if count == 0 {
            body = body.child(
                div()
                    .absolute()
                    .top(px(40.))
                    .left(px(40.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.loading {
                        "Loading repositories…"
                    } else {
                        "No repositories match this view. Add a repository or change the filter."
                    }),
            );
        }
        if self.draft.is_some() {
            body = body.child(self.render_inspector(cx));
        }
        let mut page = v_flex()
            .size_full()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .tab_group()
            .capture_key_down(cx.listener(|page, event: &gpui_kit::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    if page.gesture.is_some() {
                        page.cancel_gesture(cx);
                    } else if !page.saving {
                        page.draft = None;
                        page.error = None;
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(toolbar);
        if let Some(error) = &self.load_error {
            page = page.child(
                div()
                    .p_2()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        if self.draft.is_none()
            && let Some(error) = &self.error
        {
            page = page.child(
                div()
                    .p_2()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        if let Some(graph) = &self.graph {
            for diagnostic in &graph.diagnostics {
                page = page.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(diagnostic.clone()),
                );
            }
        }
        page.child(body).child(div().flex_none().px_3().py_1().text_xs().text_color(cx.theme().muted_foreground)
            .child(format!("{count} repositories · Provider → consumer · Drag background to pan · Scroll to zoom · Tab then Enter to inspect · Escape to cancel · {:.0}%", self.layout().viewport.zoom*100.)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{Focusable as _, point};

    #[gpui_kit::test]
    fn floating_editor_occludes_canvas_and_escape_works_in_textarea(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt;
        cx.update(gpui_kit::component::init);
        let handle = cx.add_window(|window, cx| {
            let mut page = GraphPage::new(None, window, cx);
            page.draft = Some(Draft::Edge {
                id: None,
                revision: "read".into(),
                anchor: Some(Point::new(50., 50.)),
            });
            page
        });
        cx.update_window(handle.into(), |page, window, cx| {
            let page = page.downcast::<GraphPage>().unwrap();
            let input_id = page.read(cx).description.entity_id();
            window.click(("input", input_id), cx);
            window.input("An editable relationship", cx);
            page.update(cx, |page, cx| {
                assert_eq!(
                    page.description.read(cx).value().as_str(),
                    "An editable relationship"
                );
                assert!(
                    page.gesture.is_none(),
                    "canvas must not start panning through the inspector"
                );
            });
            window.press("escape", cx);
            page.update(cx, |page, _| assert!(page.draft.is_none()));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn escape_and_focus_loss_cancel_gestures_without_losing_text_drafts(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::component::init);
        let (page, cx) = cx.add_window_view(|window, cx| GraphPage::new(None, window, cx));
        cx.update(|window, app| {
            page.update(app, |page, cx| {
                page.layout_mut()
                    .positions
                    .insert("api".into(), Point::new(200., 100.));
                page.gesture = Some(Gesture::Node {
                    key: "api".into(),
                    pointer: Point::default(),
                    initial: Point::new(10., 20.),
                });
                page.focus_handle.focus(window, cx);
            });
        });
        cx.simulate_keystrokes("escape");
        page.update(cx, |page, _| {
            assert!(page.gesture.is_none());
            assert_eq!(page.layout().positions["api"], Point::new(10., 20.));
        });
        cx.update(|window, app| {
            page.update(app, |page, cx| {
                page.draft = Some(Draft::Edge {
                    id: None,
                    revision: "read-revision".into(),
                    anchor: None,
                });
                page.description.update(cx, |input, cx| {
                    input.set_value("Draft retained", window, cx)
                });
                page.from.read(cx).focus_handle(cx).focus(window, cx);
            });
        });
        cx.simulate_keystrokes("j");
        page.update(cx, |page, cx| {
            assert_eq!(page.from.read(cx).value().as_str(), "j");
            assert_eq!(page.description.read(cx).value().as_str(), "Draft retained");
            page.gesture = Some(Gesture::Pan {
                pointer: Point::default(),
                initial: Point::new(1., 2.),
            });
            page.layout_mut().viewport.offset = Point::new(200., 200.);
        });
        cx.update(|window, app| {
            page.update(app, |page, cx| {
                page.pointer_up(
                    &MouseUpEvent {
                        position: point(px(-100.), px(-100.)),
                        button: MouseButton::Left,
                        click_count: 1,
                        modifiers: Default::default(),
                    },
                    window,
                    cx,
                )
            });
        });
        page.update(cx, |page, _| {
            assert_eq!(page.layout().viewport.offset, Point::new(1., 2.));
            assert!(page.draft.is_some());
        });
        cx.update(|window, app| {
            app.focus_handle().focus(window, app);
        });
        page.update(cx, |page, _| assert!(page.gesture.is_none()));
    }
}
