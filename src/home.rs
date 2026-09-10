//! Home dashboard and deliberately lightweight destination placeholders.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, ColorName, Disableable as _, Sizable, Size, StyledExt as _, WindowExt as _,
    h_flex, tag::Tag, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AppContext as _, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window, div, point, px,
};
use std::{collections::HashMap, path::PathBuf, time::Duration};

fn item_id(prefix: &str, id: &str) -> SharedString {
    format!("{prefix}-{id}").into()
}

use crate::data::dashboard::{Category, Dashboard, Item, Kind, safe_web_url};
use crate::data::{DataRoot, RecentRepository, SyncStatus, SyncTracker, recent_repositories};
use crate::git_status::{GitStatus, load_git_status};
use crate::relative_time::{current_unix_secs, relative_duration_label};

const PROJECT_GIT_INTERVAL: Duration = Duration::from_secs(5);

/// Cache keys include the checkout path, not just the portable repository key.
/// A reload invalidates in-flight results when bindings or recent projects change.
#[derive(Default)]
struct ProjectGitCache {
    generation: u64,
    statuses: HashMap<PathBuf, GitStatus>,
}

impl ProjectGitCache {
    fn invalidate(&mut self, paths: &[PathBuf]) {
        self.generation = self.generation.wrapping_add(1);
        self.statuses.retain(|path, _| paths.contains(path));
    }

    fn commit(&mut self, generation: u64, statuses: HashMap<PathBuf, GitStatus>) -> bool {
        if generation != self.generation || self.statuses == statuses {
            return false;
        }
        self.statuses = statuses;
        true
    }
}

fn project_branch_label(status: Option<&GitStatus>) -> String {
    let Some(status) = status else {
        return "Loading Git status…".into();
    };
    let Some(branch) = &status.branch else {
        return "Git status unavailable".into();
    };
    if status.detached {
        format!("Detached · {branch}")
    } else {
        format!("⎇ {branch}")
    }
}

fn project_working_tree_label(status: Option<&GitStatus>) -> Option<String> {
    let status = status?;
    status.branch.as_ref()?;
    let mut parts = vec![if status.dirty { "Modified" } else { "Clean" }.to_owned()];
    parts.extend(status.ahead_label());
    parts.extend(status.behind_label());
    Some(parts.join(" · "))
}

fn project_git_label(status: Option<&GitStatus>) -> String {
    let branch = project_branch_label(status);
    match project_working_tree_label(status) {
        Some(detail) => format!("{branch} · {detail}"),
        None => branch,
    }
}

/// Tag content for the git state row: `Some((label, hue))` once git
/// resolves, `None` while loading or unavailable (muted text instead).
/// Dirty is amber caution, not red — red already means blocked elsewhere.
/// Shared with the workspace titlebar so both stay the same hue.
pub(crate) fn project_state_tag(status: Option<&GitStatus>) -> Option<(&'static str, ColorName)> {
    match status {
        None => None,
        Some(status) if status.branch.is_none() => None,
        Some(status) if status.dirty => Some(("Modified", ColorName::Amber)),
        Some(_) => Some(("Clean", ColorName::Green)),
    }
}

/// Short branch text for the card pill: `⎇ main`, or `◍ abcdef1` for a
/// detached HEAD. `None` while loading or when git is unavailable.
fn project_branch_pill(status: Option<&GitStatus>) -> Option<String> {
    let status = status?;
    let branch = status.branch.as_ref()?;
    Some(if status.detached {
        format!("◍ {branch}")
    } else {
        format!("⎇ {branch}")
    })
}

/// Ahead/behind counts as one trailing fragment (`↑2 · ↓1`), if any.
fn project_sync_label(status: Option<&GitStatus>) -> Option<String> {
    let status = status?;
    status.branch.as_ref()?;
    let parts: Vec<String> = status
        .ahead_label()
        .into_iter()
        .chain(status.behind_label())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Parse the `YYYY-MM-DDTHH:MM:SSZ` timestamps written by
/// `record_repository_open`. Returns unix seconds. Hand-rolled so recency
/// needs no date dependency; rejects anything outside the exact shape.
fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let number =
        |from: usize, to: usize| -> Option<i64> { value.get(from..to)?.parse::<i64>().ok() };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `Opened 2h ago` for the card footer. `None` when never opened or when
/// the stored timestamp is malformed; callers fall back to `Not opened yet`.
fn project_opened_label(last_opened_at: Option<&str>, now_secs: i64) -> Option<String> {
    let raw = last_opened_at.filter(|value| !value.is_empty())?;
    let then = parse_rfc3339_utc(raw)?;
    let diff = now_secs.saturating_sub(then);
    Some(format!("Opened {}", relative_duration_label(diff)))
}

pub(crate) enum HomeEvent {
    OpenRepository { key: String, label: String },
    AddRepository,
    OpenAgentSession(crate::agent_sessions::SessionKey),
    RefreshSessions,
}

pub(crate) struct HomeView {
    tasks: gpui_kit::Entity<crate::tasks::TaskBrowser>,
    artifacts: gpui_kit::Entity<crate::artifacts::ArtifactBrowser>,
    pub(crate) focus_handle: FocusHandle,
    root: Option<DataRoot>,
    tracker: SyncTracker,
    data: Dashboard,
    error: Option<String>,
    projects: Vec<RecentRepository>,
    sessions: Vec<(crate::agent_sessions::SessionSummary, String)>,
    session_errors: Vec<String>,
    sessions_loaded: bool,
    page: Option<&'static str>,
    show_completed: bool,
    active: bool,
    project_git: ProjectGitCache,
    git_loading: bool,
    project_focus: HashMap<String, FocusHandle>,
    scroll: ScrollHandle,
}

impl EventEmitter<HomeEvent> for HomeView {}

#[derive(Clone)]
struct DragTodo {
    id: String,
    title: String,
}
impl Render for DragTodo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .child(self.title.clone())
    }
}

