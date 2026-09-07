//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header and tab bar) hosting the active terminal pane.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Root, StyledExt as _, WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, Styled, Window, div, px, rgb,
};

use crate::add_repository::AddRepositoryView;
use crate::agent::AgentKind;
use crate::command_palette::{
    GoToAgent, GoToEditor, GoToTerminal, PaletteCommand, PaletteItem, PaletteMode, PaletteSection,
    ToggleActionsPalette, ToggleProjectsPalette, item_at, palette_sections_for_mode,
};
use crate::data::{
    DataRoot, DeviceStore, RecentRepository, SyncStatus, SyncTracker, checkout_for,
    recent_repositories, record_repository_open, resolve_current_key, resolve_workspace_agent,
    set_workspace_agent as persist_workspace_agent, sync_portable_with_tracker,
};
use crate::git_status::{GitStatus, load_git_status};
use crate::home::{HomeEvent, HomeView};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, WORKSPACE_HEADER_HEIGHT};
use crate::pane::TerminalPane;
use crate::review::ReviewView;
use crate::settings::SettingsView;
use crate::workspace_settings::WorkspaceSettingsView;
use gpui_kit::component::button::{Button, ButtonVariants as _};

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
}

impl WorkspaceTab {
    pub(crate) const ALL: [Self; 4] = [Self::Agent, Self::Editor, Self::Terminal, Self::Review];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent",
            Self::Editor => "Editor",
            Self::Terminal => "Terminal",
            Self::Review => "Review",
        }
    }

    pub(crate) fn command(self) -> Option<&'static str> {
        match self {
            // Single source of truth lives on the default harness, so the
            // label in Settings and the spawned command cannot drift.
            Self::Agent => Some(AgentKind::DEFAULT.command()),
            Self::Editor => Some("nvim ."),
            Self::Terminal | Self::Review => None,
        }
    }

    /// Whether the tab hosts a terminal pane. `Review` renders its own
    /// (currently empty) content instead of spawning a shell.
    pub(crate) fn has_terminal(self) -> bool {
        !matches!(self, Self::Review)
    }
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
    home: Entity<HomeView>,
    home_visible: bool,
    portable_git_poll: GitPoll,
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    /// Harness the current Agent pane was spawned with. Preserved across
    /// repository switches with its tabs; picking a new default restarts
    /// the pane with it (see `restart_agent_pane`).
    session_agent: AgentKind,
    /// Persisted default harness for the current checkout: what the Agent
    /// pane launches. Updated by the workspace settings sheet, which
    /// restarts the pane so the two always agree for the visible checkout.
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
    command_state: Entity<CommandState>,
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
    fn new(working_directory: &Path, agent: AgentKind, cx: &mut Context<Workspace>) -> Self {
        let tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                tab.has_terminal()
                    .then(|| cx.new(|cx| TerminalPane::new(tab, working_directory, agent, cx)))
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

        Self {
            home,
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
            command_state,
            data_root,
            sync_tracker,
            last_sync_started: Instant::now(),
            working_directory: working_directory.to_owned(),
            current_repository,
            recent_repositories: Vec::new(),
            palette_model: Vec::new(),
        }
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = WorkspaceTab::ALL.get(index).copied() else {
            return;
        };
        self.active_tab = tab;
        self.enter_repository(cx);
        if tab == WorkspaceTab::Review {
            self.review.update(cx, |view, cx| view.activate(cx));
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    fn focus_active_pane(&self, window: &mut Window, cx: &mut App) {
        if self.home_visible {
            self.home.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if self.active_tab == WorkspaceTab::Review {
            self.review.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if let Some(Some(pane)) = self.tabs.get(self.active_tab as usize) {
            let focus_handle = pane.read(cx).focus_handle.clone();
            focus_handle.focus(window, cx);
        }
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

    fn toggle_command_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.recent_repositories = self
            .data_root
            .as_ref()
            .map(|root| recent_repositories(root, usize::MAX))
            .unwrap_or_default();
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
        match item {
            PaletteItem::SwitchRepository { key, label } => {
                self.switch_repository(&key, &label, window, cx)
            }
            PaletteItem::Command(command) => match command {
                PaletteCommand::GoAgent => self.select_tab(0, window, cx),
                PaletteCommand::GoEditor => self.select_tab(1, window, cx),
                PaletteCommand::GoTerminal => self.select_tab(2, window, cx),
                PaletteCommand::GoReview => self.select_tab(3, window, cx),
                PaletteCommand::OpenSettings => self.open_settings(window, cx),
                PaletteCommand::AddRepository => self.open_add_repository(window, cx),
                PaletteCommand::GoHome => self.go_home(window, cx),
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
        self.enter_repository(cx);
        self.project_name = checkout
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned()
            .into();
        self.current_repository = Some(key.to_owned());
        // Reset plus an immediate fresh load: the header shows the new
        // checkout's state within one scan instead of flashing the old
        // checkout's status until the next poll tick — and the load id
        // invalidates the tick that was in flight for the old checkout.
        self.refresh_git_status(cx);
        self.recent_repositories = recent_repositories(&root, usize::MAX);
        window.push_notification(format!("Switched to {label}"), cx);
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
            .unwrap_or_else(|| RepositoryTabs::new(checkout, default_agent, cx));
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

    fn enter_repository(&mut self, cx: &mut Context<Self>) {
        self.home_visible = false;
        self.home.update(cx, |view, _| view.deactivate());
        for tab in WorkspaceTab::ALL {
            if tab.has_terminal() && self.tabs[tab as usize].is_none() {
                self.tabs[tab as usize] = Some(cx.new(|cx| {
                    TerminalPane::new(tab, &self.working_directory, self.default_agent, cx)
                }));
            }
        }
    }

    fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.home_visible = true;
        self.command_open = false;
        self.home.update(cx, |view, cx| view.activate(cx));
        self.focus_active_pane(window, cx);
        cx.notify();
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
        self.session_agent = agent;
        self.restart_agent_pane(window, cx);
        window.push_notification(
            format!(
                "Default agent is now {} — Agent tab restarted with `{}`",
                agent.label(),
                agent.command()
            ),
            cx,
        );
        cx.notify();
    }

    /// Replace the current checkout's Agent pane with a fresh one launching
    /// the default harness. Dropping the old entity ends its PTY session;
    /// focus follows only when the Agent tab is showing, so a restart from
    /// the sheet never yanks focus away from another tab.
    fn restart_agent_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let working_directory = self.working_directory.clone();
        let agent = self.default_agent;
        if let Some(slot) = self.tabs.get_mut(WorkspaceTab::Agent as usize) {
            *slot =
                Some(cx.new(|cx| {
                    TerminalPane::new(WorkspaceTab::Agent, &working_directory, agent, cx)
                }));
        }
        if self.active_tab == WorkspaceTab::Agent {
            self.focus_active_pane(window, cx);
        }
    }

    /// Persisted default harness for the current checkout, for the settings
    /// sheet's live selection.
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
            .w(px(560.));
        // Install exactly the model confirmations resolve against, so render
        // and confirm can never disagree about what a row means. Labels,
        // keywords, and icons all come from the model item. The model follows
        // the open mode, so each shortcut sees only its own rows.
        self.palette_model =
            palette_sections_for_mode(&self.recent_repositories, self.palette_mode);
        let current = self.current_repository.clone();
        for section in &self.palette_model {
            command = command.group(
                CommandGroup::new().label(section.heading).items(
                    section
                        .items
                        .iter()
                        .map(|item| {
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
                                PaletteItem::Command(command) => CommandItem::new()
                                    .label(item.label())
                                    .icon(palette_icon(*command)),
                                PaletteItem::SwitchRepository { key, .. } => CommandItem::new()
                                    .label(item.label())
                                    .checked(current.as_deref() == Some(key.as_str()))
                                    .icon(IconName::Folder),
                            };
                            rendered.keywords(item.keywords())
                        })
                        .collect::<Vec<_>>(),
                ),
            );
        }
        command
    }

    fn render_active_content(&self) -> AnyElement {
        if self.home_visible {
            return self.home.clone().into_any_element();
        }
        if self.active_tab == WorkspaceTab::Review {
            return self.review.clone().into_any_element();
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
        PaletteCommand::GoHome => IconName::LayoutDashboard,
        PaletteCommand::AddRepository => IconName::Plus,
        PaletteCommand::OpenSettings => IconName::Settings,
        PaletteCommand::SyncPortable => IconName::RotateCw,
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_index = self.active_tab as usize;
        let active_content = self.render_active_content();

        v_flex()
            .relative()
            .when(self.home_visible, |this| this.tab_group())
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
            .child(
                h_flex()
                    .h(px(58.))
                    .px_4()
                    .gap_3()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0x292b2b))
                    .when(self.home_visible, |header| {
                        header.child(div().text_lg().font_semibold().child("Devcroft"))
                    })
                    .when(!self.home_visible, |header| {
                        header.child(
                            h_flex()
                                .flex_none()
                                .gap_2()
                                .items_center()
                                .child(Button::new("go-home").ghost().label("‹ Home").on_click(
                                    cx.listener(|this, _, window, cx| this.go_home(window, cx)),
                                ))
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
                        div().flex_1().flex().flex_row().justify_center().child(
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
                                    this.toggle_command_palette(PaletteMode::Actions, window, cx);
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
                        ),
                    )
                    .when(self.home_visible, |header| {
                        let status = self.portable_git_poll.status();
                        let label = if self.data_root.is_none() {
                            "Portable data unavailable".to_owned()
                        } else {
                            format!(
                                "Portable · {} {} {} {}",
                                status.branch.as_deref().unwrap_or("Git unavailable"),
                                if status.dirty {
                                    "● Modified"
                                } else if status.branch.is_some() {
                                    "Clean"
                                } else {
                                    ""
                                },
                                status.ahead_label().unwrap_or_default(),
                                status.behind_label().unwrap_or_default()
                            )
                        };
                        header.child(
                            Button::new("portable-status")
                                .ghost()
                                .max_w(px(320.))
                                .overflow_hidden()
                                .tooltip(label.clone())
                                .label(label)
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
                        .child(div().absolute().size_full().bg(rgb(0x000000)).opacity(0.4))
                        .child(
                            h_flex().justify_center().pt(px(8.)).child(
                                div()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| {
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .child(self.render_command_bar(cx)),
                            ),
                        ),
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
