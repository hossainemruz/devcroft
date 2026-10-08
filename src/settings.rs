//! The Settings dialog: app-wide preferences grouped into sections.
//!
//! General hosts the live app-wide font size — persisted to `device.json`
//! and applied to the Agent/Editor/Terminal/Review panes without a restart.
//! Sync hosts the portable-data Git remote, the automatic sync schedule, and
//! a manual Sync-now action with live status. Agent hosts the enabled
//! harness cards with per-agent toggles, the default-agent dropdown (enabled
//! agents only), the sidebar session-limit slider (10–50, step 5), and
//! Devcroft skill cards with a header refresh action.
//! Editor hosts default-editor radio cards, Neovim availability, and external
//! launcher switches. Keybindings documents the current global shortcuts.
//!
//! [`SettingsView`] is a long-lived [`Workspace`](crate::workspace::Workspace)
//! entity rendered inside a dialog (`window.open_dialog`): the dialog owns
//! open/close while the view keeps the selected section and edits across
//! reopenings.

use gpui_kit::base::Radio;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled,
    WeakEntity, Window, div, px, rgb,
};

use std::{collections::HashMap, rc::Rc};

use crate::agent::AgentKind;
use crate::agent_icons::{self, AgentIconTiles};
use crate::agent_sessions::{
    DEFAULT_SIDEBAR_LIMIT, MAX_SIDEBAR_LIMIT, MIN_SIDEBAR_LIMIT, SIDEBAR_LIMIT_STEP,
    snap_sidebar_limit,
};
use crate::command_palette::{
    PaletteMode, ToggleActionsPalette, ToggleProjectsPalette, palette_mode_for_shortcut,
};
use crate::data::{
    CheckoutOutcome, DataRoot, DeviceStore, SYNC_INTERVAL_OPTIONS, SpaceRewrite, Spaces,
    SyncStatus, SyncTracker, checkout_branch, clear_origin, current_branch_name, current_upstream,
    delete_space, get_origin, is_supported_sync_interval, list_local_branches, rename_space,
    set_origin, space_eq,
};
use crate::editor::{EditorChoice, ExternalEditorKind};
use crate::editor_icons::{self, EditorIconTiles};
use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::metrics::{
    DEFAULT_APP_FONT_SIZE, MAX_APP_FONT_SIZE, MIN_APP_FONT_SIZE, clamp_app_font_size,
    review_font_size, set_app_font_size,
};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Spaces,
    Sync,
    Agent,
    Editor,
    Keybindings,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 6] = [
        Self::General,
        Self::Spaces,
        Self::Sync,
        Self::Agent,
        Self::Editor,
        Self::Keybindings,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Spaces => "Spaces",
            Self::Sync => "Sync",
            Self::Agent => "Agent",
            Self::Editor => "Editor",
            Self::Keybindings => "Keybindings",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => "Appearance and terminal behavior.",
            Self::Spaces => "Keep work and personal projects separate.",
            Self::Sync => "Back up and sync between devices.",
            Self::Agent => "Available agents, sessions, and skills.",
            Self::Editor => "Default editor and external launchers.",
            Self::Keybindings => "Shortcuts and navigation mode.",
        }
    }
}

/// Inline row editor in the Spaces section: rename one space, or delete it
/// after choosing where its records move.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SpaceEdit {
    Rename(String),
    Delete { name: String, destination: String },
}

pub(crate) struct SettingsView {
    pub(crate) focus_handle: FocusHandle,
    active_section: SettingsSection,
    font_size: f32,
    terminal_preferences_error: Option<String>,
    editor_choice: EditorChoice,
    language_servers: Entity<crate::editor::lsp::settings::LanguageServers>,
    external_editors: HashMap<String, bool>,
    installed_editors: HashMap<String, bool>,
    external_checking: bool,
    editor_error: Option<String>,
    neovim_available: bool,
    neovim_check_error: Option<String>,
    neovim_checking: bool,
    editor_card_focus: [FocusHandle; 2],
    data_root: Option<DataRoot>,
    sync_tracker: SyncTracker,
    sync_interval: Option<u64>,
    saved_origin: Option<String>,
    origin_input: Entity<InputState>,
    origin_busy: bool,
    origin_error: Option<String>,
    origin_notice: Option<String>,
    interval_error: Option<String>,
    branches: Vec<String>,
    current_branch: Option<String>,
    branch_upstream: Option<String>,
    branch_input: Entity<InputState>,
    branch_busy: bool,
    branch_error: Option<String>,
    branch_notice: Option<String>,
    workspace: Option<WeakEntity<Workspace>>,
    skill_busy: bool,
    skill_statuses: Vec<String>,
    skill_environment: String,
    session_limit: usize,
    session_slider: Entity<SliderState>,
    session_limit_error: Option<String>,
    enabled_agents: Vec<AgentKind>,
    enabled_agents_error: Option<String>,
    default_agent: AgentKind,
    default_agent_error: Option<String>,
    /// Full-color harness logos for the default-agent options. The harness
    /// set is fixed, so tiles are rasterized once at creation and shared by
    /// every render.
    agent_icon_tiles: Rc<AgentIconTiles>,
    /// Full-color editor brand logos for the Editor section. The set is
    /// fixed, so tiles decode once at creation and are shared by every render.
    editor_icon_tiles: Rc<EditorIconTiles>,
    /// Spaces section: the portable catalog, the new-space input, and the
    /// inline rename/delete editor for one row at a time.
    spaces: Vec<String>,
    space_input: Entity<InputState>,
    space_rename_input: Entity<InputState>,
    space_edit: Option<SpaceEdit>,
    spaces_error: Option<String>,
    spaces_notice: Option<String>,
}

