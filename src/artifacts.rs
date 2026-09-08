//! Read-focused global artifact browser. All store work is serialized off the
//! UI thread; the selected ID is read independently of the browse filter.
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window, div,
};

use crate::data::DataRoot;
use crate::data::artifacts::{ArtifactList, ArtifactStore, Kind, ListOptions, Snapshot};
use crate::preview::PreviewView;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PAGE_SIZE: usize = 100;

#[derive(Default)]
struct Refresh {
    busy: bool,
    pending: bool,
    generation: u64,
}

impl Refresh {
    fn request(&mut self) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        if self.busy {
            self.pending = true;
            return None;
        }
        self.busy = true;
        self.pending = false;
        Some(self.generation)
    }

    fn finish(&mut self, generation: u64) -> bool {
        self.busy = false;
        generation == self.generation
    }
}

struct Loaded {
    list: Result<ArtifactList, String>,
    selected: Option<Result<Snapshot, String>>,
}

fn load(root: &DataRoot, include_archived: bool, limit: usize, selected: Option<&str>) -> Loaded {
    let store = ArtifactStore::new(root);
    Loaded {
        list: store
            .list(&ListOptions {
                include_archived,
                limit: Some(limit),
            })
            .map_err(|e| format!("Could not list artifacts: {e:#}")),
        selected: selected.map(|id| {
            store
                .get(id)
                .map_err(|e| format!("Artifact {id} is missing or unreadable: {e:#}"))
        }),
    }
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Rfc => "RFC",
        Kind::Plan => "Plan",
        Kind::Note => "Note",
    }
}

pub(crate) struct ArtifactBrowser {
    root: Option<DataRoot>,
    active: bool,
    refresh: Refresh,
    include_archived: bool,
    limit: usize,
    list: ArtifactList,
    list_error: Option<String>,
    selected_id: Option<String>,
    selected: Option<Snapshot>,
    selected_error: Option<String>,
    mutation_error: Option<String>,
    preview: Option<Entity<PreviewView>>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
}

impl ArtifactBrowser {
    pub(crate) fn new(root: Option<DataRoot>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        if this.active && !this.refresh.busy {
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
            active: false,
            refresh: Refresh::default(),
            include_archived: false,
            limit: PAGE_SIZE,
            list: ArtifactList::default(),
            list_error: None,
            selected_id: None,
            selected: None,
            selected_error: None,
            mutation_error: None,
            preview: None,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.refresh(cx);
        }
    }

