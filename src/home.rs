//! Home dashboard and dedicated destinations.
mod dashboard;
mod links;
mod pull_requests;
mod reading;
mod todos;
use self::links::{description_with_links, title_link};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::radio::Radio;
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

use crate::data::dashboard::{Category, Dashboard, Group, Item, Kind, safe_web_url};
use crate::data::{
    DataRoot, RecentRepository, RepositoryEntry, SyncStatus, SyncTracker, all_repositories,
    recent_repositories,
};
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
    Relationships,
    LinkRepository { key: String },
    EditRepository { key: String },
    UnlinkRepository { key: String },
    RemoveRepository { key: String },
    OpenAgentSession(crate::agent_sessions::SessionKey),
    RefreshSessions,
}

/// Session-local group filter for the Projects page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ProjectGroupFilter {
    #[default]
    All,
    Group(String),
    Ungrouped,
}

impl ProjectGroupFilter {
    fn matches(&self, entry: &RepositoryEntry) -> bool {
        match self {
            Self::All => true,
            Self::Group(group) => entry.group.as_deref() == Some(group.as_str()),
            Self::Ungrouped => entry.group.is_none(),
        }
    }
}

/// Distinct non-empty groups present, sorted. Pure so the filter row stays
/// unit-testable without a window.
fn project_groups(entries: &[RepositoryEntry]) -> Vec<String> {
    let mut groups: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.group.clone())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// Session-local project filter for Todos. `All` shows everything, `Unscoped`
/// shows todos with no project, and `Project(key)` shows one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum TodoProjectFilter {
    #[default]
    All,
    Unscoped,
    Project(String),
}

impl TodoProjectFilter {
    fn matches(&self, project: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Unscoped => project.is_none(),
            Self::Project(key) => project == Some(key.as_str()),
        }
    }
}

/// One kanban column on the Todos board: a repository or the Unscoped bucket.
/// Pure so column construction stays unit-testable without a window.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TodoColumn {
    key: Option<String>,
    title: String,
}

fn todo_board_columns(projects: &[RepositoryEntry], todos: &[Item]) -> Vec<TodoColumn> {
    let mut columns = vec![TodoColumn {
        key: None,
        title: "Unscoped".to_owned(),
    }];
    for entry in projects {
        let title = entry
            .display_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| entry.key.clone());
        columns.push(TodoColumn {
            key: Some(entry.key.clone()),
            title,
        });
    }
    let known: std::collections::HashSet<&str> =
        projects.iter().map(|entry| entry.key.as_str()).collect();
    let mut orphans: Vec<String> = todos
        .iter()
        .filter(|item| item.kind == Kind::Todo)
        .filter_map(|item| item.project_key().map(str::to_owned))
        .filter(|key| !known.contains(key.as_str()))
        .collect();
    orphans.sort();
    orphans.dedup();
    for key in orphans {
        columns.push(TodoColumn {
            title: format!("{key} (removed)"),
            key: Some(key),
        });
    }
    columns
}

pub(crate) struct HomeView {
    artifacts: gpui_kit::Entity<crate::artifacts::ArtifactBrowser>,
    pub(crate) focus_handle: FocusHandle,
    root: Option<DataRoot>,
    tracker: SyncTracker,
    data: Dashboard,
    error: Option<String>,
    projects: Vec<RecentRepository>,
    /// Every portable record joined with its device binding (linked,
    /// unlinked, and missing-checkout alike) for the Projects page.
    /// `projects` above stays linked-only for the Recent cards.
    all_projects: Vec<RepositoryEntry>,
    project_errors: Vec<String>,
    project_refresh: crate::artifacts::Refresh,
    /// Session-local group filter for the Projects page. Survives reloads;
    /// reset only by picking another filter.
    project_group_filter: ProjectGroupFilter,
    sessions: Vec<(crate::agent_sessions::SessionSummary, String)>,
    session_errors: Vec<String>,
    sessions_loaded: bool,
    page: Option<&'static str>,
    show_completed: bool,
    active: bool,
    project_git: ProjectGitCache,
    git_loading: bool,
    pr_status: crate::pull_requests::Cache,
    pr_loading: bool,
    /// Session-local group filter for PRs (`None` shows Personal + Work).
    /// Shared by the inbox list and the dedicated board.
    group_filter: Option<Group>,
    /// Session-local category filter for the PR inbox (`None` shows all
    /// categories). The dedicated board keeps showing every column; only the
    /// inbox list applies this filter.
    pr_category_filter: Option<Category>,
    /// Session-local group filter for Todos (`None` shows Personal + Work).
    /// Shared by the inbox list and the dedicated board.
    todo_group_filter: Option<Group>,
    /// Session-local project filter for the Todo inbox (`All` shows every
    /// project). The dedicated board shows every project as a column and
    /// only applies the group filter.
    todo_project_filter: TodoProjectFilter,
    project_focus: HashMap<String, FocusHandle>,
    add_project_focus: FocusHandle,
    scroll: ScrollHandle,
    /// Cursor for navigation-mode `j`/`k` through Home cards in visual order
    /// (sessions, projects, Add project, then inbox items). `None` outside navigation mode;
    /// `Enter` activates the cursor item once and exits the mode.
    navigation_cursor: Option<usize>,
    /// Full-color harness logos for the Recent Activity session cards,
    /// rasterized once at creation (the harness set is fixed).
    agent_icon_tiles: std::rc::Rc<crate::agent_icons::AgentIconTiles>,
}

