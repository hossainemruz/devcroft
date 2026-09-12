//! Repository resources, shared by the workspace and global artifact browser.
use crate::data::DataRoot;
use crate::data::artifacts::{
    ArtifactList, ArtifactPatch, ArtifactStore, CommentChange, Kind, ListOptions, Snapshot,
};
use crate::preview::{PreviewView, TocActive, TocEntry};
use crate::relative_time::{current_unix_secs, relative_duration_label};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable, Size, StyledExt as _, WindowExt as _, h_flex,
    tag::Tag, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use std::time::Duration;
const PAGE_SIZE: usize = 100;
#[derive(Default)]
pub(crate) struct Refresh {
    pub(crate) busy: bool,
    pub(crate) pending: bool,
    generation: u64,
}

impl Refresh {
    pub(crate) fn request(&mut self) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        if self.busy {
            self.pending = true;
            return None;
        }
        self.busy = true;
        self.pending = false;
        Some(self.generation)
    }

    pub(crate) fn finish(&mut self, generation: u64) -> bool {
        self.busy = false;
        generation == self.generation
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    Global,
    Repository(Option<String>),
}
pub(crate) struct OpenSession(pub(crate) crate::data::artifacts::OriginSession);
impl EventEmitter<OpenSession> for ArtifactBrowser {}

struct Draft {
    input: Entity<TextareaState>,
    snapshot: Snapshot,
    comment: Option<String>,
    document: bool,
}

pub(crate) struct ArtifactBrowser {
    root: Option<DataRoot>,
    scope: Scope,
    active: bool,
    refresh: Refresh,
    include_archived: bool,
    kind_filter: Option<Kind>,
    limit: usize,
    list: ArtifactList,
    selected_id: Option<String>,
    selected: Option<Snapshot>,
    error: Option<String>,
    preview: Option<Entity<PreviewView>>,
    toc: Vec<TocEntry>,
    toc_active: usize,
    draft: Option<Draft>,
    saved_drafts: std::collections::HashMap<Scope, Draft>,
    saving: bool,
    pub(crate) focus_handle: FocusHandle,
    sidebar_focus: FocusHandle,
    detail_focus: FocusHandle,
    comments_focus: FocusHandle,
    outline_focus: FocusHandle,
}

impl ArtifactBrowser {
    pub(crate) fn new(root: Option<DataRoot>, cx: &mut Context<Self>) -> Self {
        Self::scoped(root, Scope::Global, cx)
    }
    pub(crate) fn scoped(root: Option<DataRoot>, scope: Scope, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.active && !this.saving && !this.refresh.busy {
                            this.refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            root,
            scope,
            active: false,
            refresh: Refresh::default(),
            include_archived: false,
            kind_filter: None,
            limit: PAGE_SIZE,
            list: ArtifactList::default(),
            selected_id: None,
            selected: None,
            error: None,
            preview: None,
            toc: Vec::new(),
            toc_active: 0,
            draft: None,
            saved_drafts: Default::default(),
            saving: false,
            focus_handle: cx.focus_handle(),
            sidebar_focus: cx.focus_handle().tab_stop(true),
            detail_focus: cx.focus_handle().tab_stop(true),
            comments_focus: cx.focus_handle().tab_stop(true),
            outline_focus: cx.focus_handle().tab_stop(true),
        }
    }

    pub(crate) fn navigation_state(&self) -> crate::navigation::ResourceState {
        crate::navigation::ResourceState {
            selected: self.selected.is_some(),
            drafting: self.draft.is_some(),
            saving: self.saving,
        }
    }

    pub(crate) fn navigation_panes(&self, cx: &gpui_kit::App) -> Vec<(&'static str, FocusHandle)> {
        let mut panes = vec![("Resources", self.sidebar_focus.clone())];
        if self.selected.is_none() {
            return panes;
        }
        let document = self
            .draft
            .as_ref()
            .map(|draft| draft.input.read(cx).focus_handle(cx))
            .unwrap_or_else(|| self.detail_focus.clone());
        panes.push((
            if self.draft.is_some() {
                "Draft"
            } else {
                "Document"
            },
            document,
        ));
        if self.draft.is_none() {
            panes.push(("Comments", self.comments_focus.clone()));
            if !self.toc.is_empty() {
                panes.push(("Outline", self.outline_focus.clone()));
            }
        }
        panes
    }