    /// Shared ID-based viewer entry point for future task/subtask links. Archived
    /// records resolve even when they are excluded from the browser list.
    pub(crate) fn open(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected_id = Some(id);
        self.selected = None;
        self.preview = None;
        self.selected_error = None;
        self.mutation_error = None;
        self.refresh(cx);
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            self.list_error = Some("Portable data is unavailable".into());
            self.selected_error = self
                .selected_id
                .as_ref()
                .map(|_| "Portable data is unavailable".into());
            cx.notify();
            return;
        };
        let Some(generation) = self.refresh.request() else {
            cx.notify();
            return;
        };
        let include_archived = self.include_archived;
        let limit = self.limit;
        let selected = self.selected_id.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    load(&root, include_archived, limit, selected.as_deref())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.refresh.finish(generation) {
                    this.apply(loaded, cx);
                }
                if this.refresh.pending {
                    this.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        match loaded.list {
            Ok(list) => {
                self.list = list;
                self.list_error = None;
            }
            Err(error) => {
                self.list = ArtifactList::default();
                self.list_error = Some(error);
            }
        }
        match loaded.selected {
            Some(Ok(snapshot)) => {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|old| old.artifact.content != snapshot.artifact.content)
                {
                    let content = snapshot.artifact.content.clone();
                    if let Some(preview) = &self.preview {
                        preview.update(cx, |view, cx| view.set_content(content.into(), cx));
                    } else {
                        self.preview = Some(cx.new(|cx| PreviewView::embedded(content.into(), cx)));
                    }
                }
                self.selected = Some(snapshot);
                self.selected_error = None;
            }
            Some(Err(error)) => {
                self.selected = None;
                self.preview = None;
                self.selected_error = Some(error);
            }
            None => {}
        }
    }

    fn archive(&mut self, cx: &mut Context<Self>) {
        if self.refresh.busy {
            return;
        }
        let (Some(root), Some(snapshot)) = (self.root.clone(), self.selected.clone()) else {
            return;
        };
        self.refresh.busy = true;
        self.mutation_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    ArtifactStore::new(&root)
                        .set_archived(
                            &snapshot.artifact.id,
                            &snapshot.revision,
                            !snapshot.artifact.archived,
                        )
                        .map_err(|e| format!("Archive change failed; reload and retry: {e:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh.busy = false;
                this.mutation_error = result.err();
                this.refresh(cx);
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for ArtifactBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = v_flex()
            .id("artifact-browser")
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.focus_handle.focus(window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }),
            )
            .size_full()
            .min_h_0()
            .gap_3()
            .p_4()
            .child(
                h_flex()
                    .gap_3()
                    .child(div().text_2xl().font_semibold().child("Artifacts"))
                    .child(
                        Button::new("refresh-artifacts")
                            .label(if self.refresh.busy {
                                "Refreshing…"
                            } else {
                                "Refresh"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            );
        if let Some(id) = self.selected_id.clone() {
            let copy_id = id.clone();
            body = body.child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .child(
                        Button::new("artifact-back")
                            .ghost()
                            .label("← All artifacts")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected_id = None;
                                this.selected = None;
                                this.preview = None;
                                this.selected_error = None;
                                this.mutation_error = None;
                                this.refresh(cx);
                            })),
                    )
                    .child(div().child(id))
                    .child(Button::new("copy-artifact-id").label("Copy ID").on_click(
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            window.push_notification("Artifact ID copied", cx);
                        },
                    )),
            );
            if let Some(snapshot) = &self.selected {
                body = body.child(
                    h_flex()
                        .gap_3()
                        .flex_wrap()
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .child(snapshot.artifact.title.clone()),
                        )
                        .child(kind_label(snapshot.artifact.kind))
                        .when(snapshot.artifact.archived, |row| row.child("Archived"))
                        .child(
                            Button::new("archive-artifact")
                                .label(if snapshot.artifact.archived {
                                    "Unarchive"
                                } else {
                                    "Archive"
                                })
                                .disabled(self.refresh.busy)
                                .on_click(cx.listener(|this, _, _, cx| this.archive(cx))),
                        ),
                );
                if snapshot.artifact.content.is_empty() {
                    body = body.child("This artifact has no Markdown content.");
                }
            } else if self.selected_error.is_none() {
                body = body.child("Loading artifact…");
            }
            for error in [&self.selected_error, &self.mutation_error]
                .into_iter()
                .flatten()
            {
                body = body.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            if let Some(preview) = &self.preview {
                let focus = preview.clone();
                body = body
                    .child(
                        Button::new("read-artifact")
                            .ghost()
                            .label("Read document (focus keyboard scrolling)")
                            .on_click(move |_, window, cx| {
                                focus.focus_handle(cx).focus(window, cx)
                            }),
                    )
                    .child(div().flex_1().min_h_0().child(preview.clone()));
            }
        } else {
            body = body.child(Button::new("include-archived").label(if self.include_archived { "Showing active + archived" } else { "Show archived too" })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.include_archived = !this.include_archived; this.limit = PAGE_SIZE; this.refresh(cx);
                })))
                .child("Tab to navigate · Enter/Space to open or copy · Documents refresh every 2 seconds");
            let mut list = v_flex()
                .id("artifact-list")
                .flex_1()
                .min_h_0()
                .gap_2()
                .overflow_y_scroll()
                .track_scroll(&self.scroll);
            if let Some(error) = &self.list_error {
                list = list.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            for error in &self.list.errors {
                list = list.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            if self.list.artifacts.is_empty() && self.list_error.is_none() {
                list = list.child(if self.refresh.busy { "Loading artifacts…" } else { "No artifacts. Ask an agent to create an RFC, plan, or note with devcroft artifact create." });
            }
            for snapshot in &self.list.artifacts {
                let artifact = &snapshot.artifact;
                let id = artifact.id.clone();
                let copy_id = id.clone();
                list = list.child(
                    h_flex()
                        .gap_2()
                        .flex_none()
                        .flex_wrap()
                        .child(
                            gpui_kit::base::Button::new(SharedString::from(format!("open-{id}")))
                                .accessibility_label(format!("Open {} · {id}", artifact.title))
                                .flex_1()
                                .min_w_0()
                                .flex_col()
                                .items_start()
                                .p_3()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .hover(|style| style.bg(cx.theme().secondary))
                                .focus(|style| style.border_color(cx.theme().ring))
                                .child(
                                    div()
                                        .w_full()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .font_semibold()
                                        .child(artifact.title.clone()),
                                )
                                .child(div().text_sm().child(format!(
                                    "{} · {id}{}",
                                    kind_label(artifact.kind),
                                    if artifact.archived {
                                        " · Archived"
                                    } else {
                                        ""
                                    }
                                )))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open(id.clone(), cx);
                                    this.focus_handle.focus(window, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("copy-{copy_id}")))
                                .label("Copy ID")
                                .on_click(move |_, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_id.clone(),
                                    ));
                                    window.push_notification("Artifact ID copied", cx);
                                }),
                        ),
                );
            }
            if self.list.truncated {
                list = list.child(
                    Button::new("more-artifacts")
                        .label("Load 100 more")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.limit = this.limit.saturating_add(PAGE_SIZE);
                            this.refresh(cx);
                        })),
                );
            }
            body = body.child(list);
        }
        body
    }
}

#[cfg(test)]
#[path = "artifact_browser_tests.rs"]
mod tests;
