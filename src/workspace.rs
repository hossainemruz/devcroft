//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header and tab bar) hosting the active terminal pane.

#[path = "workspace_sessions.rs"]
mod sessions;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, ColorName, Icon, IconName, IndexPath, Root, Sizable, Size, StyledExt as _,
    WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    spinner::Spinner,
    tab::{Tab, TabBar},
    tag::Tag,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, Styled, Window, deferred, div,
    px, rgb,
};

use crate::add_repository::AddRepositoryView;
use crate::agent::AgentKind;
use crate::agent_activity::{ActivityState, AgentActivityStore};
use crate::agent_sessions::{
    Catalog, HOME_LIMIT, SIDEBAR_LIMIT, SessionKey, SessionSummary, Snapshot as SessionSnapshot,
};
use crate::command_palette::{
    GoToAgent, GoToEditor, GoToReview, GoToTasks, GoToTerminal, PaletteCommand, PaletteItem,
    PaletteMode, PaletteSection, ToggleActionsPalette, ToggleProjectsPalette, item_at,
    palette_sections_for_mode,
};
use crate::data::{
    DataRoot, DeviceStore, RecentRepository, SyncStatus, SyncTracker, checkout_for,
    recent_repositories, record_repository_open, resolve_current_key, resolve_workspace_agent,
    set_workspace_agent as persist_workspace_agent, sync_portable_with_tracker,
};
use crate::git_status::{GitStatus, load_git_status};
use crate::home::{HomeEvent, HomeView, project_state_tag};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, WORKSPACE_HEADER_HEIGHT};
use crate::pane::TerminalPane;
use crate::review::ReviewView;
use crate::settings::SettingsView;
use crate::workspace_settings::WorkspaceSettingsView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;

/// How often the header re-reads branch/dirty/ahead-behind state.
///
/// The poll runs off the main thread and only notifies on change, so the
/// cadence sets staleness, not frame cost. Two seconds keeps the dirty dot
/// feeling live after saves without churning full worktree walks on large
/// checkouts.
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often the automatic-sync scheduler re-reads the persisted interval.
///
/// The schedule itself (`device.json` `sync_interval_minutes`) is checked on
/// this cadence, so Settings edits re-arm within one tick and turning the
/// schedule off takes effect promptly. Ten seconds keeps a 2-minute minimum
/// interval reasonably tight while each tick costs one small JSON read.
const AUTO_SYNC_POLL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceTab {
    Agent,
    Editor,
    Terminal,
    Review,
    Tasks,
}

impl WorkspaceTab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Agent,
        Self::Editor,
        Self::Terminal,
        Self::Review,
        Self::Tasks,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent",
            Self::Editor => "Editor",
            Self::Terminal => "Terminal",
            Self::Review => "Review",
            Self::Tasks => "Tasks",
        }
    }

    pub(crate) fn command(self) -> Option<&'static str> {
        match self {
            // Single source of truth lives on the default harness, so the
            // label in Settings and the spawned command cannot drift.
            Self::Agent => Some(AgentKind::DEFAULT.command()),
            Self::Editor => Some("nvim ."),
            Self::Terminal | Self::Review | Self::Tasks => None,
        }
    }

    /// Review and Tasks render native content instead of hosting a shell.
    pub(crate) fn has_terminal(self) -> bool {
        !matches!(self, Self::Review | Self::Tasks)
    }
}

/// Where a home full-page (Tasks or Artifacts) should return to. Captured on
/// entry so the titlebar back button behaves like a browser back button
/// instead of always landing on Home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageOrigin {
    Home,
    Tasks,
    Artifacts,
    Repository,
}

/// Generation-guarded holder for the header's git status. Background loads
/// snapshot a load id up front and only commit while still current, so a
/// slow load for a previous checkout can never overwrite newer state —
/// e.g. the tick in flight when the user switches repositories. Same
/// pattern as `ReviewView`'s `generation`, factored out so the guard
/// itself is unit-testable without a window.
#[derive(Debug, Default)]
struct GitPoll {
    generation: u64,
    status: GitStatus,
}

impl GitPoll {
    fn status(&self) -> &GitStatus {
        &self.status
    }

    /// Claim the next load id and snapshot the directory it must read.
    /// Pure; the caller runs the load and offers the result back to
    /// [`commit`](Self::commit).
    fn begin_check(&mut self, workdir: PathBuf) -> (u64, PathBuf) {
        self.generation += 1;
        (self.generation, workdir)
    }

    /// Offer a loaded status: applies only when `generation` is still the
    /// latest check. Returns whether anything changed, so callers notify
    /// only then.
    fn commit(&mut self, generation: u64, status: GitStatus) -> bool {
        if self.generation != generation || self.status == status {
            return false;
        }
        self.status = status;
        true
    }

    /// Forget the current status (e.g. right after a switch, before the
    /// fresh load lands) without touching the generation: the pending
    /// fresh load still counts as current.
    fn reset(&mut self) {
        self.status = GitStatus::default();
    }
}

pub(crate) struct Workspace {
    tasks: Entity<crate::tasks::TaskBrowser>,
    home: Entity<HomeView>,
    home_visible: bool,
    portable_git_poll: GitPoll,
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    /// Harness the visible Agent pane was spawned with. Preserved across
    /// repository switches with its tabs; selecting a historical session
    /// changes this without touching the persisted default.
    session_agent: AgentKind,
    /// Persisted default harness for the current checkout: what *new*
    /// sessions launch. Updated by the workspace settings sheet; open
    /// sessions keep running with the harness they started with.
    default_agent: AgentKind,
    /// Strong entity handles keep hidden PTYs and their output tasks alive.
    inactive_repositories: HashMap<PathBuf, RepositoryTabs>,
    settings: Entity<SettingsView>,
    project_name: SharedString,
    git_poll: GitPoll,
    command_open: bool,
    /// Which filtered view the open bar shows. Set on every opening (and on
    /// mode switches while open); the render model follows it, so confirmations
    /// always resolve against the visible rows.
    palette_mode: PaletteMode,
    /// The attention indicator reuses Projects mode interaction while
    /// projecting only blocked agent rows and omitting Add repository.
    attention_only: bool,
    command_state: Entity<CommandState>,
    /// One provider-neutral projection for every managed Agent pane retained
    /// by this window, including panes in background repositories.
    agent_activity: AgentActivityStore,
    session_catalog: Catalog,
    session_snapshot: SessionSnapshot,
    session_projects: Vec<RecentRepository>,
    session_refreshing: bool,
    session_navigation: u64,
    open_sessions: HashMap<u64, OpenAgentSession>,
    /// Checkouts where the Agent pane has been started (fresh or resumed).
    /// Entering a checkout auto-resumes its most recent catalog session
    /// once; afterwards the user owns the pane (New session, history
    /// clicks, close), so later entries never retro-start.
    agent_autostart: HashSet<PathBuf>,
    data_root: Option<DataRoot>,
    sync_tracker: SyncTracker,
    /// When the last sync run started (manual or automatic). The automatic
    /// scheduler measures the interval from here, so manual runs reset the
    /// clock and a just-opened window waits a full interval first.
    last_sync_started: Instant,
    /// Checkout the panes and Review are rooted at. The git poll loop
    /// re-reads this every tick, so switching repositories re-roots status
    /// without restarting the loop.
    working_directory: PathBuf,
    /// Repository key matching [`working_directory`](Self::working_directory),
    /// when the checkout was opened through a linked repository.
    current_repository: Option<String>,
    /// Palette switcher cache, refreshed on every palette opening (and
    /// after each switch) so render never touches the filesystem.
    recent_repositories: Vec<RecentRepository>,
    /// Sections installed by the latest palette render; confirmations
    /// resolve against exactly this model.
    palette_model: Vec<PaletteSection>,
    /// Origin captured when entering the artifact browser, so the titlebar
    /// back button returns where the user came from. `Some` only while the
    /// artifact page is visible.
    artifacts_origin: Option<PageOrigin>,
    /// Origin captured when entering the global Tasks page. `Some` only
    /// while that page is visible.
    tasks_origin: Option<PageOrigin>,
}