    pub(crate) fn begin_markdown_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none() || self.draft.is_some() || self.saving {
            return;
        }
        self.edit(true, None, window, cx);
        if let Some(draft) = &self.draft {
            draft.input.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    pub(crate) fn begin_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none() || self.draft.is_some() || self.saving {
            return;
        }
        self.edit(false, None, window, cx);
        if let Some(draft) = &self.draft {
            draft.input.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    pub(crate) fn save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.is_none() || self.saving {
            return;
        }
        self.detail_focus.focus(window, cx);
        self.save(cx);
    }

    pub(crate) fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.is_some() && !self.saving {
            self.draft = None;
            self.error = None;
            self.detail_focus.focus(window, cx);
            cx.notify();
        }
    }
    pub(crate) fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            if let Some(draft) = self.draft.take() {
                self.saved_drafts.insert(self.scope.clone(), draft);
            }
            self.scope = scope;
            self.draft = self.saved_drafts.remove(&self.scope);
            self.selected_id = self.draft.as_ref().map(|d| d.snapshot.artifact.id.clone());
            self.selected = None;
            self.preview = None;
            self.toc = Vec::new();
            self.toc_active = 0;
            self.error = None;
            self.list = ArtifactList::default();
            self.limit = PAGE_SIZE;
            self.kind_filter = None;
            self.refresh(cx);
        }
    }
    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.refresh(cx);
        }
    }
    pub(crate) fn include_archived(&self) -> bool {
        self.include_archived
    }
    pub(crate) fn set_include_archived(&mut self, value: bool, cx: &mut Context<Self>) {
        self.include_archived = value;
        self.limit = PAGE_SIZE;
        self.refresh(cx);
    }
    pub(crate) fn set_kind_filter(&mut self, filter: Option<Kind>, cx: &mut Context<Self>) {
        if self.kind_filter != filter {
            self.kind_filter = filter;
            // Keep the detail in sync with the visible list: if the current
            // selection is filtered out, fall back to the first visible card.
            let selected_visible = self.selected_id.as_ref().is_some_and(|id| {
                self.visible_artifacts()
                    .any(|snapshot| &snapshot.artifact.id == id)
            });
            if !selected_visible {
                let fallback = self.visible_artifacts().next().cloned();
                self.select(fallback, cx);
                self.error = None;
            }
            cx.notify();
        }
    }
    fn visible_artifacts(&self) -> impl Iterator<Item = &Snapshot> {
        self.list.artifacts.iter().filter(|snapshot| {
            self.kind_filter
                .is_none_or(|kind| snapshot.artifact.kind == kind)
        })
    }
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(root) = self.root.clone() else {
            self.error = Some("Portable data is unavailable".into());
            return;
        };
        let Some(generation) = self.refresh.request() else {
            return;
        };
        let scope = self.scope.clone();
        let selected_id = self.selected_id.clone();
        let options = ListOptions {
            repository: match &scope {
                Scope::Repository(key) => key.clone(),
                Scope::Global => None,
            },
            include_archived: self.include_archived,
            limit: Some(self.limit),
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if scope == Scope::Repository(None) {
                        return Ok((ArtifactList::default(), None));
                    }
                    let store = ArtifactStore::new(&root);
                    let list = store.list(&options).map_err(|e| format!("{e:#}"))?;
                    let selected = selected_id
                        .as_ref()
                        .map(|id| store.get(id).map_err(|e| format!("{e:#}")));
                    Ok::<_, String>((list, selected))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.refresh.finish(generation) {
                    match result {
                        Ok((list, selected)) => {
                            this.list = list;
                            let pinned = selected.and_then(|result| match result {
                                Ok(snapshot)
                                    if Some(&snapshot.artifact.id) == this.selected_id.as_ref()
                                        && (this.include_archived
                                            || !snapshot.artifact.archived)
                                        && match &this.scope {
                                            Scope::Global => true,
                                            Scope::Repository(key) => {
                                                snapshot.artifact.repository == *key
                                            }
                                        } =>
                                {
                                    Some(snapshot)
                                }
                                Err(error) => {
                                    this.error = Some(error);
                                    None
                                }
                                _ => None,
                            });
                            let selection = this
                                .selected_id
                                .as_ref()
                                .and_then(|id| {
                                    this.list.artifacts.iter().find(|s| &s.artifact.id == id)
                                })
                                .cloned()
                                .or(pinned)
                                .or_else(|| this.draft.as_ref().map(|d| d.snapshot.clone()))
                                .or_else(|| this.list.artifacts.first().cloned());
                            this.select(selection, cx);
                        }
                        Err(error) => this.error = Some(error),
                    }
                }
                if this.refresh.pending {
                    this.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn select(&mut self, snapshot: Option<Snapshot>, cx: &mut Context<Self>) {
        if self.selected.as_ref().map(|s| &s.artifact.content)
            != snapshot.as_ref().map(|s| &s.artifact.content)
            || self.preview.is_none()
        {
            match (self.preview.as_ref(), snapshot.as_ref()) {
                (Some(preview), Some(s)) if self.selected_id.as_ref() == Some(&s.artifact.id) => {
                    preview.update(cx, |view, cx| {
                        view.set_content(s.artifact.content.clone().into(), cx)
                    })
                }
                (_, Some(s)) => {
                    let preview =
                        cx.new(|cx| PreviewView::embedded(s.artifact.content.clone().into(), cx));
                    cx.subscribe(&preview, |this, _, event: &TocActive, cx| {
                        this.toc_active = event.0;
                        cx.notify();
                    })
                    .detach();
                    self.preview = Some(preview);
                }
                _ => self.preview = None,
            }
        }
        self.selected_id = snapshot.as_ref().map(|s| s.artifact.id.clone());
        self.selected = snapshot;
        if let Some(preview) = self.preview.as_ref() {
            let (entries, active) = preview.read(cx).toc_snapshot();
            self.toc = entries;
            self.toc_active = active;
        } else {
            self.toc = Vec::new();
            self.toc_active = 0;
        }
    }
    fn edit(
        &mut self,
        document: bool,
        comment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.selected.clone() else {
            return;
        };
        let value = if document {
            snapshot.artifact.content.clone()
        } else {
            comment
                .as_ref()
                .and_then(|id| snapshot.artifact.comments.iter().find(|c| &c.id == id))
                .map(|c| c.body.clone())
                .unwrap_or_default()
        };
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx).rows(if document { 24 } else { 4 });
            input.set_value(value, window, cx);
            input
        });
        self.draft = Some(Draft {
            input,
            snapshot,
            comment,
            document,
        });
        self.error = None;
        cx.notify();
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &self.draft else {
            return;
        };
        let value = draft.input.read(cx).value().to_string();
        let snapshot = draft.snapshot.clone();
        let edit = if draft.document {
            Mutation::Document(value)
        } else if let Some(id) = &draft.comment {
            Mutation::Comment(CommentChange::Edit(id.clone(), value))
        } else {
            Mutation::Comment(CommentChange::Create(value))
        };
        self.mutate(snapshot, edit, true, cx);
    }
    fn mutate(
        &mut self,
        snapshot: Snapshot,
        mutation: Mutation,
        close_draft: bool,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        self.saving = true;
        self.error = None;
        self.refresh.generation = self.refresh.generation.wrapping_add(1);
        let scope = self.scope.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let store = ArtifactStore::new(&root);
                    let id = &snapshot.artifact.id;
                    let revision = &snapshot.revision;
                    match mutation {
                        Mutation::Document(content) => store.update(
                            id,
                            revision,
                            ArtifactPatch {
                                content: Some(content),
                                ..Default::default()
                            },
                        ),
                        Mutation::Comment(change) => store.comment(id, revision, change),
                        Mutation::Archive => {
                            store.set_archived(id, revision, !snapshot.artifact.archived)
                        }
                    }
                    .map_err(|e| format!("{e:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                if this.scope != scope && result.is_ok() && close_draft {
                    this.saved_drafts.remove(&scope);
                }
                if this.scope == scope {
                    match result {
                        Ok(snapshot) => {
                            if close_draft {
                                this.draft = None;
                            }
                            this.select(Some(snapshot), cx);
                        }
                        Err(error) => {
                            this.error =
                                Some(format!("Could not save. Your draft is retained. {error}"))
                        }
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn delete(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        self.saving = true;
        self.error = None;
        self.refresh.generation = self.refresh.generation.wrapping_add(1);
        let scope = self.scope.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let store = ArtifactStore::new(&root);
                    store
                        .delete(&snapshot.artifact.id, &snapshot.revision)
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                if this.scope == scope {
                    match result {
                        Ok(()) => {
                            this.draft = None;
                            this.selected_id = None;
                            this.selected = None;
                            this.preview = None;
                            this.toc = Vec::new();
                            this.toc_active = 0;
                        }
                        Err(error) => this.error = Some(format!("Could not delete. {error}")),
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    /// The artifact options menu, rendered at the top of the reader rail so
    /// it stays put whether or not the document has headings for an outline.
    fn render_options_menu(
        &self,
        snapshot: Snapshot,
        archived: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let menu_snapshot = snapshot;
        let view = cx.entity().downgrade();
        Button::new("artifact-options")
            .ghost()
            .label("⋯")
            .accessibility_label("Artifact options")
            .disabled(self.draft.is_some() || self.saving)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                menu.item({
                    let view = view.clone();
                    PopupMenuItem::new("Edit Markdown")
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.edit(true, None, window, cx);
                            });
                        })
                })
                .item({
                    let view = view.clone();
                    let snapshot = menu_snapshot.clone();
                    let label = if archived { "Unarchive" } else { "Archive" };
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.mutate(snapshot.clone(), Mutation::Archive, false, cx);
                        });
                    })
                })
                .item({
                    let view = view.clone();
                    let snapshot = menu_snapshot.clone();
                    PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                        let view = view.clone();
                        let snapshot = snapshot.clone();
                        let title = snapshot.artifact.title.clone();
                        window.open_dialog(cx, move |dialog, _, _| {
                            let view = view.clone();
                            let snapshot = snapshot.clone();
                            dialog
                                .title("Delete artifact?")
                                .child(format!(
                                    "\"{title}\" and its comments will be permanently deleted. This cannot be undone."
                                ))
                                .footer(
                                    DialogFooter::new()
                                        .child(
                                            Button::new("cancel-delete-artifact")
                                                .label("Cancel")
                                                .on_click(|_, window, cx| {
                                                    window.close_dialog(cx)
                                                }),
                                        )
                                        .child(
                                            Button::new("confirm-delete-artifact")
                                                .primary()
                                                .label("Delete")
                                                .on_click(|_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(Confirm {
                                                            secondary: false,
                                                        }),
                                                        cx,
                                                    )
                                                }),
                                        ),
                                )
                                .on_ok(move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.delete(snapshot.clone(), cx);
                                    });
                                    true
                                })
                        });
                    })
                })
            })
    }
    /// Full-height outline rail beside the document. Entries and the active
    /// index mirror the preview's own table of contents (see
    /// `PreviewView::toc_snapshot`); clicks scroll the document through the
    /// preview entity so the virtualized list offsets stay exact.
    fn render_toc_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.toc_active;
        let preview = self.preview.clone();
        let entries = self.toc.clone();
        v_flex()
            .id("resource-toc")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .pt_2()
            .pb_4()
            .gap_1()
            .children(entries.into_iter().enumerate().map(move |(index, entry)| {
                let is_active = index == active;
                // h1 flush; each deeper level indented one step.
                let indent = px(8. + f32::from(entry.level.saturating_sub(1)) * 12.);
                let preview = preview.clone();
                div()
                    .id(("resource-toc-row", index))
                    .flex_none()
                    .w_full()
                    .py(px(6.))
                    .pl(indent)
                    .pr(px(8.))
                    .rounded_md()
                    .cursor_pointer()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .when(is_active, |this| {
                        this.bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    })
                    .when(!is_active, |this| {
                        this.text_color(cx.theme().muted_foreground)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_, _, _, cx| {
                            if let Some(preview) = preview.clone() {
                                preview.update(cx, |view, cx| {
                                    view.on_toc_click(index, cx);
                                });
                            }
                        }),
                    )
                    .child(entry.title.clone())
            }))
    }
}
enum Mutation {
    Document(String),
    Comment(CommentChange),
    Archive,
}

