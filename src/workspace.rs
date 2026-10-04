//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header and tab bar) hosting the active terminal pane.

#[path = "workspace_sessions.rs"]
mod sessions;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, ColorName, Icon, IconName, IndexPath, Sizable, Size, StyledExt as _,
    WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    menu::{PopupMenu, PopupMenuItem},
    popover::Popover,
    spinner::Spinner,
    tab::{Tab, TabBar},
    tag::Tag,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, FocusHandle, Focusable as _,
    InteractiveElement, IntoElement, KeyUpEvent, KeystrokeEvent, ModifiersChangedEvent,
    MouseButton, ParentElement, Render, SharedString, Styled, Subscription, Window, deferred, div,
    px, rgb,
};

use crate::add_repository::AddRepositoryView;
use crate::agent::AgentKind;
use crate::agent_activity::{ActivityState, AgentActivityStore};
use crate::agent_icons::{self, AgentIconTiles};
use crate::agent_sessions::{
    Catalog, DEFAULT_SIDEBAR_LIMIT, HOME_LIMIT, SessionKey, SessionSummary,
    Snapshot as SessionSnapshot,
};
use crate::command_palette::{
    GoToAgent, GoToEditor, GoToResources, GoToReview, GoToTerminal, NewAgentSession,
    PaletteCommand, PaletteItem, PaletteMode, PaletteSection, ToggleActionsPalette,
    ToggleProjectsPalette, is_primary_modifier, is_quit_shortcut, item_at,
    palette_mode_for_shortcut, palette_sections_for_mode,
};
use crate::data::{
    DEFAULT_SPACE, DataRoot, DeviceStore, RecentRepository, Spaces, SyncStatus, SyncTracker,
    checkout_for, ensure_spaces, recent_repositories, record_repository_open, resolve_current_key,
    space_eq, sync_portable_with_tracker,
};
use crate::editor::{EditorChoice, ExternalEditorKind, native::NativeEditor};
use crate::git_status::{GitStatus, load_git_status};
use crate::home::{HomeEvent, HomeView, project_state_tag};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, WORKSPACE_HEADER_HEIGHT};
use crate::navigation::{
    self, Command as NavigationCommand, Context as NavigationContext,
    Decision as NavigationDecision, Input as NavigationInput,
};
use crate::pane::TerminalPane;
use crate::review::ReviewView;
use crate::settings::SettingsView;
use crate::tools::{ToolKind, ToolView};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
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

/// Key context of the workspace subtree. `bind_app_keys` (see `main.rs`)
/// unbinds Tab/Shift+Tab here, so normal-mode focus traversal stays out of
/// the focused component's way — a terminal sends both keys to the pty
/// (agent harnesses switch agents/models/modes with them) instead of moving
/// focus. Component traversal lives in navigation mode (see
/// `move_navigation_component_focus`).
pub(crate) const WORKSPACE_KEY_CONTEXT: &str = "Workspace";

/// Restore terminal Tab input inside the Git dialog without changing focus
/// traversal in other dialogs (see `bind_app_keys`).
pub(crate) const TERMINAL_DIALOG_KEY_CONTEXT: &str = "terminal-dialog";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceTab {
    Agent,
    Editor,
    Terminal,
    Review,
    Resources,
    /// Dialog-only terminal; intentionally excluded from `ALL` and the tab bar.
    Git,
}

impl WorkspaceTab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Agent,
        Self::Editor,
        Self::Terminal,
        Self::Review,
        Self::Resources,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent",
            Self::Editor => "Editor",
            Self::Terminal => "Terminal",
            Self::Review => "Review",
            Self::Resources => "Resources",
            Self::Git => "Git",
        }
    }

    pub(crate) fn command(self) -> Option<&'static str> {
        match self {
            // Single source of truth lives on the default harness, so the
            // label in Settings and the spawned command cannot drift.
            Self::Agent => Some(AgentKind::DEFAULT.command()),
            Self::Editor => Some("nvim ."),
            Self::Git => Some("lazygit"),
            Self::Terminal | Self::Review | Self::Resources => None,
        }
    }

    /// Review and Resources render native content instead of hosting a shell.
    pub(crate) fn has_terminal(self) -> bool {
        !matches!(self, Self::Review | Self::Resources)
    }
}

/// Where the global artifact browser should return to. Captured on
/// entry so the titlebar back button behaves like a browser back button
/// instead of always landing on Home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageOrigin {
    Home,
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
    resources: Entity<crate::artifacts::ArtifactBrowser>,
    home: Entity<HomeView>,
    relationships: Entity<crate::repository_graph::GraphPage>,
    relationships_visible: bool,
    relationships_origin: Option<PageOrigin>,
    home_visible: bool,
    /// Isolation profiles and the active one. `active_space` is machine-local
    /// (`device.json`) and drives every space-filtered surface; the catalog
    /// is portable and syncs with the records it scopes.
    spaces: Spaces,
    active_space: String,
    /// The titlebar space switcher's controlled popover plus the menu it
    /// shows, built per opening and owned here (never window-keyed state).
    /// The focus handle is captured at build time: closing must not read the
    /// menu entity, which is leased while its own click/dismiss handler runs.
    space_menu_open: bool,
    space_menu: Option<Entity<PopupMenu>>,
    space_menu_focus: Option<FocusHandle>,
    portable_git_poll: GitPoll,
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    editor_instance: Option<EditorChoice>,
    editor_preference: EditorChoice,
    editor_executables: HashMap<String, String>,
    native_editor: Option<Entity<NativeEditor>>,
    review: Entity<ReviewView>,
    /// Harness the visible Agent pane was spawned with. Preserved across
    /// repository switches with its tabs; selecting a historical session
    /// or starting a new one updates it.
    session_agent: AgentKind,
    /// Global default harness (Settings > Agent): what fresh Agent panes
    /// launch and what the New-session picker pre-selects. Loaded from
    /// `device.json` at startup and pushed live by Settings; open sessions
    /// keep running with the harness they started with. Always one of
    /// [`Self::enabled_agents`].
    default_agent: AgentKind,
    /// Enabled harnesses (Settings > Agent). Fresh Agent panes and the
    /// New-session picker only offer these; open sessions keep running even
    /// if their harness is later disabled. Never empty.
    enabled_agents: Vec<AgentKind>,
    /// Strong entity handles keep hidden PTYs and their output tasks alive.
    inactive_repositories: HashMap<PathBuf, RepositoryTabs>,
    settings: Entity<SettingsView>,
    /// Tool dialogs opened from the command bar's Tools section: one
    /// long-lived view per tool, created on first open. Reopening shows
    /// exactly what was left there, and a close-time save flush can still
    /// reach the view after the dialog layer drops it.
    tool_views: HashMap<ToolKind, Entity<ToolView>>,
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
    /// Recent-session rows per repository in the Agent sidebar. Loaded from
    /// `device.json` at startup and pushed live by Settings > Agent; the
    /// sidebar re-projects the in-memory snapshot on change, no rescan.
    session_limit: usize,
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
    /// The which-key style navigation overlay. It deliberately owns no focus:
    /// focus remains on real panes so `h`/`l` can move it and Enter can leave
    /// the selected pane active after the overlay disappears.
    navigation_open: bool,
    navigation_pane: usize,
    /// Cursor for navigation-mode `j`/`k` through the Agent sessions sidebar
    /// in display order (open sessions, then recent). `None` outside the
    /// sidebar or outside navigation mode; `Enter` opens the cursor session
    /// once and exits the mode, `Escape` exits without opening.
    session_cursor: Option<usize>,
    /// Keys consumed by the navigation interceptor and held until release so
    /// physical repeats cannot toggle twice or leak into a newly focused
    /// terminal. The flag per key records whether any modifier moved since
    /// the claim: a later modifier press proves the previous hold ended, so
    /// a re-pressed *trigger* is honored as fresh even if its key-up was
    /// lost. Re-arm is trigger-only — a marked non-trigger key stays
    /// swallowed, since a missed press beats an accidental edit — and a
    /// trigger repeat with no modifier movement in between stays swallowed,
    /// which is what keeps a held toggle from flickering.
    navigation_claimed_keys: HashMap<String, bool>,
    agent_sidebar_focus: FocusHandle,
    /// Full-color harness logos for the Agent sidebar, the new-session
    /// picker, and session rows. The harness set is fixed, so tiles are
    /// rasterized once at startup and shared by every render.
    agent_icon_tiles: Rc<AgentIconTiles>,
    /// Kept for the workspace lifetime so the app-level pre-keymap hook is
    /// released with this window rather than leaking into later workspaces.
    _navigation_interceptor: Subscription,
}

struct OpenAgentSession {
    pane: Entity<TerminalPane>,
    checkout: PathBuf,
    agent: AgentKind,
    key: Option<SessionKey>,
    title: String,
    /// Unix seconds when this pane was tracked. Bounds the recency fallback
    /// for harnesses without an authoritative identity signal (Codex): only
    /// catalog sessions updated after the pane existed can be its own.
    created_at: i64,
}

struct RepositoryTabs {
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    editor_instance: Option<EditorChoice>,
    native_editor: Option<Entity<NativeEditor>>,
    review: Entity<ReviewView>,
    /// Harness its Agent pane was spawned with, kept so a switch back
    /// restores the running session instead of respawning.
    agent: AgentKind,
}