struct OpenAgentSession {
    pane: Entity<TerminalPane>,
    checkout: PathBuf,
    agent: AgentKind,
    key: Option<SessionKey>,
    title: String,
}

struct RepositoryTabs {
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    /// Harness its Agent pane was spawned with, kept so a switch back
    /// restores the running session instead of respawning with a newer
    /// default.
    agent: AgentKind,
}

impl RepositoryTabs {
    fn new(
        working_directory: &Path,
        agent: AgentKind,
        activity: &AgentActivityStore,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                (tab.has_terminal() && tab != WorkspaceTab::Agent).then(|| {
                    cx.new(|cx| TerminalPane::new(tab, working_directory, agent, activity, cx))
                })
            })
            .collect();
        let review = cx.new(|cx| ReviewView::new(working_directory, cx));
        Self {
            active_tab: WorkspaceTab::Agent,
            tabs,
            review,
            agent,
        }
    }
}

/// Resolve aliases to the same checkout without making startup depend on I/O success.
fn checkout_identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

impl Workspace {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        working_directory: &Path,
    ) -> Self {
        let project_name = working_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned();

        let working_directory = &checkout_identity(working_directory);
        // Resolved before the first panes spawn so the Agent tab launches
        // the stored harness. Failures stay non-fatal (default harness) and
        // surface as a palette notification instead; this also keeps sync
        // working even if the startup path never persisted device state.
        let data_root = crate::data::ensure_ready(None).ok();
        let initial_agent = resolve_workspace_agent(data_root.as_ref(), working_directory);
        // Home must not launch an agent or editor in the startup directory.
        // Panes are created only when the user enters a repository workspace.
        let active_tab = WorkspaceTab::Agent;
        let tabs = vec![None; WorkspaceTab::ALL.len()];
        let review = cx.new(|cx| ReviewView::new(working_directory, cx));
        let session_agent = initial_agent;
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        let (agent_activity, activity_updates) = AgentActivityStore::new();
        cx.spawn(async move |this, cx| {
            while activity_updates.recv().await.is_ok() {
                while activity_updates.try_recv().is_ok() {}
                if this
                    .update(cx, |this, cx| {
                        this.publish_sessions(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            this.sync_activity_visibility(window, cx);
            cx.notify();
        })
        .detach();
        // Poll branch/dirty/ahead-behind off the main thread; the header
        // repaints only when the snapshot actually changes. The first
        // iteration loads immediately (so no worktree I/O blocks window
        // open), then repeats on the interval. The working directory is
        // re-read every tick so repository switches re-root the poll
        // without restarting the loop; each tick claims a load id so a
        // still-running load for a previous checkout can never overwrite
        // newer state (see `GitPoll`). The loop ends with the entity:
        // `update` fails once the workspace is dropped.
        cx.spawn(async move |this, cx| {
            loop {
                let next = this.update(cx, |this, _| {
                    let (generation, path) =
                        this.git_poll.begin_check(this.working_directory.clone());
                    Some((generation, path))
                });
                let Ok(Some((generation, path))) = next else {
                    break;
                };
                let status = cx
                    .background_spawn(async move { load_git_status(&path) })
                    .await;
                let dropped = this
                    .update(cx, |this, cx| {
                        if this.git_poll.commit(generation, status) {
                            cx.notify();
                        }
                    })
                    .is_err();
                if dropped {
                    break;
                }
                cx.background_executor().timer(GIT_POLL_INTERVAL).await;
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                let next = this.update(cx, |this, _| {
                    let root = this.data_root.as_ref()?;
                    Some(this.portable_git_poll.begin_check(root.portable_dir()))
                });
                let Ok(Some((generation, path))) = next else {
                    break;
                };
                let status = cx
                    .background_spawn(async move { load_git_status(&path) })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.portable_git_poll.commit(generation, status) {
                            if !this.home_visible && this.active_tab == WorkspaceTab::Tasks {
                                this.tasks.update(cx, |view, cx| view.refresh_all(cx));
                            }
                            this.home.update(cx, |view, cx| view.refresh_planning(cx));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(GIT_POLL_INTERVAL).await;
            }
        })
        .detach();
        // Automatic portable sync: re-read the persisted interval on a short
        // tick and run a silent sync once it elapses since the last run
        // started. Skips while a sync is already in flight (join, don't
        // queue) and re-arms within one tick when the interval changes or is
        // turned off. Silent on purpose: status is visible in Settings >
        // Sync, and only manual runs push notifications. The loop ends with
        // the entity: `update` fails once the workspace is dropped.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTO_SYNC_POLL).await;
                let due = this.update(cx, |this, _| {
                    let minutes = this.auto_sync_interval_minutes()?;
                    (minutes > 0
                        && this.last_sync_started.elapsed()
                            >= Duration::from_secs(minutes.saturating_mul(60)))
                    .then_some(())
                });
                match due {
                    Ok(Some(())) => {
                        let alive = this
                            .update(cx, |this, cx| {
                                if this.sync_tracker.status() == SyncStatus::Syncing {
                                    return;
                                }
                                if this.data_root.is_none() {
                                    return;
                                }
                                this.trigger_sync(cx);
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
        // Initial font size for Settings. The live global was already set
        // from the same store at startup (`main`), so this is just the
        // view's starting copy; later edits write back through the store.
        let initial_font_size = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok())
            .map(|state| state.app_font_size_or_default())
            .unwrap_or(DEFAULT_APP_FONT_SIZE);
        // One shared tracker: the palette guard, the scheduler skip, and the
        // Settings status line all read the same run state.
        let sync_tracker = SyncTracker::default();
        let home = cx.new(|cx| HomeView::new(data_root.clone(), sync_tracker.clone(), cx));
        home.read(cx).focus_handle.clone().focus(window, cx);
        cx.subscribe_in(&home, window, |this, _, event, window, cx| match event {
            HomeEvent::OpenRepository { key, label } => {
                this.switch_repository(key, label, window, cx)
            }
            HomeEvent::AddRepository => this.open_add_repository(window, cx),
            HomeEvent::OpenAgentSession(key) => this.open_agent_session(key.clone(), window, cx),
            HomeEvent::RefreshSessions => this.refresh_sessions(cx),
        })
        .detach();
        let settings = cx.new(|cx| {
            SettingsView::new(
                window,
                data_root.clone(),
                initial_font_size,
                sync_tracker.clone(),
                cx,
            )
        });
        // Best-effort current-repository match so the palette can mark it
        // without waiting for the first switch; a `--checkout` outside any
        // linked binding simply starts unmarked.
        let current_repository = data_root
            .as_ref()
            .and_then(|root| resolve_current_key(root, working_directory));

        let session_catalog = Catalog::new(data_root.as_ref());
        let initial_catalog = session_catalog.clone();
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                initial_catalog.load_cache();
            })
            .await;
            if this
                .update(cx, |this, cx| {
                    this.publish_sessions(cx);
                    this.refresh_sessions(cx);
                })
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.home_visible || this.active_tab == WorkspaceTab::Agent {
                            this.refresh_sessions(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Lightweight minute timer for relative-time labels. This only
        // re-projects the in-memory snapshot (no provider scans), so stale
        // `12m ago` rows refresh while visible without churning subprocesses.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(60))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.home_visible || this.active_tab == WorkspaceTab::Agent {
                            this.publish_sessions(cx);
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
            home,
            tasks: cx.new(|cx| {
                crate::tasks::TaskBrowser::new(
                    data_root.clone(),
                    crate::tasks::Scope::Repository(current_repository.clone()),
                    cx,
                )
            }),
            home_visible: true,
            portable_git_poll: GitPoll::default(),
            active_tab,
            tabs,
            review,
            session_agent,
            default_agent: initial_agent,
            inactive_repositories: HashMap::new(),
            settings,
            project_name: project_name.into(),
            git_poll: GitPoll::default(),
            command_open: false,
            palette_mode: PaletteMode::Actions,
            attention_only: false,
            command_state,
            agent_activity,
            session_catalog,
            session_snapshot: SessionSnapshot::default(),
            session_projects: Vec::new(),
            session_refreshing: false,
            session_navigation: 0,
            open_sessions: HashMap::new(),
            agent_autostart: HashSet::new(),
            data_root,
            sync_tracker,
            last_sync_started: Instant::now(),
            working_directory: working_directory.to_owned(),
            current_repository,
            recent_repositories: Vec::new(),
            palette_model: Vec::new(),
            artifacts_origin: None,
            tasks_origin: None,
        }
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = WorkspaceTab::ALL.get(index).copied() else {
            return;
        };
        self.active_tab = tab;
        self.enter_repository(window, cx);
        if tab == WorkspaceTab::Review {
            self.review.update(cx, |view, cx| view.activate(cx));
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    fn focus_active_pane(&mut self, window: &mut Window, cx: &mut App) {
        self.remember_agent_pane(cx);
        self.sync_activity_visibility(window, cx);
        if self.home_visible {
            self.home.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Review {
            self.review.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Tasks {
            self.tasks.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if let Some(Some(pane)) = self.tabs.get(self.active_tab as usize) {
            let focus_handle = pane.read(cx).focus_handle.clone();
            focus_handle.focus(window, cx);
        }
    }

    fn sync_activity_visibility(&self, window: &Window, cx: &App) {
        let visible = (!self.home_visible
            && self.active_tab == WorkspaceTab::Agent
            && window.is_window_active())
        .then(|| {
            self.tabs[WorkspaceTab::Agent as usize]
                .as_ref()
                .and_then(|p| p.read(cx).launch_id())
        })
        .flatten();
        self.agent_activity.set_visible_launch(visible);
    }

    /// Jump straight to the Agent tab (`cmd-a`). An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Agent as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Editor tab (`cmd-e`). An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Editor as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Terminal tab (`cmd-/`). An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Terminal as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Review tab (`cmd-r`). An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Review as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Tasks tab (`cmd-t`). An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Tasks as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    fn toggle_command_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attention_only = false;
        if self.command_open {
            // Same shortcut again closes; the other shortcut switches the
            // open bar to its view instead of closing it.
            if self.palette_mode == mode {
                self.close_command_palette(window, cx);
            } else {
                self.palette_mode = mode;
                self.reload_recent_repositories();
                self.command_state.update(cx, |state, cx| {
                    state.set_query("", window, cx);
                });
                let query_focus = self.command_state.read(cx).focus_handle(cx).clone();
                query_focus.focus(window, cx);
                cx.notify();
            }
        } else {
            self.open_command_palette(mode, window, cx);
        }
    }

    fn open_command_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_open {
            return;
        }
        self.command_open = true;
        self.attention_only = false;
        self.palette_mode = mode;
        // A few small JSON reads per opening — not per render — so the
        // switcher always reflects recent adds and switches.
        self.reload_recent_repositories();
        self.command_state.update(cx, |state, cx| {
            state.set_query("", window, cx);
        });
        // `read` borrows `cx`, so clone the handle first: focusing takes the
        // context mutably.
        let query_focus = self.command_state.read(cx).focus_handle(cx).clone();
        query_focus.focus(window, cx);
        cx.notify();
    }

    /// Refresh the palette switcher cache from disk. Opening, mode-switching,
    /// and repository switches all funnel here so render never touches the
    /// filesystem. Uncapped on purpose: the palette searches every switchable
    /// repository (recency-sorted), since Home doesn't exist yet to cover
    /// overflow.
    fn reload_recent_repositories(&mut self) {
        let mut repositories = self
            .data_root
            .as_ref()
            .map(|root| recent_repositories(root, usize::MAX))
            .unwrap_or_default();
        for repository in &mut repositories {
            repository.checkout_path = checkout_identity(&repository.checkout_path);
        }
        self.recent_repositories = repositories;
    }

    fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.command_open {
            return;
        }
        self.command_open = false;
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Open Settings as a modal dialog (fixed height, scrollable body — see
    /// the dialog component's scrollable pattern). The [`SettingsView`]
    /// entity outlives the dialog, so the selected section and pending edits
    /// survive close/reopen. Esc, backdrop click, and the close button all
    /// dismiss via the dialog layer; nothing here tracks open state.
    fn open_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        let workspace = cx.entity().downgrade();
        // Link the workspace for Sync-now delegation and reload the persisted
        // interval plus saved origin, so the Sync section never shows state
        // gone stale behind the dialog (e.g. after external git edits).
        settings.update(cx, |view, cx| {
            view.set_workspace(workspace);
            view.refresh_sync_from_disk(window, cx);
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Settings")
                .w(px(920.))
                .h(px(600.))
                .child(settings.clone().into_any_element())
        });
    }

    /// Run the confirmed palette entry. `index` addresses the model installed
    /// by the latest `Command` render (before filtering), so it resolves
    /// against [`palette_model`](Self::palette_model) however the query
    /// narrowed the list.
    fn on_palette_confirm(
        &mut self,
        index: IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = item_at(&self.palette_model, index.section, index.row) else {
            self.close_command_palette(window, cx);
            return;
        };
        // Every path below leaves the bar closed; `select_tab` focuses and
        // notifies itself, the other arms do it explicitly.
        self.command_open = false;
        let focus_attention_target = self.attention_only;
        match item {
            PaletteItem::SwitchRepository { key, label } => {
                self.switch_repository(&key, &label, window, cx);
                if focus_attention_target && !self.home_visible {
                    self.focus_attention_session(window, cx);
                    self.select_tab(WorkspaceTab::Agent as usize, window, cx);
                }
            }
            PaletteItem::OpenCheckout { checkout_path, .. } => {
                self.open_managed_checkout(&checkout_path, window, cx);
                if focus_attention_target && !self.home_visible {
                    self.focus_attention_session(window, cx);
                    self.select_tab(WorkspaceTab::Agent as usize, window, cx);
                }
            }
            PaletteItem::Command(command) => match command {
                PaletteCommand::GoAgent => self.select_tab(0, window, cx),
                PaletteCommand::GoEditor => self.select_tab(1, window, cx),
                PaletteCommand::GoTerminal => self.select_tab(2, window, cx),
                PaletteCommand::GoReview => self.select_tab(3, window, cx),
                PaletteCommand::GoTasks => self.select_tab(4, window, cx),
                PaletteCommand::OpenSettings => self.open_settings(window, cx),
                PaletteCommand::AddRepository => self.open_add_repository(window, cx),
                PaletteCommand::GoHome => self.go_home(window, cx),
                PaletteCommand::BrowseArtifacts => self.browse_artifacts(window, cx),
                PaletteCommand::ViewTasks => self.view_tasks(window, cx),
                PaletteCommand::SyncPortable => {
                    self.request_sync(window, cx);
                    self.focus_active_pane(window, cx);
                    cx.notify();
                }
            },
        }
    }

    /// Open the Add Repository dialog with a fresh form: re-harvesting on
    /// every opening beats reasoning about stale prefill. The dialog owns
    /// open/close; the view is dropped with it.
    fn open_add_repository(&self, window: &mut Window, cx: &mut Context<Self>) {
        let data_root = self.data_root.clone();
        let view = cx.new(|cx| AddRepositoryView::new(window, cx, data_root));
        cx.subscribe(
            &view,
            |this, _, _: &crate::add_repository::RepositoryAdded, cx| {
                this.home.update(cx, |view, cx| view.reload(cx));
                this.reload_recent_repositories();
            },
        )
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Add repository")
                .w(px(680.))
                .h(px(640.))
                .child(view.clone().into_any_element())
        });
    }

    /// Switch visible checkout after recording recency. Hidden checkout entities
    /// stay owned by the workspace, so their processes and view state survive
    /// until the window closes. Only a first visit creates new panes.
    fn switch_repository(
        &mut self,
        key: &str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve and record even on re-entry from Home. Restoring the same
        // checkout is a no-op, so its running PTYs remain untouched.
        let Some(root) = self.data_root.clone() else {
            window.push_notification("Portable data is unavailable", cx);
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        };
        // Resolve fresh, not from the cached palette list, so a record
        // added or linked since the palette opened still switches.
        let Some(checkout) = checkout_for(&root, key) else {
            window.push_notification(format!("No linked checkout for \"{label}\""), cx);
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        };
        if !checkout.is_dir() {
            window.push_notification(
                format!(
                    "The linked checkout for \"{label}\" is unavailable at {}",
                    checkout.display()
                ),
                cx,
            );
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        if let Err(error) = record_repository_open(&root, key) {
            window.push_notification(
                format!("Could not record recency for \"{label}\": {error:#}"),
                cx,
            );
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        let checkout = checkout_identity(&checkout);
        self.restore_repository_tabs(&checkout, cx);
        self.enter_repository(window, cx);
        self.project_name = checkout
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned()
            .into();
        self.current_repository = Some(key.to_owned());
        self.tasks.update(cx, |view, cx| {
            view.set_scope(
                crate::tasks::Scope::Repository(self.current_repository.clone()),
                cx,
            )
        });
        // Reset plus an immediate fresh load: the header shows the new
        // checkout's state within one scan instead of flashing the old
        // checkout's status until the next poll tick — and the load id
        // invalidates the tick that was in flight for the old checkout.
        self.refresh_git_status(cx);
        self.reload_recent_repositories();
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Activate a retained checkout that has no portable repository entry.
    /// These rows are synthesized only for live managed agent panes, so this
    /// path never creates a new arbitrary checkout from palette input.
    fn open_managed_checkout(
        &mut self,
        checkout: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checkout = checkout_identity(checkout);
        if checkout != self.working_directory && !self.inactive_repositories.contains_key(&checkout)
        {
            window.push_notification("That agent checkout is no longer open", cx);
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        self.restore_repository_tabs(&checkout, cx);
        self.enter_repository(window, cx);
        self.project_name = checkout
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned()
            .into();
        self.current_repository = self
            .data_root
            .as_ref()
            .and_then(|root| resolve_current_key(root, &checkout));
        self.tasks.update(cx, |view, cx| {
            view.set_scope(
                crate::tasks::Scope::Repository(self.current_repository.clone()),
                cx,
            )
        });
        self.refresh_git_status(cx);
        self.reload_recent_repositories();
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    fn restore_repository_tabs(&mut self, checkout: &Path, cx: &mut Context<Self>) {
        if self.working_directory == checkout {
            return;
        }
        // The persisted default may have changed while this checkout was
        // hidden (e.g. another window saved it), so re-resolve: fresh tabs
        // spawn with it, restored tabs keep their running session.
        let default_agent = resolve_workspace_agent(self.data_root.as_ref(), checkout);
        let next = self
            .inactive_repositories
            .remove(checkout)
            .unwrap_or_else(|| {
                RepositoryTabs::new(checkout, default_agent, &self.agent_activity, cx)
            });
        self.default_agent = default_agent;
        let previous = RepositoryTabs {
            active_tab: std::mem::replace(&mut self.active_tab, next.active_tab),
            tabs: std::mem::replace(&mut self.tabs, next.tabs),
            review: std::mem::replace(&mut self.review, next.review),
            // Swap the session agents alongside their tab sets: the active
            // panes keep the harness they were spawned with, and the hidden
            // set keeps its own.
            agent: std::mem::replace(&mut self.session_agent, next.agent),
        };
        let previous_directory =
            std::mem::replace(&mut self.working_directory, checkout.to_owned());
        self.inactive_repositories
            .insert(previous_directory, previous);
    }

    fn enter_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.artifacts_origin = None;
        self.tasks_origin = None;
        self.tasks.update(cx, |view, cx| {
            view.set_active(self.active_tab == WorkspaceTab::Tasks, cx)
        });
        self.home_visible = false;
        self.home.update(cx, |view, cx| view.deactivate(cx));
        for tab in WorkspaceTab::ALL {
            if tab.has_terminal() && self.tabs[tab as usize].is_none() {
                if tab == WorkspaceTab::Agent
                    && self.agent_autostart.insert(self.working_directory.clone())
                    && let Some(last) = self
                        .session_snapshot
                        .recent(&self.working_directory, 1)
                        .into_iter()
                        .next()
                {
                    // First entry starts the last session, not a fresh
                    // shell. The shared open path owns async validation
                    // and pane creation; a failed resume keeps the
                    // placeholder with Retry guidance instead of silently
                    // opening a new conversation.
                    self.open_agent_session(last.key, window, cx);
                    continue;
                }
                if tab == WorkspaceTab::Agent {
                    self.session_agent = self.default_agent;
                }
                self.tabs[tab as usize] = Some(cx.new(|cx| {
                    TerminalPane::new(
                        tab,
                        &self.working_directory,
                        self.default_agent,
                        &self.agent_activity,
                        cx,
                    )
                }));
            }
        }
    }

    fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.artifacts_origin = None;
        self.tasks_origin = None;
        self.tasks.update(cx, |view, cx| view.set_active(false, cx));
        self.home_visible = true;
        self.command_open = false;
        self.home.update(cx, |view, cx| view.activate(cx));
        self.refresh_sessions(cx);
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Jump to Home's global task list (Recent Tasks' **View all →**
    /// destination). The repository Tasks tab keeps its own scope; this is
    /// the cross-repository list. Deactivates the workspace task view so its
    /// poll stops while Home is visible. Captures the origin so the titlebar
    /// back button returns where the user came from.
    fn view_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let already_there = self.home_visible && self.home.read(cx).is_tasks_page();
        if !already_there {
            let origin = if !self.home_visible {
                PageOrigin::Repository
            } else if self.home.read(cx).is_artifacts_page() {
                PageOrigin::Artifacts
            } else {
                PageOrigin::Home
            };
            self.tasks_origin = Some(origin);
        }
        self.artifacts_origin = None;
        self.tasks.update(cx, |view, cx| view.set_active(false, cx));
        self.home_visible = true;
        self.command_open = false;
        self.home.update(cx, |view, cx| view.show_tasks_page(cx));
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Jump to Home's artifact browser (the command palette's
    /// **Browse artifacts** entry). Standalone RFCs/plans/notes need no task or repository
    /// association. Deactivates the workspace task view so its poll stops
    /// while Home is visible. Captures the origin so the titlebar back
    /// button returns where the user came from.
    fn browse_artifacts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let already_there = self.home_visible && self.home.read(cx).is_artifacts_page();
        if !already_there {
            let origin = if !self.home_visible {
                PageOrigin::Repository
            } else if self.home.read(cx).is_tasks_page() {
                PageOrigin::Tasks
            } else {
                PageOrigin::Home
            };
            self.artifacts_origin = Some(origin);
        }
        self.tasks_origin = None;
        self.tasks.update(cx, |view, cx| view.set_active(false, cx));
        self.home_visible = true;
        self.command_open = false;
        self.home
            .update(cx, |view, cx| view.show_artifacts_page(cx));
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Titlebar back for the artifact browser. Mirrors browser history:
    /// drill out of an open artifact first, otherwise return to the captured
    /// origin (repository workspace, global tasks, or Home dashboard).
    fn go_back_from_artifacts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.home.read(cx).artifacts_has_selection(cx) {
            self.home
                .update(cx, |view, cx| view.clear_artifact_selection(cx));
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        match self.artifacts_origin.take() {
            Some(PageOrigin::Repository) => {
                self.enter_repository(window, cx);
                self.focus_active_pane(window, cx);
                cx.notify();
            }
            Some(PageOrigin::Tasks) => {
                // Direct switch without capturing a new origin: this is a
                // back navigation, not a forward entry. Clear both so the
                // next back falls through to Home.
                self.artifacts_origin = None;
                self.tasks_origin = None;
                self.tasks.update(cx, |view, cx| view.set_active(false, cx));
                self.home_visible = true;
                self.command_open = false;
                self.home.update(cx, |view, cx| view.show_tasks_page(cx));
                self.focus_active_pane(window, cx);
                cx.notify();
            }
            Some(PageOrigin::Home) | Some(PageOrigin::Artifacts) | None => {
                self.go_home(window, cx);
            }
        }
    }

    fn artifacts_back_label(&self) -> SharedString {
        match self.artifacts_origin {
            Some(PageOrigin::Repository) => format!("‹ {}", self.project_name).into(),
            Some(PageOrigin::Tasks) => "‹ Tasks".into(),
            Some(PageOrigin::Home) | Some(PageOrigin::Artifacts) | None => "‹ Home".into(),
        }
    }

    /// Titlebar back for the global Tasks page. Drills out stepwise first
    /// (linked artifact → task detail → task list), otherwise returns to the
    /// captured origin (repository workspace, artifacts, or Home).
    fn go_back_from_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.home.read(cx).tasks_is_artifact_open(cx) {
            self.home
                .update(cx, |view, cx| view.close_task_artifact(cx));
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        if self.home.read(cx).tasks_has_selection(cx) {
            self.home
                .update(cx, |view, cx| view.clear_task_selection(cx));
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
        match self.tasks_origin.take() {
            Some(PageOrigin::Repository) => {
                self.enter_repository(window, cx);
                self.focus_active_pane(window, cx);
                cx.notify();
            }
            Some(PageOrigin::Artifacts) => {
                // Direct switch without capturing: back navigation clears
                // both origins so the next back falls through to Home.
                self.artifacts_origin = None;
                self.tasks_origin = None;
                self.tasks.update(cx, |view, cx| view.set_active(false, cx));
                self.home_visible = true;
                self.command_open = false;
                self.home
                    .update(cx, |view, cx| view.show_artifacts_page(cx));
                self.focus_active_pane(window, cx);
                cx.notify();
            }
            Some(PageOrigin::Home) | Some(PageOrigin::Tasks) | None => {
                self.go_home(window, cx);
            }
        }
    }

    fn tasks_back_label(&self) -> SharedString {
        match self.tasks_origin {
            Some(PageOrigin::Repository) => format!("‹ {}", self.project_name).into(),
            Some(PageOrigin::Artifacts) => "‹ Artifacts".into(),
            Some(PageOrigin::Home) | Some(PageOrigin::Tasks) | None => "‹ Home".into(),
        }
    }

    /// Persist the workspace default harness from the settings sheet and
    /// restart the Agent tab with it: the old PTY session is dropped with
    /// its pane entity, and a fresh login shell launches the new harness.
    /// Other tabs and hidden repositories are untouched.
    pub(crate) fn set_default_agent(
        &mut self,
        agent: AgentKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.default_agent == agent {
            return;
        }
        if let Err(error) = persist_workspace_agent(
            self.data_root.as_ref(),
            &self.working_directory.clone(),
            agent,
        ) {
            window.push_notification(format!("Could not save the default agent: {error:#}"), cx);
            return;
        }
        self.default_agent = agent;
        window.push_notification(format!("New sessions will use {}", agent.label()), cx);
        cx.notify();
    }

    pub(crate) fn default_agent(&self) -> AgentKind {
        self.default_agent
    }

    /// Open the per-workspace settings sheet with a fresh view: the sheet
    /// always reflects the current checkout and its live selection, so
    /// re-reading on every opening beats reasoning about staleness across
    /// repository switches. The sheet layer is owned by [`Root`]; like the
    /// Add Repository dialog, the view is dropped with it.
    fn open_workspace_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| {
            WorkspaceSettingsView::new(
                self.working_directory.clone(),
                self.project_name.to_string(),
                self.default_agent,
                workspace,
                cx,
            )
        });
        window.open_sheet(cx, move |sheet, _, _| {
            sheet
                .title("Workspace settings")
                .child(view.clone().into_any_element())
        });
    }

    /// Re-check git status right now instead of waiting for the next poll
    /// tick. Claims a fresh load id (invalidating any older in-flight
    /// load), clears the header to loading state, and applies the result
    /// only while still current.
    fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        let (generation, path) = self.git_poll.begin_check(self.working_directory.clone());
        self.git_poll.reset();
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_spawn(async move { load_git_status(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.git_poll.commit(generation, status) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Manual sync entry point shared by the command palette and the
    /// Settings Sync section. Guards an in-flight run and surfaces
    /// notifications; the silent scheduler goes through [`trigger_sync`](Self::trigger_sync)
    /// directly.
    pub(crate) fn request_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sync_tracker.status() == SyncStatus::Syncing {
            window.push_notification("Portable sync is already running", cx);
        } else if self.data_root.is_none() {
            window.push_notification("Portable data is unavailable", cx);
        } else {
            window.push_notification("Syncing portable data…", cx);
            self.trigger_sync(cx);
        }
        cx.notify();
    }

    /// Persisted automatic-sync interval, re-read every scheduler tick so
    /// Settings edits re-arm the schedule within one poll. `None` (or zero)
    /// means Off.
    fn auto_sync_interval_minutes(&self) -> Option<u64> {
        self.data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok())
            .and_then(|state| state.sync_interval_minutes)
            .filter(|minutes| *minutes > 0)
    }

    /// Reload portable-backed projections (Review surfaces) after the files
    /// underneath them may have moved — a sync rebase or a Settings branch
    /// switch. Shared so both paths reload exactly the same set.
    pub(crate) fn reload_portable_projections(&mut self, cx: &mut Context<Self>) {
        if !self.home_visible && self.active_tab == WorkspaceTab::Tasks {
            self.tasks.update(cx, |view, cx| view.refresh_all(cx));
        }
        self.home.update(cx, |view, cx| view.reload(cx));
        self.reload_recent_repositories();
        self.review.update(cx, |view, cx| view.reload(cx));
        for repository in self.inactive_repositories.values() {
            repository.review.update(cx, |view, cx| view.reload(cx));
        }
        cx.notify();
    }

    /// Run one portable sync off the main thread. Completion reloads the
    /// Review projection when the rebase may have moved files (including
    /// after an error, which can still leave working-tree changes behind)
    /// and repaints the status bar via the shared [`SyncTracker`].
    fn trigger_sync(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.data_root.clone() else {
            return;
        };
        // Manual runs reset the automatic schedule clock.
        self.last_sync_started = Instant::now();
        let tracker = self.sync_tracker.clone();
        let settings = self.settings.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { sync_portable_with_tracker(&root, &tracker) })
                .await;
            let reload = match &outcome {
                Ok(outcome) => outcome.reload_required,
                Err(_) => true,
            };
            let _ = this.update(cx, |this, cx| {
                if reload {
                    this.reload_portable_projections(cx);
                }
                // Repaint the Settings dialog if open so its status line
                // follows the run without polling.
                settings.update(cx, |_, cx| cx.notify());
                cx.notify();
            });
        })
        .detach();
    }

    fn render_command_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let confirm_workspace = workspace.clone();
        let attention_only = self.attention_only;
        let mut command = Command::new(&self.command_state)
            .placeholder(self.palette_mode.placeholder())
            .on_confirm(move |index, window, cx| {
                confirm_workspace
                    .update(cx, |this, cx| this.on_palette_confirm(index, window, cx))
                    .ok();
            })
            .on_cancel(move |window, cx| {
                workspace
                    .update(cx, |this, cx| this.close_command_palette(window, cx))
                    .ok();
            })
            .footer(|_, _, cx| {
                h_flex()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("↑↓ Navigate")
                    .child("Enter Select")
                    .child("Esc Close")
            })
            .empty(move |_, _, cx| {
                div()
                    .px_3()
                    .py_6()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if attention_only {
                        "No agents need attention"
                    } else {
                        "No matching commands"
                    })
            })
            .w_full();
        // Install exactly the model confirmations resolve against, so render
        // and confirm can never disagree about what a row means. Labels,
        // keywords, and icons all come from the model item. The model follows
        // the open mode, so each shortcut sees only its own rows.
        let activity = self.agent_activity.snapshot();
        let visible_repositories = if attention_only {
            self.recent_repositories
                .iter()
                .filter(|repository| {
                    activity
                        .for_checkout(&repository.checkout_path)
                        .is_some_and(|agent| agent.state == ActivityState::NeedsAttention)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            self.recent_repositories.clone()
        };
        let mut open_checkouts = activity
            .iter()
            .filter(|(checkout, agent)| {
                agent.state != ActivityState::NoAgent
                    && (!attention_only || agent.state == ActivityState::NeedsAttention)
                    && !self
                        .recent_repositories
                        .iter()
                        .any(|repository| repository.checkout_path.as_path() == *checkout)
            })
            .map(|(checkout, _)| PaletteItem::open_checkout(checkout))
            .collect::<Vec<_>>();
        open_checkouts.sort_by(|left, right| left.label().cmp(right.label()));
        self.palette_model = if attention_only {
            vec![PaletteSection {
                heading: "Needs attention",
                items: visible_repositories
                    .iter()
                    .map(PaletteItem::switch_target)
                    .chain(open_checkouts)
                    .collect(),
            }]
        } else {
            let mut sections = palette_sections_for_mode(
                &visible_repositories,
                self.palette_mode,
                self.home_visible,
            );
            if self.palette_mode == PaletteMode::Projects && !open_checkouts.is_empty() {
                sections.push(PaletteSection {
                    heading: "Open checkouts",
                    items: open_checkouts,
                });
            }
            sections
        };
        let current = self.current_repository.clone();
        let current_checkout = self.working_directory.clone();
        for section in &self.palette_model {
            command = command.group(
                CommandGroup::new().label(section.heading).items(
                    section
                        .items
                        .iter()
                        .map(|item| {
                            let mut keywords = item
                                .keywords()
                                .into_iter()
                                .map(str::to_owned)
                                .collect::<Vec<_>>();
                            let rendered = match item {
                                // Tabs with direct keybindings carry the real
                                // action: the row renders the binding hint for
                                // free, and confirming still runs
                                // `on_palette_confirm` afterwards (idempotent
                                // re-select of the same tab).
                                PaletteItem::Command(PaletteCommand::GoAgent) => CommandItem::new()
                                    .label(item.label())
                                    .icon(palette_icon(PaletteCommand::GoAgent))
                                    .action(Box::new(GoToAgent)),
                                PaletteItem::Command(PaletteCommand::GoEditor) => {
                                    CommandItem::new()
                                        .label(item.label())
                                        .icon(palette_icon(PaletteCommand::GoEditor))
                                        .action(Box::new(GoToEditor))
                                }
                                PaletteItem::Command(PaletteCommand::GoTerminal) => {
                                    CommandItem::new()
                                        .label(item.label())
                                        .icon(palette_icon(PaletteCommand::GoTerminal))
                                        .action(Box::new(GoToTerminal))
                                }
                                PaletteItem::Command(PaletteCommand::GoReview) => {
                                    CommandItem::new()
                                        .label(item.label())
                                        .icon(palette_icon(PaletteCommand::GoReview))
                                        .action(Box::new(GoToReview))
                                }
                                PaletteItem::Command(PaletteCommand::GoTasks) => CommandItem::new()
                                    .label(item.label())
                                    .icon(palette_icon(PaletteCommand::GoTasks))
                                    .action(Box::new(GoToTasks)),
                                PaletteItem::Command(command) => CommandItem::new()
                                    .label(item.label())
                                    .icon(palette_icon(*command)),
                                PaletteItem::SwitchRepository { key, .. } => {
                                    let label = item.label().to_owned();
                                    let repository = self
                                        .recent_repositories
                                        .iter()
                                        .find(|repository| repository.key == *key);
                                    let agent = repository
                                        .and_then(|repository| {
                                            activity.for_checkout(&repository.checkout_path)
                                        })
                                        .cloned();
                                    let working = agent
                                        .as_ref()
                                        .is_some_and(|agent| agent.state == ActivityState::Working);
                                    let (provider, state, color) = match agent {
                                        Some(agent) => {
                                            keywords.push(agent.agent.label().to_owned());
                                            keywords.push(agent.state.label().to_owned());
                                            (
                                                agent.agent.label().to_owned(),
                                                agent.state.label().to_owned(),
                                                activity_state_color(agent.state),
                                            )
                                        }
                                        None => (
                                            "Agent".to_owned(),
                                            ActivityState::NoAgent.label().to_owned(),
                                            activity_state_color(ActivityState::NoAgent),
                                        ),
                                    };
                                    let status = format!("{provider} · {state}");
                                    let is_current = current.as_deref() == Some(key.as_str());
                                    CommandItem::new().label(label.clone()).child(move |_, _| {
                                        h_flex()
                                            .flex_1()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Icon::new(IconName::Folder)
                                                    .size(px(16.))
                                                    .text_color(rgb(0x858989)),
                                            )
                                            .child(label.clone())
                                            .when(is_current, |row| {
                                                row.child(
                                                    div()
                                                        .size(px(6.))
                                                        .flex_shrink_0()
                                                        .rounded_full()
                                                        .bg(rgb(0x4ade80)),
                                                )
                                            })
                                            .child(
                                                h_flex()
                                                    .ml_auto()
                                                    .gap_1()
                                                    .text_xs()
                                                    .text_color(rgb(color))
                                                    .when(working, |this| {
                                                        this.child(
                                                            Spinner::new()
                                                                .with_size(px(12.))
                                                                .color(rgb(color).into()),
                                                        )
                                                    })
                                                    .child(status.clone()),
                                            )
                                    })
                                }
                                PaletteItem::OpenCheckout { checkout_path, .. } => {
                                    let label = item.label().to_owned();
                                    let agent = activity.for_checkout(checkout_path).cloned();
                                    let working = agent
                                        .as_ref()
                                        .is_some_and(|agent| agent.state == ActivityState::Working);
                                    let (provider, state, color) = match agent {
                                        Some(agent) => {
                                            keywords.push(agent.agent.label().to_owned());
                                            keywords.push(agent.state.label().to_owned());
                                            (
                                                agent.agent.label().to_owned(),
                                                agent.state.label().to_owned(),
                                                activity_state_color(agent.state),
                                            )
                                        }
                                        None => (
                                            "Agent".to_owned(),
                                            ActivityState::NoAgent.label().to_owned(),
                                            activity_state_color(ActivityState::NoAgent),
                                        ),
                                    };
                                    let status = format!("{provider} · {state}");
                                    let is_current =
                                        current_checkout.as_path() == checkout_path.as_path();
                                    CommandItem::new().label(label.clone()).child(move |_, _| {
                                        h_flex()
                                            .flex_1()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Icon::new(IconName::Folder)
                                                    .size(px(16.))
                                                    .text_color(rgb(0x858989)),
                                            )
                                            .child(label.clone())
                                            .when(is_current, |row| {
                                                row.child(
                                                    div()
                                                        .size(px(6.))
                                                        .flex_shrink_0()
                                                        .rounded_full()
                                                        .bg(rgb(0x4ade80)),
                                                )
                                            })
                                            .child(
                                                h_flex()
                                                    .ml_auto()
                                                    .gap_1()
                                                    .text_xs()
                                                    .text_color(rgb(color))
                                                    .when(working, |this| {
                                                        this.child(
                                                            Spinner::new()
                                                                .with_size(px(12.))
                                                                .color(rgb(color).into()),
                                                        )
                                                    })
                                                    .child(status.clone()),
                                            )
                                    })
                                }
                            };
                            rendered.keywords(keywords)
                        })
                        .collect::<Vec<_>>(),
                ),
            );
        }
        command
    }

    fn render_active_content(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.home_visible {
            return self.home.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Tasks {
            return self.tasks.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Review {
            return self.review.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Agent {
            return self.render_agent_sessions(cx);
        }
        match self
            .tabs
            .get(self.active_tab as usize)
            .and_then(|tab| tab.clone())
        {
            Some(pane) => pane.into_any_element(),
            None => div().size_full().into_any_element(),
        }
    }

    /// Branch pill for the header: `⎇ <branch>` on a branch, `➦ <short-sha>`
    /// on a detached HEAD. Long branch names truncate instead of pushing the
    /// command bar aside.
    fn render_branch_pill(&self, branch: SharedString) -> impl IntoElement {
        let glyph = if self.git_poll.status().detached {
            "➦"
        } else {
            "⎇"
        };
        h_flex()
            .gap_1()
            .items_center()
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x292b2b))
            .bg(rgb(0x0e0f0f))
            .text_xs()
            .child(div().text_color(rgb(0x737878)).child(glyph))
            .child(
                div()
                    .max_w(px(160.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(0x858989))
                    .child(branch),
            )
    }
}

/// Leading glyph per palette command. A command with a direct keybinding
/// (like GoAgent's `cmd-a`) additionally carries its real GPUI
/// `Action` on the row via `CommandItem::action`, which renders the binding
/// hint for free; `on_confirm` still resolves it afterwards, idempotently.
fn palette_icon(command: PaletteCommand) -> IconName {
    match command {
        PaletteCommand::GoAgent => IconName::Bot,
        PaletteCommand::GoEditor => IconName::FileText,
        PaletteCommand::GoTerminal => IconName::SquareTerminal,
        PaletteCommand::GoReview => IconName::Eye,
        PaletteCommand::GoTasks => IconName::CircleCheck,
        PaletteCommand::GoHome => IconName::LayoutDashboard,
        PaletteCommand::BrowseArtifacts => IconName::BookOpen,
        PaletteCommand::ViewTasks => IconName::CircleCheck,
        PaletteCommand::AddRepository => IconName::Plus,
        PaletteCommand::OpenSettings => IconName::Settings,
        PaletteCommand::SyncPortable => IconName::RotateCw,
    }
}

fn activity_state_color(state: ActivityState) -> u32 {
    match state {
        ActivityState::Starting | ActivityState::Idle | ActivityState::NoAgent => 0x858989,
        ActivityState::Working => 0x60a5fa,
        ActivityState::NeedsAttention => 0xf59e0b,
        ActivityState::Finished => 0x4ade80,
        ActivityState::Unavailable => 0x9ca3af,
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_index = self.active_tab as usize;
        let active_content = self.render_active_content(cx);
        // Tasks and Artifacts full-pages share the repository workspace
        // chrome: title and back navigation live here, not in a second row.
        // The archived filter also lives here; the portable git status is
        // hidden on these pages and the lists stay live through their active
        // polls.
        let is_artifacts = self.home_visible && self.home.read(cx).is_artifacts_page();
        let is_tasks = self.home_visible && self.home.read(cx).is_tasks_page();
        let show_archived = if is_artifacts {
            self.home.read(cx).artifacts_include_archived(cx)
        } else {
            false
        };
        let show_tasks_archived = if is_tasks {
            self.home.read(cx).tasks_include_archived(cx)
        } else {
            false
        };
        let artifacts_back = if is_artifacts {
            self.artifacts_back_label()
        } else {
            SharedString::from("")
        };
        let tasks_back = if is_tasks {
            self.tasks_back_label()
        } else {
            SharedString::from("")
        };
        let is_home_page = is_artifacts || is_tasks;
        let attention_count = self.agent_activity.snapshot().attention_count();
        let attention_label = if attention_count == 1 {
            "⚠ 1 agent needs attention".to_owned()
        } else {
            format!("⚠ {attention_count} agents need attention")
        };

        v_flex()
            .relative()
            .when(
                self.home_visible || self.active_tab == WorkspaceTab::Tasks,
                |this| this.tab_group(),
            )
            .size_full()
            .bg(rgb(0x080909))
            .text_color(rgb(0xe7e7e7))
            .on_action(cx.listener(|this, _: &ToggleActionsPalette, window, cx| {
                this.toggle_command_palette(PaletteMode::Actions, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleProjectsPalette, window, cx| {
                this.toggle_command_palette(PaletteMode::Projects, window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToAgent, window, cx| {
                this.go_to_agent(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToEditor, window, cx| {
                this.go_to_editor(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToTerminal, window, cx| {
                this.go_to_terminal(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToReview, window, cx| {
                this.go_to_review(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToTasks, window, cx| {
                this.go_to_tasks(window, cx);
            }))
            .child(
                h_flex()
                    .h(px(58.))
                    .px_4()
                    .gap_3()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0x292b2b))
                    .when(self.home_visible && !is_home_page, |header| {
                        header.child(div().text_lg().font_semibold().child("Devcroft"))
                    })
                    .when(is_artifacts, |header| {
                        header.child(
                            h_flex()
                                .flex_none()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("artifacts-back")
                                        .ghost()
                                        .label(artifacts_back)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.go_back_from_artifacts(window, cx)
                                        })),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child(SharedString::from("Artifacts")),
                                ),
                        )
                    })
                    .when(is_tasks, |header| {
                        header.child(
                            h_flex()
                                .flex_none()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("tasks-back")
                                        .ghost()
                                        .label(tasks_back)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.go_back_from_tasks(window, cx)
                                        })),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child(SharedString::from("Tasks")),
                                ),
                        )
                    })
                    .when(!self.home_visible, |header| {
                        header.child(
                            h_flex()
                                .flex_none()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("go-home")
                                        .ghost()
                                        .accessibility_label("Home")
                                        .child(div().text_sm().child("‹ Home"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.go_home(window, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child(self.project_name.clone()),
                                )
                                .when_some(self.git_poll.status().branch.clone(), |this, branch| {
                                    this.child(self.render_branch_pill(branch.into()))
                                })
                                // Amber dot while staged, unstaged, or untracked
                                // changes exist; hidden when clean so the steady
                                // state stays quiet.
                                .when(self.git_poll.status().dirty, |this| {
                                    this.child(div().text_xs().text_color(rgb(0xeab308)).child("●"))
                                })
                                .when_some(self.git_poll.status().ahead_label(), |this, ahead| {
                                    this.child(
                                        div().text_xs().text_color(rgb(0x858989)).child(ahead),
                                    )
                                })
                                .when_some(
                                    self.git_poll.status().behind_label(),
                                    |this, behind| {
                                        this.child(
                                            div().text_xs().text_color(rgb(0x858989)).child(behind),
                                        )
                                    },
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_center()
                            .gap_2()
                            .child(
                                div()
                                    .relative()
                                    .w(px(380.))
                                    .flex_shrink_0()
                                    .child(
                                        Button::new("workspace-command-trigger")
                                            .ghost()
                                            .accessibility_label("Open command palette")
                                            .w(px(380.))
                                            .h(px(32.))
                                            .px_3()
                                            .gap_2()
                                            .items_center()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(0x292b2b))
                                            .bg(rgb(0x0e0f0f))
                                            .text_xs()
                                            .text_color(rgb(0x737878))
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.toggle_command_palette(
                                                    PaletteMode::Actions,
                                                    window,
                                                    cx,
                                                );
                                            }))
                                            .child(
                                                Icon::new(IconName::Search)
                                                    .size(px(14.))
                                                    .text_color(rgb(0x737878)),
                                            )
                                            .child(div().flex_1().child("Type a command…"))
                                            .child(
                                                div()
                                                    .px_1()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(rgb(0x292b2b))
                                                    .text_color(rgb(0x858989))
                                                    .child("⌘K"),
                                            )
                                            .child(
                                                div()
                                                    .px_1()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(rgb(0x292b2b))
                                                    .text_color(rgb(0x858989))
                                                    .child("⌘P"),
                                            ),
                                    )
                                    // Keep the open search field on the trigger's exact
                                    // bounds, even when neighboring header controls change.
                                    .when(self.command_open, |anchor| {
                                        anchor.child(deferred(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .left_0()
                                                .w_full()
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|_, _, _, cx| {
                                                        cx.stop_propagation()
                                                    }),
                                                )
                                                .child(self.render_command_bar(cx)),
                                        ))
                                    }),
                            )
                            .when(attention_count > 0, |bar| {
                                bar.child(
                                    Button::new("agent-attention-trigger")
                                        .ghost()
                                        .accessibility_label(attention_label.clone())
                                        .h(px(32.))
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x713f12))
                                        .bg(rgb(0x211609))
                                        .text_xs()
                                        .text_color(rgb(0xf59e0b))
                                        .label(attention_label.clone())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_command_palette(
                                                PaletteMode::Projects,
                                                window,
                                                cx,
                                            );
                                            this.attention_only = true;
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .when(is_artifacts, |header| {
                        header.child(
                            Checkbox::new("show-archived-artifacts")
                                .label("Show archived")
                                .checked(show_archived)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    let checked = *checked;
                                    this.home.update(cx, |view, cx| {
                                        view.set_artifacts_archived(checked, cx)
                                    });
                                })),
                        )
                    })
                    .when(is_tasks, |header| {
                        header.child(
                            Checkbox::new("show-archived-tasks")
                                .label("Show archived")
                                .checked(show_tasks_archived)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    let checked = *checked;
                                    this.home.update(cx, |view, cx| {
                                        view.set_tasks_archived(checked, cx)
                                    });
                                })),
                        )
                    })
                    .when(self.home_visible && !is_home_page, |header| {
                        let status = self.portable_git_poll.status();
                        // Just the state tag — branch and counts live in the
                        // tooltip. Same hues as the project cards.
                        let (label, hue, detail) = if self.data_root.is_none() {
                            (
                                "Unavailable",
                                ColorName::Gray,
                                "Portable data unavailable".to_owned(),
                            )
                        } else {
                            match project_state_tag(Some(status)) {
                                Some((label, hue)) => {
                                    let mut detail = format!(
                                        "Portable · {} · {label}",
                                        status.branch.as_deref().unwrap_or("Git unavailable")
                                    );
                                    for counts in status
                                        .ahead_label()
                                        .into_iter()
                                        .chain(status.behind_label())
                                    {
                                        detail.push_str(" · ");
                                        detail.push_str(&counts);
                                    }
                                    (label, hue, detail)
                                }
                                None => (
                                    "Unavailable",
                                    ColorName::Gray,
                                    "Portable git status unavailable".to_owned(),
                                ),
                            }
                        };
                        header.child(
                            Button::new("portable-status")
                                .ghost()
                                .tooltip(detail)
                                .child(
                                    Tag::color(hue)
                                        .with_size(Size::Small)
                                        .rounded_full()
                                        .child(label),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_settings(window, cx)
                                })),
                        )
                    })
                    .when(!self.home_visible, |header| {
                        header.child(
                            h_flex()
                                .flex_none()
                                .gap_2()
                                .items_center()
                                .child(
                                    TabBar::new("workspace-tabs")
                                        .segmented()
                                        .selected_index(active_index)
                                        .on_click(cx.listener(|this, index: &usize, window, cx| {
                                            this.select_tab(*index, window, cx);
                                        }))
                                        .children(
                                            WorkspaceTab::ALL
                                                .into_iter()
                                                .map(|tab| Tab::new().label(tab.label())),
                                        ),
                                )
                                // Per-workspace settings: default agent harness for
                                // this checkout. Opens a sheet (see
                                // `open_workspace_settings`); the sheet layer below
                                // paints it.
                                .child(
                                    div()
                                        .p_2()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(rgb(0x737878))
                                        .hover(|this| {
                                            this.bg(rgb(0x1d1f1f)).text_color(rgb(0xe7e7e7))
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                this.open_workspace_settings(window, cx);
                                            }),
                                        )
                                        .child(Icon::new(IconName::Settings).size(px(16.))),
                                ),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(active_content))
            .when(self.command_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(px(WORKSPACE_HEADER_HEIGHT))
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_command_palette(window, cx);
                            }),
                        )
                        .child(div().absolute().size_full().bg(rgb(0x000000)).opacity(0.4)),
                )
            })
            // Notification layer (`push_notification`, e.g. the terminal copy
            // feedback): like dialogs, `Root` stores these without painting
            // them. Without this layer every notification is silently
            // swallowed. Above content and the palette dim, below sheets and
            // dialogs.
            .children(Root::render_notification_layer(window, cx))
            // Sheet layer (workspace settings): `Root` stores the opened
            // sheet but never paints it itself — without this layer the gear
            // button opens a sheet that stays invisible. Below dialogs so a
            // dialog opened over a sheet floats on top.
            .children(Root::render_sheet_layer(window, cx))
            // Dialog layer (settings, …): `Root` stores opened dialogs but
            // never paints them itself — the app must render this layer on
            // top of its content, otherwise an opened dialog stays invisible.
            // Last child so dialogs float above the command palette too.
            .children(Root::render_dialog_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn checkout_identity_normalizes_paths_and_preserves_missing_paths() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            checkout_identity(&directory.path().join(".")),
            checkout_identity(directory.path())
        );
        let missing = directory.path().join("missing");
        assert_eq!(checkout_identity(&missing), missing);
    }

    #[cfg(unix)]
    #[test]
    fn checkout_aliases_share_identity_but_distinct_checkouts_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        let other = directory.path().join("other");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&checkout, &alias).unwrap();
        assert_eq!(checkout_identity(&checkout), checkout_identity(&alias));
        assert_ne!(checkout_identity(&checkout), checkout_identity(&other));
    }

    #[test]
    fn tab_contract_matches_requested_commands() {
        assert_eq!(WorkspaceTab::Agent.command(), Some("opencode"));
        assert_eq!(WorkspaceTab::Editor.command(), Some("nvim ."));
        assert_eq!(WorkspaceTab::Terminal.command(), None);
        assert_eq!(WorkspaceTab::Review.command(), None);
        assert_eq!(WorkspaceTab::Review.label(), "Review");
        assert!(!WorkspaceTab::Review.has_terminal());
        assert_eq!(WorkspaceTab::Tasks.label(), "Tasks");
        assert_eq!(WorkspaceTab::Tasks.command(), None);
        assert!(!WorkspaceTab::Tasks.has_terminal());
    }

    fn dirty_status() -> GitStatus {
        GitStatus {
            branch: Some("main".to_owned()),
            dirty: true,
            ..GitStatus::default()
        }
    }

    #[test]
    fn stale_git_loads_never_overwrite_newer_state() {
        let mut poll = GitPoll::default();
        // Tick for the old checkout starts first and finishes last.
        let (old_generation, old_path) = poll.begin_check(PathBuf::from("/old"));
        let (new_generation, new_path) = poll.begin_check(PathBuf::from("/new"));
        assert_ne!(old_generation, new_generation);
        assert_eq!(old_path, PathBuf::from("/old"));
        assert_eq!(new_path, PathBuf::from("/new"));

        // The stale old-checkout load lands after the switch: discarded.
        assert!(!poll.commit(old_generation, dirty_status()));
        assert_eq!(poll.status(), &GitStatus::default());
        // The current load still applies.
        assert!(poll.commit(new_generation, dirty_status()));
        assert!(poll.status().dirty);
    }

    #[test]
    fn git_commit_reports_change_for_notify() {
        let mut poll = GitPoll::default();
        let (generation, _) = poll.begin_check(PathBuf::from("/repo"));
        // Identical status is not a change: no repaint needed.
        assert!(!poll.commit(generation, GitStatus::default()));
        assert!(poll.commit(generation, dirty_status()));
        // Re-committing the same status is not a change either.
        assert!(!poll.commit(generation, dirty_status()));
    }

    #[test]
    fn git_reset_clears_status_without_invalidating_pending_load() {
        let mut poll = GitPoll::default();
        let (old_generation, _) = poll.begin_check(PathBuf::from("/old"));
        assert!(poll.commit(old_generation, dirty_status()));
        // Switch: a fresh check resets the display but the pending load
        // stays current.
        let (generation, _) = poll.begin_check(PathBuf::from("/new"));
        poll.reset();
        assert_eq!(poll.status(), &GitStatus::default());
        // Reset already shows default, so offering it reports no change —
        // but the generation is accepted, and the fresh dirty load applies.
        assert!(!poll.commit(generation, GitStatus::default()));
        assert!(poll.commit(generation, dirty_status()));
        // ...while the pre-switch load is still stale.
        assert!(!poll.commit(old_generation, dirty_status()));
    }
}
