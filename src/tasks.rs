//! Shared read-focused task views. Repository scope affects lists only, never
//! the selected task's full cross-repository breakdown.
use crate::artifacts::{ArtifactBrowser, Refresh};
use crate::data::DataRoot;
use crate::data::tasks::{ListOptions, Snapshot, Status, Task, TaskList, TaskStore};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window, div,
};
use std::{collections::HashMap, time::Duration};

const PAGE_SIZE: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Global,
    Repository(Option<String>),
}

struct Loaded {
    list: Result<TaskList, String>,
    selected: Option<Result<Snapshot, String>>,
}

fn load(
    root: &DataRoot,
    scope: &Scope,
    archived: bool,
    limit: usize,
    selected: Option<&str>,
) -> Loaded {
    let store = TaskStore::new(root);
    let list = match scope {
        Scope::Repository(None) => Err(
            "This checkout has no repository key. Link it in Settings to browse repository tasks."
                .into(),
        ),
        _ => store
            .list(&ListOptions {
                repository: match scope {
                    Scope::Repository(key) => key.clone(),
                    Scope::Global => None,
                },
                include_archived: archived,
                limit: Some(limit),
            })
            .map_err(|e| format!("Could not list tasks: {e:#}")),
    };
    Loaded {
        list,
        selected: selected.map(|id| {
            store
                .get(id)
                .map_err(|e| format!("Task {id} is missing or unreadable: {e:#}"))
        }),
    }
}

pub(crate) fn progress_label(task: &Task) -> String {
    let progress = task.progress();
    if !progress.is_planned() {
        "Not planned".into()
    } else if progress.is_complete() {
        format!("Complete · {}/{} done", progress.completed, progress.total)
    } else {
        format!("{}/{} done", progress.completed, progress.total)
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Todo => "todo",
        Status::Doing => "doing",
        Status::Blocked => "blocked",
        Status::Done => "done",
    }
}

pub(crate) struct OpenedTask;

pub(crate) struct TaskBrowser {
    root: Option<DataRoot>,
    scope: Scope,
    active: bool,
    recent: bool,
    /// When true the workspace titlebar owns the page title, back button,
    /// and archived filter (global Tasks page). The body omits its duplicate
    /// title/refresh/archived/back controls and stays live via the poll.
    /// Repository Tasks tab and dashboard recent keep body chrome (false).
    titlebar_chrome: bool,
    refresh: Refresh,
    archived: bool,
    limit: usize,
    list: TaskList,
    error: Option<String>,
    selected_id: Option<String>,
    selected: Option<Snapshot>,
    selected_error: Option<String>,
    mutation_error: Option<String>,
    descriptions: HashMap<String, (String, Entity<TextViewState>)>,
    artifacts: Entity<ArtifactBrowser>,
    artifact_open: bool,
    list_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
    pub(crate) focus_handle: FocusHandle,
}

impl EventEmitter<OpenedTask> for TaskBrowser {}