impl SettingsView {
    pub(crate) fn new(
        window: &mut Window,
        data_root: Option<DataRoot>,
        initial_font_size: f32,
        sync_tracker: SyncTracker,
        cx: &mut Context<Self>,
    ) -> Self {
        let (sync_interval, saved_origin) = match data_root.as_ref() {
            Some(root) => {
                let interval = DeviceStore::new(root)
                    .load()
                    .ok()
                    .and_then(|state| state.sync_interval_minutes);
                let origin = get_origin(root).unwrap_or(None);
                (interval, origin)
            }
            None => (None, None),
        };
        let prefill = saved_origin.clone().unwrap_or_default();
        let origin_input = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder("e.g. git@github.com:you/portable.git");
            if !prefill.is_empty() {
                state.set_value(prefill.clone(), window, cx);
            }
            state
        });
        let branch_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. experiment"));
        let (branches, current_branch, branch_upstream) = load_branch_state(data_root.as_ref());
        let stored = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok());
        let editor_choice = stored
            .as_ref()
            .map(|state| state.editor_choice_or_default())
            .unwrap_or_default();
        let external_editors = stored
            .as_ref()
            .and_then(|state| state.external_editors.clone())
            .unwrap_or_default();
        let session_limit = stored
            .as_ref()
            .map(|state| state.recent_sessions_limit_or_default())
            .unwrap_or(DEFAULT_SIDEBAR_LIMIT);
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
        let session_slider = cx.new(|_| {
            SliderState::new()
                .min(MIN_SIDEBAR_LIMIT as f32)
                .max(MAX_SIDEBAR_LIMIT as f32)
                .step(SIDEBAR_LIMIT_STEP as f32)
                .default_value(session_limit as f32)
        });
        let space_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Client A"));
        let space_rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Space name"));
        let spaces = load_space_names(data_root.as_ref());
        cx.subscribe(
            &session_slider,
            |this, _, event: &SliderEvent, cx| match event {
                SliderEvent::Change(value) | SliderEvent::Release(value) => {
                    let limit = snap_sidebar_limit(value.end().round() as usize);
                    if limit != this.session_limit {
                        this.set_session_limit(limit, cx);
                    }
                }
            },
        )
        .detach();
        let mut agent_icon_tiles = AgentIconTiles::new();
        agent_icons::ensure_tiles(AgentKind::ALL, &mut agent_icon_tiles, cx);
        let mut editor_icon_tiles = EditorIconTiles::new();
        editor_icons::ensure_tiles(crate::editor_icons::EditorIcon::ALL, &mut editor_icon_tiles);
        let skill_statuses = crate::agent_skill::Target::ALL
            .into_iter()
            .map(|target| {
                crate::agent_skill::perform(crate::agent_skill::Action::Status, Some(target)).text
            })
            .collect::<Vec<_>>();
        Self {
            focus_handle: cx.focus_handle(),
            active_section: SettingsSection::General,
            font_size: clamp_app_font_size(initial_font_size),
            terminal_preferences_error: None,
            editor_choice,
            language_servers: cx.new(|cx| {
                crate::editor::lsp::settings::LanguageServers::new(
                    data_root.as_ref().map(|r| r.root().to_owned()),
                    None,
                    window,
                    cx,
                )
            }),
            external_editors,
            installed_editors: HashMap::new(),
            external_checking: false,
            editor_error: None,
            neovim_available: false,
            neovim_check_error: None,
            neovim_checking: false,
            editor_card_focus: [cx.focus_handle(), cx.focus_handle()],
            data_root,
            sync_tracker,
            sync_interval,
            saved_origin,
            origin_input,
            origin_busy: false,
            origin_error: None,
            origin_notice: None,
            interval_error: None,
            branches,
            current_branch,
            branch_upstream,
            branch_input,
            branch_busy: false,
            branch_error: None,
            branch_notice: None,
            workspace: None,
            skill_busy: false,
            skill_statuses,
            skill_environment: crate::agent_skill::environment(),
            session_limit,
            session_slider,
            session_limit_error: None,
            enabled_agents,
            enabled_agents_error: None,
            default_agent,
            default_agent_error: None,
            agent_icon_tiles: Rc::new(agent_icon_tiles),
            editor_icon_tiles: Rc::new(editor_icon_tiles),
            spaces,
            space_input,
            space_rename_input,
            space_edit: None,
            spaces_error: None,
            spaces_notice: None,
        }
    }

    /// Link the owning workspace for Sync-now delegation. Set by the
    /// workspace before the dialog opens; `None` until then.
    pub(crate) fn set_workspace(&mut self, workspace: WeakEntity<Workspace>) {
        self.workspace = Some(workspace);
    }

    /// Re-read persisted Sync state plus the Agent section so the dialog
    /// never shows stale state (e.g. after external git or CLI edits).
    /// Called before the dialog opens.
    pub(crate) fn refresh_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_neovim(cx);
        self.check_external_editors(cx);
        if let Some(root) = self.data_root.clone() {
            let stored = DeviceStore::new(&root).load().ok();
            self.editor_choice = stored
                .as_ref()
                .map(|state| state.editor_choice_or_default())
                .unwrap_or_default();
            self.external_editors = stored
                .as_ref()
                .and_then(|state| state.external_editors.clone())
                .unwrap_or_default();
            self.sync_interval = stored
                .as_ref()
                .and_then(|state| state.sync_interval_minutes);
            self.saved_origin = get_origin(&root).unwrap_or(None);
            self.session_limit = stored
                .as_ref()
                .map(|state| state.recent_sessions_limit_or_default())
                .unwrap_or(DEFAULT_SIDEBAR_LIMIT);
            let mut enabled = stored
                .as_ref()
                .map(|state| state.enabled_agents_or_default())
                .unwrap_or_else(|| AgentKind::ALL.to_vec());
            if enabled.is_empty() {
                enabled = AgentKind::ALL.to_vec();
            }
            self.enabled_agents = enabled;
            let mut default = stored
                .as_ref()
                .map(|state| state.default_agent_or_default())
                .unwrap_or(AgentKind::DEFAULT);
            if !self.enabled_agents.contains(&default) {
                default = self
                    .enabled_agents
                    .first()
                    .copied()
                    .unwrap_or(AgentKind::DEFAULT);
            }
            self.default_agent = default;
        } else {
            self.sync_interval = None;
            self.saved_origin = None;
            self.session_limit = DEFAULT_SIDEBAR_LIMIT;
            self.enabled_agents = AgentKind::ALL.to_vec();
            self.default_agent = AgentKind::DEFAULT;
        }
        let limit = self.session_limit;
        self.session_slider.update(cx, |state, cx| {
            state.set_value(limit as f32, window, cx);
        });
        let saved = self.saved_origin.clone().unwrap_or_default();
        self.origin_input.update(cx, |state, cx| {
            state.set_value(saved, window, cx);
        });
        let (branches, current_branch, branch_upstream) =
            load_branch_state(self.data_root.as_ref());
        self.branches = branches;
        self.current_branch = current_branch;
        self.branch_upstream = branch_upstream;
        self.branch_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        if !self.skill_busy {
            self.skill_statuses = crate::agent_skill::Target::ALL
                .into_iter()
                .map(|target| {
                    crate::agent_skill::perform(crate::agent_skill::Action::Status, Some(target))
                        .text
                })
                .collect::<Vec<_>>();
            self.skill_environment = crate::agent_skill::environment();
        }
        self.session_limit_error = None;
        self.enabled_agents_error = None;
        self.default_agent_error = None;
        self.origin_busy = false;
        self.origin_error = None;
        self.origin_notice = None;
        self.interval_error = None;
        self.branch_busy = false;
        self.branch_error = None;
        self.branch_notice = None;
        // Spaces: re-read the portable catalog and drop any inline editor
        // whose row no longer exists.
        self.spaces = load_space_names(self.data_root.as_ref());
        if let Some(SpaceEdit::Rename(name)) = &self.space_edit
            && !self.spaces.iter().any(|space| space_eq(space, name))
        {
            self.space_edit = None;
        }
        if let Some(SpaceEdit::Delete { name, .. }) = &self.space_edit
            && !self.spaces.iter().any(|space| space_eq(space, name))
        {
            self.space_edit = None;
        }
        self.spaces_error = None;
        self.spaces_notice = None;
        cx.notify();
    }

    /// Persist an automatic-sync interval choice (`None` is Off). The
    /// workspace scheduler polls `device.json`, so it picks the new schedule
    /// up within one poll tick — no explicit re-arm needed.
    fn set_sync_interval(&mut self, interval: Option<u64>, cx: &mut Context<Self>) {
        if self.sync_interval == interval {
            return;
        }
        self.sync_interval = interval;
        self.interval_error = None;
        if let Some(root) = self.data_root.clone()
            && let Err(error) = DeviceStore::new(&root).update(|state| {
                state.sync_interval_minutes = interval;
            })
        {
            self.interval_error = Some(format!("Could not save the sync interval: {error:#}"));
        }
        cx.notify();
    }

    /// Save the trimmed input as the portable `origin` remote. Runs git off
    /// the main thread; the dialog stays open with inline feedback.
    fn save_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.origin_busy {
            return;
        }
        let url = self.origin_input.read(cx).value().trim().to_owned();
        if url.is_empty() {
            self.origin_error = Some("Enter a remote URL first.".to_owned());
            self.origin_notice = None;
            cx.notify();
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.origin_error = Some("Portable data is unavailable.".to_owned());
            self.origin_notice = None;
            cx.notify();
            return;
        };
        self.origin_busy = true;
        self.origin_error = None;
        self.origin_notice = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let outcome = cx
                .background_spawn(async move { set_origin(&root, &url).map(|()| url) })
                .await;
            let _ = cx.update(|_, cx| {
                view.update(cx, |this, cx| {
                    this.origin_busy = false;
                    match outcome {
                        Ok(saved) => {
                            this.saved_origin = Some(saved);
                            this.origin_notice = Some("Origin remote saved.".to_owned());
                        }
                        Err(error) => {
                            this.origin_error = Some(format!("Could not save origin: {error:#}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    /// Remove the portable `origin` remote (idempotent). Clears the field on
    /// success so the empty state is visible, not implied.
    fn remove_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.origin_busy {
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.origin_error = Some("Portable data is unavailable.".to_owned());
            self.origin_notice = None;
            cx.notify();
            return;
        };
        self.origin_busy = true;
        self.origin_error = None;
        self.origin_notice = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let outcome = cx
                .background_spawn(async move { clear_origin(&root) })
                .await;
            let _ = cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.origin_busy = false;
                    match outcome {
                        Ok(true) => {
                            this.saved_origin = None;
                            this.origin_input.update(cx, |state, cx| {
                                state.set_value("", window, cx);
                            });
                            this.origin_notice = Some("Origin remote removed.".to_owned());
                        }
                        Ok(false) => {
                            this.origin_notice = Some("No origin remote is configured.".to_owned());
                        }
                        Err(error) => {
                            this.origin_error = Some(format!("Could not remove origin: {error:#}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    /// Delegate a Sync-now press to the owning workspace so the run shares
    /// the single [`SyncTracker`] and reloads Review on completion.
    fn request_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            self.origin_error = Some("Workspace is unavailable.".to_owned());
            cx.notify();
            return;
        };
        workspace
            .update(cx, |this, cx| {
                this.request_sync(window, cx);
            })
            .ok();
        cx.notify();
    }

    /// Ask the owning workspace to reload portable-backed projections after
    /// a branch switch changed the files underneath them.
    fn refresh_projections(&mut self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.clone() {
            workspace
                .update(cx, |this, cx| {
                    this.reload_portable_projections(cx);
                })
                .ok();
        }
    }

    /// Switch the portable repo to branch `name` (existing local branch,
    /// remote-tracking adoption, or fresh creation). Guarded against an
    /// in-flight sync and runs git off the main thread with inline feedback.
    fn switch_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_busy {
            return;
        }
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.branch_error = Some("Enter a branch name first.".to_owned());
            self.branch_notice = None;
            cx.notify();
            return;
        }
        if self.sync_tracker.status() == SyncStatus::Syncing {
            self.branch_error = Some(
                "A sync is running — wait for it to finish before switching branches.".to_owned(),
            );
            self.branch_notice = None;
            cx.notify();
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.branch_error = Some("Portable data is unavailable.".to_owned());
            self.branch_notice = None;
            cx.notify();
            return;
        };
        if self.current_branch.as_deref() == Some(name.as_str()) {
            self.branch_notice = Some(format!("Already on {name}."));
            self.branch_error = None;
            cx.notify();
            return;
        }
        self.branch_busy = true;
        self.branch_error = None;
        self.branch_notice = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let outcome = cx
                .background_spawn(async move { checkout_branch(&root, &name).map(|o| (o, name)) })
                .await;
            let _ = cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.branch_busy = false;
                    match outcome {
                        Ok((outcome, name)) => {
                            let (branches, current_branch, branch_upstream) =
                                load_branch_state(this.data_root.as_ref());
                            this.branches = branches;
                            this.current_branch = current_branch;
                            this.branch_upstream = branch_upstream;
                            this.branch_input.update(cx, |state, cx| {
                                state.set_value("", window, cx);
                            });
                            this.branch_notice = Some(checkout_notice(outcome, &name));
                            this.refresh_projections(cx);
                        }
                        Err(error) => {
                            this.branch_error = Some(format!("Could not switch branch: {error:#}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn set_copy_on_select(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let result = self
            .data_root
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Data directory unavailable"))
            .and_then(|root| {
                DeviceStore::new(root).update(|state| state.terminal_copy_on_select = Some(enabled))
            });
        match result {
            Ok(()) => {
                crate::terminal_preferences::set_copy_on_select(enabled);
                self.terminal_preferences_error = None;
            }
            Err(error) => {
                self.terminal_preferences_error =
                    Some(format!("Could not save terminal preference: {error:#}"))
            }
        }
        cx.notify();
    }

    /// Step the font size and clamp into range. Pure so the bounds stay
    /// unit-testable without a window.
    fn stepped_font_size(current: f32, delta: f32) -> f32 {
        clamp_app_font_size(current + delta)
    }

    pub(crate) fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = clamp_app_font_size(size);
        if (self.font_size - size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = size;
        // Panes read the global at render time, so this applies live; the
        // persisted copy only matters across restarts.
        set_app_font_size(size);
        if let Some(root) = self.data_root.clone() {
            // Non-fatal like the rest of startup persistence: the live size
            // above already applied, a failed write just loses it on restart.
            let _ = DeviceStore::new(&root).update(|state| {
                state.app_font_size = Some(size);
            });
        }
        cx.notify();
    }

    /// Persist the Agent sidebar limit and push it to the owning workspace
    /// so the sidebar re-projects immediately. The live value applies even
    /// when the write fails (same spirit as the font size); the error line
    /// says persistence is what broke. Values snap to the slider step.
    fn set_session_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        let limit = snap_sidebar_limit(limit);
        if self.session_limit == limit {
            return;
        }
        self.session_limit = limit;
        self.session_limit_error = None;
        if let Some(root) = self.data_root.clone()
            && let Err(error) = DeviceStore::new(&root).update(|state| {
                state.recent_sessions_limit = Some(limit as u32);
            })
        {
            self.session_limit_error = Some(format!("Could not save the session limit: {error:#}"));
        }
        if let Some(workspace) = self.workspace.clone() {
            workspace
                .update(cx, |this, cx| this.set_session_limit(limit, cx))
                .ok();
        }
        cx.notify();
    }

    /// Sync the slider thumb after a programmatic limit change (refresh from
    /// disk, reset button). Slider drags already own the thumb, so they call
    /// [`Self::set_session_limit`] directly without this.
    fn sync_session_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let limit = self.session_limit;
        self.session_slider.update(cx, |state, cx| {
            state.set_value(limit as f32, window, cx);
        });
    }

    /// Enable or disable one harness. The last enabled harness cannot be
    /// turned off; disabling the default falls back to the first remaining
    /// enabled harness. Persists to `device.json` and pushes live to the
    /// owning workspace so fresh Agent panes and the New-session picker pick
    /// it up immediately.
    fn set_agent_enabled(&mut self, agent: AgentKind, enabled: bool, cx: &mut Context<Self>) {
        let currently = self.enabled_agents.contains(&agent);
        if currently == enabled {
            return;
        }
        if !enabled && self.enabled_agents.len() <= 1 {
            self.enabled_agents_error = Some("Keep at least one agent enabled.".to_owned());
            cx.notify();
            return;
        }
        let mut next: Vec<AgentKind> = AgentKind::ALL
            .into_iter()
            .filter(|candidate| {
                if *candidate == agent {
                    enabled
                } else {
                    self.enabled_agents.contains(candidate)
                }
            })
            .collect();
        if next.is_empty() {
            next = AgentKind::ALL.to_vec();
        }
        self.enabled_agents = next;
        self.enabled_agents_error = None;
        if !self.enabled_agents.contains(&self.default_agent) {
            let fallback = self
                .enabled_agents
                .first()
                .copied()
                .unwrap_or(AgentKind::DEFAULT);
            self.default_agent = fallback;
            self.default_agent_error = None;
        }
        let enabled_ids = self
            .enabled_agents
            .iter()
            .map(|agent| agent.id().to_owned())
            .collect::<Vec<_>>();
        let default_id = self.default_agent.id().to_owned();
        if let Some(root) = self.data_root.clone()
            && let Err(error) = DeviceStore::new(&root).update(|state| {
                state.enabled_agents = Some(enabled_ids.clone());
                state.default_agent = Some(default_id.clone());
            })
        {
            self.enabled_agents_error =
                Some(format!("Could not save the enabled agents: {error:#}"));
        }
        if let Some(workspace) = self.workspace.clone() {
            let enabled = self.enabled_agents.clone();
            let default = self.default_agent;
            workspace
                .update(cx, |this, cx| this.set_enabled_agents(enabled, default, cx))
                .ok();
        }
        cx.notify();
    }

    /// Persist the default agent harness and push it to the owning workspace
    /// so fresh Agent panes and the New-session picker pick it up
    /// immediately. Only enabled harnesses are selectable; open sessions keep
    /// running with the harness they started with. The live value applies
    /// even when the write fails (same spirit as the font size), and the
    /// error line says persistence is what broke.
    fn set_default_agent(&mut self, agent: AgentKind, cx: &mut Context<Self>) {
        if !self.enabled_agents.contains(&agent) {
            self.default_agent_error =
                Some(format!("{} is disabled — enable it first.", agent.label()));
            cx.notify();
            return;
        }
        if self.default_agent == agent {
            return;
        }
        self.default_agent = agent;
        self.default_agent_error = None;
        if let Some(root) = self.data_root.clone()
            && let Err(error) = DeviceStore::new(&root).update(|state| {
                state.default_agent = Some(agent.id().to_owned());
            })
        {
            self.default_agent_error = Some(format!("Could not save the default agent: {error:#}"));
        }
        if let Some(workspace) = self.workspace.clone() {
            workspace
                .update(cx, |this, cx| this.set_default_agent(agent, cx))
                .ok();
        }
        cx.notify();
    }

    fn set_editor_choice(
        &mut self,
        choice: EditorChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if choice == EditorChoice::Neovim && (self.neovim_checking || !self.neovim_available) {
            return;
        }
        if self.editor_choice == choice {
            return;
        }
        self.editor_error = None;
        if let Some(root) = self.data_root.as_ref()
            && let Err(error) = DeviceStore::new(root).update(|state| {
                state.editor_choice = Some(choice.id().to_owned());
            })
        {
            self.editor_error = Some(format!("Could not save editor choice: {error:#}"));
            cx.notify();
            return;
        }
        self.editor_choice = choice;
        if let Some(workspace) = self.workspace.as_ref() {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.set_editor_choice(choice, window, cx)
                })
                .ok();
        }
        cx.notify();
    }

    fn set_external_editor_enabled(
        &mut self,
        kind: ExternalEditorKind,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if self.external_checking
            || !self
                .installed_editors
                .get(kind.id())
                .copied()
                .unwrap_or(false)
        {
            return;
        }
        let mut editors = self.external_editors.clone();
        editors.insert(kind.id().to_owned(), enabled);
        if let Some(root) = &self.data_root
            && let Err(error) = DeviceStore::new(root)
                .update(|state| state.external_editors = Some(editors.clone()))
        {
            self.editor_error = Some(format!(
                "Could not save external editor preference: {error:#}"
            ));
            cx.notify();
            return;
        }
        self.external_editors = editors;
        self.editor_error = None;
        if let Some(workspace) = &self.workspace {
            let _ = workspace.update(cx, |view, cx| {
                view.set_external_editor_enabled(kind, enabled, cx)
            });
        }
        cx.notify();
    }

    fn check_external_editors(&mut self, cx: &mut Context<Self>) {
        if self.external_checking {
            return;
        }
        self.external_checking = true;
        cx.spawn(async move |view, cx| {
            let installed = cx
                .background_spawn(async {
                    ExternalEditorKind::ALL
                        .into_iter()
                        .map(|kind| (kind.id().to_owned(), kind.installed_executable().is_some()))
                        .collect()
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.installed_editors = installed;
                this.external_checking = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn check_neovim(&mut self, cx: &mut Context<Self>) {
        if self.neovim_checking {
            return;
        }
        self.neovim_checking = true;
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_spawn(async { crate::editor::neovim_available() })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.neovim_available = matches!(result, Ok(true));
                this.neovim_check_error = result.err().map(|error| error.to_string());
                this.neovim_checking = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if section == SettingsSection::Editor {
            self.check_neovim(cx);
            self.check_external_editors(cx);
        }
        if self.active_section != section {
            self.active_section = section;
            cx.notify();
        }
    }

    /// Add a space to the portable catalog, then re-project the workspace.
    fn add_space(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.data_root.clone() else {
            self.spaces_error = Some("Portable data is unavailable".into());
            cx.notify();
            return;
        };
        let raw = self.space_input.read(cx).value().to_string();
        let result = (|| -> anyhow::Result<String> {
            let name = crate::data::normalize_name(&raw)?;
            let mut catalog = Spaces::load(&root)?;
            let expected = catalog.clone();
            anyhow::ensure!(
                !catalog.contains(&name),
                "The space {name:?} already exists"
            );
            catalog.ensure_name(&name);
            catalog.save(&root, &expected)?;
            Ok(name)
        })();
        match result {
            Ok(name) => {
                self.space_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.after_space_change(Some(format!("Added space {name:?}")), None, cx);
            }
            Err(error) => self.after_space_change(None, Some(format!("{error:#}")), cx),
        }
    }

    /// Start renaming one space: the row's own input prefills with its name.
    fn begin_space_rename(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.space_rename_input
            .update(cx, |input, cx| input.set_value(name.to_owned(), window, cx));
        self.space_edit = Some(SpaceEdit::Rename(name.to_owned()));
        self.spaces_error = None;
        self.spaces_notice = None;
        cx.notify();
    }

    /// Apply the pending rename. References are rewritten atomically with the
    /// catalog, so a failure changes nothing.
    fn save_space_rename(&mut self, cx: &mut Context<Self>) {
        let Some(SpaceEdit::Rename(from)) = self.space_edit.clone() else {
            return;
        };
        if self.has_unsaved_artifact_draft(cx) {
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.spaces_error = Some("Portable data is unavailable".into());
            cx.notify();
            return;
        };
        let to = self.space_rename_input.read(cx).value().to_string();
        match rename_space(&root, &from, &to) {
            Ok(touched) => self.after_space_change(
                Some(space_rewrite_notice(&format!("Renamed {from:?}"), &touched)),
                None,
                cx,
            ),
            Err(error) => self.after_space_change(None, Some(format!("{error:#}")), cx),
        }
    }

    /// Start deleting one space; the destination defaults to the first other
    /// space so the confirm action is always well-defined.
    fn begin_space_delete(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(destination) = self
            .spaces
            .iter()
            .find(|space| !space_eq(space, name))
            .cloned()
        else {
            return;
        };
        self.space_edit = Some(SpaceEdit::Delete {
            name: name.to_owned(),
            destination,
        });
        self.spaces_error = None;
        self.spaces_notice = None;
        cx.notify();
    }

    /// Delete the pending space after moving its records to the destination.
    fn confirm_space_delete(&mut self, cx: &mut Context<Self>) {
        let Some(SpaceEdit::Delete { name, destination }) = self.space_edit.clone() else {
            return;
        };
        if self.has_unsaved_artifact_draft(cx) {
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.spaces_error = Some("Portable data is unavailable".into());
            cx.notify();
            return;
        };
        match delete_space(&root, &name, &destination) {
            Ok(touched) => self.after_space_change(
                Some(space_rewrite_notice(
                    &format!("Deleted {name:?} · records moved to {destination:?}"),
                    &touched,
                )),
                None,
                cx,
            ),
            Err(error) => self.after_space_change(None, Some(format!("{error:#}")), cx),
        }
    }

    /// Refuse a catalog edit while any artifact draft is unsaved: renaming
    /// or deleting rewrites the records drafts are based on and can strand
    /// their stashed keys. Returns true when the caller must stop.
    fn has_unsaved_artifact_draft(&mut self, cx: &mut Context<Self>) -> bool {
        let blocked = self
            .workspace
            .clone()
            .and_then(|workspace| {
                workspace
                    .update(cx, |this, cx| this.has_artifact_draft(cx))
                    .ok()
            })
            .unwrap_or(false);
        if blocked {
            self.spaces_error = Some(
                "Save or cancel the unsaved artifact draft first (Home → Artifacts or the repository Resources tab)."
                    .into(),
            );
            cx.notify();
        }
        blocked
    }

    fn cancel_space_edit(&mut self, cx: &mut Context<Self>) {
        self.space_edit = None;
        self.spaces_error = None;
        cx.notify();
    }

    /// Common tail for catalog edits: refresh the list, clear the inline
    /// editor, and let the workspace re-project every space-filtered surface.
    fn after_space_change(
        &mut self,
        notice: Option<String>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.spaces = load_space_names(self.data_root.as_ref());
        self.space_edit = None;
        self.spaces_error = error;
        self.spaces_notice = notice;
        if let Some(workspace) = self.workspace.clone() {
            workspace.update(cx, |this, cx| this.reload_spaces(cx)).ok();
        }
        cx.notify();
    }

    /// Forward the command-bar toggles to the workspace, mirroring
    /// `TerminalPane::on_key_down` / `ReviewView::on_key_down`. Tab jumps and
    /// session creation are intentionally not forwarded: they live in
    /// navigation mode and the palettes now. All other keys bubble normally.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mode) = palette_mode_for_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            event.keystroke.modifiers.alt,
        ) {
            match mode {
                PaletteMode::Actions => {
                    window.dispatch_action(Box::new(ToggleActionsPalette), cx);
                }
                PaletteMode::Projects => {
                    window.dispatch_action(Box::new(ToggleProjectsPalette), cx);
                }
            }
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // No sidebar title: the dialog already carries a "Settings" title.
        v_flex()
            .w(px(154.))
            .flex_none()
            .h_full()
            .py_2()
            .px_2()
            .gap_1()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(SettingsSection::ALL.into_iter().map(|section| {
                let selected = section == self.active_section;
                div()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    })
                    .when(!selected, |this| {
                        this.text_color(cx.theme().muted_foreground)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select_section(section, cx)),
                    )
                    .child(section.label())
            }))
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .gap_3()
            .items_baseline()
            .flex_wrap()
            .child(
                div()
                    .text_size(px(16.))
                    .font_semibold()
                    .child(self.active_section.label()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.active_section.description()),
            )
    }

    /// Right-hand column: fixed section header on top, scrollable body below
    /// so long sections (and short windows) scroll inside the dialog instead
    /// of clipping.
    fn render_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(self.render_header(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .gap_4()
                            .px_4()
                            .py_3()
                            .w_full()
                            .max_w(px(760.))
                            .child(match self.active_section {
                                SettingsSection::General => {
                                    self.render_general(cx).into_any_element()
                                }
                                SettingsSection::Spaces => {
                                    self.render_spaces(cx).into_any_element()
                                }
                                SettingsSection::Sync => self.render_sync(cx).into_any_element(),
                                SettingsSection::Agent => self.render_agent(cx).into_any_element(),
                                SettingsSection::Editor => {
                                    self.render_editor(cx).into_any_element()
                                }
                                SettingsSection::Keybindings => {
                                    self.render_keybindings(cx).into_any_element()
                                }
                            }),
                    ),
            )
    }

    /// Spaces: the portable isolation profiles. Switching happens from the
    /// Home titlebar (or `s` in navigation mode); this section manages the
    /// catalog itself.
    fn render_spaces(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let last_space = self.spaces.len() <= 1;
        v_flex()
            .gap_4()
            .child(
                group(cx,
                    "Manage spaces",
                    Some("Each space has its own repositories and artifacts. Switch from Home or press s in navigation mode."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Input::new(&self.space_input).small()))
                        .child(
                            Button::new("add-space").small()
                                .primary()
                                .label("Add space")
                                .on_click(cx.listener(|this, _, window, cx| this.add_space(window, cx))),
                        ),
                )
                .child({
                    let rows: Vec<AnyElement> = self
                        .spaces
                        .iter()
                        .map(|name| self.render_space_row(name, last_space, cx))
                        .collect();
                    v_flex().gap_2().children(rows)
                }),
            )
            .when_some(self.spaces_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .when_some(self.spaces_notice.clone(), |this, notice| {
                this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(notice))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Deleting moves records to another space. Keep at least one space, and save artifact drafts before renaming or deleting."),
            )
    }

    /// One space row: the name plus rename/delete actions, or the inline
    /// editor for the pending action.
    fn render_space_row(&self, name: &str, last_space: bool, cx: &mut Context<Self>) -> AnyElement {
        match self.space_edit.clone() {
            Some(SpaceEdit::Rename(target)) if space_eq(&target, name) => h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.space_rename_input).small()),
                )
                .child(
                    Button::new("save-space-rename")
                        .small()
                        .primary()
                        .label("Rename")
                        .on_click(cx.listener(|this, _, _, cx| this.save_space_rename(cx))),
                )
                .child(
                    Button::new("cancel-space-rename")
                        .small()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_space_edit(cx))),
                )
                .into_any_element(),
            Some(SpaceEdit::Delete {
                name: target,
                destination,
            }) if space_eq(&target, name) => {
                let choices: Vec<String> = self
                    .spaces
                    .iter()
                    .filter(|space| !space_eq(space, name))
                    .cloned()
                    .collect();
                let row = cx.entity().downgrade();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(format!("Move everything in {name:?} to")),
                    )
                    .child(
                        Button::new("delete-space-destination")
                            .small()
                            .outline()
                            .label(destination.clone())
                            .dropdown_menu(move |mut menu, _, _| {
                                for choice in &choices {
                                    let choice = choice.clone();
                                    let row = row.clone();
                                    let selected = choice == destination;
                                    menu = menu.item(
                                        PopupMenuItem::new(choice.clone())
                                            .checked(selected)
                                            .on_click(move |_, _, cx| {
                                                let choice = choice.clone();
                                                let _ = row.update(cx, |this, cx| {
                                                    if let Some(SpaceEdit::Delete {
                                                        destination,
                                                        ..
                                                    }) = &mut this.space_edit
                                                    {
                                                        *destination = choice.clone();
                                                    }
                                                    cx.notify();
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        Button::new("confirm-space-delete")
                            .small()
                            .danger()
                            .label("Delete space")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_space_delete(cx))),
                    )
                    .child(
                        Button::new("cancel-space-delete")
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_space_edit(cx))),
                    )
                    .into_any_element()
            }
            _ => {
                let rename_name = name.to_owned();
                let delete_name = name.to_owned();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(name.to_owned()),
                    )
                    .child(
                        Button::new(format!("rename-space-{name}"))
                            .ghost()
                            .label("Rename")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_space_rename(&rename_name, window, cx)
                            })),
                    )
                    .child(
                        Button::new(format!("delete-space-{name}"))
                            .ghost()
                            .label("Delete")
                            .disabled(last_space)
                            .tooltip(if last_space {
                                "Keep at least one space"
                            } else {
                                "Move this space's records to another space and delete it"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.begin_space_delete(&delete_name, cx)
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_tiles = self.editor_icon_tiles.clone();
        v_flex().gap_4()
            .child(v_flex().gap_2()
                .child(v_flex().gap_1()
                    .child(h_flex().gap_2().items_center().justify_between()
                        .child(div().text_sm().font_semibold().text_color(cx.theme().foreground).child("Default editor"))
                        .child(Button::new("check-neovim").small().label("Refresh").ghost().disabled(self.neovim_checking)
                            .on_click(cx.listener(|this, _, _, cx| this.check_neovim(cx)))))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Choose how project files open. Existing sessions and unsaved drafts are preserved.")))
                .child(gpui_kit::base::RadioGroup::new("default-editor-cards")
                    .axis(gpui_kit::Axis::Horizontal).flex().flex_row().items_stretch().gap_3().w_full()
                    .children([EditorChoice::BuiltIn, EditorChoice::Neovim].into_iter().enumerate().map(|(index, choice)| {
                        let selected = self.editor_choice == choice;
                        let unavailable = choice == EditorChoice::Neovim && (self.neovim_checking || !self.neovim_available);
                        let view = cx.entity().downgrade();
                        let focus = self.editor_card_focus[index].clone();
                        let title = choice.label();
                        let tiles = editor_tiles.clone();
                        let description = match choice {
                            EditorChoice::BuiltIn => "File tabs, search, Markdown preview, and language support.",
                            EditorChoice::Neovim => "Your Neovim configuration and plugins in a terminal.",
                        };
                        let icon: AnyElement = match choice {
                            EditorChoice::BuiltIn => crate::editor_icons::editor_icon(crate::editor_icons::EditorIcon::Devcroft, &tiles, crate::editor_icons::ICON_PX),
                            EditorChoice::Neovim => crate::editor_icons::editor_icon(crate::editor_icons::EditorIcon::Neovim, &tiles, crate::editor_icons::ICON_PX),
                        };
                        Radio::new(format!("default-editor-{}", choice.id()))
                            .accessibility_label(format!("Default editor: {title}{}", if unavailable { " (unavailable)" } else { "" }))
                            .track_focus(&focus).set_position(index + 1, 2)
                            .checked(selected).disabled(unavailable)
                            .flex().flex_row().items_start().gap_2()
                            .flex_1().min_w_0().px_3().py_2()
                            .focus(|style| style.border_color(rgb(0x9acfff)))
                            .child(div().size(px(16.)).mt(px(1.)).flex_shrink_0().rounded_full().border_1()
                                .border_color(rgb(if unavailable { 0x4b5057 } else if selected { 0x61afef } else { 0x737983 }))
                                .flex().items_center().justify_center()
                                .when(selected, |circle| circle.child(div().size(px(7.)).rounded_full().bg(rgb(if unavailable { 0x737983 } else { 0x61afef })))))
                            .when(!unavailable, |radio| radio
                                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                    focus.focus(window, cx);
                                    cx.stop_propagation();
                                })
                                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                                    let next = match event.keystroke.key.as_str() {
                                        "space" | "enter" => Some(choice),
                                        "arrowdown" | "arrowright" | "down" | "right" => Some(EditorChoice::Neovim),
                                        "arrowup" | "arrowleft" | "up" | "left" => Some(EditorChoice::BuiltIn),
                                        _ => None,
                                    };
                                    if let Some(next) = next {
                                        this.set_editor_choice(next, window, cx);
                                        if this.editor_choice == next {
                                            this.editor_card_focus[usize::from(next == EditorChoice::Neovim)].focus(window, cx);
                                        }
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    }
                                })))
                            .child(v_flex().gap_1().flex_1().min_w_0()
                                .child(h_flex().items_center().gap_2()
                                    .child(icon)
                                    .child(div().text_sm().font_semibold().text_color(cx.theme().foreground).child(title)))
                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(description))
                                .when(choice == EditorChoice::Neovim, |card| {
                                    let status = if self.neovim_checking {
                                        "Checking for Neovim…".to_owned()
                                    } else if let Some(error) = &self.neovim_check_error {
                                        error.clone()
                                    } else if self.neovim_available {
                                        "Available in your login shell".to_owned()
                                    } else {
                                        "Neovim was not found. Install nvim and make it available on PATH.".to_owned()
                                    };
                                    let status_color = if self.neovim_checking {
                                        cx.theme().muted_foreground
                                    } else if self.neovim_check_error.is_some() || !self.neovim_available {
                                        cx.theme().danger
                                    } else {
                                        cx.theme().success
                                    };
                                    card.child(div().text_xs().text_color(status_color).child(status))
                                }))
                            .on_change(move |_, _, window, cx| { let _ = view.update(cx, |this, cx| this.set_editor_choice(choice, window, cx)); })
                            .map(|radio| v_flex().flex_1().min_w_0().rounded_md().border_1()
                                .border_color(if selected { cx.theme().ring } else { cx.theme().border })
                                .bg(if selected { cx.theme().accent } else { cx.theme().background })
                                .child(radio))
                    }))))
            .when_some(self.editor_error.clone(), |view, error| view.child(div().text_sm().text_color(cx.theme().danger).child(error)))
            .child(self.language_servers.clone())
            .child(v_flex().gap_2()
                .child(v_flex().gap_1()
                    .child(h_flex().gap_2().items_center().justify_between()
                        .child(div().text_sm().font_semibold().text_color(cx.theme().foreground).child("External editors"))
                        .child(Button::new("check-external-editors").small().label("Refresh").ghost().disabled(self.external_checking)
                            .on_click(cx.listener(|this, _, _, cx| this.check_external_editors(cx)))))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Choose which applications appear in the editor’s Open in menu.")))
                .children(ExternalEditorKind::ALL.into_iter().map(|kind| {
                    let installed = self.installed_editors.get(kind.id()).copied().unwrap_or(false);
                    let enabled = self.external_editors.get(kind.id()).copied().unwrap_or(true);
                    let view = cx.entity().downgrade();
                    let icon = match kind {
                        ExternalEditorKind::Zed => crate::editor_icons::editor_icon(crate::editor_icons::EditorIcon::Zed, &editor_tiles, crate::editor_icons::ICON_PX),
                        ExternalEditorKind::VsCode => crate::editor_icons::editor_icon(crate::editor_icons::EditorIcon::VsCode, &editor_tiles, crate::editor_icons::ICON_PX),
                    };
                    div().px_3().py_2().rounded_md().border_1().border_color(cx.theme().border).bg(cx.theme().background)
                        .child(h_flex().gap_3().items_center().justify_between()
                            .child(h_flex().gap_2().items_center().flex_1().min_w_0()
                                .child(icon)
                                .child(v_flex().gap_0().flex_1().min_w_0()
                                    .child(div().text_sm().font_semibold().text_color(cx.theme().foreground).child(kind.label()))
                                    .child(div().text_xs().text_color(if self.external_checking { cx.theme().muted_foreground } else if installed { cx.theme().success } else { cx.theme().danger })
                                        .child(if self.external_checking { "Checking installation…" } else if installed { "Installed · Available from the editor’s Open in menu." } else { "Not found. Install the application or its command-line launcher." }))))
                            .child(Switch::new(format!("external-editor-{}", kind.id())).checked(enabled && installed).disabled(!installed || self.external_checking)
                                .on_change(move |enabled, _, cx| { let _ = view.update(cx, |this, cx| this.set_external_editor_enabled(kind, *enabled, cx)); }))
                    )
                })))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let size = self.font_size;
        let is_default = (size - DEFAULT_APP_FONT_SIZE).abs() < f32::EPSILON;
        v_flex()
            .gap_4()
            .child(group(cx, "Terminal", None).child(live_row(cx,
                "Copy on select",
                "Automatically copy selected terminal text. Hold Shift to select inside mouse-aware applications. Explicit copy: Ctrl+Shift+C (Cmd+C on macOS).",
                Switch::new("terminal-copy-on-select")
                    .checked(crate::terminal_preferences::copy_on_select())
                    .on_change(move |enabled, _, cx| {
                        view.update(cx, |this, cx| this.set_copy_on_select(*enabled, cx)).ok();
                    }),
            )))
            .when_some(self.terminal_preferences_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .child(
                group(cx, "Appearance", None).child(live_row(cx,
                    "App font size",
                    "Applies to the Agent, Editor, Terminal, and Review panes.",
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(step_button(cx,
                            "-",
                            cx.listener(move |this, _, _, cx| {
                                this.set_font_size(SettingsView::stepped_font_size(size, -1.0), cx)
                            }),
                        ))
                        .child(
                            div()
                                .w(px(64.))
                                .text_center()
                                .text_sm()
                                .text_color(cx.theme().foreground)
                                .child(format_font_size(size)),
                        )
                        .child(step_button(cx,
                            "+",
                            cx.listener(move |this, _, _, cx| {
                                this.set_font_size(SettingsView::stepped_font_size(size, 1.0), cx)
                            }),
                        ))
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                            "{}–{} px",
                            MIN_APP_FONT_SIZE as u32, MAX_APP_FONT_SIZE as u32
                        )))
                        .when(!is_default, |this| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            this.set_font_size(DEFAULT_APP_FONT_SIZE, cx)
                                        }),
                                    )
                                    .child(format!(
                                        "Reset to {}",
                                        format_font_size(DEFAULT_APP_FONT_SIZE)
                                    )),
                            )
                        }),
                )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(
                        div()
                            .font_family(TERMINAL_FONT_FAMILY)
                            .text_size(px(size))
                            .text_color(cx.theme().foreground)
                            .child("The quick brown fox jumps over the lazy dog 0123456789"),
                    )
                    .child(
                        div()
                            .font_family(TERMINAL_FONT_FAMILY)
                            .text_size(px(review_font_size()))
                            .text_color(cx.theme().muted_foreground)
                            .child("Review diffs render proportionally smaller  +12 −34"),
                    ),
            )
    }

    fn render_sync(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let save_view = view.clone();
        let remove_view = view.clone();
        let sync_view = view.clone();
        let checkout_view = view.clone();
        let syncing = self.sync_tracker.status() == SyncStatus::Syncing;
        let (status_text, status_is_error) = sync_status_line(
            self.sync_tracker.status(),
            self.sync_tracker.last_error().as_deref(),
        );
        let interval = self.sync_interval;
        let custom_note = match interval {
            Some(minutes) if !is_supported_sync_interval(minutes) => Some(format!(
                "Custom interval from device.json: every {minutes} minutes."
            )),
            _ => None,
        };
        let saved_line = match self.saved_origin.as_deref() {
            Some(url) if !url.is_empty() => format!("Saved origin: {url}"),
            _ => "No origin remote configured.".to_owned(),
        };
        let repo_line = match self.data_root.as_ref() {
            Some(root) => format!("Local repo: {}", root.portable_dir().display()),
            None => "Portable data is unavailable.".to_owned(),
        };
        v_flex()
            .gap_4()
            .child(
                group(cx,
                    "Remote",
                    Some("Sync repositories, artifacts, and spaces through Git. Device preferences stay local."),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Origin URL"),
                        )
                        .child(
                            // Stop the click at the field: the Settings root
                            // focuses the view itself on every mouse-down,
                            // which would steal focus back from the input
                            // right after it focuses itself and leave it
                            // untypable. The field is deeper, so its own
                            // focus handling still runs first.
                            div()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .child(Input::new(&self.origin_input).small()),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("sync-save-origin").small()
                                        .label(if self.origin_busy {
                                            "Saving…"
                                        } else {
                                            "Save"
                                        })
                                        .primary()
                                        .on_click(move |_, window, cx| {
                                            save_view
                                                .update(cx, |this, cx| {
                                                    this.save_origin(window, cx)
                                                })
                                                .ok();
                                        }),
                                )
                                .child(
                                    Button::new("sync-remove-origin").small()
                                        .label("Remove")
                                        .ghost()
                                        .on_click(move |_, window, cx| {
                                            remove_view
                                                .update(cx, |this, cx| {
                                                    this.remove_origin(window, cx)
                                                })
                                                .ok();
                                        }),
                                ),
                        )
                        .when_some(self.origin_error.clone(), |this, error| {
                            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        })
                        .when_some(self.origin_notice.clone(), |this, notice| {
                            this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(notice))
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(saved_line),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(repo_line),
                        ),
                ),
            )
            .child(
                group(cx,
                    "Branch",
                    Some("Local branch of the portable repo. Switching replaces portable files with that branch's content."),
                )
                .child(self.render_branch_current(cx))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(self.branches.iter().map(|branch| {
                            let selected = self.current_branch.as_deref() == Some(branch.as_str());
                            let name = branch.clone();
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .text_sm()
                                .cursor_pointer()
                                .when(selected, |this| {
                                    this.border_color(cx.theme().ring)
                                        .bg(cx.theme().accent)
                                        .text_color(cx.theme().accent_foreground)
                                        .text_color(cx.theme().foreground)
                                })
                                .when(!selected, |this| {
                                    this.border_color(cx.theme().border)
                                        .bg(cx.theme().background)
                                        .text_color(cx.theme().muted_foreground)
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.switch_branch(name.clone(), window, cx)
                                    }),
                                )
                                .child(branch.clone())
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .child(Input::new(&self.branch_input).small()),
                        )
                        .child(
                            Button::new("sync-checkout-branch").small()
                                .label(if self.branch_busy {
                                    "Switching…"
                                } else {
                                    "Switch or create"
                                })
                                .primary()
                                .on_click(move |_, window, cx| {
                                    checkout_view
                                        .update(cx, |this, cx| {
                                            let name =
                                                this.branch_input.read(cx).value().to_string();
                                            this.switch_branch(name, window, cx)
                                        })
                                        .ok();
                                }),
                        ),
                )
                .when_some(self.branch_error.clone(), |this, error| {
                    this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                })
                .when_some(self.branch_notice.clone(), |this, notice| {
                    this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(notice))
                }),
            )
            .child(
                group(cx,
                    "Automatic sync",
                    Some("Sync in the background on a schedule. Manual sync is always available from the command palette."),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(
                            [None]
                                .into_iter()
                                .chain(SYNC_INTERVAL_OPTIONS.into_iter().map(Some))
                                .map(|option| {
                                    let selected = interval == option;
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .text_sm()
                                    .cursor_pointer()
                                    .when(selected, |this| {
                                        this.border_color(cx.theme().ring)
                                            .bg(cx.theme().accent)
                                        .text_color(cx.theme().accent_foreground)
                                            .text_color(cx.theme().foreground)
                                    })
                                    .when(!selected, |this| {
                                        this.border_color(cx.theme().border)
                                            .bg(cx.theme().background)
                                            .text_color(cx.theme().muted_foreground)
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            this.set_sync_interval(option, cx)
                                        }),
                                    )
                                    .child(format_sync_interval(option))
                            }),
                        ),
                )
                .when_some(custom_note, |this, note| {
                    this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(note))
                })
                .when_some(self.interval_error.clone(), |this, error| {
                    this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                }),
            )
            .child(
                group(cx,
                    "Manual sync",
                    Some("Stage, commit, fetch, rebase, and push portable data now."),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            Button::new("sync-now").small()
                                .label(if syncing { "Syncing…" } else { "Sync now" })
                                .primary()
                                .loading(syncing)
                                .on_click(move |_, window, cx| {
                                    sync_view
                                        .update(cx, |this, cx| {
                                            this.request_sync(window, cx)
                                        })
                                        .ok();
                                }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(if status_is_error {
                                    cx.theme().danger
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(status_text),
                        ),
                ),
            )
    }

    /// Current-branch line for the Branch group: the checked-out branch plus
    /// what sync will push to, or the honest degraded states.
    fn render_branch_current(&self, cx: &App) -> impl IntoElement {
        let current = match self.current_branch.as_deref() {
            Some(branch) if !branch.is_empty() => format!("Current: {branch}"),
            _ if self.data_root.is_none() => "Portable data is unavailable.".to_owned(),
            _ if !self.branches.is_empty() => {
                "Detached HEAD — switch to a branch to sync.".to_owned()
            }
            _ => "No branches yet — create one below.".to_owned(),
        };
        let tracking = match self.branch_upstream.as_deref() {
            Some(upstream) if !upstream.is_empty() => format!("Tracking {upstream}."),
            _ if self.current_branch.is_some() => {
                "No upstream — set automatically on next sync.".to_owned()
            }
            _ => "".to_owned(),
        };
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(current),
            )
            .when(!tracking.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(tracking),
                )
            })
    }

    fn manage_skill(
        &mut self,
        action: crate::agent_skill::Action,
        target: Option<crate::agent_skill::Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.skill_busy {
            return;
        }
        self.skill_busy = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let (statuses, environment) = cx
                .background_spawn(async move {
                    // Run the requested action, then re-read per-target status
                    // so the cards show the fresh result.
                    let _ = crate::agent_skill::perform(action, target);
                    let statuses = crate::agent_skill::Target::ALL
                        .into_iter()
                        .map(|target| {
                            crate::agent_skill::perform(
                                crate::agent_skill::Action::Status,
                                Some(target),
                            )
                            .text
                        })
                        .collect::<Vec<_>>();
                    (statuses, crate::agent_skill::environment())
                })
                .await;
            let _ = cx.update(|_, cx| {
                view.update(cx, |this, cx| {
                    this.skill_busy = false;
                    this.skill_statuses = statuses;
                    this.skill_environment = environment;
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn render_agent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::agent_skill::{Action, Target};
        let busy = self.skill_busy;
        let current = self.default_agent;
        let enabled = self.enabled_agents.clone();
        let tiles = self.agent_icon_tiles.clone();
        let view = cx.entity().downgrade();

        let agents = group(
            cx,
            "Agents",
            Some("Enabled agents appear in New session and the default-agent menu."),
        )
        .child(
            v_flex()
                .gap_2()
                .children(
                    AgentKind::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(index, agent)| {
                            let is_enabled = enabled.contains(&agent);
                            let icon = agent_icons::agent_icon(agent, &tiles, agent_icons::ICON_PX);
                            let toggle_view = view.clone();
                            div()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().background)
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .flex_1()
                                                .min_w_0()
                                                .child(icon)
                                                .child(
                                                    v_flex()
                                                        .gap_0()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .font_semibold()
                                                                .text_color(cx.theme().foreground)
                                                                .child(agent.label().to_owned()),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child(
                                                                    agent.description().to_owned(),
                                                                ),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            Switch::new(("agent-enabled", index))
                                                .checked(is_enabled)
                                                .on_change(move |next, _, cx| {
                                                    toggle_view
                                                        .update(cx, |this, cx| {
                                                            this.set_agent_enabled(agent, *next, cx)
                                                        })
                                                        .ok();
                                                }),
                                        ),
                                )
                        }),
                ),
        )
        .when_some(self.enabled_agents_error.clone(), |this, error| {
            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
        });

        let dropdown_view = view.clone();
        let dropdown_enabled = enabled.clone();
        let dropdown_tiles = tiles.clone();
        let current_icon = agent_icons::agent_icon(current, &tiles, agent_icons::ICON_PX);
        let default = group(cx,
            "Default agent",
            Some("Launched when the Agent tab opens without history, and pre-selected in New session. Open sessions keep running."),
        )
        .child(
            h_flex().gap_2().items_center().child(
                Button::new("default-agent").small()
                    .accessibility_label(current.label().to_owned())
                    .outline()
                    .dropdown_caret(true)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(current_icon)
                            .child(div().child(current.label().to_owned())),
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        for agent in dropdown_enabled.clone() {
                            let item_view = dropdown_view.clone();
                            let row_tiles = dropdown_tiles.clone();
                            menu = menu.item(
                                PopupMenuItem::element(move |_, _| {
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(agent_icons::agent_icon(
                                            agent,
                                            &row_tiles,
                                            agent_icons::ICON_PX,
                                        ))
                                        .child(div().child(agent.label().to_owned()))
                                })
                                .checked(agent == current)
                                .on_click(move |_, _, cx| {
                                    item_view
                                        .update(cx, |this, cx| {
                                            this.set_default_agent(agent, cx)
                                        })
                                        .ok();
                                }),
                            );
                        }
                        menu
                    }),
            ),
        )
        .when_some(self.default_agent_error.clone(), |this, error| {
            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
        });

        let limit = self.session_limit;
        let is_default = limit == DEFAULT_SIDEBAR_LIMIT;
        let sessions = group(cx,
            "Sessions",
            Some("How many recent sessions the Agent sidebar lists per repository."),
        )
        .child(
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Slider::new(&self.session_slider)),
                        )
                        .child(
                            div()
                                .w(px(48.))
                                .text_center()
                                .text_sm()
                                .text_color(cx.theme().foreground)
                                .child(format!("{limit}")),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                            "{MIN_SIDEBAR_LIMIT}–{MAX_SIDEBAR_LIMIT} · step {SIDEBAR_LIMIT_STEP}"
                        )))
                        .when(!is_default, |this| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.set_session_limit(DEFAULT_SIDEBAR_LIMIT, cx);
                                            this.sync_session_slider(window, cx);
                                        }),
                                    )
                                    .child(format!("Reset to {DEFAULT_SIDEBAR_LIMIT}")),
                            )
                        }),
                ),
        )
        .when_some(self.session_limit_error.clone(), |this, error| {
            this.child(div().text_sm().text_color(cx.theme().danger).child(error))
        });

        let skill_header = v_flex().gap_1().child(
            h_flex()
                .gap_2()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child("Devcroft skill".to_owned()),
                )
                .child(
                    div().flex_none().child(
                        Button::new("skill-status").small()
                            .label(if busy { "Working…" } else { "Refresh" })
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.manage_skill(Action::Status, None, window, cx);
                            })),
                    ),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Teach agents to use repository artifacts and artifact/review comments. OpenCode also reads these skill locations."
                        .to_owned(),
                ),
        );
        let mut skill_cards = v_flex().gap_2();
        for (index, target) in Target::ALL.into_iter().enumerate() {
            let status = self
                .skill_statuses
                .get(index)
                .cloned()
                .unwrap_or_else(|| "Status unavailable.".to_owned());
            let mut lines = status.lines();
            let status_line = lines.next().unwrap_or("").trim().to_owned();
            let path_line = lines.next().unwrap_or("").trim().to_owned();
            skill_cards = skill_cards.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        v_flex()
                                            .gap_0()
                                            .flex_1()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_semibold()
                                                    .text_color(cx.theme().foreground)
                                                    .child(target.label().to_owned()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(target.description().to_owned()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(target.location_label().to_owned()),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Button::new(("skill-install", index))
                                                    .label("Install / update")
                                                    .disabled(busy)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.manage_skill(
                                                                Action::Install,
                                                                Some(target),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                            .child(
                                                Button::new(("skill-remove", index))
                                                    .label("Remove")
                                                    .disabled(busy)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.manage_skill(
                                                                Action::Uninstall,
                                                                Some(target),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_0()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(status_line),
                                    )
                                    .when(!path_line.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(path_line),
                                        )
                                    }),
                            ),
                    ),
            );
        }
        let skill = v_flex()
            .gap_3()
            .child(skill_header)
            .child(skill_cards)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.skill_environment.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Updates and removal preserve modified or unmanaged skill folders."),
            );
        v_flex()
            .gap_4()
            .child(agents)
            .child(default)
            .child(sessions)
            .child(skill)
    }

    fn render_keybindings(&self, cx: &App) -> impl IntoElement {
        v_flex().gap_4()
            .child(group(cx, "Global shortcuts", Some("Available everywhere, including terminals."))
                .child(live_row(cx, "Actions palette", "Find commands, settings, and sync.", kbd(cx, if cfg!(target_os = "macos") { "⌘K" } else { "Ctrl+K" })))
                .child(live_row(cx, "Projects palette", "Switch between recent repositories.", kbd(cx, if cfg!(target_os = "macos") { "⌘P" } else { "Ctrl+P" })))
                .child(live_row(cx, "Navigation mode", "Show shortcuts for the current tab.", kbd(cx, if cfg!(target_os = "macos") { "⌘J" } else { "Ctrl+J" })))
                .child(live_row(cx, "Close dialog or palette", "For Git changes, use Shift+Esc or the close button.", kbd(cx, "Esc"))))
            .child(group(cx, "In navigation mode", Some("Press a key after opening navigation mode. Available actions appear in the overlay."))
                .child(live_row(cx, "Switch tab", "Agent · Editor · Terminal · Review · Resources", kbd(cx, "a e t d r")))
                .child(live_row(cx, "Change pane", "Move focus left or right.", kbd(cx, "h / l")))
                .child(live_row(cx, "Move through items", "Move down or up in the active pane.", kbd(cx, "j / k")))
                .child(live_row(cx, "Cycle controls", "Use Shift+Tab to move backward.", kbd(cx, "Tab")))
                .child(live_row(cx, "Accept focus", "Open the selected item where supported.", kbd(cx, "Enter")))
                .child(live_row(cx, "Markdown preview", "Toggle preview for a Markdown file in the built-in editor.", kbd(cx, "p"))))
    }
}