impl HomeView {
    pub(crate) fn set_sessions(
        &mut self,
        sessions: Vec<(crate::agent_sessions::SessionSummary, String)>,
        errors: Vec<String>,
        loaded: bool,
        cx: &mut Context<Self>,
    ) {
        self.sessions = sessions;
        self.session_errors = errors;
        self.sessions_loaded = loaded;
        cx.notify();
    }

    pub(crate) fn new(
        root: Option<DataRoot>,
        tracker: SyncTracker,
        cx: &mut Context<Self>,
    ) -> Self {
        let tasks = cx.new(|cx| {
            crate::tasks::TaskBrowser::new(root.clone(), crate::tasks::Scope::Global, cx)
        });
        tasks.update(cx, |view, cx| {
            view.show_recent(true, cx);
            view.set_titlebar_owned(false);
        });
        cx.subscribe(&tasks, |this, _, _: &crate::tasks::OpenedTask, cx| {
            this.page = Some("Tasks");
            this.tasks
                .update(cx, |view, _| view.set_titlebar_owned(true));
            cx.notify();
        })
        .detach();
        let mut view = Self {
            tasks,
            artifacts: cx.new(|cx| crate::artifacts::ArtifactBrowser::new(root.clone(), cx)),
            focus_handle: cx.focus_handle(),
            root,
            tracker,
            data: Dashboard::default(),
            error: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            session_errors: Vec::new(),
            sessions_loaded: false,
            page: None,
            show_completed: false,
            active: true,
            project_git: ProjectGitCache::default(),
            git_loading: false,
            project_focus: HashMap::new(),
            scroll: ScrollHandle::new(),
        };
        view.reload(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROJECT_GIT_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.refresh_project_git(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        view
    }

    pub(crate) fn activate(&mut self, cx: &mut Context<Self>) {
        self.tasks.update(cx, |view, cx| {
            view.show_recent(true, cx);
            view.set_titlebar_owned(false);
        });
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.active = true;
        self.page = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    pub(crate) fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.tasks.update(cx, |view, cx| view.set_active(false, cx));
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.active = false;
    }

    /// Show the global task list: the same destination as Recent Tasks'
    /// **View all →**. Used by the command palette so tasks are reachable
    /// without scrolling Home. Mirrors the heading button: the full list
    /// replaces recent summaries, the artifact browser stays inactive, and
    /// a reload refreshes projects/dashboard plus the now-visible list.
    pub(crate) fn show_tasks_page(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.page = Some("Tasks");
        self.tasks.update(cx, |view, cx| {
            view.show_recent(false, cx);
            view.set_titlebar_owned(true);
        });
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    /// Show the artifact browser, reachable through the command palette's
    /// **Browse artifacts** entry. The task list deactivates, the artifact
    /// browser activates, and a reload refreshes projects/dashboard plus
    /// the now-visible browser.
    pub(crate) fn show_artifacts_page(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.page = Some("Artifacts");
        self.tasks.update(cx, |view, cx| view.set_active(false, cx));
        self.artifacts
            .update(cx, |view, cx| view.set_active(true, cx));
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    pub(crate) fn is_artifacts_page(&self) -> bool {
        self.page == Some("Artifacts")
    }

    pub(crate) fn is_tasks_page(&self) -> bool {
        self.page == Some("Tasks")
    }

    pub(crate) fn artifacts_include_archived(&self, cx: &gpui_kit::App) -> bool {
        self.artifacts.read(cx).include_archived()
    }

    pub(crate) fn set_artifacts_archived(&self, include: bool, cx: &mut Context<Self>) {
        self.artifacts
            .update(cx, |view, cx| view.set_include_archived(include, cx));
    }

    pub(crate) fn artifacts_has_selection(&self, cx: &gpui_kit::App) -> bool {
        self.artifacts.read(cx).has_selection()
    }

    pub(crate) fn clear_artifact_selection(&self, cx: &mut Context<Self>) {
        self.artifacts
            .update(cx, |view, cx| view.clear_selection(cx));
    }

    pub(crate) fn tasks_include_archived(&self, cx: &gpui_kit::App) -> bool {
        self.tasks.read(cx).include_archived()
    }

    pub(crate) fn set_tasks_archived(&self, include: bool, cx: &mut Context<Self>) {
        self.tasks
            .update(cx, |view, cx| view.set_include_archived(include, cx));
    }

    pub(crate) fn tasks_is_artifact_open(&self, cx: &gpui_kit::App) -> bool {
        self.tasks.read(cx).is_artifact_open()
    }

    pub(crate) fn tasks_has_selection(&self, cx: &gpui_kit::App) -> bool {
        self.tasks.read(cx).has_selection()
    }

    pub(crate) fn close_task_artifact(&self, cx: &mut Context<Self>) {
        self.tasks.update(cx, |view, cx| view.close_artifact(cx));
    }

    pub(crate) fn clear_task_selection(&self, cx: &mut Context<Self>) {
        self.tasks.update(cx, |view, cx| view.back_to_list(cx));
    }

    fn refresh_project_git(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.page.is_some() || self.git_loading || self.projects.is_empty() {
            return;
        }
        self.git_loading = true;
        let generation = self.project_git.generation;
        let paths: Vec<_> = self
            .projects
            .iter()
            .map(|project| project.checkout_path.clone())
            .collect();
        cx.spawn(async move |this, cx| {
            let statuses = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            let status = load_git_status(&path);
                            (path, status)
                        })
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_loading = false;
                if this.project_git.commit(generation, statuses) {
                    cx.notify();
                }
                if generation != this.project_git.generation {
                    this.refresh_project_git(cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.refresh_planning(cx);
        if let Some(root) = &self.root {
            self.projects = recent_repositories(root, 4);
            match Dashboard::load(root) {
                Ok(data) => {
                    self.data = data;
                    self.error = None;
                }
                Err(error) => {
                    self.data = Dashboard::default();
                    self.error = Some(format!("Could not load Home: {error:#}"));
                }
            }
        } else {
            self.error = Some("Portable data is unavailable".into());
        }
        self.project_focus
            .retain(|key, _| self.projects.iter().any(|project| &project.key == key));
        for project in &self.projects {
            self.project_focus
                .entry(project.key.clone())
                .or_insert_with(|| cx.focus_handle());
        }
        self.project_git.invalidate(
            &self
                .projects
                .iter()
                .map(|project| project.checkout_path.clone())
                .collect::<Vec<_>>(),
        );
        self.refresh_project_git(cx);
        cx.notify();
    }

    pub(crate) fn refresh_planning(&mut self, cx: &mut Context<Self>) {
        if self.active && (self.page.is_none() || self.page == Some("Tasks")) {
            self.tasks.update(cx, |view, cx| view.refresh_all(cx));
        }
        if self.active && self.page == Some("Artifacts") {
            self.artifacts.update(cx, |view, cx| view.refresh(cx));
        }
    }

    fn change(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut Dashboard) -> anyhow::Result<()>,
    ) -> bool {
        let result = (|| {
            anyhow::ensure!(
                self.tracker.status() != SyncStatus::Syncing,
                "Wait for portable sync to finish before editing Home"
            );
            anyhow::ensure!(
                self.error.is_none(),
                "Reload Home successfully before editing"
            );
            let root = self
                .root
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Portable data is unavailable"))?;
            let mut next = self.data.clone();
            edit(&mut next)?;
            next.save(root, &self.data)?;
            self.data = next;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                cx.notify();
                true
            }
            Err(error) => {
                window.push_notification(format!("Could not save: {error:#}"), cx);
                false
            }
        }
    }

    fn editor(&self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx).placeholder(placeholder);
                    state.set_value(value.to_owned(), window, cx);
                    state
                })
            };
        let title = input(&item.title, "Title", window, cx);
        let description = input(&item.description, "Description (optional)", window, cx);
        let label = input(&item.label, "Label (optional)", window, cx);
        let url = input(&item.url, "https://…", window, cx);
        let home = cx.entity().downgrade();
        let category = std::rc::Rc::new(std::cell::Cell::new(item.category));
        let original = self.data.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let mut form = v_flex().gap_3().child("Title").child(Input::new(&title));
            if item.kind == Kind::Todo {
                form = form.child("Description").child(Input::new(&description)).child("Label").child(Input::new(&label));
            } else {
                form = form.child("URL").child(Input::new(&url));
            }
            if item.kind == Kind::PullRequest {
                form = form.child("Category").child(h_flex().gap_2().flex_wrap().children(Category::ALL.into_iter().enumerate().map(|(index, choice)| {
                    let category = category.clone();
                    Button::new(("category", index)).label(if category.get() == choice { format!("✓ {}", choice.label()) } else { choice.label().to_owned() })
                        .on_click(move |_, _, cx| { category.set(choice); cx.refresh_windows(); })
                }))).child("GitHub status fetching is not implemented yet.");
            }
            let (title, description, label, url, home, category, item, original) = (title.clone(), description.clone(), label.clone(), url.clone(), home.clone(), category.clone(), item.clone(), original.clone());
            dialog.title("Edit Home item").w(px(560.)).child(form)
                .footer(DialogFooter::new()
                    .child(Button::new("cancel-home-item").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("save-home-item").primary().label("Save").on_click(|_, window, cx| window.dispatch_action(Box::new(Confirm { secondary: false }), cx))))
                .on_ok(move |_, window, cx| {
                let mut item = item.clone();
                item.title = title.read(cx).value().to_string();
                item.description = description.read(cx).value().to_string();
                item.label = label.read(cx).value().to_string();
                item.url = url.read(cx).value().to_string();
                item.category = category.get();
                home.update(cx, |this, cx| this.change(window, cx, |data| {
                    anyhow::ensure!(*data == original, "Home changed while this form was open. Close it and reopen the item.");
                    data.upsert(item)
                })).unwrap_or(false)
            })
        });
    }

    fn heading(
        &self,
        title: &'static str,
        destination: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .gap_2()
            .child(div().text_lg().font_semibold().child(title))
            .child(
                Button::new(item_id("view-all", title))
                    .ghost()
                    .label("View all →")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = Some(destination);
                        this.tasks.update(cx, |view, cx| {
                            if destination == "Tasks" {
                                view.show_recent(false, cx);
                                view.set_titlebar_owned(true);
                            } else {
                                view.set_active(false, cx);
                            }
                        });
                        this.scroll.set_offset(point(px(0.), px(0.)));
                        cx.notify();
                    })),
            )
    }

    fn todo_card(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = v_flex()
            .id(item_id("home-item", &item.id))
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border);
        if !item.completed {
            let drag = DragTodo {
                id: item.id.clone(),
                title: item.title.clone(),
            };
            row = row.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()));
            let target = item.id.clone();
            row = row.on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                this.change(window, cx, |data| {
                    data.move_todo(&drag.id, &target);
                    Ok(())
                });
            }));
        }
        let toggle_id = item.id.clone();
        let title = item.title.clone();
        let home = cx.entity().downgrade();
        let edit = item.clone();
        let delete_id = item.id.clone();
        let (move_up, move_down) = if item.completed {
            (None, None)
        } else {
            let pending: Vec<_> = self
                .data
                .items
                .iter()
                .filter(|i| i.kind == Kind::Todo && !i.completed)
                .collect();
            let index = pending.iter().position(|i| i.id == item.id).unwrap_or(0);
            (
                index
                    .checked_sub(1)
                    .and_then(|i| pending.get(i))
                    .map(|i| i.id.clone()),
                pending.get(index + 1).map(|i| i.id.clone()),
            )
        };
        let source_up = item.id.clone();
        let source_down = item.id.clone();
        row = row.child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    Checkbox::new(item_id("complete", &item.id))
                        .checked(item.completed)
                        .accessibility_label(title.clone())
                        .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                            this.change(window, cx, |data| {
                                if let Some(i) = data.items.iter_mut().find(|i| i.id == toggle_id) {
                                    i.completed = *checked;
                                }
                                Ok(())
                            });
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_medium()
                        .child(title.clone()),
                )
                .child(
                    Button::new(item_id("options", &item.id))
                        .ghost()
                        .label("⋯")
                        .accessibility_label(format!("Options for {title}"))
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            menu.item({
                                let home = home.clone();
                                let edit = edit.clone();
                                PopupMenuItem::new("Edit").on_click(move |_, window, cx| {
                                    let _ = home.update(cx, |this, cx| {
                                        this.editor(edit.clone(), window, cx);
                                    });
                                })
                            })
                            .item({
                                let home = home.clone();
                                let delete_id = delete_id.clone();
                                PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                                    let _ = home.update(cx, |this, cx| {
                                        this.change(window, cx, |data| {
                                            data.items.retain(|i| i.id != delete_id);
                                            Ok(())
                                        });
                                    });
                                })
                            })
                            .separator()
                            .item({
                                let home = home.clone();
                                let source = source_up.clone();
                                let target = move_up.clone();
                                PopupMenuItem::new("Move up")
                                    .disabled(target.is_none())
                                    .on_click(move |_, window, cx| {
                                        if let Some(target) = target.clone() {
                                            let _ = home.update(cx, |this, cx| {
                                                this.change(window, cx, |data| {
                                                    data.move_todo(&source, &target);
                                                    Ok(())
                                                });
                                            });
                                        }
                                    })
                            })
                            .item({
                                let home = home.clone();
                                let source = source_down.clone();
                                let target = move_down.clone();
                                PopupMenuItem::new("Move down")
                                    .disabled(target.is_none())
                                    .on_click(move |_, window, cx| {
                                        if let Some(target) = target.clone() {
                                            let _ = home.update(cx, |this, cx| {
                                                this.change(window, cx, |data| {
                                                    data.move_todo(&source, &target);
                                                    Ok(())
                                                });
                                            });
                                        }
                                    })
                            })
                        }),
                ),
        );
        if !item.description.is_empty() {
            row = row.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(item.description.clone()),
            );
        }
        if !item.label.is_empty() {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("#{}", item.label)),
            );
        }
        row
    }

    fn list(&self, kind: Kind, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, destination) = match kind {
            Kind::Todo => ("Todos", "Todos"),
            Kind::PullRequest => ("Pull Requests", "Pull Requests"),
            Kind::Reading => ("To Read", "To Read"),
        };
        let items: Vec<_> = self
            .data
            .items
            .iter()
            .filter(|i| i.kind == kind && (self.show_completed || !i.completed))
            .cloned()
            .collect();
        let mut panel = v_flex()
            .flex_1()
            .min_w(px(290.))
            .min_h(px(380.))
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(self.heading(title, destination, cx));
        if kind == Kind::Todo {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Next items to do · drag a card to reorder"),
            );
        }
        if kind == Kind::PullRequest {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Manually tracked · status not fetched"),
            );
        }
        if items.is_empty() {
            panel = panel.child(
                div()
                    .py_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("Nothing here yet. Add your first item below."),
            );
        }
        for category in Category::ALL {
            if kind == Kind::PullRequest && items.iter().any(|item| item.category == category) {
                panel = panel.child(div().text_sm().font_semibold().child(category.label()));
            }
            for item in items
                .iter()
                .filter(|i| kind != Kind::PullRequest || i.category == category)
            {
                if kind == Kind::Todo {
                    panel = panel.child(self.todo_card(item, cx));
                    continue;
                }
                let id = item.id.clone();
                let mut row = v_flex()
                    .id(item_id("home-item", &id))
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border);
                let mut top = h_flex().gap_2();
                if kind != Kind::PullRequest {
                    top = top.child(
                        Checkbox::new(item_id("complete", &id))
                            .checked(item.completed)
                            .label(if kind == Kind::Todo { "Done" } else { "Read" })
                            .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                this.change(window, cx, |data| {
                                    if let Some(i) = data.items.iter_mut().find(|i| i.id == id) {
                                        i.completed = *checked;
                                    }
                                    Ok(())
                                });
                            })),
                    );
                }
                if kind == Kind::Todo && !item.completed {
                    let drag = DragTodo {
                        id: item.id.clone(),
                        title: item.title.clone(),
                    };
                    top = top.child(
                        div()
                            .id(item_id("drag", &item.id))
                            .cursor_pointer()
                            .child("⋮⋮")
                            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone())),
                    );
                    let target = item.id.clone();
                    row = row.on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                        this.change(window, cx, |data| {
                            data.move_todo(&drag.id, &target);
                            Ok(())
                        });
                    }));
                }
                row = row
                    .child(top)
                    .child(div().font_medium().child(item.title.clone()));
                if !item.description.is_empty() {
                    row = row.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(item.description.clone()),
                    );
                }
                if !item.label.is_empty() {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("#{}", item.label)),
                    );
                }
                let edit = item.clone();
                let delete_id = item.id.clone();
                let mut actions = h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(item_id("edit", &item.id))
                            .ghost()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editor(edit.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(item_id("delete", &item.id))
                            .ghost()
                            .label("Delete")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.change(window, cx, |data| {
                                    data.items.retain(|i| i.id != delete_id);
                                    Ok(())
                                });
                            })),
                    );
                if kind == Kind::Todo && !item.completed {
                    let pending: Vec<_> = self
                        .data
                        .items
                        .iter()
                        .filter(|i| i.kind == Kind::Todo && !i.completed)
                        .collect();
                    let index = pending.iter().position(|i| i.id == item.id).unwrap_or(0);
                    for (name, target) in [
                        ("Move up", index.checked_sub(1).and_then(|i| pending.get(i))),
                        ("Move down", pending.get(index + 1)),
                    ] {
                        let source = item.id.clone();
                        let target = target.map(|i| i.id.clone());
                        actions = actions.child(
                            Button::new(item_id(name, &source))
                                .ghost()
                                .label(name)
                                .disabled(target.is_none())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(target) = &target {
                                        this.change(window, cx, |data| {
                                            data.move_todo(&source, target);
                                            Ok(())
                                        });
                                    }
                                })),
                        );
                    }
                }
                if kind != Kind::Todo {
                    let url = item.url.clone();
                    actions = actions.child(
                        Button::new(item_id("open", &item.id))
                            .ghost()
                            .label("Open ↗")
                            .on_click(move |_, window, cx| {
                                if safe_web_url(&url) {
                                    cx.open_url(&url);
                                } else {
                                    window.push_notification("Invalid web URL", cx);
                                }
                            }),
                    );
                }
                panel = panel.child(row.child(actions));
            }
            if kind != Kind::PullRequest {
                break;
            }
        }
        panel.child(h_flex().mt_auto().pt_3().justify_end().child(
            Button::new(item_id("add", title)).label("+ Add").on_click(
                cx.listener(move |this, _, window, cx| this.editor(Item::new(kind), window, cx)),
            ),
        ))
    }
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.page == Some("Tasks") {
            // Navigation and filter live in the workspace titlebar, matching
            // the repository workspace and artifact browser layout.
            return div()
                .size_full()
                .min_h_0()
                .flex_1()
                .child(self.tasks.clone())
                .into_any_element();
        }
        if self.page == Some("Artifacts") {
            // Navigation and filter live in the workspace titlebar, matching
            // the repository workspace layout.
            return div()
                .size_full()
                .min_h_0()
                .flex_1()
                .child(self.artifacts.clone())
                .into_any_element();
        }
        let card_width = recent_card_width(f32::from(window.viewport_size().width));
        let mut body = v_flex()
            .w_full()
            // Fill the area below the titlebar while allowing the dashboard
            // to grow and scroll on shorter windows or with more inbox items.
            .min_h(px((f32::from(window.viewport_size().height)
                - crate::metrics::WORKSPACE_HEADER_HEIGHT)
                .max(0.)))
            .max_w(px(1440.))
            .mx_auto()
            .gap_4()
            .px_6()
            .pt_4()
            .pb_6();
        if let Some(page) = self.page {
            body = body
                .child(
                    Button::new("back-home")
                        .self_start()
                        .ghost()
                        .label("← Home")
                        .on_click(cx.listener(|this, _, _, cx| this.activate(cx))),
                )
                .child(div().text_2xl().font_semibold().child(page))
                .child("Coming soon — this dedicated page is a placeholder.")
                .child(
                    "Use Home to manage your current items. More views and workflows will follow.",
                );
        } else {
            if let Some(error) = &self.error {
                body = body.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            body = body.child(
                h_flex()
                    .justify_between()
                    .child(div().text_lg().font_semibold().child("Recent Activity"))
                    .child(
                        Button::new("refresh-home-sessions")
                            .ghost()
                            .label("Refresh")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(HomeEvent::RefreshSessions)),
                            ),
                    ),
            );
            let mut sessions = h_flex().gap_4().flex_wrap();
            for (session, repository) in &self.sessions {
                let key = session.key.clone();
                let id: SharedString = format!(
                    "home-session-{}-{}-{}",
                    key.provider,
                    key.store.display(),
                    key.id
                )
                .into();
                sessions = sessions.child(
                    Button::new(id)
                        .ghost()
                        .w(px(card_width))
                        .h_auto()
                        .p_4()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .tooltip(session.tooltip())
                        .child(
                            v_flex()
                                .items_start()
                                .w_full()
                                .gap_2()
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .font_semibold()
                                        .child(session.title.clone()),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_sm()
                                        .child(repository.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "{} · {}",
                                            session.provider_label(),
                                            session.age()
                                        )),
                                ),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(HomeEvent::OpenAgentSession(key.clone()))
                        })),
                );
            }
            if self.sessions.is_empty() {
                sessions = sessions.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(if self.sessions_loaded {
                            "No recent agent sessions."
                        } else {
                            "Loading agent sessions…"
                        }),
                );
            }
            body = body.child(sessions);
            for error in &self.session_errors {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(error.clone()),
                );
            }
            body = body.child(self.heading("Recent Projects", "Projects", cx));
            let mut projects = h_flex().gap_4().flex_wrap();
            let now_secs = current_unix_secs();
            for (index, project) in self.projects.iter().enumerate() {
                let key = project.key.clone();
                let label = project.display_name.clone().unwrap_or_else(|| key.clone());
                let open_label = label.clone();
                let git_status = self.project_git.statuses.get(&project.checkout_path);
                let status = project_git_label(git_status);
                let group = project
                    .group
                    .clone()
                    .filter(|value| !value.trim().is_empty());
                let description = project
                    .description
                    .clone()
                    .filter(|value| !value.trim().is_empty());
                let state = project_state_tag(git_status);
                let pending = match git_status {
                    None => Some("Loading Git status…"),
                    Some(status) if status.branch.is_none() => Some("Git status unavailable"),
                    _ => None,
                };
                let branch_pill = project_branch_pill(git_status);
                let sync = project_sync_label(git_status);
                let opened = project_opened_label(project.last_opened_at.as_deref(), now_secs)
                    .unwrap_or_else(|| "Not opened yet".to_owned());
                let accessible = format!("Open {label}. {status}. {opened}");
                projects = projects.child(
                    // gpui-kit Base Button supplies pointer, Enter/Space, tab
                    // traversal and accessibility semantics for the whole card.
                    gpui_kit::base::Button::new(item_id("project", &key))
                        .track_focus(&self.project_focus[&key])
                        .accessibility_label(accessible)
                        .cursor_pointer()
                        .flex_col()
                        .items_start()
                        .justify_start()
                        .flex_none()
                        .w(px(card_width))
                        .min_h(px(160.))
                        .gap_2()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .hover(|style| style.bg(cx.theme().secondary))
                        .focus(|style| style.border_color(cx.theme().ring).bg(cx.theme().secondary))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(HomeEvent::OpenRepository {
                                key: key.clone(),
                                label: open_label.clone(),
                            });
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.modifiers.modified() {
                                return;
                            }
                            let columns = recent_columns(f32::from(window.viewport_size().width));
                            if let Some(next) = project_navigation(
                                &event.keystroke.key,
                                index,
                                this.projects.len(),
                                columns,
                            ) {
                                this.project_focus[&this.projects[next].key].focus(window, cx);
                                cx.stop_propagation();
                                window.prevent_default();
                                cx.notify();
                            }
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .font_semibold()
                                        .child(label),
                                )
                                .when_some(group, |this, tag| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .px_2()
                                            .rounded_full()
                                            .bg(cx.theme().secondary)
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(tag),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .gap_2()
                                .flex_wrap()
                                .when_some(state, |row, (label, hue)| {
                                    row.child(
                                        Tag::color(hue)
                                            .with_size(Size::Small)
                                            .rounded_full()
                                            .flex_none()
                                            .child(label),
                                    )
                                })
                                .when_some(pending, |row, text| {
                                    row.child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(text),
                                    )
                                })
                                .when_some(branch_pill, |this, pill| {
                                    this.child(
                                        Tag::secondary()
                                            .with_size(Size::Small)
                                            .rounded_full()
                                            .flex_none()
                                            .child(pill),
                                    )
                                })
                                .when_some(sync, |this, counts| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(counts),
                                    )
                                }),
                        )
                        .when_some(description, |this, text| {
                            this.child(
                                div()
                                    .w_full()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(text),
                            )
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .mt_auto()
                                .pt_1()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(opened),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Open →"),
                                ),
                        ),
                );
            }
            if self.projects.is_empty() {
                projects = projects.child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("No recent projects yet."),
                );
            }
            body = body.child(projects).child(
                Button::new("add-project-home")
                    .self_start()
                    .ghost()
                    .label("+ Add project")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeEvent::AddRepository))),
            );
            body = body
                .child(self.heading("Recent Tasks", "Tasks", cx))
                .child(self.tasks.clone());
            body = body.child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().font_semibold().child("Inbox"))
                    .child(
                        Checkbox::new("show-completed")
                            .label("Show completed")
                            .checked(self.show_completed)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.show_completed = *checked;
                                cx.notify();
                            })),
                    ),
            );
            body = body.child(
                h_flex()
                    .flex_grow(1.)
                    .flex_shrink_0()
                    .items_stretch()
                    .gap_4()
                    .flex_wrap()
                    .child(self.list(Kind::PullRequest, cx))
                    .child(self.list(Kind::Todo, cx))
                    .child(self.list(Kind::Reading, cx)),
            );
        }
        div()
            .id("home")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let height = f32::from(this.scroll.bounds().size.height) * 0.85;
                let delta = match event.keystroke.key.as_str() {
                    "pageup" => height,
                    "pagedown" => -height,
                    _ => return,
                };
                let mut offset = this.scroll.offset();
                offset.y =
                    px((f32::from(offset.y) + delta)
                        .clamp(-f32::from(this.scroll.max_offset().y), 0.));
                this.scroll.set_offset(offset);
                window.prevent_default();
                cx.stop_propagation();
                cx.notify();
            }))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .vertical_scrollbar(&self.scroll)
            .child(body)
            .into_any_element()
    }
}