impl TaskBrowser {
    pub(crate) fn new(root: Option<DataRoot>, scope: Scope, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.active && !this.refresh.busy {
                            this.reload(cx);
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
            artifacts: cx.new(|cx| ArtifactBrowser::new(root.clone(), cx)),
            root,
            scope,
            active: false,
            recent: false,
            titlebar_chrome: false,
            refresh: Refresh::default(),
            archived: false,
            limit: PAGE_SIZE,
            list: TaskList::default(),
            error: None,
            selected_id: None,
            selected: None,
            selected_error: None,
            mutation_error: None,
            descriptions: HashMap::new(),
            artifact_open: false,
            list_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.active = active;
        self.artifacts.update(cx, |view, cx| {
            view.set_active(active && self.artifact_open, cx)
        });
        if active {
            self.reload(cx);
        }
    }

    pub(crate) fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.back(cx);
            self.list = TaskList::default();
            self.list_scroll = ScrollHandle::new();
        }
    }

    pub(crate) fn show_recent(&mut self, recent: bool, cx: &mut Context<Self>) {
        self.recent = recent;
        self.limit = if recent { 4 } else { PAGE_SIZE };
        if recent {
            self.archived = false;
            // Returning from an archived/paginated list must not flash those
            // records into Home while the bounded active reload is in flight.
            self.list.tasks.retain(|snapshot| !snapshot.task.archived);
            self.list.tasks.truncate(4);
            self.back(cx);
        }
        self.set_active(true, cx);
    }

    pub(crate) fn set_titlebar_owned(&mut self, owned: bool) {
        self.titlebar_chrome = owned;
    }

    pub(crate) fn include_archived(&self) -> bool {
        self.archived
    }

    pub(crate) fn set_include_archived(&mut self, include: bool, cx: &mut Context<Self>) {
        if self.archived == include {
            return;
        }
        self.archived = include;
        self.limit = PAGE_SIZE;
        self.reload(cx);
    }

    pub(crate) fn is_artifact_open(&self) -> bool {
        self.artifact_open
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selected_id.is_some()
    }

    pub(crate) fn close_artifact(&mut self, cx: &mut Context<Self>) {
        self.artifact_open = false;
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.reload(cx);
    }

    pub(crate) fn back_to_list(&mut self, cx: &mut Context<Self>) {
        self.back(cx);
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.selected_id = None;
        self.selected = None;
        self.selected_error = None;
        self.mutation_error = None;
        self.descriptions.clear();
        self.artifact_open = false;
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.reload(cx);
    }

    fn open(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.recent {
            self.limit = PAGE_SIZE;
        }
        self.recent = false;
        self.selected_id = Some(id);
        self.selected = None;
        self.selected_error = None;
        self.mutation_error = None;
        self.descriptions.clear();
        self.detail_scroll = ScrollHandle::new();
        self.focus_handle.focus(window, cx);
        self.reload(cx);
        cx.emit(OpenedTask);
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            self.error = Some("Portable data is unavailable".into());
            cx.notify();
            return;
        };
        let Some(generation) = self.refresh.request() else {
            cx.notify();
            return;
        };
        let scope = self.scope.clone();
        let archived = self.archived;
        let limit = self.limit;
        let selected = self.selected_id.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    load(&root, &scope, archived, limit, selected.as_deref())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.refresh.finish(generation) {
                    this.apply(loaded, cx);
                }
                if this.refresh.pending {
                    this.reload(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn refresh_all(&mut self, cx: &mut Context<Self>) {
        if self.artifact_open && self.active {
            self.artifacts.update(cx, |view, cx| view.refresh(cx));
        }
        self.reload(cx);
    }

    fn apply(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        match loaded.list {
            Ok(list) => {
                self.list = list;
                self.error = None;
            }
            Err(error) => {
                self.list = TaskList::default();
                self.error = Some(error);
            }
        }
        match loaded.selected {
            Some(Ok(snapshot)) => {
                let sources =
                    std::iter::once((snapshot.task.id.clone(), snapshot.task.description.clone()))
                        .chain(
                            snapshot
                                .task
                                .subtasks
                                .iter()
                                .map(|s| (s.id.clone(), s.description.clone())),
                        )
                        .collect::<HashMap<_, _>>();
                self.descriptions.retain(|id, _| sources.contains_key(id));
                for (id, content) in sources {
                    if self
                        .descriptions
                        .get(&id)
                        .is_none_or(|(old, _)| old != &content)
                    {
                        let state = cx.new(|cx| TextViewState::markdown(&content, cx));
                        self.descriptions.insert(id, (content, state));
                    }
                }
                self.selected = Some(snapshot);
                self.selected_error = None;
            }
            Some(Err(error)) => {
                self.selected = None;
                self.descriptions.clear();
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
        let id = snapshot.task.id.clone();
        self.refresh.busy = true;
        self.mutation_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    TaskStore::new(&root)
                        .set_archived(
                            &snapshot.task.id,
                            &snapshot.revision,
                            !snapshot.task.archived,
                        )
                        .map_err(|e| format!("Archive change failed; reload and retry: {e:#}"))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh.busy = false;
                if this.selected_id.as_ref() == Some(&id) {
                    this.mutation_error = result.err();
                }
                this.reload(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn links(&self, prefix: &str, ids: &[String], cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().gap_2().flex_wrap().children(ids.iter().map(|id| {
            let id = id.clone();
            Button::new(SharedString::from(format!("{prefix}-{id}")))
                .ghost()
                .label(format!("Artifact: {id}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.artifact_open = true;
                    this.artifacts.update(cx, |view, cx| {
                        view.open(id.clone(), cx);
                        view.set_active(true, cx);
                    });
                    cx.notify();
                }))
        }))
    }

    fn description(&self, id: &str) -> impl IntoElement {
        let mut body = div().w_full().flex_none();
        if let Some((content, state)) = self.descriptions.get(id)
            && !content.is_empty()
        {
            body = body.child(TextView::new(state));
        }
        body
    }
}

impl Render for TaskBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.artifact_open {
            // Titlebar back drills out (artifact → task → list → origin), so
            // no duplicate back button when the titlebar owns chrome.
            let mut page = v_flex()
                .id("task-artifact")
                .track_focus(&self.focus_handle)
                .size_full()
                .min_h_0()
                .gap_3()
                .p_3();
            if !self.titlebar_chrome {
                page = page.child(
                    Button::new("back-to-task")
                        .label("← Back to task")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.artifact_open = false;
                            this.artifacts
                                .update(cx, |view, cx| view.set_active(false, cx));
                            this.reload(cx);
                            this.focus_handle.focus(window, cx);
                        })),
                );
            }
            return page
                .child(div().flex_1().min_h_0().child(self.artifacts.clone()))
                .into_any_element();
        }
        let mut body = v_flex()
            .id("task-browser")
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    if event.keystroke.key == "escape" && this.selected_id.is_some() {
                        this.back(cx);
                        this.focus_handle.focus(window, cx);
                    } else {
                        let scroll = if this.selected_id.is_some() {
                            &this.detail_scroll
                        } else {
                            &this.list_scroll
                        };
                        let height = f32::from(scroll.bounds().size.height) * 0.85;
                        let delta = match event.keystroke.key.as_str() {
                            "pageup" => height,
                            "pagedown" => -height,
                            _ => return,
                        };
                        let mut offset = scroll.offset();
                        offset.y = gpui_kit::px(
                            (f32::from(offset.y) + delta)
                                .clamp(-f32::from(scroll.max_offset().y), 0.),
                        );
                        scroll.set_offset(offset);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .size_full()
            .min_h_0()
            .gap_3()
            .p_3();
        // No body title or manual refresh: the repository tab already shows
        // the checkout in the titlebar, the global page shows "Tasks" there,
        // and new/updated tasks arrive through the active 2-second poll.
        if let Some(id) = self.selected_id.clone() {
            let copy = id.clone();
            // Titlebar back handles drill-out when it owns chrome.
            let mut row = h_flex().gap_3().flex_wrap().items_center();
            if !self.titlebar_chrome {
                row = row.child(
                    Button::new("task-back")
                        .ghost()
                        .label("← Task list")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.back(cx);
                            this.focus_handle.focus(window, cx);
                        })),
                );
            }
            body = body.child(
                row.child(div().text_sm().text_color(cx.theme().muted_foreground).child(id))
                    .child(Button::new("copy-task").label("Copy ID").on_click(
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            window.push_notification("Task ID copied", cx);
                        },
                    )),
            );
            for error in [&self.selected_error, &self.mutation_error]
                .into_iter()
                .flatten()
            {
                body = body.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            let mut detail = v_flex()
                .id("task-detail")
                .flex_1()
                .min_h_0()
                .gap_3()
                .overflow_y_scroll()
                .track_scroll(&self.detail_scroll);
            if let Some(snapshot) = &self.selected {
                let task = &snapshot.task;
                detail =
                    detail
                        .child(div().text_xl().font_semibold().child(task.title.clone()))
                        .child(
                            h_flex()
                                .gap_3()
                                .flex_wrap()
                                .child(progress_label(task))
                                .when(task.archived, |row| row.child("Archived"))
                                .child(
                                    Button::new("archive-task")
                                        .label(if task.archived {
                                            "Unarchive"
                                        } else {
                                            "Archive"
                                        })
                                        .disabled(self.refresh.busy)
                                        .on_click(cx.listener(|this, _, _, cx| this.archive(cx))),
                                ),
                        )
                        .child(repository_badges(task, cx))
                        .children(snapshot.warnings.iter().map(|warning| {
                            div().text_color(cx.theme().danger).child(warning.clone())
                        }))
                        .child(self.description(&task.id))
                        .child(self.links(&task.id, &task.artifacts, cx));
                for subtask in &task.subtasks {
                    detail = detail.child(
                        v_flex()
                            .id(SharedString::from(format!("subtask-{}", subtask.id)))
                            .flex_none()
                            .gap_2()
                            .p_3()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded_md()
                            .child(
                                div()
                                    .font_semibold()
                                    .child(format!("{} · {}", subtask.id, subtask.title)),
                            )
                            .child(format!(
                                "{} · Repository: {}",
                                status_label(subtask.status),
                                subtask.repository
                            ))
                            .child(format!(
                                "Depends on: {}",
                                if subtask.dependencies.is_empty() {
                                    "None".into()
                                } else {
                                    subtask.dependencies.join(", ")
                                }
                            ))
                            .child(self.description(&subtask.id))
                            .child(self.links(&subtask.id, &subtask.artifacts, cx)),
                    );
                }
            } else if self.selected_error.is_none() {
                detail = detail.child("Loading task…");
            }
            body = body.child(detail);
        } else {
            // Archived filter lives in the workspace titlebar (global page
            // and repository tab), so the body never shows it. Dashboard
            // recent (`recent`) never filters.
            let mut list = v_flex()
                .id("task-list")
                .flex_1()
                .min_h_0()
                .gap_2()
                .overflow_y_scroll()
                .track_scroll(&self.list_scroll);
            for error in self.error.iter().chain(self.list.errors.iter()) {
                list = list.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            if self.list.tasks.is_empty() && self.error.is_none() {
                list = list.child(if self.refresh.busy {
                    "Loading tasks…"
                } else {
                    "No tasks. Ask an agent to create an idea with devcroft task create."
                });
            }
            for snapshot in &self.list.tasks {
                let task = &snapshot.task;
                let id = task.id.clone();
                let copy = id.clone();
                list = list.child(
                    h_flex()
                        .gap_2()
                        .flex_none()
                        .child(
                            gpui_kit::base::Button::new(SharedString::from(format!("open-{id}")))
                                .accessibility_label(format!("Open {} · {id}", task.title))
                                .flex_1()
                                .min_w_0()
                                .flex_col()
                                .items_start()
                                .p_3()
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded_md()
                                .focus(|style| style.border_color(cx.theme().ring))
                                .hover(|style| style.bg(cx.theme().secondary))
                                .child(
                                    div()
                                        .w_full()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .font_semibold()
                                        .child(task.title.clone()),
                                )
                                .child(format!(
                                    "{id} · {}{}",
                                    progress_label(task),
                                    if task.archived { " · Archived" } else { "" }
                                ))
                                .child(repository_badges(task, cx))
                                .when(!snapshot.warnings.is_empty(), |row| {
                                    row.child(
                                        "⚠ Missing or unreadable references — open for details",
                                    )
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open(id.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("copy-{copy}")))
                                .label("Copy ID")
                                .on_click(move |_, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    window.push_notification("Task ID copied", cx);
                                }),
                        ),
                );
            }
            if self.list.truncated && !self.recent {
                list = list.child(Button::new("more-tasks").label("Load 100 more").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.limit = this.limit.saturating_add(PAGE_SIZE);
                        this.reload(cx);
                    }),
                ));
            }
            body = body.child(list);
        }
        body.into_any_element()
    }
}

fn repository_badges(task: &Task, cx: &gpui_kit::App) -> impl IntoElement {
    let repositories = task.involved_repositories();
    h_flex()
        .gap_2()
        .flex_wrap()
        .when(repositories.is_empty(), |row| row.child("No repositories"))
        .children(repositories.into_iter().map(|key| {
            div()
                .px_2()
                .rounded_md()
                .bg(cx.theme().secondary)
                .text_sm()
                .child(key)
        }))
}

#[cfg(test)]
#[path = "task_browser_tests.rs"]
mod tests;