fn format_font_size(size: f32) -> String {
    if size.fract() == 0.0 {
        format!("{:.0} px", size)
    } else {
        format!("{:.1} px", size)
    }
}

/// Human label for an automatic-sync choice (`None` is Off). Pure so the
/// interval pills stay unit-testable without a window.
fn format_sync_interval(interval: Option<u64>) -> String {
    match interval {
        None => "Off".to_owned(),
        Some(minutes) => format!("Every {minutes} min"),
    }
}

/// One-line sync status for the Manual group. Returns the line plus whether
/// it is an error (red) or informational (muted). Pure for tests; the
/// rendered line reads the shared [`SyncTracker`].
fn sync_status_line(status: SyncStatus, last_error: Option<&str>) -> (String, bool) {
    match status {
        SyncStatus::Idle => ("Idle".to_owned(), false),
        SyncStatus::Syncing => ("Syncing…".to_owned(), false),
        SyncStatus::Error => match last_error {
            Some(error) if !error.trim().is_empty() => (format!("Error: {}", error.trim()), true),
            _ => ("Error.".to_owned(), true),
        },
    }
}

/// Branch state for the Sync section, read tolerantly from disk: anything
/// unreadable (no data root, missing repo) degrades to empty rather than
/// failing the whole dialog.
fn load_branch_state(
    data_root: Option<&DataRoot>,
) -> (Vec<String>, Option<String>, Option<String>) {
    let Some(root) = data_root else {
        return (Vec::new(), None, None);
    };
    let branches = list_local_branches(root).unwrap_or_default();
    let current = current_branch_name(&root.portable_dir()).unwrap_or(None);
    let upstream = current
        .as_ref()
        .and_then(|_| current_upstream(root).unwrap_or(None));
    (branches, current, upstream)
}

