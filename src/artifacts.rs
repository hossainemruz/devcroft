//! Repository resources, shared by the workspace and global artifact browser.
mod comments;

use crate::data::DataRoot;
use crate::data::artifacts::{
    ArtifactList, ArtifactPatch, ArtifactStore, CommentChange, Kind, ListOptions, Snapshot,
};
use crate::preview::{PreviewView, TocActive, TocEntry};
use crate::relative_time::{current_unix_secs, relative_duration_label};
use crate::tutorial_view::TutorialView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, ColorName, Disableable as _, IconName, Sizable, Size, StyledExt as _,
    WindowExt as _, h_flex, tag::Tag, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable as _, InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
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

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Scope {
    Global,
    Repository(Option<String>),
}
pub(crate) struct OpenSession(pub(crate) crate::data::artifacts::OriginSession);
impl EventEmitter<OpenSession> for ArtifactBrowser {}
impl EventEmitter<crate::preview::OpenReference> for ArtifactBrowser {}

struct Draft {
    input: Entity<TextareaState>,
    snapshot: Snapshot,
    comment: Option<String>,
    document: bool,
    block: Option<usize>,
    selection: Option<std::ops::Range<usize>>,
    quote: Option<String>,
}

/// Identity a draft belongs to: a draft is only restored under the same
/// scope *and* space, so switching isolation profiles never shows another
/// space's editor — and switching back keeps the unsaved work.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DraftKey {
    scope: Scope,
    space: Option<String>,
}

impl DraftKey {
    fn new(scope: &Scope, space: Option<&str>) -> Self {
        Self {
            scope: scope.clone(),
            space: space.map(str::to_owned),
        }
    }
}