impl RepositoryTabs {
    fn new(
        working_directory: &Path,
        agent: AgentKind,
        activity: &AgentActivityStore,
        editor: EditorChoice,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                (tab.has_terminal()
                    && tab != WorkspaceTab::Agent
                    && (tab != WorkspaceTab::Editor || editor == EditorChoice::Neovim))
                    .then(|| {
                        cx.new(|cx| TerminalPane::new(tab, working_directory, agent, activity, cx))
                    })
            })
            .collect();
        let review = Workspace::new_review(working_directory, cx);
        Self {
            active_tab: WorkspaceTab::Agent,
            tabs,
            editor_instance: Some(editor),
            native_editor: None,
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
        let data_root = crate::data::ensure_ready(None).ok();
        // Global default harness (Settings > Agent). Fresh Agent panes
        // start with it; returning to a checkout resumes its most recent
        // session instead. Later edits come through `set_default_agent`.
        // The default always stays within the enabled set; later edits come
        // through `set_enabled_agents` / `set_default_agent`.
        let stored = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok());
        let editor_preference = stored
            .as_ref()
            .map(|state| state.editor_choice_or_default())
            .unwrap_or_default();
        let editor_executables = stored
            .as_ref()
            .and_then(|state| state.editor_executables.clone())
            .unwrap_or_default();
        // Isolation profiles: seed/reconcile the portable catalog (Personal
        // and Work plus any legacy group names the records reference) and
        // resume this device's last active space. A catalog that cannot be
        // read falls back to the seeded pair so Home still filters sanely.
        let spaces = data_root
            .as_ref()
            .and_then(|root| ensure_spaces(root).ok())
            .unwrap_or_else(|| {
                let mut catalog = Spaces::default();
                for name in crate::data::spaces::DEFAULT_SEED {
                    catalog.ensure_name(name);
                }
                catalog
            });
        let active_space = stored
            .as_ref()
            .and_then(|state| state.active_space.as_deref())
            .and_then(|name| spaces.canonical(name))
            .or_else(|| spaces.first().map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_SPACE.to_owned());
        let mut enabled_agents = stored
            .as_ref()
            .map(|state| state.enabled_agents_or_default())
            .unwrap_or_else(|| AgentKind::ALL.to_vec());
        if enabled_agents.is_empty() {
            enabled_agents = AgentKind::ALL.to_vec();
        }
        let mut default_agent = stored
            .as_ref()
            .map(|state| state.default_agent_or_default())
            .unwrap_or(AgentKind::DEFAULT);
        if !enabled_agents.contains(&default_agent) {
            default_agent = enabled_agents
                .first()
                .copied()
                .unwrap_or(AgentKind::DEFAULT);
        }
        // Home must not launch an agent or editor in the startup directory.
        // Panes are created only when the user enters a repository workspace.
        let active_tab = WorkspaceTab::Agent;
        let tabs = vec![None; WorkspaceTab::ALL.len()];
        let review = Workspace::new_review(working_directory, cx);
        let session_agent = default_agent;
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        // Interceptors run before GPUI resolves key bindings (unlike element
        // capture handlers). This is essential while the HUD is open: Escape,
        // Enter, arrows, and native-input bindings must not act before the
        // navigation mode consumes them.
        let interceptor_workspace = cx.entity().downgrade();
        let interceptor_window = window.window_handle();
        let navigation_interceptor = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != interceptor_window {
                return;
            }
            let _ = interceptor_workspace.update(cx, |workspace, cx| {
                workspace.on_navigation_keystroke(event, window, cx);
            });
        });
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
            if !window.is_window_active() {
                this.close_navigation(cx);
                this.navigation_claimed_keys.clear();
            }
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
                            if !this.home_visible && this.active_tab == WorkspaceTab::Resources {
                                this.resources.update(cx, |view, cx| view.refresh(cx));
                            }
                            this.home.update(cx, |view, cx| view.refresh_artifacts(cx));
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
        // Initial Agent sidebar limit, mirroring the font size above. Later
        // edits come through `set_session_limit` (Settings > Agent).
        let session_limit = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok())
            .map(|state| state.recent_sessions_limit_or_default())
            .unwrap_or(DEFAULT_SIDEBAR_LIMIT);
        // One shared tracker: the palette guard, the scheduler skip, and the
        // Settings status line all read the same run state.
        let sync_tracker = SyncTracker::default();
        let home = cx.new(|cx| HomeView::new(data_root.clone(), sync_tracker.clone(), cx));
        home.update(cx, |view, cx| {
            view.set_space(spaces.names(), active_space.clone(), cx)
        });
        home.read(cx).focus_handle.clone().focus(window, cx);
        cx.subscribe_in(&home, window, |this, _, event, window, cx| match event {
            HomeEvent::OpenReference(reference) => {
                this.open_reference(reference.clone(), window, cx)
            }
            HomeEvent::OpenRepository { key, label } => {
                this.switch_repository(key, label, window, cx)
            }
            HomeEvent::Relationships => this.open_relationships(window, cx),
            HomeEvent::AddRepository => this.open_add_repository(window, cx),
            HomeEvent::LinkRepository { key } => this.open_link_repository(key, window, cx),
            HomeEvent::EditRepository { key } => this.open_edit_repository(key, window, cx),
            HomeEvent::UnlinkRepository { key } => this.unlink_repository(key, window, cx),
            HomeEvent::RemoveRepository { key } => this.confirm_remove_repository(key, window, cx),
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
                        if this.sessions_visible() {
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
                        if this.sessions_visible() {
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
        let resources = cx.new(|cx| {
            crate::artifacts::ArtifactBrowser::scoped(
                data_root.clone(),
                crate::artifacts::Scope::Repository(current_repository.clone()),
                cx,
            )
        });
        cx.subscribe_in(
            &resources,
            window,
            |this, _, event: &crate::preview::OpenReference, window, cx| {
                this.open_reference(event.0.clone(), window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &resources,
            window,
            |this, _, event: &crate::artifacts::OpenSession, window, cx| {
                this.open_agent_session(event.0.key.clone(), window, cx);
            },
        )
        .detach();
        let relationships =
            cx.new(|cx| crate::repository_graph::GraphPage::new(data_root.clone(), window, cx));
        relationships.update(cx, |view, cx| {
            view.set_space(active_space.clone(), spaces.names(), cx)
        });
        cx.subscribe_in(
            &relationships,
            window,
            |this, _, event, window, cx| match event {
                crate::repository_graph::GraphEvent::AddRepository => {
                    this.open_add_repository(window, cx)
                }
                crate::repository_graph::GraphEvent::OpenRepository { key, label } => {
                    this.switch_repository(key, label, window, cx)
                }
                crate::repository_graph::GraphEvent::MetadataChanged => {
                    this.home.update(cx, |view, cx| view.reload(cx));
                    this.reload_recent_repositories();
                }
            },
        )
        .detach();
        let mut agent_icon_tiles = AgentIconTiles::new();
        agent_icons::ensure_tiles(AgentKind::ALL, &mut agent_icon_tiles, cx);
        Self {
            relationships,
            relationships_visible: false,
            relationships_origin: None,
            home,
            resources,
            home_visible: true,
            spaces,
            active_space,
            space_menu_open: false,
            space_menu: None,
            space_menu_focus: None,
            portable_git_poll: GitPoll::default(),
            active_tab,
            tabs,
            editor_instance: None,
            editor_preference,
            editor_executables,
            native_editor: None,
            review,
            session_agent,
            default_agent,
            enabled_agents,
            inactive_repositories: HashMap::new(),
            settings,
            tool_views: HashMap::new(),
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
            session_limit,
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
            navigation_open: false,
            navigation_pane: 0,
            session_cursor: None,
            navigation_claimed_keys: HashMap::new(),
            agent_sidebar_focus: cx.focus_handle().tab_stop(true),
            agent_icon_tiles: Rc::new(agent_icon_tiles),
            _navigation_interceptor: navigation_interceptor,
        }
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = WorkspaceTab::ALL.get(index).copied() else {
            return;
        };
        self.close_navigation(cx);
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
        if self.relationships_visible {
            self.relationships
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
            return;
        }
        if self.home_visible {
            self.home.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Review {
            self.review.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Resources {
            self.resources
                .read(cx)
                .focus_handle
                .clone()
                .focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Editor
            && self.editor_instance == Some(EditorChoice::BuiltIn)
        {
            if let Some(editor) = self.native_editor.as_ref() {
                editor.read(cx).editor_focus(cx).focus(window, cx);
            }
            return;
        }
        if let Some(Some(pane)) = self.tabs.get(self.active_tab as usize) {
            let focus_handle = pane.read(cx).focus_handle.clone();
            focus_handle.focus(window, cx);
        }
    }

    fn sync_activity_visibility(&self, window: &Window, cx: &App) {
        let visible = if !self.home_visible && window.is_window_active() {
            match self.active_tab {
                WorkspaceTab::Agent => self.tabs[WorkspaceTab::Agent as usize]
                    .as_ref()
                    .and_then(|pane| pane.read(cx).launch_id()),
                _ => None,
            }
        } else {
            None
        };
        self.agent_activity.set_visible_launch(visible);
    }

    /// Jump straight to the Agent tab. An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Agent as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Editor tab. An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Editor as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump back to the origin of the last follow-definition hop in the
    /// built-in editor. A no-op without a built-in session or history, so
    /// the command stays harmless wherever it is offered.
    fn editor_go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_history = self
            .native_editor
            .as_ref()
            .is_some_and(|editor| editor.read(cx).has_jump_history());
        if self.home_visible || !has_history {
            return;
        }
        self.select_tab(WorkspaceTab::Editor as usize, window, cx);
        if let Some(editor) = self.native_editor.as_ref() {
            editor.update(cx, |editor, cx| editor.go_back(window, cx));
        }
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    pub(crate) fn set_editor_choice(
        &mut self,
        choice: EditorChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_preference = choice;
        if !self.home_visible {
            self.editor_instance = Some(choice);
            if choice == EditorChoice::BuiltIn && self.native_editor.is_none() {
                let executables = self.editor_executables.clone();
                let draft_root = self.data_root.as_ref().map(|root| root.root().to_owned());
                self.native_editor = Some(cx.new(|cx| {
                    NativeEditor::new(
                        &self.working_directory,
                        executables,
                        draft_root.as_deref(),
                        window,
                        cx,
                    )
                }));
            }
            if choice == EditorChoice::Neovim && self.tabs[WorkspaceTab::Editor as usize].is_none()
            {
                self.tabs[WorkspaceTab::Editor as usize] = Some(cx.new(|cx| {
                    TerminalPane::new(
                        WorkspaceTab::Editor,
                        &self.working_directory,
                        self.default_agent,
                        &self.agent_activity,
                        cx,
                    )
                }));
            }
        }
        self.refresh_review_editor_action(cx);
        cx.notify();
    }

    pub(crate) fn set_external_executable(
        &mut self,
        kind: ExternalEditorKind,
        executable: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(executable) = executable {
            self.editor_executables
                .insert(kind.id().to_owned(), executable);
        } else {
            self.editor_executables.remove(kind.id());
        }
        let executables = self.editor_executables.clone();
        if let Some(editor) = self.native_editor.as_ref() {
            editor.update(cx, |view, cx| view.set_executables(executables.clone(), cx));
        }
        for tabs in self.inactive_repositories.values() {
            if let Some(editor) = tabs.native_editor.as_ref() {
                editor.update(cx, |view, cx| view.set_executables(executables.clone(), cx));
            }
        }
        cx.notify();
    }

    fn refresh_review_editor_action(&mut self, cx: &mut Context<Self>) {
        let available = self.editor_instance == Some(EditorChoice::BuiltIn);
        self.review
            .update(cx, |view, cx| view.set_editor_open_available(available, cx));
    }

    /// Jump straight to the Terminal tab. An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Terminal as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Review tab. An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Review as usize, window, cx);
        if self.command_open {
            self.close_command_palette(window, cx);
        }
    }

    /// Jump straight to the Resources tab. An open command bar
    /// closes first, so the shortcut never leaves the palette stranded over
    /// the new tab.
    fn go_to_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tab(WorkspaceTab::Resources as usize, window, cx);
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
        // Opening another overlay clears stale navigation state: the palette
        // trap owns the keyboard from here, so the HUD must not stay behind it.
        self.close_navigation(cx);
        self.command_open = true;
        self.attention_only = false;
        self.palette_mode = mode;
        self.sync_tutorial_palette(cx);
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
        self.sync_tutorial_palette(cx);
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Keep tutorial webviews behind the palette: native children paint over
    /// GPUI popups, so both artifact browsers hide while the bar is open and
    /// re-show on the next render once it closes.
    fn sync_tutorial_palette(&mut self, cx: &mut Context<Self>) {
        let open = self.command_open;
        self.home
            .update(cx, |view, cx| view.set_palette_open(open, cx));
        self.resources
            .update(cx, |view, cx| view.set_palette_open(open, cx));
    }

    /// Same cover rule for the navigation HUD.
    fn sync_tutorial_navigation(&mut self, cx: &mut Context<Self>) {
        let open = self.navigation_open;
        self.home
            .update(cx, |view, cx| view.set_navigation_open(open, cx));
        self.resources
            .update(cx, |view, cx| view.set_navigation_open(open, cx));
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
            view.refresh_from_disk(window, cx);
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Settings")
                .w(px(920.))
                .h(px(600.))
                .child(settings.clone().into_any_element())
        });
    }

    /// Open a developer tool as a modal dialog. The view is created once per
    /// tool and kept here, so its inputs and derived output survive
    /// close/reopen; the tool view owns the dialog chrome, including the
    /// close-time save flush.
    fn open_tool(&mut self, tool: ToolKind, window: &mut Window, cx: &mut Context<Self>) {
        let view = match self.tool_views.get(&tool) {
            Some(view) => view.clone(),
            None => {
                let data_root = self.data_root.clone();
                let view = cx.new(|cx| ToolView::new(window, cx, tool, data_root));
                self.tool_views.insert(tool, view.clone());
                view
            }
        };
        ToolView::open_dialog(view, window, cx);
    }

    /// A window-sized, minimally framed terminal dialog running lazygit in the
    /// current checkout. Each opening starts a new session, so quitting
    /// lazygit never leaves a stale shell prompt on the next opening.
    fn open_git_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = cx.new(|cx| {
            TerminalPane::new(
                WorkspaceTab::Git,
                &self.working_directory,
                self.default_agent,
                &self.agent_activity,
                cx,
            )
        });
        let dialog_pane = pane.clone();
        window.open_dialog(cx, move |dialog, window, _| {
            dialog
                .title(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .pr(px(40.))
                        .child("Git changes")
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x808888))
                                .child("Shift+Esc to close"),
                        ),
                )
                // The dialog component clamps to a small safety inset; ask
                // for the whole viewport and let it take everything it can.
                .w(window.viewport_size().width)
                .h(window.viewport_size().height)
                .margin_top(px(0.))
                .p(px(8.))
                // Enter and Escape must reach lazygit, not confirm/dismiss
                // the dialog. Shift+Esc, Cmd/Ctrl+J, and the close button do.
                .keyboard(false)
                .child(dialog_pane.clone().into_any_element())
        });
        let focus = pane.read(cx).focus_handle.clone();
        focus.focus(window, cx);
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
        self.sync_tutorial_palette(cx);
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
            PaletteItem::Tool(tool) => self.open_tool(tool, window, cx),
            PaletteItem::Command(command) => match command {
                PaletteCommand::GoAgent => self.select_tab(0, window, cx),
                PaletteCommand::GoEditor => self.select_tab(1, window, cx),
                PaletteCommand::GoTerminal => self.select_tab(2, window, cx),
                PaletteCommand::GoReview => self.select_tab(3, window, cx),
                PaletteCommand::GoResources => self.select_tab(4, window, cx),
                PaletteCommand::OpenSettings => self.open_settings(window, cx),
                PaletteCommand::EditorGoBack => self.editor_go_back(window, cx),
                PaletteCommand::AddRepository => self.open_add_repository(window, cx),
                PaletteCommand::GoHome => self.go_home(window, cx),
                PaletteCommand::RepositoryRelationships => self.open_relationships(window, cx),
                PaletteCommand::BrowseArtifacts => self.browse_artifacts(window, cx),
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
        let spaces = self.spaces.names();
        let active_space = self.active_space.clone();
        let view = cx.new(|cx| AddRepositoryView::new(window, cx, data_root, spaces, active_space));
        cx.subscribe(
            &view,
            |this, _, _: &crate::add_repository::RepositoryAdded, cx| {
                this.home.update(cx, |view, cx| view.reload(cx));
                this.relationships.update(cx, |view, cx| view.refresh(cx));
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

    /// Open the Link dialog for an existing portable record: checkout picker
    /// only, key fixed. Used from the Projects page for records synced from
    /// another device (or left unbound by an interrupted create).
    fn open_link_repository(&self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let data_root = self.data_root.clone();
        let key = key.to_owned();
        let spaces = self.spaces.names();
        let active_space = self.active_space.clone();
        let view = cx.new(|cx| {
            AddRepositoryView::for_link(window, cx, data_root, key.clone(), spaces, active_space)
        });
        cx.subscribe(
            &view,
            |this, _, _: &crate::add_repository::RepositoryAdded, cx| {
                this.home.update(cx, |view, cx| view.reload(cx));
                this.relationships.update(cx, |view, cx| view.refresh(cx));
                this.reload_recent_repositories();
            },
        )
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(format!("Link checkout for \"{key}\""))
                .w(px(680.))
                .h(px(320.))
                .child(view.clone().into_any_element())
        });
    }

    /// Open the Edit dialog for an existing portable record: metadata fields
    /// prefilled, key fixed (renames deferred). Only the portable JSON is
    /// rewritten; the device binding is untouched.
    fn open_edit_repository(&self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.data_root.clone() else {
            window.push_notification("Portable data is unavailable", cx);
            return;
        };
        let key = key.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let read_key = key.clone();
            let result = cx
                .background_spawn(
                    async move { crate::data::get_repository_metadata(&root, &read_key) },
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let metadata = match result {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        window
                            .push_notification(format!("Could not load \"{key}\": {error:#}"), cx);
                        return;
                    }
                };
                let data_root = this.data_root.clone();
                let spaces = this.spaces.names();
                let active_space = this.active_space.clone();
                let view = cx.new(|cx| {
                    AddRepositoryView::for_edit(
                        window,
                        cx,
                        data_root,
                        key.clone(),
                        &metadata,
                        spaces,
                        active_space,
                    )
                });
                cx.subscribe(
                    &view,
                    |this, _, _: &crate::add_repository::RepositoryAdded, cx| {
                        this.home.update(cx, |view, cx| view.reload(cx));
                        this.relationships.update(cx, |view, cx| view.refresh(cx));
                        this.reload_recent_repositories();
                    },
                )
                .detach();
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog
                        .title(format!("Edit \"{key}\""))
                        .w(px(680.))
                        .h(px(640.))
                        .child(view.clone().into_any_element())
                });
            });
        })
        .detach();
    }

    /// Drop this device's checkout binding, keeping the portable record.
    /// Non-destructive by construction: the Projects page offers Re-link
    /// right where the row stays.
    fn unlink_repository(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.data_root.clone() else {
            window.push_notification("Portable data is unavailable", cx);
            return;
        };
        match crate::data::unlink_repository(&root, key) {
            Ok(true) => {
                window.push_notification(format!("Unlinked \"{key}\""), cx);
            }
            Ok(false) => {
                window.push_notification(format!("\"{key}\" had no checkout binding"), cx);
            }
            Err(error) => {
                window.push_notification(format!("Could not unlink \"{key}\": {error:#}"), cx);
                return;
            }
        }
        self.home.update(cx, |view, cx| view.reload(cx));
        self.reload_recent_repositories();
        cx.notify();
    }

    /// Confirm, then delete a portable record and its device binding. The
    /// local checkout stays on disk; only Devcroft's records go away.
    fn confirm_remove_repository(&self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let key = key.to_owned();
        window.open_dialog(cx, move |dialog, _, _| {
            let workspace = workspace.clone();
            let key = key.clone();
            dialog
                .title(format!("Delete \"{key}\"?"))
                .child(format!(
                    "This removes the portable record for \"{key}\" and this device's checkout binding. The local checkout itself stays on disk. This cannot be undone."
                ))
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel-remove-repository")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-remove-repository")
                                .primary()
                                .label("Delete")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    workspace
                        .update(cx, |this, cx| {
                            this.remove_repository_confirmed(&key, window, cx)
                        })
                        .unwrap_or(false)
                })
        });
    }

    /// Run the confirmed removal: delete records, reload lists, notify.
    /// Returns true so the confirm dialog closes either way; failures are
    /// surfaced as notifications.
    fn remove_repository_confirmed(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(root) = self.data_root.clone() else {
            window.push_notification("Portable data is unavailable", cx);
            return true;
        };
        let key = key.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let delete_key = key.clone();
            let result = cx
                .background_spawn(async move { crate::data::remove_repository(&root, &delete_key) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        window.push_notification(format!("Deleted \"{key}\""), cx);
                        this.home.update(cx, |view, cx| view.reload(cx));
                        this.relationships.update(cx, |view, cx| view.refresh(cx));
                        this.reload_recent_repositories();
                    }
                    Err(error) => window
                        .push_notification(format!("Could not delete \"{key}\": {error:#}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
        true
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
        self.close_navigation(cx);
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
        self.resources.update(cx, |view, cx| {
            view.set_scope(
                crate::artifacts::Scope::Repository(self.current_repository.clone()),
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
        // A location change clears stale navigation state.
        self.close_navigation(cx);
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
        self.resources.update(cx, |view, cx| {
            view.set_scope(
                crate::artifacts::Scope::Repository(self.current_repository.clone()),
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
        // Restored tabs keep their running session; unseen checkouts start
        // fresh and resolve to the global default on entry.
        let next = self
            .inactive_repositories
            .remove(checkout)
            .unwrap_or_else(|| {
                RepositoryTabs::new(
                    checkout,
                    self.default_agent,
                    &self.agent_activity,
                    self.editor_preference,
                    cx,
                )
            });
        let previous = RepositoryTabs {
            active_tab: std::mem::replace(&mut self.active_tab, next.active_tab),
            tabs: std::mem::replace(&mut self.tabs, next.tabs),
            editor_instance: std::mem::replace(&mut self.editor_instance, next.editor_instance),
            native_editor: std::mem::replace(&mut self.native_editor, next.native_editor),
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
        self.relationships_visible = false;
        self.relationships
            .update(cx, |view, cx| view.set_active(false, cx));
        // Catch-all for tab/location changes (select, switch, session open):
        // a stale HUD must not survive the move.
        self.close_navigation(cx);
        self.artifacts_origin = None;
        self.resources.update(cx, |view, cx| {
            view.set_active(self.active_tab == WorkspaceTab::Resources, cx)
        });
        self.home_visible = false;
        self.home.update(cx, |view, cx| view.deactivate(cx));
        if self.editor_instance.is_none() {
            self.editor_instance = Some(self.editor_preference);
        }
        if self.editor_instance == Some(EditorChoice::BuiltIn) && self.native_editor.is_none() {
            let executables = self.editor_executables.clone();
            let draft_root = self.data_root.as_ref().map(|root| root.root().to_owned());
            self.native_editor = Some(cx.new(|cx| {
                NativeEditor::new(
                    &self.working_directory,
                    executables,
                    draft_root.as_deref(),
                    window,
                    cx,
                )
            }));
        }
        self.refresh_review_editor_action(cx);
        for tab in WorkspaceTab::ALL {
            if tab.has_terminal()
                && self.tabs[tab as usize].is_none()
                && (tab != WorkspaceTab::Editor
                    || self.editor_instance == Some(EditorChoice::Neovim))
            {
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

    fn open_relationships(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.relationships_visible {
            self.relationships_origin = Some(if self.home_visible {
                PageOrigin::Home
            } else {
                PageOrigin::Repository
            });
        }
        self.close_navigation(cx);
        self.home.update(cx, |view, cx| view.deactivate(cx));
        self.resources
            .update(cx, |view, cx| view.set_active(false, cx));
        self.relationships_visible = true;
        self.home_visible = true;
        self.command_open = false;
        self.sync_tutorial_palette(cx);
        self.relationships
            .update(cx, |view, cx| view.set_active(true, cx));
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    fn back_from_relationships(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.relationships_visible = false;
        self.relationships
            .update(cx, |view, cx| view.set_active(false, cx));
        match self.relationships_origin.take() {
            Some(PageOrigin::Repository) => self.enter_repository(window, cx),
            _ => {
                self.home.update(cx, |view, cx| view.resume_page(cx));
            }
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Switch isolation profiles: persist the choice machine-locally and
    /// re-project every space-filtered surface. The open repository workspace
    /// stays open (an explicit context); its sidebar keeps its own sessions.
    pub(crate) fn switch_space(
        &mut self,
        space: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(canonical) = self.spaces.canonical(&space) else {
            return;
        };
        if space_eq(&canonical, &self.active_space) {
            return;
        }
        self.active_space = canonical.clone();
        self.close_space_menu(window, cx);
        self.close_navigation(cx);
        if let Some(root) = &self.data_root {
            let _ = DeviceStore::new(root).update(|state| {
                state.active_space = Some(canonical.clone());
            });
        }
        self.refresh_spaces();
        let names = self.spaces.names();
        self.home.update(cx, |view, cx| {
            view.set_space(names.clone(), self.active_space.clone(), cx);
        });
        self.relationships.update(cx, |view, cx| {
            view.set_space(self.active_space.clone(), names, cx);
        });
        self.reload_recent_repositories();
        self.publish_sessions(cx);
        cx.notify();
    }

    /// Toggle the titlebar space switcher (the navigation-mode `s` entry
    /// opens it). Reconciles the catalog first so a space added on another
    /// device shows up without a restart.
    fn toggle_space_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.space_menu_open {
            self.close_space_menu(window, cx);
        } else {
            self.open_space_menu(window, cx);
        }
    }

    /// Whether either artifact browser (the global Home one or the
    /// repository Resources tab) holds an unsaved draft. Settings blocks
    /// catalog renames/deletes then: the rewrite replaces the records a
    /// draft is based on, and a stashed draft's key can name the space being
    /// edited. Catalog edits are rare enough that a blanket guard is
    /// cheaper than tracking every draft-to-space relationship.
    pub(crate) fn has_artifact_draft(&self, cx: &App) -> bool {
        self.home.read(cx).has_artifact_draft(cx) || self.resources.read(cx).has_draft()
    }

    /// Attention count for the active space: agent panes in registered
    /// repositories count under their repository's space; unregistered
    /// checkouts (explicitly opened ad-hoc) always count, matching the Home
    /// session cards' rule.
    fn attention_count(&self) -> usize {
        self.agent_activity
            .snapshot()
            .launches_by_checkout()
            .filter(|(checkout, agent)| {
                agent.state == ActivityState::NeedsAttention
                    && self
                        .session_space(checkout)
                        .is_none_or(|space| space_eq(&space, &self.active_space))
            })
            .count()
    }

    /// Re-read the portable catalog. Best effort: a read failure keeps the
    /// in-memory catalog so the switcher still works.
    fn refresh_spaces(&mut self) {
        if let Some(root) = &self.data_root
            && let Ok(catalog) = ensure_spaces(root)
        {
            self.spaces = catalog;
        }
    }

    /// Re-read the catalog after Settings edits and re-project every
    /// space-filtered surface. When the active name no longer exists (it was
    /// renamed or deleted), follow the retargeted `device.json` value; a
    /// value the catalog does not know falls back to the first space.
    pub(crate) fn reload_spaces(&mut self, cx: &mut Context<Self>) {
        self.refresh_spaces();
        if !self.spaces.contains(&self.active_space) {
            let stored = self
                .data_root
                .as_ref()
                .and_then(|root| DeviceStore::new(root).load().ok())
                .and_then(|state| state.active_space);
            self.active_space = stored
                .as_deref()
                .and_then(|name| self.spaces.canonical(name))
                .or_else(|| self.spaces.first().map(str::to_owned))
                .unwrap_or_else(|| DEFAULT_SPACE.to_owned());
            if let Some(root) = &self.data_root {
                let active = self.active_space.clone();
                let _ = DeviceStore::new(root).update(|state| {
                    state.active_space = Some(active.clone());
                });
            }
        }
        let names = self.spaces.names();
        self.home.update(cx, |view, cx| {
            view.set_space(names.clone(), self.active_space.clone(), cx);
        });
        self.relationships.update(cx, |view, cx| {
            view.set_space(self.active_space.clone(), names, cx);
        });
        self.reload_recent_repositories();
        self.publish_sessions(cx);
        cx.notify();
    }

    /// Titlebar space switcher: one controlled popover so the mouse and the
    /// navigation-mode `s` entry open the same menu. The menu lists the
    /// catalog in order with the active space checked; picking one switches
    /// every space-filtered surface.
    fn space_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_space.clone();
        let menu = self.space_menu.clone();
        // The popover focuses its tracked handle when it opens; pointing that
        // at the menu is what makes the `s` entry keyboard-navigable (the
        // menu is built before the popover's own open transition runs).
        let menu_focus = menu.as_ref().map(|menu| menu.focus_handle(cx));
        Popover::new("space-switcher")
            .open(self.space_menu_open)
            .appearance(false)
            .when_some(menu_focus, |popover, focus| popover.track_focus(&focus))
            .on_open_change(cx.listener(|this, open, window, cx| {
                // The popover owns its trigger's toggling; this callback is
                // the single place that opens and closes the menu.
                if *open {
                    this.open_space_menu(window, cx);
                } else {
                    this.close_space_menu(window, cx);
                }
            }))
            .trigger(
                Button::new("space-switcher-trigger")
                    .ghost()
                    .small()
                    .dropdown_caret(true)
                    .max_w(px(200.))
                    .label(active.clone())
                    .tooltip("Switch space · s in navigation mode"),
            )
            .content(move |_, _, _| match &menu {
                Some(menu) => menu.clone().into_any_element(),
                None => div().into_any_element(),
            })
    }

    /// Build the switcher's menu for the current catalog and show it. The
    /// menu is created per opening (catalog edits and space switches are
    /// reflected immediately) and owned by the workspace, so no window-keyed
    /// element state is involved.
    fn open_space_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_spaces();
        let names = self.spaces.names();
        let active = self.active_space.clone();
        let workspace = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu.min_w(px(200.));
            for name in names {
                let selected = space_eq(&name, &active);
                let workspace = workspace.clone();
                menu = menu.item(PopupMenuItem::new(name.clone()).checked(selected).on_click(
                    move |_, window, cx| {
                        let _ = workspace
                            .update(cx, |this, cx| this.switch_space(name.clone(), window, cx));
                    },
                ));
            }
            menu
        });
        // Dismissing the menu (Escape, an item pick) closes the popover too.
        let this = cx.entity().downgrade();
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                let _ = this.update(cx, |this, cx| this.close_space_menu(window, cx));
            })
            .detach();
        // The popover focuses its own handle; the menu needs focus for its
        // arrow keys and Enter to work, like the built-in dropdown.
        self.space_menu_focus = Some(menu.focus_handle(cx));
        menu.focus_handle(cx).focus(window, cx);
        self.space_menu = Some(menu);
        self.space_menu_open = true;
        cx.notify();
    }

    /// Hide the switcher and drop its menu. When the menu itself holds
    /// focus, focus moves back to Home first: leaving it on a dropped
    /// handle would swallow every following keystroke.
    fn close_space_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.space_menu_open && self.space_menu.is_none() {
            return;
        }
        let menu_focused = self
            .space_menu_focus
            .as_ref()
            .is_some_and(|handle| handle.contains_focused(window, cx));
        self.space_menu = None;
        self.space_menu_focus = None;
        self.space_menu_open = false;
        if menu_focused {
            self.home.read(cx).focus_handle.clone().focus(window, cx);
        }
        cx.notify();
    }

    fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.relationships_visible = false;
        self.relationships
            .update(cx, |view, cx| view.set_active(false, cx));
        self.close_navigation(cx);
        self.artifacts_origin = None;
        self.resources
            .update(cx, |view, cx| view.set_active(false, cx));
        self.home_visible = true;
        self.command_open = false;
        self.sync_tutorial_palette(cx);
        self.home.update(cx, |view, cx| view.activate(cx));
        self.refresh_sessions(cx);
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Jump to Home's artifact browser (the command palette's
    /// **Browse artifacts** entry). The global view includes unassociated legacy artifacts.
    /// Deactivates the repository resources view so its poll stops
    /// while Home is visible. Captures the origin so the titlebar back
    /// button returns where the user came from.
    fn browse_artifacts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.relationships_visible = false;
        self.relationships
            .update(cx, |view, cx| view.set_active(false, cx));
        self.close_navigation(cx);
        let already_there = self.home_visible && self.home.read(cx).is_artifacts_page();
        if !already_there {
            let origin = if !self.home_visible {
                PageOrigin::Repository
            } else {
                PageOrigin::Home
            };
            self.artifacts_origin = Some(origin);
        }
        self.resources
            .update(cx, |view, cx| view.set_active(false, cx));
        self.home_visible = true;
        self.command_open = false;
        self.sync_tutorial_palette(cx);
        self.home
            .update(cx, |view, cx| view.show_artifacts_page(cx));
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    pub(crate) fn open_reference(
        &mut self,
        reference: crate::markdown_references::Reference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::markdown_references::Reference;
        match reference {
            Reference::Repository(key) => self.switch_repository(&key, &key, window, cx),
            Reference::Artifact(id) => {
                match self.home.update(cx, |home, cx| home.open_artifact(id, cx)) {
                    Ok(()) => self.browse_artifacts(window, cx),
                    Err(error) => window.push_notification(error.to_string(), cx),
                }
            }
            reference @ Reference::Session { .. } => {
                let Some(root) = self.data_root.clone() else {
                    window.push_notification("Portable data is unavailable", cx);
                    return;
                };
                self.session_navigation = self.session_navigation.wrapping_add(1);
                let generation = self.session_navigation;
                let catalog = self.session_catalog.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            catalog.refresh();
                            reference.resolve_session(&root, &catalog.snapshot().sessions)
                        })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        if this.session_navigation != generation {
                            return;
                        }
                        match result {
                            Ok(key) => this.open_agent_session(key, window, cx),
                            Err(error) => window.push_notification(
                                format!("Could not open session reference: {error}"),
                                cx,
                            ),
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// Return from the global artifact browser to its captured origin.
    fn go_back_from_artifacts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_navigation(cx);
        match self.artifacts_origin.take() {
            Some(PageOrigin::Repository) => {
                self.enter_repository(window, cx);
                self.focus_active_pane(window, cx);
                cx.notify();
            }
            Some(PageOrigin::Home) | None => {
                self.go_home(window, cx);
            }
        }
    }

    fn artifacts_back_label(&self) -> SharedString {
        match self.artifacts_origin {
            Some(PageOrigin::Repository) => format!("‹ {}", self.project_name).into(),
            Some(PageOrigin::Home) | None => "‹ Home".into(),
        }
    }

    /// Apply a Settings > Agent sidebar limit live: re-project the
    /// in-memory snapshot so the sidebar updates immediately. The caller
    /// owns persistence; values snap to the slider step for defense in depth.
    pub(crate) fn set_session_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        use crate::agent_sessions::snap_sidebar_limit;
        let limit = snap_sidebar_limit(limit);
        if self.session_limit == limit {
            return;
        }
        self.session_limit = limit;
        self.publish_sessions(cx);
    }

    /// Apply a Settings > Agent enable set live: fresh Agent panes and the
    /// New-session picker only offer these afterwards. The caller owns
    /// persistence; open sessions keep running. Never ends empty — an empty
    /// update falls back to all harnesses — and the default always stays
    /// within the enabled set.
    pub(crate) fn set_enabled_agents(
        &mut self,
        enabled: Vec<AgentKind>,
        default: AgentKind,
        cx: &mut Context<Self>,
    ) {
        let mut enabled = enabled;
        if enabled.is_empty() {
            enabled = AgentKind::ALL.to_vec();
        }
        let mut ordered: Vec<AgentKind> = AgentKind::ALL
            .into_iter()
            .filter(|agent| enabled.contains(agent))
            .collect();
        if ordered.is_empty() {
            ordered = AgentKind::ALL.to_vec();
        }
        let default = if ordered.contains(&default) {
            default
        } else {
            ordered.first().copied().unwrap_or(AgentKind::DEFAULT)
        };
        let changed = self.enabled_agents != ordered || self.default_agent != default;
        self.enabled_agents = ordered;
        self.default_agent = default;
        if !self.enabled_agents.contains(&self.session_agent) {
            // Open panes keep running; only future fresh panes use the
            // default. No session_agent rewrite needed.
        }
        if changed {
            cx.notify();
        }
    }

    /// Apply a Settings > Agent default harness live: fresh Agent panes and
    /// the New-session picker pick it up immediately. The caller owns
    /// persistence; open sessions keep running with the harness they
    /// started with. Disabled harnesses are ignored so the default never
    /// leaves the enabled set.
    pub(crate) fn set_default_agent(&mut self, agent: AgentKind, cx: &mut Context<Self>) {
        if !self.enabled_agents.contains(&agent) {
            return;
        }
        if self.default_agent == agent {
            return;
        }
        self.default_agent = agent;
        cx.notify();
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
        self.relationships.update(cx, |view, cx| view.refresh(cx));
        if !self.home_visible && self.active_tab == WorkspaceTab::Resources {
            self.resources.update(cx, |view, cx| view.refresh(cx));
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
                    space_eq(&repository.space, &self.active_space)
                        && activity
                            .for_checkout(&repository.checkout_path)
                            .is_some_and(|agent| agent.state == ActivityState::NeedsAttention)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            self.recent_repositories
                .iter()
                .filter(|repository| space_eq(&repository.space, &self.active_space))
                .cloned()
                .collect::<Vec<_>>()
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
                                PaletteItem::Command(PaletteCommand::GoResources) => {
                                    CommandItem::new()
                                        .label(item.label())
                                        .icon(palette_icon(PaletteCommand::GoResources))
                                        .action(Box::new(GoToResources))
                                }
                                PaletteItem::Command(command) => CommandItem::new()
                                    .label(item.label())
                                    .icon(palette_icon(*command)),
                                PaletteItem::Tool(tool) => {
                                    CommandItem::new().label(item.label()).icon(tool.icon())
                                }
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
                                    // Custom content on purpose: the colored provider/state
                                    // text, the working spinner and the current dot are this
                                    // palette's live status presentation. gpui-kit's command
                                    // row cache rejects any model containing custom content
                                    // (`same_layout`), so the projects palette keeps
                                    // remeasuring; that is the accepted trade for the status
                                    // display.
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
                                    // Custom child for the same live status as the switch
                                    // targets above.
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
        if self.relationships_visible {
            return self.relationships.clone().into_any_element();
        }
        if self.home_visible {
            return self.home.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Resources {
            return self.resources.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Review {
            return self.review.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Agent {
            return self.render_agent_sessions(cx);
        }
        if self.active_tab == WorkspaceTab::Editor
            && self.editor_instance == Some(EditorChoice::BuiltIn)
        {
            return self
                .native_editor
                .as_ref()
                .map(|editor| editor.clone().into_any_element())
                .unwrap_or_else(|| div().size_full().into_any_element());
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

    fn navigation_context(&self, cx: &App) -> NavigationContext {
        if self.relationships_visible {
            return NavigationContext::Relationships;
        }
        if self.home_visible {
            if self.home.read(cx).is_artifacts_page() {
                NavigationContext::Artifacts
            } else {
                NavigationContext::Home
            }
        } else {
            NavigationContext::Workspace
        }
    }

    fn navigation_resource_state(&self, cx: &App) -> navigation::ResourceState {
        if self.relationships_visible {
            return navigation::ResourceState::default();
        }
        if self.home_visible && self.home.read(cx).is_artifacts_page() {
            self.home.read(cx).artifacts_navigation_state(cx)
        } else if !self.home_visible && self.active_tab == WorkspaceTab::Resources {
            self.resources.read(cx).navigation_state()
        } else {
            navigation::ResourceState {
                built_in_editor: self.editor_instance == Some(EditorChoice::BuiltIn),
                ..Default::default()
            }
        }
    }

    fn navigation_panes(&self, cx: &App) -> Vec<(&'static str, FocusHandle)> {
        if self.relationships_visible {
            return vec![(
                "Relationships",
                self.relationships.read(cx).focus_handle.clone(),
            )];
        }
        if self.home_visible {
            return if self.home.read(cx).is_artifacts_page() {
                self.home.read(cx).artifacts_navigation_panes(cx)
            } else {
                vec![("Home", self.home.read(cx).focus_handle.clone())]
            };
        }
        match self.active_tab {
            WorkspaceTab::Agent => {
                let mut panes = vec![("Sessions", self.agent_sidebar_focus.clone())];
                if let Some(Some(pane)) = self.tabs.get(WorkspaceTab::Agent as usize) {
                    panes.push(("Agent", pane.read(cx).focus_handle.clone()));
                }
                panes
            }
            WorkspaceTab::Resources => self.resources.read(cx).navigation_panes(cx),
            WorkspaceTab::Review => self.review.read(cx).navigation_panes(cx),
            WorkspaceTab::Editor if self.editor_instance == Some(EditorChoice::BuiltIn) => self
                .native_editor
                .as_ref()
                .map(|editor| vec![("Editor", editor.read(cx).editor_focus(cx))])
                .unwrap_or_default(),
            WorkspaceTab::Editor | WorkspaceTab::Terminal => self
                .tabs
                .get(self.active_tab as usize)
                .and_then(|pane| pane.as_ref())
                .map(|pane| vec![(self.active_tab.label(), pane.read(cx).focus_handle.clone())])
                .unwrap_or_default(),
            // Dialog-only surface, never selected as the active workspace tab.
            WorkspaceTab::Git => Vec::new(),
        }
    }

    fn open_navigation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Dialog and sheet layers own their own keyboard traps. Do not open a
        // workspace HUD behind either, and close any stale HUD before they can
        // receive a keystroke.
        if window.has_active_dialog(cx) || window.has_active_sheet(cx) || self.command_open {
            self.close_navigation(cx);
            return;
        }
        let panes = self.navigation_panes(cx);
        self.navigation_pane = panes
            .iter()
            // Detail containers contain their comments/outline descendants;
            // prefer the deepest rendered target so reopening the HUD names
            // the region the pointer or keyboard actually left focused.
            .rposition(|(_, focus)| focus.contains_focused(window, cx))
            .unwrap_or(0);
        self.navigation_open = true;
        self.sync_tutorial_navigation(cx);
        // Start item cursors where `j`/`k` should repeat from: the active
        // session in the sidebar, the first Home card on the dashboard.
        // Artifact and review lists reuse their existing selection.
        if self.home_visible
            && !self.relationships_visible
            && !self.home.read(cx).is_artifacts_page()
        {
            self.home
                .update(cx, |view, cx| view.set_navigation_active(true, cx));
            self.session_cursor = None;
        } else if !self.home_visible
            && self.active_tab == WorkspaceTab::Agent
            && self.navigation_pane == 0
        {
            let agent_index = WorkspaceTab::Agent as usize;
            let active = self.tabs[agent_index]
                .as_ref()
                .and_then(|pane| pane.read(cx).launch_id());
            let active_key = active
                .and_then(|id| self.open_sessions.get(&id))
                .and_then(|session| session.key.clone());
            self.init_session_cursor(active, active_key);
            self.home
                .update(cx, |view, cx| view.set_navigation_active(false, cx));
        } else {
            self.session_cursor = None;
            self.home
                .update(cx, |view, cx| view.set_navigation_active(false, cx));
        }
        cx.notify();
    }

    fn close_navigation(&mut self, cx: &mut Context<Self>) {
        let was_open = self.navigation_open;
        if was_open {
            self.navigation_open = false;
            self.sync_tutorial_navigation(cx);
        }
        if self.session_cursor.is_some() {
            self.session_cursor = None;
        }
        // Clearing the Home highlight is cheap and keeps a stale cursor from
        // reappearing on the next visit; `notify` only when something changed.
        let home_cleared = self.home.update(cx, |view, cx| {
            if view.take_navigation_cursor().is_some() {
                cx.notify();
                true
            } else {
                false
            }
        });
        if was_open || home_cleared {
            cx.notify();
        }
    }

    fn move_navigation_focus(&mut self, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.navigation_panes(cx);
        self.navigation_pane = navigation::move_index(self.navigation_pane, panes.len(), right);
        if let Some((_, focus)) = panes.get(self.navigation_pane) {
            focus.focus(window, cx);
        }
        if !self.home_visible && self.active_tab == WorkspaceTab::Review {
            self.review.update(cx, |review, cx| {
                review.focus_navigation_pane(self.navigation_pane, window, cx)
            });
        }
        cx.notify();
    }

    /// Navigation-mode `Tab`/`Shift+Tab`: move keyboard focus to the next or
    /// previous focusable component and keep the mode open so traversal can
    /// repeat, like `h`/`l`. This is the workspace's only component
    /// traversal: outside navigation mode the key belongs to the focused
    /// component (see `WORKSPACE_KEY_CONTEXT`), so terminals forward it to
    /// the pty and inputs keep it.
    fn move_navigation_component_focus(
        &mut self,
        next: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if next {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        // Traversal can cross a pane boundary (or leave every pane for a
        // titlebar control). Re-anchor the HUD's pane label — and `h`/`l`'s
        // starting point — whenever focus landed inside a pane.
        let panes = self.navigation_panes(cx);
        if let Some(index) = panes
            .iter()
            .rposition(|(_, focus)| focus.contains_focused(window, cx))
        {
            self.navigation_pane = index;
        }
        cx.notify();
    }

    /// Move the item cursor within the focused pane for navigation-mode
    /// `j`/`k`. Keeps the mode open for repeats; locations without a list
    /// consume the key and stay open. Clamps at the ends like pane movement.
    fn move_navigation_item(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.relationships_visible {
            return;
        }
        if self.home_visible {
            if self.home.read(cx).is_artifacts_page() {
                self.home
                    .update(cx, |view, cx| view.artifacts_move_selection(down, cx));
            } else {
                self.home
                    .update(cx, |view, cx| view.move_navigation_cursor(down, cx));
            }
        } else {
            match self.active_tab {
                WorkspaceTab::Agent => {
                    if self.navigation_pane == 0 {
                        self.move_session_cursor(down);
                    }
                }
                WorkspaceTab::Resources => {
                    if self.navigation_pane == 0 {
                        self.resources
                            .update(cx, |view, cx| view.move_selection(down, cx));
                    }
                }
                WorkspaceTab::Review => match self.navigation_pane {
                    0 => self
                        .review
                        .update(cx, |view, cx| view.move_file_selection(down, cx)),
                    1 => self
                        .review
                        .update(cx, |view, cx| view.move_diff_scroll(down, cx)),
                    _ => {}
                },
                WorkspaceTab::Editor | WorkspaceTab::Terminal | WorkspaceTab::Git => {}
            }
        }
        cx.notify();
    }

    /// Navigation-mode `Enter`: open the `j`/`k` cursor item when one is
    /// active (sessions sidebar, Home cards), otherwise keep the focused pane
    /// like before. Runs once and returns to normal mode either way.
    fn accept_navigation_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.home_visible
            && self.active_tab == WorkspaceTab::Agent
            && self.navigation_pane == 0
            && self.session_cursor.is_some()
            && self.activate_session_cursor(window, cx)
        {
            return;
        }
        if self.home_visible
            && !self.relationships_visible
            && !self.home.read(cx).is_artifacts_page()
        {
            let activated = self
                .home
                .update(cx, |view, cx| view.activate_navigation_cursor(window, cx));
            if activated {
                self.close_navigation(cx);
                return;
            }
        }
        self.close_navigation(cx);
    }

    fn run_navigation_command(
        &mut self,
        command: NavigationCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Availability was checked to render the HUD, but actions validate it
        // again here so a refresh/save cannot make a stale row unsafe.
        // Close through the shared path so item cursors clear with the mode.
        self.close_navigation(cx);
        match command {
            NavigationCommand::AddRepository => self.open_add_repository(window, cx),
            NavigationCommand::BrowseArtifacts => self.browse_artifacts(window, cx),
            NavigationCommand::SwitchSpace => self.toggle_space_menu(window, cx),
            NavigationCommand::Agent => self.select_tab(WorkspaceTab::Agent as usize, window, cx),
            NavigationCommand::Editor => self.select_tab(WorkspaceTab::Editor as usize, window, cx),
            NavigationCommand::Terminal => {
                self.select_tab(WorkspaceTab::Terminal as usize, window, cx)
            }
            NavigationCommand::Review => self.select_tab(WorkspaceTab::Review as usize, window, cx),
            NavigationCommand::Resources => {
                self.select_tab(WorkspaceTab::Resources as usize, window, cx)
            }
            NavigationCommand::GitChanges => self.open_git_changes(window, cx),
            NavigationCommand::Home => self.go_home(window, cx),
            NavigationCommand::Back => {
                if self.relationships_visible {
                    self.back_from_relationships(window, cx);
                    return;
                }
                if self.home_visible
                    && (self.home.read(cx).is_projects_page()
                        || self.home.read(cx).is_pull_requests_page()
                        || self.home.read(cx).is_todos_page()
                        || self.home.read(cx).is_reading_page())
                {
                    self.go_home(window, cx);
                } else {
                    self.go_back_from_artifacts(window, cx);
                }
            }
            NavigationCommand::NewSession => self.prompt_new_agent_session(window, cx),
            NavigationCommand::OpenFile => {
                if self.editor_instance == Some(EditorChoice::BuiltIn) {
                    self.select_tab(WorkspaceTab::Editor as usize, window, cx);
                    if let Some(editor) = self.native_editor.as_ref() {
                        editor.update(cx, |editor, cx| editor.open_file_finder(window, cx));
                    }
                }
            }
            command @ (NavigationCommand::EditMarkdown
            | NavigationCommand::AddComment
            | NavigationCommand::SaveDraft
            | NavigationCommand::CancelDraft) => {
                if self.home_visible && self.home.read(cx).is_artifacts_page() {
                    self.home.update(cx, |view, cx| {
                        view.run_artifact_command(command, window, cx)
                    });
                } else if !self.home_visible && self.active_tab == WorkspaceTab::Resources {
                    self.resources.update(cx, |view, cx| match command {
                        NavigationCommand::EditMarkdown => view.begin_markdown_edit(window, cx),
                        NavigationCommand::AddComment => view.begin_comment(window, cx),
                        NavigationCommand::SaveDraft => view.save_draft(window, cx),
                        NavigationCommand::CancelDraft => view.cancel_draft(window, cx),
                        _ => unreachable!(),
                    });
                }
                cx.notify();
            }
        }
    }

    /// Whether navigation-mode `j`/`k` moves something at the current
    /// location: Home cards, the Artifacts list, the sessions sidebar, the
    /// Resources sidebar, or Review files/diff. Other panes consume the keys
    /// and stay open.
    fn navigation_item_available(&self, cx: &App) -> bool {
        if self.relationships_visible {
            return false;
        }
        if self.home_visible {
            if self.home.read(cx).is_artifacts_page() {
                return true;
            }
            return self.home.read(cx).has_navigation_targets();
        }
        match self.active_tab {
            WorkspaceTab::Agent => {
                self.navigation_pane == 0 && !self.session_nav_order().is_empty()
            }
            WorkspaceTab::Resources => self.navigation_pane == 0,
            WorkspaceTab::Review => matches!(self.navigation_pane, 0 | 1),
            WorkspaceTab::Editor | WorkspaceTab::Terminal | WorkspaceTab::Git => false,
        }
    }

    /// Whether navigation-mode `Enter` opens the `j`/`k` cursor item instead
    /// of only keeping the focused pane: the sessions sidebar and Home cards.
    /// Artifact and review lists apply their selection live, so `Enter` there
    /// only keeps focus.
    fn navigation_enter_opens(&self, cx: &App) -> bool {
        if self.relationships_visible {
            return false;
        }
        if !self.home_visible
            && self.active_tab == WorkspaceTab::Agent
            && self.navigation_pane == 0
            && self.session_cursor.is_some()
        {
            return true;
        }
        if self.home_visible
            && !self.relationships_visible
            && !self.home.read(cx).is_artifacts_page()
        {
            return self.home.read(cx).navigation_cursor_active();
        }
        false
    }

    /// The mode toggle: `cmd-j` on macOS, `ctrl-j` on Linux/Windows.
    /// (`cmd-m`/`super-m` previously collided with system Minimize —
    /// `Cmd+M` on macOS, `Win+M` minimize-all on Windows — so the toggle
    /// moved to `J`, which has no OS reservation. `J` also echoes the
    /// in-mode movement keys `j`/`k`, while the required modifier keeps it
    /// distinct from plain-`j` move-down.) Plain `m` stays free as the Edit
    /// Markdown action key, `cmd-m` returns to the system (Minimize on
    /// macOS), and `ctrl-m` returns to terminals as an Enter alias.
    pub(crate) fn is_trigger(keystroke: &gpui_kit::Keystroke) -> bool {
        let modifiers = &keystroke.modifiers;
        keystroke.key.eq_ignore_ascii_case("j")
            && is_primary_modifier(modifiers.platform, modifiers.control)
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.function
    }

    fn navigation_input(keystroke: &gpui_kit::Keystroke) -> NavigationInput {
        let modifiers = &keystroke.modifiers;
        let key = keystroke.key.to_ascii_lowercase();
        let trigger = Self::is_trigger(keystroke);
        if trigger {
            return NavigationInput::Trigger;
        }
        // Tab/Shift+Tab are the mode's component traversal (see
        // `move_navigation_component_focus`); they are the only modified
        // keystrokes the mode acts on, every other one stays consumed below.
        if key == "tab" {
            let shift_only = modifiers.shift
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.platform
                && !modifiers.function;
            return if shift_only {
                NavigationInput::BackTab
            } else if modifiers.modified() {
                NavigationInput::Modified
            } else {
                NavigationInput::Tab
            };
        }
        if modifiers.modified() {
            return NavigationInput::Modified;
        }
        match key.as_str() {
            "escape" => NavigationInput::Escape,
            "enter" => NavigationInput::Enter,
            // GPUI reports Space by name rather than as a one-character key.
            "space" => NavigationInput::Key(' '),
            "h" | "left" | "arrowleft" => NavigationInput::Left,
            "l" | "right" | "arrowright" => NavigationInput::Right,
            "j" | "down" | "arrowdown" => NavigationInput::Down,
            "k" | "up" | "arrowup" => NavigationInput::Up,
            _ => {
                let mut chars = keystroke.key.chars();
                match (chars.next(), chars.next()) {
                    (Some(key), None) => NavigationInput::Key(key),
                    _ => NavigationInput::Modified,
                }
            }
        }
    }

    fn on_navigation_keystroke(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Quit stays live everywhere, including navigation mode and held-key
        // repeats: close the HUD (if open) and let the keystroke continue to
        // normal dispatch, which quits via the global OS-primary binding.
        // Must run before the claimed-key rearm below, or repeats would stay
        // swallowed.
        if is_quit_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            event.keystroke.modifiers.shift,
            event.keystroke.modifiers.alt,
        ) {
            if self.navigation_open {
                self.close_navigation(cx);
            }
            return;
        }
        let key = event.keystroke.key.to_ascii_lowercase();
        // `KeystrokeEvent` intentionally omits raw `is_held`; retain each
        // claimed key until its real KeyUp event so physical repeats cannot
        // toggle a newly changed navigation state or leak into an overlay.
        // Escape hatch for a lost key-up: if any modifier moved since the
        // claim, the previous hold provably ended, so a re-pressed trigger
        // is honored as fresh instead of staying wedged. Trigger-only, and
        // only with modifier movement — a genuine held repeat (nothing
        // moved) stays swallowed, as does any marked non-trigger key.
        if self.navigation_claimed_keys.contains_key(&key) {
            let rearm = self.navigation_claimed_keys[&key] && Self::is_trigger(&event.keystroke);
            if !rearm {
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
            self.navigation_claimed_keys.remove(&key);
        }
        // A dialog/sheet is rendered above the workspace and must retain its
        // own keyboard trap. It can have appeared through another workflow
        // while the HUD was open, so clear the stale mode before considering
        // the current key and let that key reach the overlay.
        if window.has_active_dialog(cx) || window.has_active_sheet(cx) {
            self.close_navigation(cx);
            // The Git dialog lets this trigger reach its terminal pane to
            // dismiss it. Claim the chord so held repeats cannot reopen the
            // HUD after the dialog closes; leave ordinary dialog typing alone.
            if Self::is_trigger(&event.keystroke) {
                self.navigation_claimed_keys.insert(key, false);
            }
            return;
        }
        // The two palette shortcuts stay live inside navigation mode (quit is
        // handled above and also stays live):
        // opening another overlay clears the mode (same rule as dialogs
        // above) and the keystroke continues to normal dispatch, which
        // opens the palette. Every other modified keystroke is still
        // consumed below, so nothing can edit while navigating.
        if self.navigation_open
            && palette_mode_for_shortcut(
                &event.keystroke.key,
                event.keystroke.modifiers.platform,
                event.keystroke.modifiers.control,
                event.keystroke.modifiers.alt,
            )
            .is_some()
        {
            self.close_navigation(cx);
            return;
        }
        let input = Self::navigation_input(&event.keystroke);
        let decision = navigation::decide(
            self.navigation_open,
            input,
            self.navigation_context(cx),
            self.navigation_resource_state(cx),
        );
        if decision == NavigationDecision::Ignore {
            return;
        }
        self.navigation_claimed_keys.insert(key, false);
        window.prevent_default();
        cx.stop_propagation();
        match decision {
            NavigationDecision::Open => self.open_navigation(window, cx),
            NavigationDecision::Close => self.close_navigation(cx),
            NavigationDecision::AcceptFocus => self.accept_navigation_focus(window, cx),
            NavigationDecision::FocusLeft => self.move_navigation_focus(false, window, cx),
            NavigationDecision::FocusRight => self.move_navigation_focus(true, window, cx),
            NavigationDecision::FocusNext => self.move_navigation_component_focus(true, window, cx),
            NavigationDecision::FocusPrevious => {
                self.move_navigation_component_focus(false, window, cx)
            }
            NavigationDecision::PrevItem => self.move_navigation_item(false, cx),
            NavigationDecision::NextItem => self.move_navigation_item(true, cx),
            NavigationDecision::Execute(command) => {
                self.run_navigation_command(command, window, cx)
            }
            NavigationDecision::Consume | NavigationDecision::Ignore => {}
        }
    }

    fn render_navigation_hud(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = navigation::rows(
            self.navigation_context(cx),
            self.navigation_resource_state(cx),
        );
        let panes = self.navigation_panes(cx);
        let pane = panes
            .get(self.navigation_pane)
            .map(|(label, _)| *label)
            .unwrap_or("workspace");
        let context_title = match self.navigation_context(cx) {
            NavigationContext::Home => "Home",
            NavigationContext::Artifacts => "Artifacts",
            NavigationContext::Relationships => "Relationships",
            NavigationContext::Workspace => self.active_tab.label(),
        };
        let viewport = window.viewport_size();
        let hud_width = (f32::from(viewport.width) - 32.).clamp(200., 330.);
        let hud_height = (f32::from(viewport.height) - WORKSPACE_HEADER_HEIGHT - 36.).max(120.);
        let mut commands = v_flex().gap_1();
        let mut previous = "";
        for row in rows {
            if row.group != previous {
                previous = row.group;
                commands = commands.child(
                    div()
                        .pt_2()
                        .text_xs()
                        .text_color(rgb(0x858989))
                        .child(row.group),
                );
            }
            commands = commands.child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(20.))
                            .text_sm()
                            .font_semibold()
                            .text_color(rgb(0x60a5fa))
                            .child(row.key_label()),
                    )
                    .child(div().text_sm().child(row.label)),
            );
        }
        div()
            .absolute()
            .bottom(px(16.))
            .right(px(16.))
            .child(
                v_flex()
                    .w(px(hud_width))
                    .max_h(px(hud_height))
                    .overflow_y_scrollbar()
                    .p_4()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x3a3d3d))
                    .bg(rgb(0x151717))
                    .shadow_lg()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(format!("Navigation · {context_title}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x858989))
                                    .child(format!("Pane: {pane}")),
                            ),
                    )
                    .child(commands)
                    .child({
                        let item = self.navigation_item_available(cx);
                        let enter = if self.navigation_enter_opens(cx) {
                            "Enter open"
                        } else {
                            "Enter keep focus"
                        };
                        let toggle = if cfg!(target_os = "macos") {
                            "⌘J"
                        } else {
                            "Ctrl+J"
                        };
                        let hint = match (panes.len() > 1, item) {
                            (true, true) => format!(
                                "Tab focus · h/l pane · j/k move · {enter} · Esc/{toggle} close"
                            ),
                            (true, false) => {
                                format!("Tab focus · h/l pane · {enter} · Esc/{toggle} close")
                            }
                            (false, true) => {
                                format!("Tab focus · j/k move · {enter} · Esc/{toggle} close")
                            }
                            (false, false) => format!("Tab focus · Esc/{toggle} close"),
                        };
                        div()
                            .pt_2()
                            .border_t_1()
                            .border_color(rgb(0x292b2b))
                            .text_xs()
                            .text_color(rgb(0x858989))
                            .child(hint)
                    }),
            )
            .into_any_element()
    }

    /// Reserve room for the active label so switching modes never shifts search.
    /// Display-only on purpose — pointer presses dismiss the mode through
    /// the root capture handler, so a click-to-toggle here would race that
    /// dismissal. The hint stays the way in and out.
    fn render_navigation_indicator(&self) -> AnyElement {
        h_flex()
            .flex_none()
            .w(px(104.))
            .items_center()
            // The wider palette covers part of this slot. Keep its layout
            // space while hiding the whole badge, including its border.
            .when(self.command_open, |indicator| indicator.invisible())
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .px_2()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(0x252828))
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .when(self.navigation_open, |this| {
                        this.bg(rgb(0x211609))
                            .border_color(rgb(0x713f12))
                            .text_color(rgb(0xf59e0b))
                    })
                    .child(Icon::new(gpui_kit::assets::IconName::Keyboard).size(px(14.)))
                    .child(if self.navigation_open {
                        "Navigation"
                    } else if cfg!(target_os = "macos") {
                        "⌘J"
                    } else {
                        "Ctrl+J"
                    }),
            )
            .into_any_element()
    }

    /// Branch pill with a commit icon for detached HEADs. Long names truncate
    /// instead of pushing the command bar aside.
    fn render_branch_pill(&self, branch: SharedString) -> impl IntoElement {
        let icon = if self.git_poll.status().detached {
            gpui_kit::assets::IconName::GitCommitHorizontal
        } else {
            gpui_kit::assets::IconName::GitBranch
        };
        h_flex()
            .min_w_0()
            .gap_1()
            .items_center()
            .h(px(22.))
            .px_2()
            .rounded_md()
            .bg(rgb(0x171919))
            .text_xs()
            .child(
                Icon::new(icon)
                    .size(px(12.))
                    .flex_none()
                    .text_color(rgb(0x858989)),
            )
            .child(
                div()
                    .min_w_0()
                    .max_w(px(120.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(0x858989))
                    .child(branch),
            )
    }
}

/// Leading glyph per palette command. A command with a direct keybinding
/// additionally carries its real GPUI `Action` on the row via
/// `CommandItem::action`, which renders the binding hint for free; `on_confirm` still resolves it afterwards, idempotently.
fn palette_icon(command: PaletteCommand) -> IconName {
    match command {
        PaletteCommand::GoAgent => IconName::Bot,
        PaletteCommand::GoEditor => IconName::FileText,
        PaletteCommand::GoTerminal => IconName::SquareTerminal,
        PaletteCommand::GoReview => IconName::Eye,
        PaletteCommand::GoResources => IconName::CircleCheck,
        PaletteCommand::GoHome => IconName::LayoutDashboard,
        PaletteCommand::BrowseArtifacts => IconName::BookOpen,
        PaletteCommand::RepositoryRelationships => IconName::LayoutDashboard,
        PaletteCommand::AddRepository => IconName::Plus,
        PaletteCommand::OpenSettings => IconName::Settings,
        PaletteCommand::EditorGoBack => IconName::ArrowLeft,
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
        self.sync_activity_visibility(window, cx);
        let active_index = self.active_tab as usize;
        let active_content = self.render_active_content(cx);
        // Resources and Artifacts full-pages share the repository workspace
        // chrome: title and back navigation live here, not in a second row.
        // The archived filter also lives here; the portable git status is
        // hidden on these pages and the lists stay live through their active
        // polls.
        let is_artifacts = self.home_visible
            && !self.relationships_visible
            && self.home.read(cx).is_artifacts_page();
        let is_projects = self.home_visible
            && !self.relationships_visible
            && self.home.read(cx).is_projects_page();
        let show_archived = if is_artifacts {
            self.home.read(cx).artifacts_include_archived(cx)
        } else {
            false
        };
        let artifacts_back = if is_artifacts {
            self.artifacts_back_label()
        } else {
            SharedString::from("")
        };
        let is_pull_requests = self.home_visible
            && !self.relationships_visible
            && self.home.read(cx).is_pull_requests_page();
        let is_todos =
            self.home_visible && !self.relationships_visible && self.home.read(cx).is_todos_page();
        let is_reading = self.home_visible
            && !self.relationships_visible
            && self.home.read(cx).is_reading_page();
        let is_home_page = self.relationships_visible
            || is_artifacts
            || is_projects
            || is_pull_requests
            || is_todos
            || is_reading;
        let attention_count = self.attention_count();
        let attention_label = if attention_count == 1 {
            "⚠ 1 agent needs attention".to_owned()
        } else {
            format!("⚠ {attention_count} agents need attention")
        };
        // Center the field itself, independently of unequal header controls.
        // Retain the flexible layout in narrow windows so the adjacent badges
        // cannot overlap the workspace tabs.
        let command_width = 280.;
        let palette_width = 380.;
        let centered_min_width = if self.home_visible {
            960.
        } else if attention_count > 0 {
            1400.
        } else {
            1280.
        };
        let center_command = window.viewport_size().width >= px(centered_min_width);
        let command_left = (window.viewport_size().width - px(command_width)) / 2.;

        v_flex()
            .relative()
            // The workspace's key context (`bind_app_keys` unbinds
            // Tab/Shift+Tab here): normal mode leaves both keys to the
            // focused component, component traversal lives in navigation
            // mode.
            .key_context(WORKSPACE_KEY_CONTEXT)
            .when(
                self.home_visible || self.active_tab == WorkspaceTab::Resources,
                |this| this.tab_group(),
            )
            .size_full()
            .bg(rgb(0x080909))
            .text_color(rgb(0xe7e7e7))
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                this.close_navigation(cx);
            }))
            // Release bookkeeping for the navigation interceptor's held-key
            // set: an element bubble listener wires up during paint (unlike
            // `window.on_key_event`, which panics outside paint because view
            // render runs in layout). Key-up bubbles from any focused
            // descendant — terminals only handle key-down — so releases are
            // observed window-wide and repeats stay swallowed until then.
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| {
                let key = event.keystroke.key.to_ascii_lowercase();
                this.navigation_claimed_keys.remove(&key);
            }))
            // Redundant re-arm for the held-key set: if a key-up is ever
            // lost by the platform, the modifier release that always
            // follows a modified press (the trigger included) clears the
            // set once navigation mode is closed. Closed-only on purpose:
            // clearing while open would let a still-held key re-fire as a
            // fresh action. While closed the only entry that must survive
            // is the trigger's own flicker guard, and its repeats always
            // carry modifiers — an all-released event can never arrive
            // mid-flicker. Plain action-key repeats involve no modifiers,
            // so they cannot trip this either; a modifier tap mid-hold
            // merely degrades to typing a physically held key.
            //
            // Any modifier movement also marks every claim: a later
            // modifier press proves the previous hold ended, which re-arms
            // a re-pressed trigger (see `on_navigation_keystroke`) even
            // when no all-released event ever arrives.
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, _| {
                if !this.navigation_open && event.modifiers.number_of_modifiers() == 0 {
                    this.navigation_claimed_keys.clear();
                } else {
                    for marked in this.navigation_claimed_keys.values_mut() {
                        *marked = true;
                    }
                }
            }))
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
            .on_action(cx.listener(|this, _: &GoToResources, window, cx| {
                this.go_to_resources(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewAgentSession, window, cx| {
                // A dialog over the open bar would strand the query, so close
                // it first. The workspace entry happens when an agent is
                // picked, not here: cancelling must leave Home untouched.
                if this.command_open {
                    this.close_command_palette(window, cx);
                }
                this.prompt_new_agent_session(window, cx);
            }))
            .child(
                h_flex()
                    .relative()
                    .h(px(WORKSPACE_HEADER_HEIGHT))
                    .flex_none()
                    .px_3()
                    .gap_3()
                    .items_center()
                    .bg(rgb(0x0c0d0d))
                    .border_b_1()
                    .border_color(rgb(0x222525))
                    .when(self.home_visible && !is_home_page, |header| {
                        header.child(div().text_sm().font_semibold().child("Devcroft"))
                    })
                    .when(self.home_visible && !self.relationships_visible, |header| {
                        header.child(self.space_switcher(cx))
                    })
                    .when(self.relationships_visible, |header| {
                        header.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("relationships-back")
                                        .ghost()
                                        .small()
                                        .label("‹ Back")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.back_from_relationships(window, cx)
                                        })),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child("Repository Relationships"),
                                ),
                        )
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
                                        .small()
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
                    .when(
                        is_projects || is_pull_requests || is_todos || is_reading,
                        |header| {
                            header.child(
                                h_flex()
                                    .flex_none()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Button::new("projects-back")
                                            .ghost()
                                            .small()
                                            .label("‹ Home")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.go_home(window, cx)
                                            })),
                                    )
                                    .child(div().text_sm().font_semibold().child(
                                        SharedString::from(if is_pull_requests {
                                            "Pull Requests"
                                        } else if is_todos {
                                            "Todos"
                                        } else if is_reading {
                                            "To Read"
                                        } else {
                                            "Projects"
                                        }),
                                    )),
                            )
                        },
                    )
                    .when(!self.home_visible, |header| {
                        header.child(
                            h_flex()
                                .min_w_0()
                                .max_w(px(360.))
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("go-home")
                                        .ghost()
                                        .small()
                                        .size(px(28.))
                                        .icon(IconName::ChevronLeft)
                                        .accessibility_label("Home")
                                        .tooltip("Back to Home")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.go_home(window, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .max_w(px(160.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
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
                    .when(center_command, |header| header.child(div().flex_1()))
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .when(center_command, |bar| {
                                bar.absolute().top_0().h_full().left(command_left)
                            })
                            .when(!center_command, |bar| bar.flex_1().justify_center())
                            .gap_1()
                            .child(
                                div()
                                    .relative()
                                    .w(px(command_width))
                                    .min_w(px(140.))
                                    .when(center_command, |field| field.flex_none())
                                    .child(
                                        Button::new("workspace-command-trigger")
                                            .ghost()
                                            .small()
                                            .accessibility_label("Open command palette")
                                            .tooltip(if cfg!(target_os = "macos") {
                                                "Commands ⌘K · Projects ⌘P"
                                            } else {
                                                "Commands Ctrl+K · Projects Ctrl+P"
                                            })
                                            .w_full()
                                            .h(px(28.))
                                            .px_2()
                                            .items_center()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(0x252828))
                                            .bg(rgb(0x121414))
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
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .text_ellipsis()
                                                    .child("Search commands…"),
                                            )
                                            .child(
                                                div()
                                                    .px_1()
                                                    .flex_none()
                                                    .rounded_sm()
                                                    .bg(rgb(0x1d2020))
                                                    .text_xs()
                                                    .text_color(rgb(0x858989))
                                                    .child(if cfg!(target_os = "macos") {
                                                        "⌘K"
                                                    } else {
                                                        "Ctrl+K"
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .px_1()
                                                    .flex_none()
                                                    .rounded_sm()
                                                    .bg(rgb(0x1d2020))
                                                    .text_xs()
                                                    .text_color(rgb(0x858989))
                                                    .child(if cfg!(target_os = "macos") {
                                                        "⌘P"
                                                    } else {
                                                        "Ctrl+P"
                                                    }),
                                            ),
                                    )
                                    // Keep the wider palette on the same center as
                                    // the trigger, leaving room for labels and hints.
                                    .when(self.command_open, |anchor| {
                                        anchor.child(deferred(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .left_0()
                                                .when(center_command, |palette| {
                                                    palette.left(px((command_width
                                                        - palette_width)
                                                        / 2.))
                                                })
                                                .w(px(palette_width))
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
                            .child(self.render_navigation_indicator())
                            .when(attention_count > 0, |bar| {
                                bar.child(
                                    Button::new("agent-attention-trigger")
                                        .ghost()
                                        .small()
                                        .accessibility_label(attention_label.clone())
                                        .tooltip(attention_label.clone())
                                        .h(px(28.))
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x713f12))
                                        .bg(rgb(0x211609))
                                        .text_xs()
                                        .text_color(rgb(0xf59e0b))
                                        .icon(IconName::TriangleAlert)
                                        .label(attention_count.to_string())
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
                                .small()
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
                            TabBar::new("workspace-tabs")
                                .pill()
                                .small()
                                .flex_none()
                                .h(px(28.))
                                .p(px(2.))
                                .rounded_md()
                                .bg(rgb(0x171919))
                                .border_1()
                                .border_color(rgb(0x222525))
                                .selected_index(active_index)
                                .on_click(cx.listener(|this, index: &usize, window, cx| {
                                    this.select_tab(*index, window, cx);
                                }))
                                .children(WorkspaceTab::ALL.into_iter().map(|tab| {
                                    Tab::new()
                                        .label(tab.label())
                                        .when(tab as usize == active_index, |tab| {
                                            tab.font_semibold()
                                        })
                                })),
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
            .when(self.navigation_open, |this| {
                this.child(self.render_navigation_hud(window, cx))
            })
        // Dialogs, sheets, notifications, menus and tooltips are hosted
        // by the component `Root` automatically (registered by
        // `gpui_kit::init`), above this content.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::Root;
    use std::path::PathBuf;

    /// End-to-end toggle lifecycle through the real dispatch path: press the
    /// toggle, hold-repeat it (must not double-toggle), release, toggle off,
    /// release, toggle on again. Guards the reported "triggers once, then
    /// never again" regression.
    #[gpui_kit::test]
    fn navigation_mode_retriggers_after_key_release(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{Capslock, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent};
        use std::cell::RefCell;
        use std::rc::Rc;
        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, directory.path()));
            *holder_for_window.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        let is_open = |test_cx: &mut gpui_kit::VisualTestContext| {
            view.update(test_cx, |view, _| view.navigation_open)
        };
        let claimed = |test_cx: &mut gpui_kit::VisualTestContext| {
            view.update(test_cx, |view, _| {
                view.navigation_claimed_keys
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
            })
        };
        let release = |test_cx: &mut gpui_kit::VisualTestContext, key: &str| {
            test_cx.simulate_event(KeyUpEvent {
                keystroke: Keystroke::parse(key).unwrap(),
            });
        };
        // OS-primary chords under test: `cmd-j`/`cmd-p` on macOS,
        // `ctrl-j`/`ctrl-p` on Linux/Windows.
        const TRIGGER: &str = if cfg!(target_os = "macos") {
            "cmd-j"
        } else {
            "ctrl-j"
        };
        const TRIGGER_KEY: &str = "j";
        const PALETTE: &str = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        const PALETTE_KEY: &str = "p";

        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx), "first toggle opens navigation mode");
        assert!(
            claimed(test_cx).iter().any(|key| key == TRIGGER_KEY),
            "trigger key is held until release"
        );
        // Held repeat before release: swallowed, must not toggle back off.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            is_open(test_cx),
            "held trigger repeat must not double-toggle"
        );
        release(test_cx, TRIGGER_KEY);
        assert!(
            claimed(test_cx).is_empty(),
            "release clears the held-key set"
        );
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(!is_open(test_cx), "second toggle exits navigation mode");
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            is_open(test_cx),
            "third toggle re-opens navigation mode after release"
        );
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes("escape");
        assert!(!is_open(test_cx));
        release(test_cx, "escape");

        // Exit via an overlay (add-repository dialog): closing the dialog
        // must not leave stale state that refuses the next toggle.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx));
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes("a");
        assert!(!is_open(test_cx), "action exits navigation mode");
        release(test_cx, "a");
        // Close the dialog the way a user does: Escape must reach the
        // dialog trap (the interceptor yields to active overlays).
        test_cx.simulate_keystrokes("escape");
        release(test_cx, "escape");
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            is_open(test_cx),
            "toggle re-opens after a dialog opened and closed"
        );
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes("escape");
        release(test_cx, "escape");

        // The palette shortcuts stay live inside navigation mode: they exit
        // to normal mode (in the app the keystroke continues to the
        // palette, which has no binding in this harness).
        test_cx.simulate_keystrokes(TRIGGER);
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes(PALETTE);
        assert!(!is_open(test_cx), "palette shortcut exits navigation mode");
        release(test_cx, PALETTE_KEY);
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx), "toggle re-opens after a palette shortcut");
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes("escape");
        release(test_cx, "escape");

        // Lost key-ups must not wedge the toggle: open and close with no
        // releases at all, then release only the modifiers. The
        // all-released signal re-arms the trigger on its own.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx));
        test_cx.simulate_keystrokes("escape");
        assert!(!is_open(test_cx));
        test_cx.simulate_event(ModifiersChangedEvent {
            modifiers: Modifiers::default(),
            capslock: Capslock { on: false },
        });
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            is_open(test_cx),
            "toggle re-opens after modifier release without key-ups"
        );
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes("escape");
        release(test_cx, "escape");

        // Lost key-ups with no all-released event: modifier movement alone
        // marks the claim, so the next trigger press is honored as fresh.
        // Shift, then Ctrl, then Shift release keeps the modifier set
        // non-empty throughout — only the marks can re-arm here.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx));
        test_cx.simulate_keystrokes("escape");
        assert!(!is_open(test_cx));
        let modifiers = |shift: bool, control: bool| ModifiersChangedEvent {
            modifiers: Modifiers {
                shift,
                control,
                ..Default::default()
            },
            capslock: Capslock { on: false },
        };
        test_cx.simulate_event(modifiers(true, false));
        test_cx.simulate_event(modifiers(true, true));
        test_cx.simulate_event(modifiers(false, true));
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            is_open(test_cx),
            "toggle re-opens after modifier movement without key-ups"
        );
        // Drain the simulated loss: real releases clear both claims, so the
        // next segment starts clean. (A marked non-trigger key stays
        // swallowed by design — a missed press beats an accidental edit.)
        release(test_cx, TRIGGER_KEY);
        release(test_cx, "escape");
        test_cx.simulate_keystrokes("escape");
        assert!(!is_open(test_cx));
        release(test_cx, "escape");

        // Re-arm must not flicker: a held trigger repeat with no modifier
        // movement in between stays swallowed after a toggle-close.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx));
        release(test_cx, TRIGGER_KEY);
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(!is_open(test_cx), "toggle closes");
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(
            !is_open(test_cx),
            "held trigger repeat without modifier movement stays swallowed"
        );
    }

    /// Component focus traversal is a navigation-mode command. In normal mode
    /// the workspace unbind (`bind_app_keys`) keeps gpui-component's `Root`
    /// traversal from stealing Tab, so the key stays with the focused
    /// component — a terminal sends it to the pty there. The mode itself
    /// traverses with Tab/Shift+Tab and stays open for repeats.
    #[gpui_kit::test]
    fn tab_traverses_components_only_in_navigation_mode(cx: &mut gpui_kit::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_kit::init);
        // The shipped keymap, so the unbind under test is the real one.
        cx.update(crate::bind_app_keys);
        const TRIGGER: &str = if cfg!(target_os = "macos") {
            "cmd-j"
        } else {
            "ctrl-j"
        };
        let directory = tempfile::tempdir().unwrap();
        let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, directory.path()));
            *holder_for_window.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        let home_focus = view.update(test_cx, |view, cx| view.home.read(cx).focus_handle.clone());
        let focused = |test_cx: &mut gpui_kit::VisualTestContext| {
            test_cx.update(|window, cx| window.focused(cx))
        };
        let claimed_tab = |test_cx: &mut gpui_kit::VisualTestContext| {
            view.update(test_cx, |view, _| {
                view.navigation_claimed_keys.contains_key("tab")
            })
        };
        let is_open = |test_cx: &mut gpui_kit::VisualTestContext| {
            view.update(test_cx, |view, _| view.navigation_open)
        };
        // Held-key bookkeeping: a claimed key stays swallowed until its
        // release, so consecutive Tab presses need a key-up in between.
        let release_tab = |test_cx: &mut gpui_kit::VisualTestContext| {
            test_cx.simulate_event(gpui_kit::KeyUpEvent {
                keystroke: gpui_kit::Keystroke::parse("tab").unwrap(),
            });
        };
        // Home's handle is not a tab stop, so traversal must visibly move
        // focus; a stolen-and-dropped key would leave it exactly here.
        test_cx.update(|window, cx| home_focus.focus(window, cx));

        // Normal mode: Tab must reach the focused component instead of
        // moving focus. Without the unbind, `Root`'s traversal would jump to
        // the first tab stop (the dashboard's add-project button) here.
        test_cx.simulate_keystrokes("tab");
        assert_eq!(
            focused(test_cx),
            Some(home_focus.clone()),
            "normal mode must not traverse focus"
        );
        assert!(
            !claimed_tab(test_cx),
            "normal mode must not consume Tab for navigation"
        );

        // Navigation mode: both directions traverse and keep the mode open.
        test_cx.simulate_keystrokes(TRIGGER);
        assert!(is_open(test_cx));
        test_cx.simulate_keystrokes("shift-tab");
        assert!(is_open(test_cx), "Shift+Tab keeps navigation mode open");
        assert!(
            claimed_tab(test_cx),
            "Shift+Tab is a navigation input, not a component key"
        );
        assert_ne!(
            focused(test_cx),
            Some(home_focus.clone()),
            "Shift+Tab moves focus to the previous component"
        );
        release_tab(test_cx);
        test_cx.update(|window, cx| home_focus.focus(window, cx));
        test_cx.simulate_keystrokes("tab");
        assert!(is_open(test_cx), "Tab keeps navigation mode open");
        assert_ne!(
            focused(test_cx),
            Some(home_focus.clone()),
            "Tab moves focus to the next component"
        );
    }

    /// Switching spaces through the real menu and rendering every surface
    /// (Home, the global Artifacts page, a repository workspace) stays
    /// stable. Regression coverage for the reported crash on a space pick.
    #[gpui_kit::test]
    fn switching_spaces_through_the_menu_stays_stable(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_kit::init);
        let directory = tempfile::tempdir().unwrap();
        let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, directory.path()));
            *holder_for_window.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        let (active, other) = view.update(test_cx, |view, _| {
            let active = view.active_space.clone();
            let other = view
                .spaces
                .names()
                .into_iter()
                .find(|name| !space_eq(name, &active));
            (active, other)
        });
        let Some(other) = other else {
            panic!("need two spaces for the switcher regression test");
        };
        // Switch to the other space through the titlebar menu. Escape first,
        // then reopen: dismissal must leave no stale menu behind.
        test_cx.update(|window, cx| window.click("space-switcher-trigger", cx));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        test_cx.simulate_keystrokes("escape");
        test_cx.run_until_parked();
        assert!(
            !view.update(test_cx, |view, _| view.space_menu_open),
            "Escape closes the switcher"
        );
        assert!(view.update(test_cx, |view, _| view.space_menu.is_none()));
        test_cx.update(|window, cx| window.click("space-switcher-trigger", cx));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        let other_index = view.update(test_cx, |view, _| {
            view.spaces
                .names()
                .iter()
                .position(|name| space_eq(name, &other))
                .unwrap()
        });
        test_cx.update(|window, cx| window.click(other_index, cx));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        assert_eq!(
            view.update(test_cx, |view, _| view.active_space.clone()),
            other
        );
        // Exercise the global Artifacts page in the new space before
        // switching back. (A repository workspace would spawn PTY reader
        // threads, which the test scheduler rejects as nondeterministic.)
        test_cx.update(|window, cx| view.update(cx, |view, cx| view.browse_artifacts(window, cx)));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        // Switch back through the menu trigger and the first item.
        test_cx.update(|window, cx| window.click("space-switcher-trigger", cx));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        let index = view.update(test_cx, |view, _| {
            view.spaces
                .names()
                .iter()
                .position(|name| space_eq(name, &active))
                .unwrap()
        });
        test_cx.update(|window, cx| window.click(index, cx));
        test_cx.run_until_parked();
        for _ in 0..3 {
            test_cx.update(|window, cx| window.render_frame(cx));
            test_cx.run_until_parked();
        }
        assert_eq!(
            view.update(test_cx, |view, _| view.active_space.clone()),
            active
        );
    }

    /// The switcher must be usable from the keyboard alone: `Cmd+J`, `s`,
    /// arrow keys, Enter.
    #[gpui_kit::test]
    fn space_switcher_navigates_with_arrow_keys(cx: &mut gpui_kit::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_kit::init);
        cx.update(crate::bind_app_keys);
        const TRIGGER: &str = if cfg!(target_os = "macos") {
            "cmd-j"
        } else {
            "ctrl-j"
        };
        let directory = tempfile::tempdir().unwrap();
        let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, directory.path()));
            *holder_for_window.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        let release = |test_cx: &mut gpui_kit::VisualTestContext, key: &str| {
            test_cx.simulate_event(gpui_kit::KeyUpEvent {
                keystroke: gpui_kit::Keystroke::parse(key).unwrap(),
            });
        };

        // Open the switcher through the navigation-mode entry.
        test_cx.simulate_keystrokes(TRIGGER);
        release(test_cx, "j");
        test_cx.simulate_keystrokes("s");
        release(test_cx, "s");
        test_cx.run_until_parked();
        let names = view.update(test_cx, |view, _| view.spaces.names());
        let active = view.update(test_cx, |view, _| view.active_space.clone());
        assert!(view.update(test_cx, |view, _| view.space_menu_open));
        assert!(names.len() >= 2, "need two spaces for this test");
        let target_index = names
            .iter()
            .position(|name| !space_eq(name, &active))
            .unwrap();

        // The menu must hold focus for its key bindings to fire.
        let menu_focus = view.update(test_cx, |view, cx| {
            view.space_menu.as_ref().map(|menu| menu.focus_handle(cx))
        });
        let focused = test_cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused, menu_focus,
            "the space menu must hold focus for arrow keys and Enter"
        );
        // Nothing is selected on open: the first Down selects the first
        // entry, each further Down moves one step.
        for _ in 0..=target_index {
            test_cx.simulate_keystrokes("down");
        }
        test_cx.simulate_keystrokes("enter");
        test_cx.run_until_parked();
        assert_eq!(
            view.update(test_cx, |view, _| view.active_space.clone()),
            names[target_index],
            "arrow keys + Enter must pick the highlighted space"
        );
        assert!(!view.update(test_cx, |view, _| view.space_menu_open));
        // Closing must not leave focus on the dropped menu handle.
        let after = test_cx.update(|window, cx| window.focused(cx));
        assert!(after.is_some(), "focus must land somewhere after closing");
        assert_ne!(after, menu_focus, "focus must leave the dropped menu");
    }

    /// The navigation-mode `s` entry opens the titlebar space switcher and
    /// leaves navigation mode; pressing it again (or the switcher's own
    /// dismissal) closes it.
    #[gpui_kit::test]
    fn switch_space_entry_toggles_the_titlebar_switcher(cx: &mut gpui_kit::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_kit::init);
        cx.update(crate::bind_app_keys);
        const TRIGGER: &str = if cfg!(target_os = "macos") {
            "cmd-j"
        } else {
            "ctrl-j"
        };
        let directory = tempfile::tempdir().unwrap();
        let holder: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(window, cx, directory.path()));
            *holder_for_window.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        let state = |test_cx: &mut gpui_kit::VisualTestContext| {
            view.update(test_cx, |view, _| {
                (
                    view.space_menu_open,
                    view.navigation_open,
                    view.active_space.clone(),
                    view.spaces.names(),
                )
            })
        };

        // Start on Home in navigation mode.
        assert!(test_cx.update(|_, cx| view.read(cx).home_visible));
        test_cx.simulate_keystrokes(TRIGGER);
        let (menu_open, navigation_open, active, spaces) = state(test_cx);
        assert!(navigation_open);
        assert!(!menu_open);
        assert!(!spaces.is_empty(), "the catalog seeds at startup");
        assert!(spaces.iter().any(|name| space_eq(name, &active)));
        // Held-key bookkeeping swallows repeats until a key-up.
        test_cx.simulate_event(gpui_kit::KeyUpEvent {
            keystroke: gpui_kit::Keystroke::parse("j").unwrap(),
        });

        // `s` opens the switcher and returns to normal mode.
        test_cx.simulate_keystrokes("s");
        let (menu_open, navigation_open, ..) = state(test_cx);
        assert!(menu_open, "s opens the space switcher");
        assert!(!navigation_open, "an action key leaves navigation mode");

        // The same entry toggles it closed.
        test_cx.simulate_event(gpui_kit::KeyUpEvent {
            keystroke: gpui_kit::Keystroke::parse("s").unwrap(),
        });
        test_cx.simulate_keystrokes(TRIGGER);
        test_cx.simulate_event(gpui_kit::KeyUpEvent {
            keystroke: gpui_kit::Keystroke::parse("j").unwrap(),
        });
        test_cx.simulate_keystrokes("s");
        let (menu_open, ..) = state(test_cx);
        assert!(!menu_open, "s toggles the switcher closed");
    }

    /// Two focusable tab stops inside the workspace key context, with the
    /// first one recording raw Tab key events. Isolates the unbind's
    /// mechanism from the dashboard's tab-stop ring.
    struct TabUnbindHarness {
        first: gpui_kit::FocusHandle,
        second: gpui_kit::FocusHandle,
        tab_events: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl gpui_kit::Render for TabUnbindHarness {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            _: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{InteractiveElement as _, Styled as _};
            let tab_events = self.tab_events.clone();
            gpui_kit::div()
                .key_context(WORKSPACE_KEY_CONTEXT)
                .size_full()
                .child(
                    gpui_kit::div()
                        .id("first")
                        .track_focus(&self.first)
                        .size(px(8.))
                        .on_key_down(move |event, _, _| {
                            if event.keystroke.key == "tab" {
                                tab_events.set(tab_events.get() + 1);
                            }
                        }),
                )
                .child(
                    gpui_kit::div()
                        .id("second")
                        .track_focus(&self.second)
                        .size(px(8.)),
                )
        }
    }

    /// Open the harness window with `first` focused, returning the recorded
    /// Tab-event counter.
    fn open_tab_unbind_harness(
        cx: &mut gpui_kit::TestAppContext,
    ) -> (
        Entity<TabUnbindHarness>,
        std::rc::Rc<std::cell::Cell<usize>>,
        &mut gpui_kit::VisualTestContext,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let tab_events = Rc::new(std::cell::Cell::new(0));
        let events = tab_events.clone();
        let holder: Rc<RefCell<Option<Entity<TabUnbindHarness>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let (_root, test_cx) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| TabUnbindHarness {
                first: cx.focus_handle().tab_stop(true),
                second: cx.focus_handle().tab_stop(true),
                tab_events: events,
            });
            *holder_for_window.borrow_mut() = Some(harness.clone());
            // Wrapped like the app's workspace, so gpui-component's `Root`
            // traversal and its "Root" key context are in play.
            Root::new(harness, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        let first = view.update(test_cx, |view, _| view.first.clone());
        test_cx.update(|window, cx| first.focus(window, cx));
        (view, tab_events, test_cx)
    }

    /// Control for the unbind test below: without the shipped bindings,
    /// gpui-component's `Root` traversal fires first, moves focus to the
    /// second tab stop, and the key never reaches the focused component.
    /// This is the behavior `bind_app_keys` removes inside the workspace.
    #[gpui_kit::test]
    fn root_traversal_steals_tab_without_the_workspace_unbind(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, tab_events, test_cx) = open_tab_unbind_harness(cx);
        let second = view.update(test_cx, |view, _| view.second.clone());
        test_cx.simulate_keystrokes("tab");
        assert_eq!(
            test_cx.update(|window, cx| window.focused(cx)),
            Some(second),
            "Root's traversal owns Tab without the unbind"
        );
        assert_eq!(tab_events.get(), 0, "the component never sees the key");
    }

    /// Inside the workspace key context the unbind suppresses `Root`'s Tab
    /// binding: focus stays put and the raw key reaches the focused
    /// component's own listener — what terminal panes use to forward it to
    /// the pty.
    #[gpui_kit::test]
    fn workspace_context_unbinds_tab_for_the_focused_component(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(crate::bind_app_keys);
        let (view, tab_events, test_cx) = open_tab_unbind_harness(cx);
        let first = view.update(test_cx, |view, _| view.first.clone());
        test_cx.simulate_keystrokes("tab");
        assert_eq!(
            test_cx.update(|window, cx| window.focused(cx)),
            Some(first.clone()),
            "the unbind must keep focus on the component"
        );
        assert_eq!(tab_events.get(), 1, "the component receives the raw key");
        test_cx.simulate_keystrokes("shift-tab");
        assert_eq!(
            test_cx.update(|window, cx| window.focused(cx)),
            Some(first),
            "Shift+Tab is unbound the same way"
        );
        assert_eq!(tab_events.get(), 2, "the component receives Shift+Tab too");
    }

    #[test]
    fn navigation_tab_maps_to_component_traversal() {
        let input =
            |key: &str| Workspace::navigation_input(&gpui_kit::Keystroke::parse(key).unwrap());
        assert_eq!(input("tab"), NavigationInput::Tab);
        assert_eq!(input("shift-tab"), NavigationInput::BackTab);
        assert_eq!(input("space"), NavigationInput::Key(' '));
        // Every other modified form stays a consumed modified key: only
        // Shift+Tab is a navigation input.
        assert_eq!(input("cmd-tab"), NavigationInput::Modified);
        assert_eq!(input("ctrl-shift-tab"), NavigationInput::Modified);
    }

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
        assert_eq!(WorkspaceTab::Resources.label(), "Resources");
        assert_eq!(WorkspaceTab::Resources.command(), None);
        assert!(!WorkspaceTab::Resources.has_terminal());
        assert!(!WorkspaceTab::ALL.contains(&WorkspaceTab::Git));
        assert_eq!(WorkspaceTab::Git.label(), "Git");
        assert_eq!(WorkspaceTab::Git.command(), Some("lazygit"));
        assert!(WorkspaceTab::Git.has_terminal());
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
