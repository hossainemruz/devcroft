//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header and tab bar) hosting the active terminal pane.

use std::{
    path::{Path, PathBuf},
    time::Duration,
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
use crate::command_palette::{
    PaletteCommand, PaletteItem, PaletteSection, ToggleCommandPalette, item_at, palette_sections,
};
use crate::data::{
    DataRoot, DeviceStore, MAX_RECENT_REPOSITORIES, RecentRepository, SyncStatus, SyncTracker,
    checkout_for, recent_repositories, record_repository_open, resolve_current_key,
    sync_portable_with_tracker,
};
use crate::git_status::{GitStatus, load_git_status};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, WORKSPACE_HEADER_HEIGHT};
use crate::pane::TerminalPane;
use crate::review::ReviewView;
use crate::settings::SettingsView;

/// How often the header re-reads branch/dirty/ahead-behind state.
///
/// The poll runs off the main thread and only notifies on change, so the
/// cadence sets staleness, not frame cost. Two seconds keeps the dirty dot
/// feeling live after saves without churning full worktree walks on large
/// checkouts.
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
            Self::Agent => Some("opencode"),
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
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    settings: Entity<SettingsView>,
    project_name: SharedString,
    git_poll: GitPoll,
    command_open: bool,
    command_state: Entity<CommandState>,
    data_root: Option<DataRoot>,
    sync_tracker: SyncTracker,
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

        let tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                if !tab.has_terminal() {
                    return None;
                }
                let cwd = working_directory.to_path_buf();
                Some(cx.new(|cx| TerminalPane::new(tab, &cwd, cx)))
            })
            .collect::<Vec<_>>();
        if let Some(Some(initial)) = tabs.first() {
            initial.read(cx).focus_handle.clone().focus(window, cx);
        }
        let review_cwd = working_directory.to_path_buf();
        let review = cx.new(|cx| ReviewView::new(&review_cwd, cx));
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
        // Resolved here (not passed in) so sync keeps working even if the
        // startup path never persisted device state; failures stay non-fatal
        // and surface as a palette notification instead.
        let data_root = crate::data::ensure_ready(None).ok();
        // Initial font size for Settings. The live global was already set
        // from the same store at startup (`main`), so this is just the
        // view's starting copy; later edits write back through the store.
        let initial_font_size = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok())
            .map(|state| state.app_font_size_or_default())
            .unwrap_or(DEFAULT_APP_FONT_SIZE);
        let settings = cx.new(|cx| SettingsView::new(data_root.clone(), initial_font_size, cx));
        // Best-effort current-repository match so the palette can mark it
        // without waiting for the first switch; a `--checkout` outside any
        // linked binding simply starts unmarked.
        let current_repository = data_root
            .as_ref()
            .and_then(|root| resolve_current_key(root, working_directory));

        Self {
            active_tab: WorkspaceTab::Agent,
            tabs,
            review,
            settings,
            project_name: project_name.into(),
            git_poll: GitPoll::default(),
            command_open: false,
            command_state,
            data_root,
            sync_tracker: SyncTracker::default(),
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
        if tab == WorkspaceTab::Review {
            self.review.update(cx, |view, cx| view.activate(cx));
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    fn focus_active_pane(&self, window: &mut Window, cx: &mut App) {
        if self.active_tab == WorkspaceTab::Review {
            self.review.read(cx).focus_handle.clone().focus(window, cx);
            return;
        }
        if let Some(Some(pane)) = self.tabs.get(self.active_tab as usize) {
            let focus_handle = pane.read(cx).focus_handle.clone();
            focus_handle.focus(window, cx);
        }
    }

    fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_open {
            self.close_command_palette(window, cx);
        } else {
            self.open_command_palette(window, cx);
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_open {
            return;
        }
        self.command_open = true;
        // A few small JSON reads per opening — not per render — so the
        // switcher always reflects recent adds and switches.
        self.recent_repositories = self
            .data_root
            .as_ref()
            .map(|root| recent_repositories(root, MAX_RECENT_REPOSITORIES))
            .unwrap_or_default();
        self.command_state.update(cx, |state, cx| {
            state.set_query("", window, cx);
        });
        // `read` borrows `cx`, so clone the handle first: focusing takes the
        // context mutably.
        let query_focus = self.command_state.read(cx).focus_handle(cx).clone();
        query_focus.focus(window, cx);
        cx.notify();
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
                // Placeholder until its view exists: visible and searchable
                // so the bar advertises the roadmap, honest about doing
                // nothing yet.
                PaletteCommand::GoHome => {
                    window.push_notification(format!("{} — coming soon", command.label()), cx);
                    self.focus_active_pane(window, cx);
                    cx.notify();
                }
                PaletteCommand::SyncPortable => {
                    if self.sync_tracker.status() == SyncStatus::Syncing {
                        window.push_notification("Portable sync is already running", cx);
                    } else if self.data_root.is_none() {
                        window.push_notification("Portable data is unavailable", cx);
                    } else {
                        window.push_notification("Syncing portable data…", cx);
                        self.trigger_sync(cx);
                    }
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
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Add repository")
                .w(px(680.))
                .h(px(640.))
                .child(view.clone().into_any_element())
        });
    }

    /// Instantly re-root the workspace at another linked checkout: record
    /// recency first (a failure there leaves the current workspace
    /// untouched), then rebuild the terminal panes and Review around the
    /// new directory. Dropping the old panes ends their PTY children with
    /// them — each session owns its child handle — and the orphaned output
    /// tasks exit on their next entity update, so no restart handshake is
    /// needed.
    fn switch_repository(
        &mut self,
        key: &str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Re-selecting the current repository is a no-op: re-rooting would
        // pointlessly kill its running PTYs.
        if self.current_repository.as_deref() == Some(key) {
            self.focus_active_pane(window, cx);
            cx.notify();
            return;
        }
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
        self.working_directory = checkout.clone();
        self.project_name = checkout
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned()
            .into();
        self.current_repository = Some(key.to_owned());
        self.tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                if !tab.has_terminal() {
                    return None;
                }
                let cwd = checkout.clone();
                Some(cx.new(|cx| TerminalPane::new(tab, &cwd, cx)))
            })
            .collect();
        // `ReviewView::new` loads immediately, so no explicit `activate`.
        self.review = cx.new(|cx| ReviewView::new(&checkout, cx));
        // Reset plus an immediate fresh load: the header shows the new
        // checkout's state within one scan instead of flashing the old
        // checkout's status until the next poll tick — and the load id
        // invalidates the tick that was in flight for the old checkout.
        self.refresh_git_status(cx);
        self.recent_repositories = recent_repositories(&root, MAX_RECENT_REPOSITORIES);
        window.push_notification(format!("Switched to {label}"), cx);
        self.focus_active_pane(window, cx);
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

    /// Run one portable sync off the main thread. Completion reloads the
    /// Review projection when the rebase may have moved files (including
    /// after an error, which can still leave working-tree changes behind)
    /// and repaints the status bar via the shared [`SyncTracker`].
    fn trigger_sync(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.data_root.clone() else {
            return;
        };
        let tracker = self.sync_tracker.clone();
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
                    this.review.update(cx, |view, cx| view.reload(cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_command_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let confirm_workspace = workspace.clone();
        let mut command = Command::new(&self.command_state)
            .placeholder("Type a command or search…")
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
        // keywords, and icons all come from the model item.
        self.palette_model = palette_sections(&self.recent_repositories);
        let current = self.current_repository.clone();
        for section in &self.palette_model {
            command = command.group(
                CommandGroup::new().label(section.heading).items(
                    section
                        .items
                        .iter()
                        .map(|item| {
                            let rendered = match item {
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

/// Leading glyph per palette command. Promote a command to a real GPUI
/// `Action` (instead of the `on_confirm` path above) when it needs a direct
/// keybinding: `CommandItem::action` then renders the binding hint for free.
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
            .size_full()
            .bg(rgb(0x080909))
            .text_color(rgb(0xe7e7e7))
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
                this.toggle_command_palette(window, cx);
            }))
            .child(
                h_flex()
                    .h(px(58.))
                    .px_4()
                    .gap_3()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(0x292b2b))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .items_center()
                            .child(div().text_color(rgb(0x8e9494)).child("‹"))
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
                                this.child(div().text_xs().text_color(rgb(0x858989)).child(ahead))
                            })
                            .when_some(self.git_poll.status().behind_label(), |this, behind| {
                                this.child(div().text_xs().text_color(rgb(0x858989)).child(behind))
                            }),
                    )
                    .child(
                        div().flex_1().flex().flex_row().justify_center().child(
                            h_flex()
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
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        this.toggle_command_palette(window, cx);
                                    }),
                                )
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
                                ),
                        ),
                    )
                    .child(
                        div().flex_none().child(
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
                        ),
                    ),
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
            // swallowed. Above content and the palette dim, below dialogs.
            .children(Root::render_notification_layer(window, cx))
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