pub(crate) struct ArtifactBrowser {
    root: Option<DataRoot>,
    scope: Scope,
    active: bool,
    refresh: Refresh,
    include_archived: bool,
    kind_filter: Option<Kind>,
    /// Active isolation profile. `None` (repository-scoped browsers) shows
    /// every artifact in scope; the global browser follows the workspace's
    /// active space. Artifacts whose space could not be resolved (their
    /// repository record is gone) stay visible in every space rather than
    /// being hidden silently.
    space_filter: Option<String>,
    limit: usize,
    list: ArtifactList,
    selected_id: Option<String>,
    selected: Option<Snapshot>,
    error: Option<String>,
    preview: Option<Entity<PreviewView>>,
    /// Sandboxed HTML viewer for the selected tutorial artifact. Exactly one
    /// of `preview`/`tutorial` is populated for a selection.
    tutorial: Option<Entity<TutorialView>>,
    toc: Vec<TocEntry>,
    toc_active: usize,
    show_comments: bool,
    active_comment: Option<String>,
    comments_scroll: gpui_kit::ScrollHandle,
    draft: Option<Draft>,
    saved_drafts: std::collections::HashMap<DraftKey, Draft>,
    saving: bool,
    opening: bool,
    open_generation: u64,
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
            space_filter: None,
            limit: PAGE_SIZE,
            list: ArtifactList::default(),
            selected_id: None,
            selected: None,
            error: None,
            preview: None,
            tutorial: None,
            toc: Vec::new(),
            toc_active: 0,
            show_comments: false,
            active_comment: None,
            comments_scroll: gpui_kit::ScrollHandle::new(),
            draft: None,
            saved_drafts: Default::default(),
            saving: false,
            opening: false,
            open_generation: 0,
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
            editable: self
                .selected
                .as_ref()
                .is_some_and(|snapshot| snapshot.artifact.kind != Kind::Tutorial),
        }
    }

    pub(crate) fn navigation_panes(&self, cx: &gpui_kit::App) -> Vec<(&'static str, FocusHandle)> {
        let mut panes = vec![("Resources", self.sidebar_focus.clone())];
        if self.selected.is_none() {
            return panes;
        }
        if let Some(draft) = self.draft.as_ref().filter(|d| d.document) {
            panes.push(("Draft", draft.input.read(cx).focus_handle(cx)));
        } else if let Some(tutorial) = &self.tutorial {
            panes.push(("Document", tutorial.read(cx).focus_handle()));
        } else {
            panes.push(("Document", self.detail_focus.clone()));
            if self.show_comments {
                let focus = self
                    .draft
                    .as_ref()
                    .map(|d| d.input.read(cx).focus_handle(cx))
                    .unwrap_or_else(|| self.comments_focus.clone());
                panes.push(("Comments", focus));
            } else {
                panes.push(("On this page", self.outline_focus.clone()));
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

    /// Open the selected tutorial in the platform browser. Shares the cache
    /// and launch path with the viewer's own button.
    fn open_tutorial_in_browser(&mut self, cx: &mut Context<Self>) {
        let Some(tutorial) = self.tutorial.clone() else {
            return;
        };
        tutorial.update(cx, |view, cx| {
            let _ = view.open_in_browser(cx);
        });
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
            self.sync_preview_comments(cx);
            self.detail_focus.focus(window, cx);
            cx.notify();
        }
    }
    pub(crate) fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.open_generation = self.open_generation.wrapping_add(1);
            self.opening = false;
            if let Some(draft) = self.draft.take() {
                self.saved_drafts.insert(
                    DraftKey::new(&self.scope, self.space_filter.as_deref()),
                    draft,
                );
            }
            self.scope = scope;
            self.draft = self
                .saved_drafts
                .remove(&DraftKey::new(&self.scope, self.space_filter.as_deref()));
            self.show_comments |= self.draft.as_ref().is_some_and(|d| !d.document);
            self.active_comment = None;
            self.selected_id = self.draft.as_ref().map(|d| d.snapshot.artifact.id.clone());
            self.selected = None;
            self.preview = None;
            self.tutorial = None;
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
        } else if let Some(tutorial) = self.tutorial.clone() {
            // The native tutorial webview sits above GPUI content, so hide it
            // when the Resources tab leaves the screen instead of leaving it
            // painted over the next tab.
            tutorial.update(cx, |view, cx| view.set_visible(false, cx));
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
    /// Whether any unsaved draft (active or stashed) exists. Settings blocks
    /// catalog renames/deletes while one does: the rewrite replaces the
    /// records a draft is based on, and a draft's restoration key can name
    /// the space being edited.
    pub(crate) fn has_draft(&self) -> bool {
        self.draft.is_some() || !self.saved_drafts.is_empty()
    }

    /// Follow the active space. Repository-scoped browsers ignore this (the
    /// open repository is an explicit context). Like a scope change, the
    /// current draft is stashed under the old space and the new space's own
    /// draft (if any) is restored, so isolation holds without discarding
    /// unsaved work.
    pub(crate) fn set_space(&mut self, space: String, cx: &mut Context<Self>) {
        let next = (self.scope == Scope::Global).then_some(space);
        if self.space_filter == next {
            return;
        }
        self.open_generation = self.open_generation.wrapping_add(1);
        self.opening = false;
        if let Some(draft) = self.draft.take() {
            self.saved_drafts.insert(
                DraftKey::new(&self.scope, self.space_filter.as_deref()),
                draft,
            );
        }
        self.space_filter = next;
        self.draft = self
            .saved_drafts
            .remove(&DraftKey::new(&self.scope, self.space_filter.as_deref()));
        self.show_comments |= self.draft.as_ref().is_some_and(|d| !d.document);
        self.active_comment = None;
        self.selected_id = self.draft.as_ref().map(|d| d.snapshot.artifact.id.clone());
        self.selected = None;
        self.preview = None;
        self.tutorial = None;
        self.toc = Vec::new();
        self.toc_active = 0;
        self.error = None;
        self.list = ArtifactList::default();
        // Pagination is per space; the kind filter is a browsing preference.
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
                && self.in_space(snapshot)
        })
    }

    /// Whether an artifact belongs to the active space. Empty means its
    /// repository is unknown: shown in every space instead of hidden.
    fn in_space(&self, snapshot: &Snapshot) -> bool {
        self.space_filter.as_deref().is_none_or(|space| {
            snapshot.artifact.space.trim().is_empty()
                || crate::data::space_eq(&snapshot.artifact.space, space)
        })
    }

    /// Move the sidebar selection one step for navigation-mode `j`/`k`.
    /// Clamps at the ends like pane movement, keeps any draft, and leaves
    /// focus where the pane left it: the detail preview follows the new
    /// selection while the mode stays open for repeated presses.
    pub(crate) fn move_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.draft.is_some() || self.saving {
            return;
        }
        let visible: Vec<Snapshot> = self.visible_artifacts().cloned().collect();
        if visible.is_empty() {
            return;
        }
        let Some(current) = self
            .selected_id
            .as_ref()
            .and_then(|id| visible.iter().position(|s| &s.artifact.id == id))
        else {
            let end = if down { 0 } else { visible.len() - 1 };
            self.select(visible.get(end).cloned(), cx);
            self.error = None;
            cx.notify();
            return;
        };
        let next = crate::navigation::move_index(current, visible.len(), down);
        if next == current {
            return;
        }
        let selection = visible.get(next).cloned();
        self.select(selection, cx);
        self.error = None;
        cx.notify();
    }
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.opening {
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
            // Space filtering happens in the store, before the page limit:
            // another space's records must not consume the page.
            space: self.space_filter.clone(),
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
                                        && this.in_space(&snapshot)
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
                            if let Some(snapshot) = &pinned
                                && !this
                                    .list
                                    .artifacts
                                    .iter()
                                    .any(|s| s.artifact.id == snapshot.artifact.id)
                            {
                                this.list.artifacts.insert(0, snapshot.clone());
                            }
                            let selection = this
                                .selected_id
                                .as_ref()
                                .and_then(|id| {
                                    this.list.artifacts.iter().find(|s| &s.artifact.id == id)
                                })
                                .cloned()
                                .or(pinned)
                                .filter(|s| {
                                    this.kind_filter.is_none_or(|kind| s.artifact.kind == kind)
                                })
                                .or_else(|| this.draft.as_ref().map(|d| d.snapshot.clone()))
                                .or_else(|| this.visible_artifacts().next().cloned());
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
    /// Open a referenced resource even if it is archived or beyond the current
    /// page. Do not replace a draft or let an older list refresh win the race.
    pub(crate) fn open_by_id(&mut self, id: String, cx: &mut Context<Self>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.draft.is_none() && !self.saving,
            "Save or cancel the resource draft before opening another resource"
        );
        let root = self
            .root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Portable data is unavailable"))?;
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        self.opening = true;
        self.refresh.generation = self.refresh.generation.wrapping_add(1);
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    ArtifactStore::new(&root)
                        .get(&id)
                        .map_err(|e| format!("{e:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.open_generation {
                    return;
                }
                this.opening = false;
                match result {
                    Ok(snapshot) if this.draft.is_none() && !this.saving => {
                        this.kind_filter = None;
                        this.include_archived |= snapshot.artifact.archived;
                        if !this
                            .list
                            .artifacts
                            .iter()
                            .any(|s| s.artifact.id == snapshot.artifact.id)
                        {
                            this.list.artifacts.insert(0, snapshot.clone());
                        }
                        this.select(Some(snapshot), cx);
                    }
                    Ok(_) => {
                        this.error = Some(
                            "Save or cancel the resource draft before opening another resource"
                                .into(),
                        )
                    }
                    Err(error) => this.error = Some(format!("Could not open resource: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        Ok(())
    }

    fn select(&mut self, snapshot: Option<Snapshot>, cx: &mut Context<Self>) {
        self.open_generation = self.open_generation.wrapping_add(1);
        self.opening = false;
        let selected_id = snapshot.as_ref().map(|s| s.artifact.id.clone());
        let content_changed = self.selected.as_ref().map(|s| &s.artifact.content)
            != snapshot.as_ref().map(|s| &s.artifact.content);
        if self.selected_id != selected_id {
            self.active_comment = None;
        }
        if snapshot
            .as_ref()
            .is_some_and(|s| s.artifact.kind == Kind::Tutorial)
        {
            // Tutorials render as one sandboxed HTML document: no Markdown
            // preview, outline, or comment plumbing applies.
            self.preview = None;
            if self.selected_id != selected_id || content_changed || self.tutorial.is_none() {
                match (self.tutorial.as_ref(), snapshot.as_ref()) {
                    (Some(tutorial), Some(s))
                        if self.selected_id.as_ref() == Some(&s.artifact.id) =>
                    {
                        let root = self.root.clone();
                        tutorial.update(cx, |view, cx| {
                            view.show(
                                root,
                                &s.artifact.id,
                                &s.artifact.title,
                                s.artifact.content.clone(),
                                cx,
                            )
                        });
                    }
                    (_, Some(s)) => {
                        let root = self.root.clone();
                        let tutorial = cx.new(TutorialView::new);
                        tutorial.update(cx, |view, cx| {
                            view.show(
                                root,
                                &s.artifact.id,
                                &s.artifact.title,
                                s.artifact.content.clone(),
                                cx,
                            )
                        });
                        self.tutorial = Some(tutorial);
                    }
                    _ => self.tutorial = None,
                }
            }
        } else {
            self.tutorial = None;
            if self.selected_id != selected_id || content_changed || self.preview.is_none() {
                match (self.preview.as_ref(), snapshot.as_ref()) {
                    (Some(preview), Some(s))
                        if self.selected_id.as_ref() == Some(&s.artifact.id) =>
                    {
                        preview.update(cx, |view, cx| {
                            view.set_content(s.artifact.content.clone().into(), cx)
                        })
                    }
                    (_, Some(s)) => {
                        let preview = cx.new(|cx| {
                            let mut view =
                                PreviewView::embedded(s.artifact.content.clone().into(), cx);
                            view.set_reference_root(self.root.clone(), cx);
                            view
                        });
                        cx.subscribe(
                            &preview,
                            |_, _, event: &crate::preview::OpenReference, cx| {
                                cx.emit(event.clone())
                            },
                        )
                        .detach();
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
        }
        self.selected_id = selected_id;
        self.selected = snapshot;
        self.sync_preview_comments(cx);
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
        if self.draft.is_some() || self.saving {
            return;
        }
        let Some(snapshot) = self.selected.clone() else {
            return;
        };
        // Tutorials are view-only HTML: no Markdown draft and no comments.
        if snapshot.artifact.kind == Kind::Tutorial {
            return;
        }
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
            let mut input = if document {
                TextareaState::new(window, cx).rows(24)
            } else {
                // Sidebar comment drafts start roomier than the 2-row default
                // and grow with content instead of trapping long feedback.
                TextareaState::new(window, cx).auto_grow(4, 10)
            };
            input.set_value(value, window, cx);
            input
        });
        self.draft = Some(Draft {
            input,
            snapshot,
            comment,
            document,
            block: None,
            selection: None,
            quote: None,
        });
        if !document {
            self.show_comments = true;
        }
        if let Some(draft) = &self.draft {
            draft.input.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.sync_preview_comments(cx);
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
        } else if let Some(range) = &draft.selection {
            Mutation::Comment(CommentChange::CreateSelection {
                body: value,
                range: range.clone(),
                quote: draft.quote.clone(),
            })
        } else if let Some(block) = draft.block {
            Mutation::Comment(CommentChange::CreateBlock {
                body: value,
                block,
                quote: draft.quote.clone(),
            })
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
        self.sync_preview_comments(cx);
        self.error = None;
        self.refresh.generation = self.refresh.generation.wrapping_add(1);
        let scope = self.scope.clone();
        let space = self.space_filter.clone();
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
                let same_context = this.scope == scope && this.space_filter == space;
                if !same_context && result.is_ok() && close_draft {
                    this.saved_drafts
                        .remove(&DraftKey::new(&scope, space.as_deref()));
                }
                if same_context {
                    match result {
                        Ok(snapshot) => {
                            if close_draft && this.draft.as_ref().is_some_and(|d| !d.document) {
                                let id =
                                    this.draft.as_ref().and_then(|d| d.comment.clone()).or_else(
                                        || snapshot.artifact.comments.last().map(|c| c.id.clone()),
                                    );
                                this.active_comment = id;
                                if let Some(index) = snapshot
                                    .artifact
                                    .comments
                                    .iter()
                                    .position(|c| Some(&c.id) == this.active_comment.as_ref())
                                {
                                    this.comments_scroll.scroll_to_item(index);
                                }
                            }
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
                this.sync_preview_comments(cx);
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
                let mut menu = menu;
                if menu_snapshot.artifact.kind == Kind::Tutorial {
                    let view = view.clone();
                    menu = menu
                        .item({
                            PopupMenuItem::new("Open in browser").on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_tutorial_in_browser(cx);
                                });
                            })
                        })
                        .item({
                            let content = menu_snapshot.artifact.content.clone();
                            PopupMenuItem::new("Copy HTML").on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                                window.push_notification("Copied HTML", cx);
                            })
                        })
                        .item({
                            let id = menu_snapshot.artifact.id.clone();
                            PopupMenuItem::new("Copy ID").on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                                window.push_notification("Copied artifact ID", cx);
                            })
                        });
                } else {
                    menu = menu
                        .item({
                            let content = menu_snapshot.artifact.content.clone();
                            PopupMenuItem::new("Copy Markdown").on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                                window.push_notification("Copied Markdown", cx);
                            })
                        })
                        .item({
                            let id = menu_snapshot.artifact.id.clone();
                            PopupMenuItem::new("Copy ID").on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                                window.push_notification("Copied artifact ID", cx);
                            })
                        })
                        .item({
                            let view = view.clone();
                            PopupMenuItem::new("Edit Markdown")
                                .on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.edit(true, None, window, cx);
                                    });
                                })
                        });
                }
                menu.item({
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
            .on_open_change(cx.listener(|this, open, _, cx| {
                if let Some(tutorial) = &this.tutorial {
                    tutorial.update(cx, |view, cx| view.set_menu_open(*open, cx));
                }
            }))
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

/// Distinct sidebar/detail pill per artifact type so kinds are scannable
/// without reading. Hues are type-neutral on purpose: green/red are reserved
/// elsewhere for status (open/pass/clean vs closed/fail), so `Plan` uses sky
/// instead of green and `RFC` uses violet for proposal-like content.
/// `Review` uses teal and `Tutorial` uses rose to stay distinct from
/// sky/violet/amber while avoiding status hues.
fn kind_tag_color(kind: Kind) -> ColorName {
    match kind {
        Kind::Rfc => ColorName::Violet,
        Kind::Plan => ColorName::Sky,
        Kind::Note => ColorName::Amber,
        Kind::Review => ColorName::Teal,
        Kind::Tutorial => ColorName::Rose,
    }
}

fn kind_tag(kind: Kind) -> Tag {
    Tag::color(kind_tag_color(kind))
        .with_size(Size::Small)
        .rounded_full()
        .flex_none()
        .child(kind.label())
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
            .focus(|style| style.border_1().border_color(cx.theme().ring))
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
                                    .child(kind_tag(artifact.kind))
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
            .focus(|style| style.border_1().border_color(cx.theme().ring))
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0();
        if let Some(snapshot) = self.selected.clone() {
            let artifact = &snapshot.artifact;
            let archived = artifact.archived;
            // Right padding stays tight so the document scrollbar docks
            // beside the outline rail instead of floating mid-pane.
            let mut main = v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .min_h_0()
                .p_4()
                .pt_2()
                .pr(px(4.))
                .gap_2();
            if let Some(error) = &self.error {
                main = main.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            // Single compact meta line — kind, recency, ID, repository, and
            // originating sessions share one wrapping row so the document
            // starts higher. The ID copies on click for CLI and agent use.
            let mut meta = h_flex()
                .gap_2()
                .items_center()
                .child(kind_tag(artifact.kind))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(updated_label(artifact.updated_at, current_unix_secs())),
                );
            {
                let artifact_id = artifact.id.clone();
                let tooltip_id = artifact_id.clone();
                let label_id = artifact_id.clone();
                meta = meta
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("·"),
                    )
                    .child(
                        Button::new("copy-artifact-id")
                            .ghost()
                            .small()
                            .compact()
                            .icon(IconName::Copy)
                            .label(artifact_id.clone())
                            .tooltip(format!("Copy artifact ID: {tooltip_id}"))
                            .accessibility_label(format!("Copy artifact ID {label_id}"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    artifact_id.clone(),
                                ));
                                window.push_notification("Copied artifact ID", cx);
                            }),
                    );
            }
            if let Some(repository) = artifact.repository.clone() {
                meta = meta
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("·"),
                    )
                    .child(
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
            if !artifact.sessions.is_empty() {
                meta = meta
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("·"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("From"),
                    );
                for (index, origin) in artifact.sessions.iter().enumerate() {
                    let origin = origin.clone();
                    meta =
                        meta.child(
                            Button::new(("origin-session", index))
                                .ghost()
                                .small()
                                .compact()
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
            }
            let is_tutorial = artifact.kind == Kind::Tutorial;
            let meta_row = h_flex().w_full().justify_center().child(
                meta.flex_wrap()
                    .w_full()
                    .max_w(px(crate::preview::READING_WIDTH))
                    .pr(px(16.)),
            );
            main = main.child(meta_row);
            if let Some(draft) = self.draft.as_ref().filter(|draft| draft.document) {
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
            } else if let Some(tutorial) = self.tutorial.as_ref().filter(|_| is_tutorial) {
                // The sandbox draws the outline below this native rail header.
                // Keep actions at the same right edge as Markdown artifacts.
                detail = detail
                    .child(
                        h_flex()
                            .flex_none()
                            .items_stretch()
                            .child(main.h_auto().pb_2())
                            .child(
                                h_flex()
                                    .w(px(310.))
                                    .flex_none()
                                    .items_start()
                                    .border_l_1()
                                    .border_color(cx.theme().border)
                                    .px_2()
                                    .py_2()
                                    .gap_1()
                                    .child(
                                        Button::new("resource-outline-tab")
                                            .ghost()
                                            .small()
                                            .label("On this page")
                                            .bg(cx.theme().accent),
                                    )
                                    .child(div().flex_1())
                                    .child(self.render_options_menu(
                                        snapshot.clone(),
                                        archived,
                                        cx,
                                    )),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(tutorial.clone()));
            } else {
                if let Some(preview) = &self.preview {
                    main = main.child(div().flex_1().min_h_0().child(preview.clone()));
                }
                let rail = v_flex()
                    .w(px(310.))
                    .flex_none()
                    .h_full()
                    .min_h_0()
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .px_2()
                            .py_2()
                            .gap_1()
                            .child(
                                Button::new("resource-outline-tab")
                                    .ghost()
                                    .small()
                                    .label("On this page")
                                    .when(!self.show_comments, |b| b.bg(cx.theme().accent))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_comments = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("resource-comments-tab")
                                    .ghost()
                                    .small()
                                    .label(format!("Comments ({})", artifact.comments.len()))
                                    .when(self.show_comments, |b| b.bg(cx.theme().accent))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_comments = true;
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1())
                            .child(self.render_options_menu(snapshot.clone(), archived, cx)),
                    )
                    .child(if self.show_comments {
                        self.render_comments_panel(&snapshot, cx).into_any_element()
                    } else {
                        v_flex()
                            .track_focus(&self.outline_focus)
                            .flex_1()
                            .min_h_0()
                            .when(self.toc.is_empty(), |d| {
                                d.child(
                                    div()
                                        .p_3()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No headings on this page"),
                                )
                            })
                            .child(self.render_toc_panel(cx))
                            .into_any_element()
                    });
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
            if self.refresh.busy || self.opening {
                detail = detail.child("Loading resources…");
            } else if self.error.is_some() || !self.list.errors.is_empty() {
                detail = detail.child(Button::new("retry-resources").label("Retry").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.error = None;
                        this.refresh(cx);
                    }),
                ));
            } else if self.kind_filter.is_some() {
                detail = detail.child(
                    crate::empty_state::empty_state(
                        gpui_kit::component::IconName::Search,
                        "No matching resources",
                        "Try showing every resource type.",
                    )
                    .content(
                        gpui_kit::component::empty::EmptyContent::new().child(
                            Button::new("clear-resource-filter")
                                .label("Clear filter")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.set_kind_filter(None, cx)),
                                ),
                        ),
                    ),
                );
            } else if self.scope == Scope::Repository(None) {
                detail = detail.child(crate::empty_state::empty_state(
                    gpui_kit::component::IconName::Folder,
                    "Repository not registered",
                    "Add this checkout from Projects to give its resources a home.",
                ));
            } else {
                detail = detail.child(crate::empty_state::empty_state(
                    gpui_kit::component::IconName::FileText, "No resources yet",
                    "Ask your agent to save a plan, RFC, note, review, or tutorial here. It will appear automatically.",
                ));
            }
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