/// Use the same grid for projects and tasks, regardless of how many projects
/// exist. The dashboard has 24px gutters and 16px gaps; spare columns stay empty.
/// Widths are floored to whole pixels so fractional rounding can't wrap a card
/// that mathematically fits (notably on Retina 2x).
fn recent_card_width(viewport_width: f32) -> f32 {
    let available = (viewport_width.min(1440.) - 48.).max(1.);
    let columns = recent_columns(viewport_width) as f32;
    ((available - (columns - 1.) * 16.) / columns).floor()
}

fn recent_columns(viewport_width: f32) -> usize {
    let available = (viewport_width.min(1440.) - 48.).max(1.);
    ((available + 16.) / (280. + 16.)).floor().clamp(1., 4.) as usize
}

fn project_navigation(key: &str, index: usize, count: usize, columns: usize) -> Option<usize> {
    if count == 0 || index >= count || columns == 0 {
        return None;
    }
    Some(match key {
        "left" => index.saturating_sub(1),
        "right" => (index + 1).min(count - 1),
        "up" => index.checked_sub(columns).unwrap_or(index),
        "down" => {
            if index + columns < count {
                index + columns
            } else {
                index
            }
        }
        "home" => 0,
        "end" => count - 1,
        _ => return None,
    })
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn project_keys_follow_the_responsive_grid_without_escaping_it() {
        assert_eq!(project_navigation("right", 0, 4, 4), Some(1));
        assert_eq!(project_navigation("left", 0, 4, 4), Some(0));
        assert_eq!(project_navigation("right", 3, 4, 4), Some(3));
        assert_eq!(project_navigation("down", 0, 4, 2), Some(2));
        assert_eq!(project_navigation("up", 3, 4, 2), Some(1));
        assert_eq!(project_navigation("down", 1, 3, 2), Some(1));
        assert_eq!(project_navigation("down", 0, 4, 1), Some(1));
        assert_eq!(project_navigation("end", 0, 4, 2), Some(3));
        assert_eq!(project_navigation("home", 3, 4, 2), Some(0));
        for key in ["tab", "enter", "space", "escape", "pageup"] {
            assert_eq!(project_navigation(key, 0, 4, 2), None);
        }
        assert_eq!(project_navigation("right", 0, 0, 4), None);
    }

    #[test]
    fn stale_git_batches_cannot_repopulate_removed_checkouts() {
        let mut cache = ProjectGitCache::default();
        let first = PathBuf::from("first");
        let second = PathBuf::from("second");
        cache.invalidate(std::slice::from_ref(&first));
        let old_generation = cache.generation;
        let status = GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        };
        assert!(cache.commit(
            old_generation,
            HashMap::from([(first.clone(), status.clone())])
        ));
        cache.invalidate(std::slice::from_ref(&second));
        assert!(cache.statuses.is_empty());
        assert!(!cache.commit(old_generation, HashMap::from([(first, status.clone())])));
        assert!(cache.statuses.is_empty());
        assert!(cache.commit(cache.generation, HashMap::from([(second, status)])));
    }

    #[test]
    fn project_status_distinguishes_loading_missing_dirty_and_detached() {
        assert_eq!(project_git_label(None), "Loading Git status…");
        assert_eq!(
            project_git_label(Some(&GitStatus::default())),
            "Git status unavailable"
        );
        let mut status = GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        };
        assert_eq!(project_git_label(Some(&status)), "⎇ main · Clean");
        status.dirty = true;
        status.has_upstream = true;
        status.ahead = 2;
        status.behind = 1;
        assert_eq!(
            project_git_label(Some(&status)),
            "⎇ main · Modified · ↑2 · ↓1"
        );
        status.detached = true;
        status.branch = Some("abcdef1".into());
        assert!(project_git_label(Some(&status)).starts_with("Detached · abcdef1"));
    }

    #[test]
    fn wide_screens_do_not_stretch_recent_cards() {
        assert_eq!(recent_card_width(1440.), 336.);
        assert_eq!(recent_card_width(2560.), 336.);
        assert_eq!(recent_card_width(3840.), 336.);
    }

    #[test]
    fn recent_grid_fits_smaller_windows() {
        for (viewport, columns) in [(1200., 3.), (900., 2.), (600., 1.), (320., 1.)] {
            let occupied = recent_card_width(viewport) * columns + (columns - 1.) * 16.;
            let available = viewport - 48.;
            // Floored to whole pixels: must fit, leaving less than one pixel
            // of slack per card for rounding (notably Retina 2x).
            assert!(
                occupied <= available + 0.01,
                "{viewport}: {occupied} > {available}"
            );
            assert!(
                available - occupied < columns,
                "{viewport}: {occupied} leaves too much slack in {available}"
            );
        }
    }

    #[test]
    fn project_cards_distinguish_state_branch_and_sync() {
        assert_eq!(project_state_tag(None), None);
        assert_eq!(project_state_tag(Some(&GitStatus::default())), None);
        let clean = GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        };
        assert_eq!(
            project_state_tag(Some(&clean)),
            Some(("Clean", ColorName::Green))
        );
        assert_eq!(project_branch_pill(Some(&clean)).as_deref(), Some("⎇ main"));
        assert_eq!(project_sync_label(Some(&clean)), None);
        let dirty = GitStatus {
            branch: Some("main".into()),
            dirty: true,
            has_upstream: true,
            ahead: 2,
            behind: 1,
            ..Default::default()
        };
        assert_eq!(
            project_state_tag(Some(&dirty)),
            Some(("Modified", ColorName::Amber))
        );
        assert_eq!(project_sync_label(Some(&dirty)).as_deref(), Some("↑2 · ↓1"));
        let detached = GitStatus {
            branch: Some("abcdef1".into()),
            detached: true,
            ..Default::default()
        };
        assert_eq!(
            project_branch_pill(Some(&detached)).as_deref(),
            Some("◍ abcdef1")
        );
        assert_eq!(project_branch_pill(None), None);
    }

    #[test]
    fn project_cards_label_recency_without_a_date_dependency() {
        assert_eq!(
            parse_rfc3339_utc("2024-06-01T00:00:00Z"),
            Some(1_717_200_000)
        );
        assert_eq!(parse_rfc3339_utc("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_utc("2024-13-01T00:00:00Z"), None);
        let now = parse_rfc3339_utc("2024-06-01T00:00:00Z").unwrap();
        assert_eq!(
            project_opened_label(Some("2024-06-01T00:00:00Z"), now).as_deref(),
            Some("Opened just now")
        );
        assert_eq!(
            project_opened_label(Some("2024-05-31T22:00:00Z"), now).as_deref(),
            Some("Opened 2h ago")
        );
        assert_eq!(
            project_opened_label(Some("2024-05-25T00:00:00Z"), now).as_deref(),
            Some("Opened 1w ago")
        );
        assert_eq!(project_opened_label(None, now), None);
        assert_eq!(project_opened_label(Some(""), now), None);
        assert_eq!(project_opened_label(Some("broken"), now), None);
    }
}