/// `Updated 2h ago` for sidebar cards. `updated_at_ms` is unix millis (see
/// `data::record::timestamp`); future values read as `just now` rather than
/// a negative duration. Pure over `now_secs` for tests.
fn updated_label(updated_at_ms: u64, now_secs: i64) -> String {
    let then_secs = updated_at_ms.saturating_div(1_000) as i64;
    let diff = now_secs.saturating_sub(then_secs);
    format!("Updated {}", relative_duration_label(diff))
}

impl Render for ArtifactBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sidebar = v_flex()
            .id("resource-sidebar")
            .track_focus(&self.sidebar_focus)
            .focus(|style| style.border_2().border_color(cx.theme().ring))
            .w(px(260.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .child({
                let active = self.kind_filter;
                let disabled = self.draft.is_some() || self.saving;
                let current = active.map(Kind::label).unwrap_or("All");
                let view = cx.entity().downgrade();
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Button::new("resource-kind-filter")
                                .label(current)
                                .w_full()
                                .accessibility_label(format!(
                                    "Filter resources by type, currently {current}"
                                ))
                                .dropdown_caret(true)
                                .outline()
                                .disabled(disabled)
                                .dropdown_menu_with_anchor(
                                    Anchor::BottomLeft,
                                    move |menu, _, _| {
                                        let mut menu = menu;
                                        for kind in [None]
                                            .into_iter()
                                            .chain(Kind::ALL.iter().copied().map(Some))
                                        {
                                            let view: WeakEntity<Self> = view.clone();
                                            let checked = active == kind;
                                            let label = kind.map(Kind::label).unwrap_or("All");
                                            menu = menu.item(
                                                PopupMenuItem::new(label)
                                                    .checked(checked)
                                                    .on_click(move |_, _, cx| {
                                                        let _ = view.update(cx, |this, cx| {
                                                            this.set_kind_filter(kind, cx)
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    },
                                ),
                        ),
                    )
                    .child(
                        Checkbox::new("resource-archived")
                            .label("Archived")
                            .accessibility_label("Show archived resources")
                            .checked(self.include_archived)
                            .disabled(disabled)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_include_archived(*checked, cx);
                            })),
                    )
            });
        let visible: Vec<Snapshot> = self.visible_artifacts().cloned().collect();
        if visible.is_empty() && !self.list.artifacts.is_empty() {
            sidebar = sidebar.child(
                div()
                    .text_xs()
                    .child("No matching resources for this filter."),
            );
        }
        for snapshot in &visible {
            let snapshot = snapshot.clone();
            let artifact = &snapshot.artifact;
            let age = updated_label(artifact.updated_at, current_unix_secs());
            let mut recency = age.clone();
            if artifact.archived {
                recency.push_str(" · Archived");
            }
            let tooltip = format!("{}\n{} · {recency}", artifact.title, artifact.kind.label());
            sidebar = sidebar.child(
                Button::new(SharedString::from(artifact.id.clone()))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .p_2()
                    .tooltip(tooltip)
                    .child(
                        v_flex()
                            .w_full()
                            .items_start()
                            .gap_1()
                            .child(div().w_full().truncate().child(artifact.title.clone()))
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Tag::secondary()
                                            .with_size(Size::Small)
                                            .rounded_full()
                                            .flex_none()
                                            .child(artifact.kind.label()),
                                    )
                                    .child(div().text_xs().child(recency)),
                            ),
                    )
                    .disabled(self.draft.is_some() || self.saving)
                    .when(self.selected_id.as_ref() == Some(&artifact.id), |b| {
                        b.bg(cx.theme().secondary)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(Some(snapshot.clone()), cx);
                        this.error = None;
                        cx.notify();
                    })),
            );
        }
        if self.list.truncated {
            sidebar = sidebar.child(Button::new("more-resources").label("Load more").on_click(
                cx.listener(|this, _, _, cx| {
                    this.limit += PAGE_SIZE;
                    this.refresh(cx);
                }),
            ));
        }
        for error in &self.list.errors {
            sidebar = sidebar.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        let mut detail = v_flex()
            .track_focus(&self.detail_focus)
            .focus(|style| style.border_2().border_color(cx.theme().ring))
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0();
        if let Some(snapshot) = self.selected.clone() {
            let artifact = &snapshot.artifact;
            let archived = artifact.archived;
            let mut main = v_flex().flex_1().min_w_0().h_full().min_h_0().p_4().gap_3();
            if let Some(error) = &self.error {
                main = main.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            let mut meta = h_flex()
                .gap_2()
                .items_center()
                .child(
                    Tag::secondary()
                        .with_size(Size::Small)
                        .rounded_full()
                        .flex_none()
                        .child(artifact.kind.label()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(updated_label(artifact.updated_at, current_unix_secs())),
                );
            if let Some(repository) = artifact.repository.clone() {
                meta = meta.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(repository),
                );
            }
            if archived {
                meta = meta.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child("Archived"),
                );
            }
            main = main.child(meta);
            if !artifact.sessions.is_empty() {
                let mut origins = h_flex().gap_2().flex_wrap().child("Originating sessions:");
                for (index, origin) in artifact.sessions.iter().enumerate() {
                    let origin = origin.clone();
                    origins =
                        origins.child(
                            Button::new(("origin-session", index))
                                .ghost()
                                .label(if origin.title.is_empty() {
                                    origin.key.id.clone()
                                } else {
                                    origin.title.clone()
                                })
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(OpenSession(origin.clone()))
                                })),
                        );
                }
                main = main.child(origins);
            }
            if let Some(draft) = &self.draft {
                main = main
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(Textarea::new(&draft.input).h_full()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("save-resource")
                                    .label("Save")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_draft(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("cancel-resource-edit")
                                    .label("Cancel")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_draft(window, cx)
                                    })),
                            ),
                    );
                detail = detail.child(main);
            } else {
                if let Some(preview) = &self.preview {
                    main = main.child(div().flex_1().min_h_0().child(preview.clone()));
                }
                let mut comments = v_flex()
                    .id("artifact-comments")
                    .track_focus(&self.comments_focus)
                    .focus(|style| style.border_2().border_color(cx.theme().ring))
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_semibold().child("Comments"))
                            .child(
                                Button::new("new-artifact-comment")
                                    .label("Add comment")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit(false, None, window, cx)
                                    })),
                            ),
                    );
                for (index, comment) in artifact.comments.iter().enumerate() {
                    let edit_id = comment.id.clone();
                    let resolve_id = comment.id.clone();
                    let delete_id = comment.id.clone();
                    let resolved = comment.resolved;
                    comments = comments.child(
                        v_flex()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .child(div().text_sm().child(comment.body.clone()))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .when(resolved, |row| row.child("Resolved"))
                                    .child(
                                        Button::new(("edit-comment", index))
                                            .ghost()
                                            .label("Edit")
                                            .disabled(self.saving)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.edit(false, Some(edit_id.clone()), window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new(("resolve-comment", index))
                                            .ghost()
                                            .label(if resolved { "Reopen" } else { "Resolve" })
                                            .disabled(self.saving)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(s) = this.selected.clone() {
                                                    this.mutate(
                                                        s,
                                                        Mutation::Comment(CommentChange::Resolve(
                                                            resolve_id.clone(),
                                                            !resolved,
                                                        )),
                                                        false,
                                                        cx,
                                                    );
                                                }
                                            })),
                                    )
                                    .child(
                                        Button::new(("delete-comment", index))
                                            .ghost()
                                            .label("Delete")
                                            .disabled(self.saving)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(s) = this.selected.clone() {
                                                    this.mutate(
                                                        s,
                                                        Mutation::Comment(CommentChange::Delete(
                                                            delete_id.clone(),
                                                        )),
                                                        false,
                                                        cx,
                                                    );
                                                }
                                            })),
                                    ),
                            ),
                    );
                }
                main = main.child(comments);
                let mut rail = v_flex()
                    .track_focus(&self.outline_focus)
                    .focus(|style| style.border_2().border_color(cx.theme().ring))
                    .flex_none()
                    .h_full()
                    .when(!self.toc.is_empty(), |rail| {
                        rail.w(px(260.))
                            .border_l_1()
                            .border_color(cx.theme().border)
                    });
                rail = rail.child({
                    let mut head = h_flex().items_center().gap_2().px_4().pt_4();
                    if !self.toc.is_empty() {
                        head = head.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("On this page"),
                        );
                    } else {
                        head = head.justify_end();
                    }
                    head.child(self.render_options_menu(snapshot.clone(), archived, cx))
                });
                if !self.toc.is_empty() {
                    rail = rail.child(self.render_toc_panel(cx));
                }
                detail = detail.child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(main)
                        .child(rail),
                );
            }
        } else {
            detail = detail.p_4().gap_3();
            if let Some(error) = &self.error {
                detail = detail.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            detail = detail.child(if self.refresh.busy { "Loading resources…" } else { "No resources for this repository. Ask an agent to create an artifact with devcroft artifact create --repository <key>." });
        }
        h_flex()
            .id("resources")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0()
            .child(sidebar)
            .child(detail)
    }
}

#[cfg(test)]
#[path = "artifact_browser_tests.rs"]
mod tests;