/// Success line for a branch switch. Pure for tests.
fn checkout_notice(outcome: CheckoutOutcome, name: &str) -> String {
    match outcome {
        CheckoutOutcome::Switched => format!("Switched to {name}."),
        CheckoutOutcome::TrackedRemote => {
            format!("Switched to {name}, tracking origin/{name}.")
        }
        CheckoutOutcome::Created => format!("Created and switched to new branch {name}."),
    }
}

/// Catalog names for the Spaces section (best effort; empty when the
/// catalog cannot be read, with the section's own error line explaining).
fn load_space_names(root: Option<&DataRoot>) -> Vec<String> {
    root.and_then(|root| Spaces::load(root).ok())
        .map(|catalog| catalog.names())
        .unwrap_or_default()
}

/// Settings notice for a completed catalog rewrite, including records that
/// could not be parsed: a rename must never look complete while some record
/// still holds the old name.
fn space_rewrite_notice(prefix: &str, touched: &SpaceRewrite) -> String {
    let mut notice = format!(
        "{prefix} · {} repositories and {} Home items updated",
        touched.repositories, touched.items
    );
    if touched.skipped > 0 {
        notice.push_str(&format!(
            "; {} record(s) could not be read and may still reference the old name",
            touched.skipped
        ));
    }
    notice
}

