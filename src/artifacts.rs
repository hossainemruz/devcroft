//! Repository resources, shared by the workspace and global artifact browser.
use crate::data::DataRoot;
use crate::data::artifacts::{
    ArtifactList, ArtifactPatch, ArtifactStore, CommentChange, ListOptions, Snapshot,
};
use crate::preview::PreviewView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
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
    limit: usize,
    list: ArtifactList,
    selected_id: Option<String>,
    selected: Option<Snapshot>,
    error: Option<String>,
    preview: Option<Entity<PreviewView>>,
    draft: Option<Draft>,
    saved_drafts: std::collections::HashMap<Scope, Draft>,
    saving: bool,
    pub(crate) focus_handle: FocusHandle,
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
            limit: PAGE_SIZE,
            list: ArtifactList::default(),
            selected_id: None,
            selected: None,
            error: None,
            preview: None,
            draft: None,
            saved_drafts: Default::default(),
            saving: false,
            focus_handle: cx.focus_handle(),
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
            self.error = None;
            self.list = ArtifactList::default();
            self.limit = PAGE_SIZE;
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
                    self.preview = Some(
                        cx.new(|cx| PreviewView::embedded(s.artifact.content.clone().into(), cx)),
                    )
                }
                _ => self.preview = None,
            }
        }
        self.selected_id = snapshot.as_ref().map(|s| s.artifact.id.clone());
        self.selected = snapshot;
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
}
enum Mutation {
    Document(String),
    Comment(CommentChange),
    Archive,
}

impl Render for ArtifactBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sidebar = v_flex()
            .id("resource-sidebar")
            .w(px(260.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().font_semibold().child("Resources"))
            .child(
                Button::new("resource-archived")
                    .ghost()
                    .label(if self.include_archived {
                        "Hide archived"
                    } else {
                        "Show archived"
                    })
                    .disabled(self.draft.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_include_archived(!this.include_archived, cx)
                    })),
            );
        for snapshot in &self.list.artifacts {
            let snapshot = snapshot.clone();
            let artifact = &snapshot.artifact;
            sidebar = sidebar.child(
                Button::new(SharedString::from(artifact.id.clone()))
                    .ghost()
                    .w_full()
                    .label(artifact.title.clone())
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
        let mut detail = v_flex().flex_1().min_w_0().h_full().p_4().gap_3();
        if let Some(error) = &self.error {
            detail = detail.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        if let Some(snapshot) = self.selected.clone() {
            let artifact = &snapshot.artifact;
            detail =
                detail.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_lg()
                                .font_semibold()
                                .child(artifact.title.clone()),
                        )
                        .child(
                            Button::new("edit-resource")
                                .label("Edit Markdown")
                                .disabled(self.draft.is_some() || self.saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.edit(true, None, window, cx)
                                })),
                        )
                        .child(
                            Button::new("archive-resource")
                                .label(if artifact.archived {
                                    "Unarchive"
                                } else {
                                    "Archive"
                                })
                                .disabled(self.draft.is_some() || self.saving)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(s) = this.selected.clone() {
                                        this.mutate(s, Mutation::Archive, false, cx);
                                    }
                                })),
                        ),
                );
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
                detail = detail.child(origins);
            }
            if let Some(draft) = &self.draft {
                detail = detail
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
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            )
                            .child(
                                Button::new("cancel-resource-edit")
                                    .label("Cancel")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.draft = None;
                                        this.error = None;
                                        cx.notify();
                                    })),
                            ),
                    );
            } else {
                if let Some(preview) = &self.preview {
                    detail = detail.child(div().flex_1().min_h_0().child(preview.clone()));
                }
                let mut comments = v_flex()
                    .id("artifact-comments")
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
                detail = detail.child(comments);
            }
        } else {
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