/// One stop for navigation-mode `j`/`k` on Home, in visual order.
#[derive(Clone, PartialEq, Eq)]
enum HomeNavTarget {
    Session(usize),
    Project(usize),
    AddProject,
    Item(String),
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
        let artifacts = cx.new(|cx| crate::artifacts::ArtifactBrowser::new(root.clone(), cx));
        cx.subscribe(
            &artifacts,
            |_, _, event: &crate::artifacts::OpenSession, cx| {
                cx.emit(HomeEvent::OpenAgentSession(event.0.key.clone()))
            },
        )
        .detach();
        let mut agent_icon_tiles = crate::agent_icons::AgentIconTiles::new();
        crate::agent_icons::ensure_tiles(crate::agent::AgentKind::ALL, &mut agent_icon_tiles, cx);
        let mut view = Self {
            artifacts,
            focus_handle: cx.focus_handle(),
            root,
            tracker,
            data: Dashboard::default(),
            error: None,
            projects: Vec::new(),
            all_projects: Vec::new(),
            project_errors: Vec::new(),
            project_refresh: crate::artifacts::Refresh::default(),
            project_group_filter: ProjectGroupFilter::All,
            sessions: Vec::new(),
            session_errors: Vec::new(),
            sessions_loaded: false,
            page: None,
            show_completed: false,
            active: true,
            project_git: ProjectGitCache::default(),
            git_loading: false,
            pr_status: crate::pull_requests::Cache::default(),
            pr_loading: false,
            group_filter: None,
            pr_category_filter: None,
            todo_group_filter: None,
            todo_project_filter: TodoProjectFilter::All,
            project_focus: HashMap::new(),
            add_project_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            navigation_cursor: None,
            agent_icon_tiles: std::rc::Rc::new(agent_icon_tiles),
        };
        view.reload(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROJECT_GIT_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh_project_git(cx);
                        this.refresh_pull_requests(false, cx);
                    })
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
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.active = true;
        self.page = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    pub(crate) fn resume_page(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.artifacts.update(cx, |view, cx| {
            view.set_active(self.page == Some("Artifacts"), cx)
        });
        self.reload(cx);
    }

    pub(crate) fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.artifacts
            .update(cx, |view, cx| view.set_active(false, cx));
        self.active = false;
    }

    /// Show the artifact browser, reachable through the command palette's
    /// **Browse artifacts** entry. The artifact
    /// browser activates, and a reload refreshes projects/dashboard plus
    /// the now-visible browser.
    pub(crate) fn show_artifacts_page(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.page = Some("Artifacts");
        self.artifacts
            .update(cx, |view, cx| view.set_active(true, cx));
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    pub(crate) fn is_artifacts_page(&self) -> bool {
        self.page == Some("Artifacts")
    }

    pub(crate) fn is_projects_page(&self) -> bool {
        self.page == Some("Projects")
    }

    /// Projects-page rows in render order: every portable record matching
    /// the group filter, sorted by key.
    fn visible_projects(&self) -> Vec<RepositoryEntry> {
        self.all_projects
            .iter()
            .filter(|entry| self.project_group_filter.matches(entry))
            .cloned()
            .collect()
    }

    /// Linked checkout paths across Recent cards and the Projects page, for
    /// git-status cache invalidation and background refresh.
    fn linked_checkout_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .projects
            .iter()
            .map(|project| project.checkout_path.clone())
            .collect();
        for entry in self.all_projects.iter().filter(|entry| entry.is_linked()) {
            if let Some(path) = &entry.checkout_path
                && !paths.contains(path)
            {
                paths.push(path.clone());
            }
        }
        paths
    }

    pub(crate) fn artifacts_include_archived(&self, cx: &gpui_kit::App) -> bool {
        self.artifacts.read(cx).include_archived()
    }

    pub(crate) fn set_artifacts_archived(&self, include: bool, cx: &mut Context<Self>) {
        self.artifacts
            .update(cx, |view, cx| view.set_include_archived(include, cx));
    }

    pub(crate) fn artifacts_navigation_state(
        &self,
        cx: &gpui_kit::App,
    ) -> crate::navigation::ResourceState {
        self.artifacts.read(cx).navigation_state()
    }

    pub(crate) fn artifacts_navigation_panes(
        &self,
        cx: &gpui_kit::App,
    ) -> Vec<(&'static str, FocusHandle)> {
        self.artifacts.read(cx).navigation_panes(cx)
    }

    pub(crate) fn run_artifact_command(
        &mut self,
        command: crate::navigation::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.artifacts.update(cx, |artifacts, cx| match command {
            crate::navigation::Command::EditMarkdown => artifacts.begin_markdown_edit(window, cx),
            crate::navigation::Command::AddComment => artifacts.begin_comment(window, cx),
            crate::navigation::Command::SaveDraft => artifacts.save_draft(window, cx),
            crate::navigation::Command::CancelDraft => artifacts.cancel_draft(window, cx),
            _ => {}
        });
    }

    /// Navigation-mode `j`/`k` on the global Artifacts page: move the
    /// artifact sidebar selection, keeping the mode open for repeats.
    pub(crate) fn artifacts_move_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        self.artifacts
            .update(cx, |view, cx| view.move_selection(down, cx));
    }

    /// Ordered stops for navigation-mode `j`/`k` in visual order: recent
    /// sessions, recent projects, Add project, then inbox items (PRs grouped
    /// by category, then todos, then reading) matching the dashboard render order. Only
    /// the dashboard and PR board participate; the board follows its filter.
    fn navigation_targets(&self) -> Vec<HomeNavTarget> {
        if self.is_pull_requests_page() {
            return Category::ALL
                .into_iter()
                .flat_map(|category| {
                    self.visible_pull_requests(category)
                        .into_iter()
                        .map(|item| HomeNavTarget::Item(item.id))
                })
                .collect();
        }
        if self.is_todos_page() {
            return self
                .todo_board_columns()
                .into_iter()
                .flat_map(|column| self.column_todos(column.key.as_deref()))
                .map(|item| HomeNavTarget::Item(item.id))
                .collect();
        }
        if self.is_reading_page() {
            return self
                .visible_reading()
                .into_iter()
                .map(|item| HomeNavTarget::Item(item.id))
                .collect();
        }
        if self.page.is_some() {
            return Vec::new();
        }
        let mut targets = Vec::new();
        for index in 0..self.sessions.len() {
            targets.push(HomeNavTarget::Session(index));
        }
        for index in 0..self.projects.len() {
            targets.push(HomeNavTarget::Project(index));
        }
        targets.push(HomeNavTarget::AddProject);
        let visible: Vec<Item> = self
            .data
            .items
            .iter()
            .filter(|i| self.show_completed || !i.completed)
            .cloned()
            .collect();
        for category in Category::ALL {
            if self
                .pr_category_filter
                .is_some_and(|filter| filter != category)
            {
                continue;
            }
            for item in visible.iter().filter(|i| {
                i.kind == Kind::PullRequest
                    && (self.show_completed || !i.completed)
                    && i.category == category
                    && self.group_filter.is_none_or(|group| i.group == group)
            }) {
                targets.push(HomeNavTarget::Item(item.id.clone()));
            }
        }
        for item in visible.iter().filter(|i| {
            i.kind == Kind::Todo
                && (self.show_completed || !i.completed)
                && self.todo_group_filter.is_none_or(|group| i.group == group)
                && self.todo_project_filter.matches(i.project_key())
        }) {
            targets.push(HomeNavTarget::Item(item.id.clone()));
        }
        for item in visible
            .iter()
            .filter(|i| i.kind == Kind::Reading && (self.show_completed || !i.completed))
        {
            targets.push(HomeNavTarget::Item(item.id.clone()));
        }
        targets
    }

    /// Whether a dashboard item id holds the navigation cursor, for card
    /// highlight while navigation mode owns the keyboard.
    fn is_cursor_item(&self, id: &str) -> bool {
        let targets = self.navigation_targets();
        self.navigation_cursor
            .and_then(|cursor| targets.get(cursor))
            .is_some_and(|target| matches!(target, HomeNavTarget::Item(item_id) if item_id == id))
    }

    /// Enter/exit navigation-mode cursor tracking on Home. Entering starts at
    /// the first card so `j`/`k` repeat predictably; exiting clears the
    /// highlight. Reloads keep the cursor clamped rather than dropping it.
    pub(crate) fn set_navigation_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if active {
            let count = self.navigation_targets().len();
            self.navigation_cursor = if count == 0 { None } else { Some(0) };
        } else {
            self.navigation_cursor = None;
        }
        cx.notify();
    }

    /// Clear the cursor without notifying; the caller notifies once.
    pub(crate) fn take_navigation_cursor(&mut self) -> Option<usize> {
        self.navigation_cursor.take()
    }

    /// Whether a `j`/`k` cursor is active for navigation-mode `Enter` to open.
    pub(crate) fn navigation_cursor_active(&self) -> bool {
        self.navigation_cursor.is_some()
    }

    /// Whether the dashboard offers any `j`/`k` stops right now.
    pub(crate) fn has_navigation_targets(&self) -> bool {
        !self.navigation_targets().is_empty()
    }

    /// Move the Home cursor one step, clamping at the ends like pane movement.
    /// Keeps navigation mode open for repeats; a no-op with no targets. A
    /// first press with no cursor lands at the nearest end.
    pub(crate) fn move_navigation_cursor(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.navigation_targets().len();
        if count == 0 {
            self.navigation_cursor = None;
            return;
        }
        let Some(current) = self.navigation_cursor else {
            self.navigation_cursor = Some(if down { 0 } else { count - 1 });
            cx.notify();
            return;
        };
        let current = current.min(count - 1);
        self.navigation_cursor = Some(crate::navigation::move_index(current, count, down));
        cx.notify();
    }

    /// Activate the cursor item for navigation-mode `Enter`: open sessions and
    /// projects, toggle todo/reading checkboxes, edit PRs. Returns true when
    /// something ran so the caller exits navigation mode.
    pub(crate) fn activate_navigation_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let targets = self.navigation_targets();
        let Some(cursor) = self.navigation_cursor else {
            return false;
        };
        let Some(target) = targets.get(cursor) else {
            return false;
        };
        match target {
            HomeNavTarget::Session(index) => {
                if let Some((session, _)) = self.sessions.get(*index) {
                    let key = session.key.clone();
                    cx.emit(HomeEvent::OpenAgentSession(key));
                    return true;
                }
                false
            }
            HomeNavTarget::Project(index) => {
                if let Some(project) = self.projects.get(*index) {
                    let key = project.key.clone();
                    let label = project.display_name.clone().unwrap_or_else(|| key.clone());
                    cx.emit(HomeEvent::OpenRepository { key, label });
                    return true;
                }
                false
            }
            HomeNavTarget::AddProject => {
                cx.emit(HomeEvent::AddRepository);
                true
            }
            HomeNavTarget::Item(id) => {
                let Some(item) = self.data.items.iter().find(|i| &i.id == id).cloned() else {
                    return false;
                };
                if item.kind == Kind::PullRequest {
                    self.editor(item, window, cx);
                    return true;
                }
                let id = id.clone();
                self.change(window, cx, |data| {
                    if let Some(i) = data.items.iter_mut().find(|i| i.id == id) {
                        i.completed = !i.completed;
                    }
                    Ok(())
                });
                true
            }
        }
    }

    fn refresh_project_git(&mut self, cx: &mut Context<Self>) {
        let on_projects = self.page == Some("Projects");
        if !self.active || (!on_projects && self.page.is_some()) || self.git_loading {
            return;
        }
        let paths = self.linked_checkout_paths();
        if paths.is_empty() {
            return;
        }
        self.git_loading = true;
        let generation = self.project_git.generation;
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
        self.refresh_artifacts(cx);
        self.reload_projects(cx);
        if let Some(root) = &self.root {
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
        self.refresh_pull_requests(false, cx);
        cx.notify();
    }

    /// Catalog locks can wait behind sync or graph edits; never take them on the
    /// UI thread. Coalesce reloads and reject an older in-flight projection.
    fn reload_projects(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let Some(generation) = self.project_refresh.request() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let (projects, result) = cx
                .background_spawn(async move {
                    (recent_repositories(&root, 4), all_repositories(&root))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.project_refresh.finish(generation) {
                    this.projects = projects;
                    match result {
                        Ok((entries, errors)) => {
                            this.all_projects = entries;
                            this.project_errors = errors;
                        }
                        Err(error) => {
                            this.project_errors =
                                vec![format!("Could not list repositories: {error:#}")]
                        }
                    }
                    this.refresh_project_views(cx);
                    cx.notify();
                } else {
                    this.reload_projects(cx);
                }
            });
        })
        .detach();
    }

    fn refresh_project_views(&mut self, cx: &mut Context<Self>) {
        for project in &self.projects {
            self.project_focus
                .entry(project.key.clone())
                .or_insert_with(|| cx.focus_handle());
        }
        for entry in &self.all_projects {
            self.project_focus
                .entry(entry.key.clone())
                .or_insert_with(|| cx.focus_handle());
        }
        // Drop handles for records that no longer exist anywhere.
        self.project_focus.retain(|key, _| {
            self.projects.iter().any(|project| &project.key == key)
                || self.all_projects.iter().any(|entry| &entry.key == key)
        });
        self.project_git.invalidate(&self.linked_checkout_paths());
        self.refresh_project_git(cx);
    }

    pub(crate) fn refresh_artifacts(&mut self, cx: &mut Context<Self>) {
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
                self.refresh_pull_requests(false, cx);
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
        let url = input(&item.url, "https://…", window, cx);
        let home = cx.entity().downgrade();
        let category = std::rc::Rc::new(std::cell::Cell::new(item.category));
        let group = std::rc::Rc::new(std::cell::Cell::new(item.group));
        let todo_project =
            std::rc::Rc::new(std::cell::RefCell::new(item.project.trim().to_owned()));
        // Project choices for the Todo editor: Unscoped plus every known
        // repository, plus the current value when it points at a removed
        // record so existing scope is never silently dropped.
        let mut project_options: Vec<(String, String)> = self
            .all_projects
            .iter()
            .map(|entry| {
                let title = entry
                    .display_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| entry.key.clone());
                (entry.key.clone(), title)
            })
            .collect();
        let current_project = item.project.trim().to_owned();
        if !current_project.is_empty()
            && !project_options
                .iter()
                .any(|(key, _)| key == &current_project)
        {
            project_options.push((
                current_project.clone(),
                format!("{current_project} (removed)"),
            ));
        }
        let original = self.data.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let mut form = v_flex().gap_3();
            if item.kind != Kind::PullRequest {
                form = form.child("Title").child(Input::new(&title));
            }
            if item.kind == Kind::Todo {
                form = form.child("Description").child(Input::new(&description));
            } else {
                form = form.child("URL").child(Input::new(&url));
            }
            if item.kind == Kind::PullRequest {
                form = form.child("The title is fetched automatically from GitHub.").child("Category").child(h_flex().gap_2().flex_wrap().children(Category::ALL.into_iter().enumerate().map(|(index, choice)| {
                    let category = category.clone();
                    Button::new(("category", index)).label(if category.get() == choice { format!("✓ {}", choice.label()) } else { choice.label().to_owned() })
                        .on_click(move |_, _, cx| { category.set(choice); cx.refresh_windows(); })
                })))
                .child("Group")
                .child(h_flex().gap_2().children(Group::ALL.into_iter().enumerate().map(|(index, choice)| {
                    let group = group.clone();
                    Button::new(("group", index))
                        .label(if group.get() == choice { format!("✓ {}", choice.label()) } else { choice.label().to_owned() })
                        .on_click(move |_, _, cx| { group.set(choice); cx.refresh_windows(); })
                })));
            }
            if item.kind == Kind::Todo {
                form = form.child("Group").child(h_flex().gap_2().children(Group::ALL.into_iter().enumerate().map(|(index, choice)| {
                    let group = group.clone();
                    Button::new(("todo-group", index))
                        .label(if group.get() == choice { format!("✓ {}", choice.label()) } else { choice.label().to_owned() })
                        .on_click(move |_, _, cx| { group.set(choice); cx.refresh_windows(); })
                })));
                let selected = todo_project.borrow().clone();
                let selected_label = if selected.is_empty() {
                    "Unscoped".to_owned()
                } else {
                    project_options
                        .iter()
                        .find(|(key, _)| key == &selected)
                        .map(|(_, title)| title.clone())
                        .unwrap_or_else(|| selected.clone())
                };
                let todo_project = todo_project.clone();
                let project_options = project_options.clone();
                form = form.child("Project").child(
                    Button::new("todo-project")
                        .label(selected_label)
                        .dropdown_caret(true)
                        .dropdown_menu(move |mut menu, _, _| {
                            for (key, title) in std::iter::once((
                                String::new(),
                                "Unscoped".to_owned(),
                            ))
                            .chain(project_options.clone())
                            {
                                let todo_project = todo_project.clone();
                                let is_current = *todo_project.borrow() == key;
                                menu = menu.item(
                                    PopupMenuItem::new(title)
                                        .checked(is_current)
                                        .on_click(move |_, _, cx| {
                                            *todo_project.borrow_mut() = key.clone();
                                            cx.refresh_windows();
                                        }),
                                );
                            }
                            menu
                        }),
                );
            }
            let group = group.clone();
            let todo_project = todo_project.clone();
            let (title, description, url, home, category, item, original) = (title.clone(), description.clone(), url.clone(), home.clone(), category.clone(), item.clone(), original.clone());
            dialog.title("Edit Home item").w(px(560.)).child(form)
                .footer(DialogFooter::new()
                    .child(Button::new("cancel-home-item").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("save-home-item").primary().label("Save").on_click(|_, window, cx| window.dispatch_action(Box::new(Confirm { secondary: false }), cx))))
                .on_ok(move |_, window, cx| {
                let mut item = item.clone();
                item.title = if item.kind == Kind::PullRequest {
                    String::new()
                } else {
                    title.read(cx).value().to_string()
                };
                item.description = description.read(cx).value().to_string();
                item.url = url.read(cx).value().to_string();
                item.category = category.get();
                item.group = group.get();
                if item.kind == Kind::Todo {
                    item.project = todo_project.borrow().clone();
                }
                home.update(cx, |this, cx| this.change(window, cx, |data| {
                    anyhow::ensure!(*data == original, "Home changed while this form was open. Close it and reopen the item.");
                    data.upsert(item)
                })).unwrap_or(false)
            })
        });
    }

    fn view_all(&self, destination: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new(item_id("view-all", destination))
            .ghost()
            .small()
            .label("View all")
            .icon(gpui_kit::component::IconName::ArrowRight)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus_handle.focus(window, cx);
                this.page = Some(destination);
                this.navigation_cursor = None;
                this.refresh_pull_requests(false, cx);
                this.scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }))
    }

    /// Category dropdown for the PR inbox: All plus every category
    /// (Waiting for Approval, To Review, Watching). Matches the "Category"
    /// section of the PR editor and the board columns.
    fn pr_category_filter_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.pr_category_filter;
        let label = current.map_or("All", Category::label);
        let home = cx.entity().downgrade();
        Button::new("pr-category-filter")
            .ghost()
            .label(format!("{label} ▾"))
            .dropdown_menu(move |mut menu, _, _| {
                for option in [
                    None,
                    Some(Category::WaitingForReview),
                    Some(Category::ToReview),
                    Some(Category::Watching),
                ] {
                    let option_label = option.map_or("All", Category::label);
                    let home = home.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label)
                            .checked(current == option)
                            .on_click(move |_, _, cx| {
                                let _ = home.update(cx, |this, cx| {
                                    if this.pr_category_filter != option {
                                        this.pr_category_filter = option;
                                        this.navigation_cursor = None;
                                        cx.notify();
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// Group dropdown for the PR inbox: All, Personal, Work. Matches the
    /// "Group" section of the PR editor.
    fn group_filter_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.group_filter;
        let label = current.map_or("All", Group::label);
        let home = cx.entity().downgrade();
        Button::new("group-filter")
            .ghost()
            .label(format!("{label} ▾"))
            .dropdown_menu(move |mut menu, _, _| {
                for option in [None, Some(Group::Personal), Some(Group::Work)] {
                    let option_label = option.map_or("All", Group::label);
                    let home = home.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label)
                            .checked(current == option)
                            .on_click(move |_, _, cx| {
                                let _ = home.update(cx, |this, cx| {
                                    if this.group_filter != option {
                                        this.group_filter = option;
                                        this.navigation_cursor = None;
                                        cx.notify();
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    pub(crate) fn is_todos_page(&self) -> bool {
        self.page == Some("Todos")
    }

    /// Display name for a todo project key: the repository display name when
    /// known, otherwise the raw key. Empty means Unscoped.
    fn todo_project_title(&self, key: &str) -> String {
        if key.trim().is_empty() {
            return "Unscoped".to_owned();
        }
        self.all_projects
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| {
                entry
                    .display_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| entry.key.clone())
            })
            .unwrap_or_else(|| format!("{key} (removed)"))
    }

    /// Inbox todos in global order, filtered by completion, group, and
    /// project. Pure ordering: the inbox stays a flat list; the board groups
    /// the same items into columns.
    fn visible_todos(&self) -> Vec<Item> {
        self.data
            .items
            .iter()
            .filter(|i| {
                i.kind == Kind::Todo
                    && (self.show_completed || !i.completed)
                    && self.todo_group_filter.is_none_or(|group| i.group == group)
                    && self.todo_project_filter.matches(i.project_key())
            })
            .cloned()
            .collect()
    }

    /// Board columns: Unscoped first, then every known repository in key
    /// order, then orphan keys from removed repositories so scoped todos are
    /// never hidden.
    fn todo_board_columns(&self) -> Vec<TodoColumn> {
        todo_board_columns(&self.all_projects, &self.data.items)
    }

    /// Todos for one board column in global order, filtered by completion
    /// and the shared group filter.
    fn column_todos(&self, project: Option<&str>) -> Vec<Item> {
        let normalized = project.unwrap_or_default().trim();
        self.data
            .items
            .iter()
            .filter(|i| {
                i.kind == Kind::Todo
                    && (self.show_completed || !i.completed)
                    && self.todo_group_filter.is_none_or(|group| i.group == group)
                    && i.project.trim() == normalized
            })
            .cloned()
            .collect()
    }

    /// Group dropdown for the Todo inbox and board: All, Personal, Work.
    /// Matches the "Group" section of the Todo editor.
    fn todo_group_filter_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.todo_group_filter;
        let label = current.map_or("All", Group::label);
        let home = cx.entity().downgrade();
        Button::new("todo-group-filter")
            .ghost()
            .label(format!("{label} ▾"))
            .dropdown_menu(move |mut menu, _, _| {
                for option in [None, Some(Group::Personal), Some(Group::Work)] {
                    let option_label = option.map_or("All", Group::label);
                    let home = home.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label)
                            .checked(current == option)
                            .on_click(move |_, _, cx| {
                                let _ = home.update(cx, |this, cx| {
                                    if this.todo_group_filter != option {
                                        this.todo_group_filter = option;
                                        this.navigation_cursor = None;
                                        cx.notify();
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// Project dropdown for the Todo inbox: All, Unscoped, plus every known
    /// repository. An orphaned filter value is kept as an option so active
    /// filtering never hides its own selection.
    fn todo_project_filter_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.todo_project_filter.clone();
        let label = match &current {
            TodoProjectFilter::All => "All".to_owned(),
            TodoProjectFilter::Unscoped => "Unscoped".to_owned(),
            TodoProjectFilter::Project(key) => self.todo_project_title(key),
        };
        let home = cx.entity().downgrade();
        let mut options: Vec<(TodoProjectFilter, String)> = vec![
            (TodoProjectFilter::All, "All".to_owned()),
            (TodoProjectFilter::Unscoped, "Unscoped".to_owned()),
        ];
        for entry in &self.all_projects {
            let title = entry
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| entry.key.clone());
            options.push((TodoProjectFilter::Project(entry.key.clone()), title));
        }
        if let TodoProjectFilter::Project(key) = &current
            && !self.all_projects.iter().any(|entry| &entry.key == key)
        {
            options.push((current.clone(), format!("{key} (removed)")));
        }
        Button::new("todo-project-filter")
            .ghost()
            .label(format!("{label} ▾"))
            .dropdown_menu(move |mut menu, _, _| {
                for (option, option_label) in options.clone() {
                    let home = home.clone();
                    let option_clone = option.clone();
                    let current_clone = current.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label)
                            .checked(current_clone == option_clone)
                            .on_click(move |_, _, cx| {
                                let option = option_clone.clone();
                                let _ = home.update(cx, |this, cx| {
                                    if this.todo_project_filter != option {
                                        this.todo_project_filter = option;
                                        this.navigation_cursor = None;
                                        cx.notify();
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// The dedicated Projects page: every portable record, including ones
    /// with no checkout on this device, as Home-style cards with a `⋯`
    /// options menu. Back navigation and the title live in the workspace
    /// titlebar, matching the Artifacts page.
    fn projects_page(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let linked = self.all_projects.iter().filter(|e| e.is_linked()).count();
        let missing = self
            .all_projects
            .iter()
            .filter(|e| e.checkout_missing)
            .count();
        let unlinked = self.all_projects.len() - linked - missing;
        let card_width = recent_card_width(f32::from(window.viewport_size().width));
        let visible = self.visible_projects();
        let mut page = v_flex().gap_4().child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_3()
                .child(self.group_filter_row(cx))
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .whitespace_nowrap()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} linked · {} not linked · {} missing checkout",
                            linked, unlinked, missing
                        )),
                ),
        );
        if visible.is_empty() {
            page = page.child(div().text_color(cx.theme().muted_foreground).child(
                if self.all_projects.is_empty() {
                    "No repositories yet. Add your first project below."
                } else {
                    "No repositories in this group."
                },
            ));
        }
        let mut grid = h_flex().gap_4().flex_wrap();
        for (index, entry) in visible.iter().enumerate() {
            grid = grid.child(self.project_card(entry, index, visible.len(), card_width, cx));
        }
        page = page.child(grid);
        for error in &self.project_errors {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        page.child(
            Button::new("project-relationships")
                .self_start()
                .ghost()
                .label("Relationships")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeEvent::Relationships))),
        )
        .child(
            Button::new("add-project-page")
                .self_start()
                .ghost()
                .label("+ Add project")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeEvent::AddRepository))),
        )
    }

    /// Group radios: All, each present group, and Ungrouped when relevant.
    fn group_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = project_groups(&self.all_projects);
        let has_ungrouped = self.all_projects.iter().any(|entry| entry.group.is_none());
        let mut row = h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Group:"),
            )
            .child(self.group_pill(ProjectGroupFilter::All, "All", cx));
        for group in groups {
            let label = group.clone();
            row = row.child(self.group_pill(ProjectGroupFilter::Group(group), &label, cx));
        }
        if has_ungrouped {
            row = row.child(self.group_pill(ProjectGroupFilter::Ungrouped, "Ungrouped", cx));
        }
        row
    }

    fn group_pill(
        &self,
        filter: ProjectGroupFilter,
        label: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.project_group_filter == filter;
        Radio::new(item_id("project-group", label))
            .label(label.to_owned())
            .checked(active)
            .on_click(cx.listener(move |this, _, _, cx| {
                // Radios never toggle off: selecting the active one is a
                // no-op, any other pick becomes the filter.
                if this.project_group_filter != filter {
                    this.project_group_filter = filter.clone();
                    cx.notify();
                }
            }))
    }

    /// One Projects card mirroring the Recent-Projects card visuals: title
    /// plus group pill plus `⋯` menu, git-state row, description, and an
    /// opened/footer row. Linked titles and footers open the workspace;
    /// unlinked cards act through their menu and footer link buttons, since
    /// a nested whole-card button would swallow the menu's clicks.
    fn project_card(
        &self,
        entry: &RepositoryEntry,
        index: usize,
        total: usize,
        card_width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = entry.key.clone();
        let title = entry.display_name.clone().unwrap_or_else(|| key.clone());
        let group = entry.group.clone().filter(|value| !value.trim().is_empty());
        let description = entry
            .description
            .clone()
            .filter(|value| !value.trim().is_empty());
        let has_binding = entry.checkout_path.is_some();
        let linked = entry.is_linked();
        let now_secs = current_unix_secs();
        let opened = project_opened_label(entry.last_opened_at.as_deref(), now_secs)
            .unwrap_or_else(|| "Not opened yet".to_owned());
        let git_status = entry
            .checkout_path
            .as_ref()
            .and_then(|path| self.project_git.statuses.get(path));
        let state = project_state_tag(git_status);
        let branch_pill = project_branch_pill(git_status);
        let sync = project_sync_label(git_status);

        let home = cx.entity().downgrade();
        let link_key = key.clone();
        let edit_key = key.clone();
        let unlink_key = key.clone();
        let remove_key = key.clone();
        let link_label = if has_binding {
            "Re-link"
        } else {
            "Link checkout"
        };
        let mut card = v_flex()
            .flex_none()
            .w(px(card_width))
            .min_h(px(160.))
            .gap_2()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .track_focus(&self.project_focus[&key])
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                if event.keystroke.key.as_str() == "enter" {
                    let visible = this.visible_projects();
                    if visible.get(index).is_some_and(|entry| entry.is_linked()) {
                        let entry = &visible[index];
                        let label = entry
                            .display_name
                            .clone()
                            .unwrap_or_else(|| entry.key.clone());
                        cx.emit(HomeEvent::OpenRepository {
                            key: entry.key.clone(),
                            label,
                        });
                    }
                    return;
                }
                let columns = recent_columns(f32::from(window.viewport_size().width));
                if let Some(next) = project_navigation(&event.keystroke.key, index, total, columns)
                {
                    let visible = this.visible_projects();
                    if let Some(next_key) = visible.get(next).map(|entry| entry.key.clone()) {
                        this.project_focus[&next_key].focus(window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                        cx.notify();
                    }
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
                            .child(title.clone()),
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
                    })
                    .child(
                        Button::new(item_id("repo-options", &key))
                            .ghost()
                            .label("⋯")
                            .accessibility_label(format!("Options for {title}"))
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let home_open = home.clone();
                                let open_key = key.clone();
                                let open_title = title.clone();
                                let home_link = home.clone();
                                let link_key = link_key.clone();
                                let home_edit = home.clone();
                                let edit_key = edit_key.clone();
                                let home_unlink = home.clone();
                                let unlink_key = unlink_key.clone();
                                let home_remove = home.clone();
                                let remove_key = remove_key.clone();
                                let mut menu = menu;
                                if linked {
                                    menu = menu.item(PopupMenuItem::new("Open").on_click(
                                        move |_, _, cx| {
                                            let _ = home_open.update(cx, |_, cx| {
                                                cx.emit(HomeEvent::OpenRepository {
                                                    key: open_key.clone(),
                                                    label: open_title.clone(),
                                                });
                                            });
                                        },
                                    ));
                                }
                                menu = menu.item(PopupMenuItem::new(link_label).on_click(
                                    move |_, _, cx| {
                                        let _ = home_link.update(cx, |_, cx| {
                                            cx.emit(HomeEvent::LinkRepository {
                                                key: link_key.clone(),
                                            });
                                        });
                                    },
                                ));
                                menu = menu.item(PopupMenuItem::new("Edit").on_click(
                                    move |_, _, cx| {
                                        let _ = home_edit.update(cx, |_, cx| {
                                            cx.emit(HomeEvent::EditRepository {
                                                key: edit_key.clone(),
                                            });
                                        });
                                    },
                                ));
                                if has_binding {
                                    menu = menu.item(PopupMenuItem::new("Unlink").on_click(
                                        move |_, _, cx| {
                                            let _ = home_unlink.update(cx, |_, cx| {
                                                cx.emit(HomeEvent::UnlinkRepository {
                                                    key: unlink_key.clone(),
                                                });
                                            });
                                        },
                                    ));
                                }
                                menu.separator().item(PopupMenuItem::new("Delete").on_click(
                                    move |_, _, cx| {
                                        let _ = home_remove.update(cx, |_, cx| {
                                            cx.emit(HomeEvent::RemoveRepository {
                                                key: remove_key.clone(),
                                            });
                                        });
                                    },
                                ))
                            }),
                    ),
            );
        if linked {
            card = card.child(
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
                    .when(git_status.is_none(), |row| {
                        row.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Loading Git status…"),
                        )
                    })
                    .when_some(branch_pill, |this, pill| {
                        this.child(
                            Tag::secondary()
                                .with_size(Size::Small)
                                .rounded_full()
                                .min_w_0()
                                .max_w_full()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(pill),
                                ),
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
            );
        } else if entry.checkout_missing {
            card = card.child(div().text_sm().text_color(cx.theme().danger).child(format!(
                        "Checkout missing: {}",
                        entry
                            .checkout_path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    )));
        } else {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Not linked to this device"),
            );
        }
        card = card.when_some(description, |this, text| {
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
        });
        for warning in &entry.warnings {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(warning.clone()),
            );
        }
        let footer_key = entry.key.clone();
        let footer_title = entry
            .display_name
            .clone()
            .unwrap_or_else(|| entry.key.clone());
        card.child(
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
                    Button::new(item_id("repo-open", &entry.key))
                        .ghost()
                        .label(if linked {
                            "Open →"
                        } else if has_binding {
                            "Re-link"
                        } else {
                            "Link"
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if linked {
                                cx.emit(HomeEvent::OpenRepository {
                                    key: footer_key.clone(),
                                    label: footer_title.clone(),
                                });
                            } else {
                                cx.emit(HomeEvent::LinkRepository {
                                    key: footer_key.clone(),
                                });
                            }
                        })),
                ),
        )
    }

    fn todo_card(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let cursor = self.is_cursor_item(&item.id);
        let on_board = self.is_todos_page();
        let mut row = v_flex()
            .id(item_id("home-item", &item.id))
            .w_full()
            .min_w_0()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(cx.theme().background)
            .border_1()
            .border_color(if cursor {
                cx.theme().ring
            } else {
                cx.theme().border
            });
        if !item.completed {
            let drag = DragTodo {
                id: item.id.clone(),
                title: item.title.clone(),
            };
            row = row.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()));
            let target = item.id.clone();
            let target_project = item.project.clone();
            row = row.on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                let on_board = this.is_todos_page();
                this.change(window, cx, |data| {
                    // On the board a drop onto another column's card re-scopes
                    // the dragged todo before reordering; in the flat inbox a
                    // drop only reorders.
                    if on_board {
                        data.set_todo_project(&drag.id, Some(&target_project));
                    }
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
        } else if on_board {
            let project = item.project.trim();
            let pending: Vec<_> = self
                .data
                .items
                .iter()
                .filter(|i| {
                    i.kind == Kind::Todo
                        && !i.completed
                        && i.project.trim() == project
                        && self.todo_group_filter.is_none_or(|group| i.group == group)
                })
                .collect();
            let index = pending.iter().position(|i| i.id == item.id).unwrap_or(0);
            (
                index
                    .checked_sub(1)
                    .and_then(|i| pending.get(i))
                    .map(|i| i.id.clone()),
                pending.get(index + 1).map(|i| i.id.clone()),
            )
        } else {
            let pending: Vec<_> = self
                .visible_todos()
                .into_iter()
                .filter(|i| !i.completed)
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
        row = row.child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Tag::secondary()
                        .with_size(Size::Small)
                        .child(item.group.label()),
                )
                .when(!item.project.trim().is_empty(), |badges| {
                    badges.child(
                        Tag::secondary()
                            .with_size(Size::Small)
                            .child(self.todo_project_title(item.project.trim())),
                    )
                }),
        );
        if !item.description.is_empty() {
            row = row.child(description_with_links(item, "todo", cx));
        }
        row
    }

    fn reading_card(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let cursor = self.is_cursor_item(&item.id);
        let toggle_id = item.id.clone();
        let title = item.title.clone();
        let home = cx.entity().downgrade();
        let edit = item.clone();
        let delete_id = item.id.clone();
        let mut card = v_flex()
            .id(item_id("home-item", &item.id))
            .w_full()
            .min_w_0()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(cx.theme().background)
            .border_1()
            .border_color(if cursor {
                cx.theme().ring
            } else {
                cx.theme().border
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        Checkbox::new(item_id("complete", &item.id))
                            .checked(item.completed)
                            .accessibility_label(title.clone())
                            .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                this.change(window, cx, |data| {
                                    if let Some(i) =
                                        data.items.iter_mut().find(|i| i.id == toggle_id)
                                    {
                                        i.completed = *checked;
                                    }
                                    Ok(())
                                });
                            })),
                    )
                    .child(
                        title_link(item, "reading-title", &title, cx)
                            .child(div().w_full().truncate().child(title.clone())),
                    )
                    .child(
                        Button::new(item_id("reading-menu", &item.id))
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
                            }),
                    ),
            );
        if !item.description.is_empty() {
            card = card.child(description_with_links(item, "reading", cx));
        }
        card
    }

    fn list(&self, kind: Kind, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, destination) = match kind {
            Kind::Todo => ("Todos", "Todos"),
            Kind::PullRequest => ("Pull Requests", "Pull Requests"),
            Kind::Reading => ("To Read", "To Read"),
        };
        let is_pr = kind == Kind::PullRequest;
        let is_todo = kind == Kind::Todo;
        let items: Vec<_> = if is_todo {
            self.visible_todos()
        } else if kind == Kind::Reading {
            self.visible_reading()
        } else {
            self.data
                .items
                .iter()
                .filter(|i| {
                    i.kind == kind
                        && (self.show_completed || !i.completed)
                        && (!is_pr
                            || self
                                .pr_category_filter
                                .is_none_or(|category| i.category == category))
                        && (!is_pr || self.group_filter.is_none_or(|group| i.group == group))
                })
                .cloned()
                .collect()
        };
        let (icon, color, empty_title, empty_description, add_label) = match kind {
            Kind::PullRequest => (
                gpui_kit::component::IconName::Github,
                cx.theme().info,
                "A clear review queue",
                "Track a pull request to follow reviews and checks.",
                "Add pull request",
            ),
            Kind::Todo => (
                gpui_kit::component::IconName::CircleCheck,
                cx.theme().success,
                "Room for your next step",
                "Capture a task, big or small, and take it from here.",
                "Add todo",
            ),
            Kind::Reading => (
                gpui_kit::component::IconName::BookOpen,
                cx.theme().warning,
                "Keep something worth reading",
                "Save articles and references for a quieter moment.",
                "Add reading",
            ),
        };
        let mut panel = v_flex()
            .flex_1()
            .min_w(px(290.))
            .min_h(px(320.))
            .gap_3()
            .p_4()
            .rounded_xl()
            .border_1()
            .border_color(cx.theme().border.opacity(0.7))
            .bg(cx.theme().secondary.opacity(0.22))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_wrap()
                    .child(dashboard::panel_title(title, items.len(), icon, color, cx))
                    .child(self.view_all(destination, cx)),
            );
        if kind == Kind::Todo {
            panel = panel.child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Group"),
                            )
                            .child(self.todo_group_filter_dropdown(cx)),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Project"),
                            )
                            .child(self.todo_project_filter_dropdown(cx)),
                    ),
            );
        }
        if is_pr {
            panel = panel.child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Group"),
                            )
                            .child(self.group_filter_dropdown(cx)),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Category"),
                            )
                            .child(self.pr_category_filter_dropdown(cx)),
                    ),
            );
        }
        if items.is_empty() {
            panel = panel.child(
                v_flex()
                    .flex_1()
                    .justify_center()
                    .gap_2()
                    .py_6()
                    .child(div().text_sm().font_medium().child(empty_title))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(empty_description),
                    ),
            );
        }
        for category in Category::ALL {
            for item in items
                .iter()
                .filter(|i| kind != Kind::PullRequest || i.category == category)
            {
                if kind == Kind::PullRequest {
                    panel = panel.child(self.pr_card(item, false, cx));
                    continue;
                }
                if kind == Kind::Todo {
                    panel = panel.child(self.todo_card(item, cx));
                    continue;
                }
                panel = panel.child(self.reading_card(item, cx));
            }
            if kind != Kind::PullRequest {
                break;
            }
        }
        if is_pr {
            panel.child(
                h_flex()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border.opacity(0.5))
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .child(self.pr_refresh_button(cx))
                    .child(
                        Button::new(item_id("add", title))
                            .ghost()
                            .small()
                            .icon(gpui_kit::component::IconName::Plus)
                            .label(add_label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editor(Item::new(kind), window, cx)
                            })),
                    ),
            )
        } else if is_todo {
            let group = self.todo_group_filter.unwrap_or_default();
            let project = match &self.todo_project_filter {
                TodoProjectFilter::All | TodoProjectFilter::Unscoped => String::new(),
                TodoProjectFilter::Project(key) => key.clone(),
            };
            panel.child(
                h_flex()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border.opacity(0.5))
                    .child(
                        Button::new(item_id("add", title))
                            .ghost()
                            .small()
                            .icon(gpui_kit::component::IconName::Plus)
                            .label(add_label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let mut item = Item::new(kind);
                                item.group = group;
                                item.project = project.clone();
                                this.editor(item, window, cx)
                            })),
                    ),
            )
        } else {
            panel.child(
                h_flex()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border.opacity(0.5))
                    .child(
                        Button::new(item_id("add", title))
                            .ghost()
                            .small()
                            .icon(gpui_kit::component::IconName::Plus)
                            .label(add_label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editor(Item::new(kind), window, cx)
                            })),
                    ),
            )
        }
    }
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            if page == "Projects" {
                // Back navigation and the title live in the workspace
                // titlebar, matching the Artifacts page.
                body = body.child(self.projects_page(window, cx));
            } else if page == "Pull Requests" {
                body = body.child(self.pull_requests_page(cx));
            } else if page == "Todos" {
                body = body.child(self.todos_page(cx));
            } else if page == "To Read" {
                body = body.child(self.reading_page(window, cx));
            } else {
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
            }
        } else {
            body = body.child(self.dashboard(card_width, cx));
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

/// Use the same grid for projects and activity, regardless of how many projects
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

    fn group_entry(key: &str, group: Option<&str>) -> RepositoryEntry {
        RepositoryEntry {
            revision: String::new(),
            key: key.to_owned(),
            display_name: None,
            description: None,
            group: group.map(str::to_owned),
            owner: None,
            name: None,
            checkout_path: None,
            checkout_missing: false,
            last_opened_at: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn project_groups_lists_distinct_sorted_groups() {
        let entries = vec![
            group_entry("b", Some("work")),
            group_entry("a", Some("personal")),
            group_entry("c", Some("work")),
            group_entry("d", None),
        ];
        assert_eq!(
            project_groups(&entries),
            vec!["personal".to_owned(), "work".to_owned()]
        );
        assert!(project_groups(&[]).is_empty());
    }

    #[test]
    fn project_group_filter_matches_all_group_and_ungrouped() {
        let personal = group_entry("a", Some("personal"));
        let ungrouped = group_entry("b", None);
        assert!(ProjectGroupFilter::All.matches(&personal));
        assert!(ProjectGroupFilter::All.matches(&ungrouped));
        assert!(ProjectGroupFilter::Group("personal".to_owned()).matches(&personal));
        assert!(!ProjectGroupFilter::Group("personal".to_owned()).matches(&ungrouped));
        assert!(!ProjectGroupFilter::Group("work".to_owned()).matches(&personal));
        assert!(!ProjectGroupFilter::Ungrouped.matches(&personal));
        assert!(ProjectGroupFilter::Ungrouped.matches(&ungrouped));
    }

    fn todo_item(id: &str, project: &str) -> Item {
        let mut item = Item::new(Kind::Todo);
        item.id = id.to_owned();
        item.title = id.to_owned();
        item.project = project.to_owned();
        item
    }

    #[test]
    fn todo_project_filter_matches_all_unscoped_and_one_project() {
        assert!(TodoProjectFilter::All.matches(None));
        assert!(TodoProjectFilter::All.matches(Some("website")));
        assert!(TodoProjectFilter::Unscoped.matches(None));
        assert!(!TodoProjectFilter::Unscoped.matches(Some("website")));
        assert!(TodoProjectFilter::Project("website".to_owned()).matches(Some("website")));
        assert!(!TodoProjectFilter::Project("website".to_owned()).matches(None));
        assert!(!TodoProjectFilter::Project("website".to_owned()).matches(Some("other")));
    }

    #[test]
    fn todo_board_columns_start_unscoped_then_projects_then_orphans() {
        let projects = vec![group_entry("website", None), group_entry("api", None)];
        let todos = vec![
            todo_item("a", ""),
            todo_item("b", "website"),
            todo_item("c", "gone"),
        ];
        let columns = todo_board_columns(&projects, &todos);
        assert_eq!(
            columns
                .iter()
                .map(|column| column.key.clone())
                .collect::<Vec<_>>(),
            vec![
                None,
                Some("website".to_owned()),
                Some("api".to_owned()),
                Some("gone".to_owned()),
            ]
        );
        assert_eq!(columns[0].title, "Unscoped");
        assert_eq!(columns[3].title, "gone (removed)");
        assert!(todo_board_columns(&[], &[]).iter().any(|c| c.key.is_none()));
    }
}