fn group(cx: &App, title: &str, description: Option<&str>) -> gpui_kit::Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(title.to_owned()),
                )
                .when_some(description, |this, text| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(text.to_owned()),
                    )
                }),
        )
}

/// A live (interactive) setting row at full opacity.
fn live_row(
    cx: &App,
    title: impl Into<String>,
    description: impl Into<String>,
    control: impl IntoElement,
) -> gpui_kit::Div {
    h_flex()
        .justify_between()
        .items_center()
        .gap_3()
        .py_1()
        .child(
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(description.into()),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

fn step_button(
    cx: &App,
    label: &str,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui_kit::Div {
    div()
        .w(px(28.))
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .text_sm()
        .text_center()
        .text_color(cx.theme().foreground)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label.to_owned())
}

fn kbd(cx: &App, label: &str) -> gpui_kit::Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label.to_owned())
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let track = self.focus_handle.clone();
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&track)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| focus.focus(window, cx)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_sidebar(cx))
            .child(self.render_section(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    #[gpui_kit::test]
    fn editor_cards_preserve_selection_when_neovim_is_unavailable(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use std::cell::RefCell;
        cx.update(gpui_kit::init);
        let holder = Rc::new(RefCell::new(None));
        let slot = holder.clone();
        let (_, test_cx) = cx.add_window_view(move |window, cx| {
            let settings = cx.new(|cx| {
                SettingsView::new(
                    window,
                    None,
                    DEFAULT_APP_FONT_SIZE,
                    SyncTracker::default(),
                    cx,
                )
            });
            settings.update(cx, |view, _| {
                view.active_section = SettingsSection::Editor;
                view.editor_choice = EditorChoice::BuiltIn;
                view.neovim_available = false;
                view.neovim_checking = false;
            });
            *slot.borrow_mut() = Some(settings.clone());
            gpui_kit::component::Root::new(settings, window, cx)
        });
        let view = holder.borrow().clone().unwrap();
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("default-editor-built_in").visible());
            assert!(window.find("default-editor-neovim").visible());
            let builtin = window.find("default-editor-built_in").bounds();
            let neovim = window.find("default-editor-neovim").bounds();
            let retry = window.find("check-neovim").bounds();
            assert_eq!(builtin.origin.y, neovim.origin.y);
            assert!((builtin.size.width - neovim.size.width).abs() < px(1.));
            assert!(builtin.right() <= neovim.left());
            // "Refresh" sits in the Default editor title line, above the
            // cards and right-aligned with the section.
            assert!(retry.bottom() <= builtin.top());
            assert!(retry.bottom() <= neovim.top());
            assert!(retry.right() >= neovim.right());
            window.click("default-editor-neovim", cx);
            window.click("default-editor-built_in", cx);
            window.press("down", cx);
        });
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.editor_choice, EditorChoice::BuiltIn)
        });
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                // The handler also guards against unavailable or in-flight checks.
                view.set_editor_choice(EditorChoice::Neovim, window, cx);
                assert_eq!(view.editor_choice, EditorChoice::BuiltIn);
                view.neovim_available = true;
                view.neovim_checking = true;
                view.set_editor_choice(EditorChoice::Neovim, window, cx);
                assert_eq!(view.editor_choice, EditorChoice::BuiltIn);
                view.neovim_checking = false;
                cx.notify();
            })
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().to_owned());
        view.update(test_cx, |view, _| view.data_root = Some(root.clone()));
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("default-editor-neovim", cx);
        });
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.editor_choice, EditorChoice::Neovim)
        });
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .editor_choice_or_default(),
            EditorChoice::Neovim
        );
        test_cx.update(|window, cx| {
            window.click("default-editor-built_in", cx);
        });
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .editor_choice_or_default(),
            EditorChoice::BuiltIn
        );
        test_cx.update(|window, cx| {
            window.press("down", cx);
        });
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.editor_choice, EditorChoice::Neovim)
        });
        test_cx.update(|window, cx| {
            window.press("up", cx);
        });
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.editor_choice, EditorChoice::BuiltIn)
        });
        for key in ["space", "enter"] {
            view.downgrade()
                .update_in(test_cx, |view, window, cx| {
                    view.set_editor_choice(EditorChoice::BuiltIn, window, cx);
                    view.editor_card_focus[1].focus(window, cx);
                })
                .unwrap();
            test_cx.update(|window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
            });
            view.read_with(test_cx, |view, _| {
                assert_eq!(view.editor_choice, EditorChoice::Neovim)
            });
        }
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.set_editor_choice(EditorChoice::BuiltIn, window, cx)
            })
            .unwrap();
        view.update(test_cx, |view, cx| {
            view.external_checking = false;
            view.installed_editors.insert("zed".into(), true);
            view.installed_editors.insert("vscode".into(), false);
            view.set_external_editor_enabled(ExternalEditorKind::VsCode, true, cx);
            assert!(!view.external_editors.contains_key("vscode"));
            view.set_external_editor_enabled(ExternalEditorKind::Zed, false, cx);
            assert_eq!(view.external_editors.get("zed"), Some(&false));
        });
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .external_editors
                .unwrap()
                .get("zed"),
            Some(&false)
        );
        view.update(test_cx, |view, cx| {
            view.set_external_editor_enabled(ExternalEditorKind::Zed, true, cx);
        });
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .external_editors
                .unwrap()
                .get("zed"),
            Some(&true)
        );
        // A failed save must not report a selection that was never persisted.
        std::fs::remove_file(root.device_path()).unwrap();
        std::fs::create_dir(root.device_path()).unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.set_editor_choice(EditorChoice::Neovim, window, cx);
                assert_eq!(view.editor_choice, EditorChoice::BuiltIn);
                assert!(view.editor_error.is_some());
            })
            .unwrap();
    }

    #[test]
    fn sections_cover_the_requested_set_in_order() {
        let labels: Vec<_> = SettingsSection::ALL
            .into_iter()
            .map(|section| section.label())
            .collect();
        assert_eq!(
            labels,
            vec![
                "General",
                "Spaces",
                "Sync",
                "Agent",
                "Editor",
                "Keybindings"
            ]
        );
        for section in SettingsSection::ALL {
            assert!(
                !section.description().is_empty(),
                "{section:?} needs a header description"
            );
        }
    }

    #[test]
    fn sync_interval_labels_cover_off_and_options() {
        assert_eq!(format_sync_interval(None), "Off");
        assert_eq!(format_sync_interval(Some(2)), "Every 2 min");
        assert_eq!(format_sync_interval(Some(30)), "Every 30 min");
    }

    #[test]
    fn sync_status_lines_flag_errors_only() {
        assert_eq!(
            sync_status_line(SyncStatus::Idle, None),
            ("Idle".to_owned(), false)
        );
        assert_eq!(
            sync_status_line(SyncStatus::Syncing, None),
            ("Syncing…".to_owned(), false)
        );
        // The error line surfaces the sanitized tracker message.
        assert_eq!(
            sync_status_line(SyncStatus::Error, Some("  boom  ")),
            ("Error: boom".to_owned(), true)
        );
        assert_eq!(
            sync_status_line(SyncStatus::Error, None),
            ("Error.".to_owned(), true)
        );
    }

    #[test]
    fn checkout_notices_name_the_resolution() {
        assert_eq!(
            checkout_notice(CheckoutOutcome::Switched, "main"),
            "Switched to main."
        );
        assert_eq!(
            checkout_notice(CheckoutOutcome::TrackedRemote, "main"),
            "Switched to main, tracking origin/main."
        );
        assert_eq!(
            checkout_notice(CheckoutOutcome::Created, "experiment"),
            "Created and switched to new branch experiment."
        );
    }

    #[test]
    fn font_steps_stay_in_bounds() {
        assert_eq!(SettingsView::stepped_font_size(15.0, 1.0), 16.0);
        assert_eq!(SettingsView::stepped_font_size(15.0, -1.0), 14.0);
        assert_eq!(
            SettingsView::stepped_font_size(MAX_APP_FONT_SIZE, 1.0),
            MAX_APP_FONT_SIZE
        );
        assert_eq!(
            SettingsView::stepped_font_size(MIN_APP_FONT_SIZE, -5.0),
            MIN_APP_FONT_SIZE
        );
    }

    #[test]
    fn session_slider_snaps_and_stays_in_bounds() {
        assert_eq!(snap_sidebar_limit(25), 25);
        assert_eq!(snap_sidebar_limit(23), 25);
        assert_eq!(snap_sidebar_limit(22), 20);
        assert_eq!(snap_sidebar_limit(MIN_SIDEBAR_LIMIT), MIN_SIDEBAR_LIMIT);
        assert_eq!(snap_sidebar_limit(MAX_SIDEBAR_LIMIT), MAX_SIDEBAR_LIMIT);
        assert_eq!(snap_sidebar_limit(1), MIN_SIDEBAR_LIMIT);
        assert_eq!(snap_sidebar_limit(usize::MAX), MAX_SIDEBAR_LIMIT);
        assert_eq!(SIDEBAR_LIMIT_STEP, 5);
        assert_eq!(MIN_SIDEBAR_LIMIT, 10);
        assert_eq!(MAX_SIDEBAR_LIMIT, 50);
    }

    #[test]
    fn enabled_agents_default_to_all_and_tolerate_unknown_ids() {
        use crate::data::DeviceState;
        assert_eq!(
            DeviceState::default().enabled_agents_or_default(),
            AgentKind::ALL.to_vec()
        );
        let exact = DeviceState {
            enabled_agents: Some(vec!["claude".to_owned(), "codex".to_owned()]),
            ..DeviceState::default()
        };
        assert_eq!(
            exact.enabled_agents_or_default(),
            vec![AgentKind::Claude, AgentKind::Codex]
        );
        assert!(exact.is_agent_enabled(AgentKind::Claude));
        assert!(!exact.is_agent_enabled(AgentKind::Opencode));
        for stored in [None, Some(vec![]), Some(vec!["gemini".to_owned()])] {
            let state = DeviceState {
                enabled_agents: stored,
                ..DeviceState::default()
            };
            assert_eq!(
                state.enabled_agents_or_default(),
                AgentKind::ALL.to_vec(),
                "empty or unknown should fall back to all"
            );
        }
    }
}
