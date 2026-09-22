//! The Settings dialog: app-wide preferences grouped into sections.
//!
//! General hosts the live app-wide font size — persisted to `device.json`
//! and applied to the Agent/Editor/Terminal/Review panes without a restart.
//! Sync hosts the portable-data Git remote, the automatic sync schedule, and
//! a manual Sync-now action with live status. Agent hosts the enabled
//! harness cards with per-agent toggles, the default-agent dropdown (enabled
//! agents only), the sidebar session-limit slider (10–50, step 5), and
//! Devcroft skill cards with a header refresh action.
//! Keybindings documents the current global shortcuts.
//!
//! [`SettingsView`] is a long-lived [`Workspace`](crate::workspace::Workspace)
//! entity rendered inside a dialog (`window.open_dialog`): the dialog owns
//! open/close while the view keeps the selected section and edits across
//! reopenings.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, WeakEntity, Window,
    div, px, rgb,
};

use std::rc::Rc;

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
    CheckoutOutcome, DataRoot, DeviceStore, SYNC_INTERVAL_OPTIONS, SyncStatus, SyncTracker,
    checkout_branch, clear_origin, current_branch_name, current_upstream, get_origin,
    is_supported_sync_interval, list_local_branches, set_origin,
};
use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::metrics::{
    DEFAULT_APP_FONT_SIZE, MAX_APP_FONT_SIZE, MIN_APP_FONT_SIZE, clamp_app_font_size,
    review_font_size, set_app_font_size,
};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Sync,
    Agent,
    Keybindings,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 4] = [
        Self::General,
        Self::Sync,
        Self::Agent,
        Self::Keybindings,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Sync => "Sync",
            Self::Agent => "Agent",
            Self::Keybindings => "Keybindings",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => "App-wide appearance.",
            Self::Sync => "Portable data Git sync.",
            Self::Agent => "Agent pane preferences.",
            Self::Keybindings => "Current shortcuts.",
        }
    }
}

pub(crate) struct SettingsView {
    pub(crate) focus_handle: FocusHandle,
    active_section: SettingsSection,
    font_size: f32,
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
        let stored = data_root.as_ref().and_then(|root| {
            DeviceStore::new(root).load().ok()
        });
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
        let skill_statuses = crate::agent_skill::Target::ALL
            .into_iter()
            .map(|target| {
                crate::agent_skill::perform(
                    crate::agent_skill::Action::Status,
                    Some(target),
                )
                .text
            })
            .collect::<Vec<_>>();
        Self {
            focus_handle: cx.focus_handle(),
            active_section: SettingsSection::General,
            font_size: clamp_app_font_size(initial_font_size),
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
        if let Some(root) = self.data_root.clone() {
            let stored = DeviceStore::new(&root).load().ok();
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
            self.enabled_agents_error =
                Some("Keep at least one agent enabled.".to_owned());
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
                .update(cx, |this, cx| {
                    this.set_enabled_agents(enabled, default, cx)
                })
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
            self.default_agent_error = Some(format!(
                "{} is disabled — enable it first.",
                agent.label()
            ));
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

    fn select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.active_section != section {
            self.active_section = section;
            cx.notify();
        }
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
            .w(px(200.))
            .flex_none()
            .h_full()
            .py_3()
            .px_2()
            .gap_1()
            .border_r_1()
            .border_color(rgb(0x292b2b))
            .children(SettingsSection::ALL.into_iter().map(|section| {
                let selected = section == self.active_section;
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(rgb(0x1d1f1f)).text_color(rgb(0xe7e7e7))
                    })
                    .when(!selected, |this| this.text_color(rgb(0x858989)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.select_section(section, cx)),
                    )
                    .child(section.label())
            }))
    }

    fn render_header(&self) -> impl IntoElement {
        v_flex()
            .gap_1()
            .pb_4()
            .border_b_1()
            .border_color(rgb(0x292b2b))
            .child(
                div()
                    .text_size(px(18.))
                    .font_semibold()
                    .text_color(rgb(0xe7e7e7))
                    .child(self.active_section.label()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x858989))
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
            .child(div().px_6().pt_5().child(self.render_header()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .gap_4()
                            .px_6()
                            .py_5()
                            .w_full()
                            .max_w(px(680.))
                            .child(match self.active_section {
                                SettingsSection::General => {
                                    self.render_general(cx).into_any_element()
                                }
                                SettingsSection::Sync => self.render_sync(cx).into_any_element(),
                                SettingsSection::Agent => self.render_agent(cx).into_any_element(),
                                SettingsSection::Keybindings => {
                                    self.render_keybindings().into_any_element()
                                }
                            }),
                    ),
            )
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.font_size;
        let is_default = (size - DEFAULT_APP_FONT_SIZE).abs() < f32::EPSILON;
        v_flex()
            .gap_4()
            .child(
                group("Appearance", None).child(live_row(
                    "App font size",
                    "Applies to the Agent, Editor, Terminal, and Review panes.",
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(step_button(
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
                                .text_color(rgb(0xe7e7e7))
                                .child(format_font_size(size)),
                        )
                        .child(step_button(
                            "+",
                            cx.listener(move |this, _, _, cx| {
                                this.set_font_size(SettingsView::stepped_font_size(size, 1.0), cx)
                            }),
                        ))
                        .child(div().text_xs().text_color(rgb(0x555a5a)).child(format!(
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
                                    .text_color(rgb(0x858989))
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
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x292b2b))
                    .bg(rgb(0x0e0f0f))
                    .child(
                        div()
                            .font_family(TERMINAL_FONT_FAMILY)
                            .text_size(px(size))
                            .text_color(rgb(0xe7e7e7))
                            .child("The quick brown fox jumps over the lazy dog 0123456789"),
                    )
                    .child(
                        div()
                            .font_family(TERMINAL_FONT_FAMILY)
                            .text_size(px(review_font_size()))
                            .text_color(rgb(0x858989))
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
                group(
                    "Remote",
                    Some("Git remote for portable data. Only portable/ is ever synced; device.json stays on this machine."),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x858989))
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
                                .child(Input::new(&self.origin_input)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("sync-save-origin")
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
                                    Button::new("sync-remove-origin")
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
                            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
                        })
                        .when_some(self.origin_notice.clone(), |this, notice| {
                            this.child(div().text_sm().text_color(rgb(0x858989)).child(notice))
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x737878))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(saved_line),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x555a5a))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(repo_line),
                        ),
                ),
            )
            .child(
                group(
                    "Branch",
                    Some("Local branch of the portable repo. Switching replaces portable files with that branch's content."),
                )
                .child(self.render_branch_current())
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
                                    this.border_color(rgb(0x2f81f7))
                                        .bg(rgb(0x0e1a2b))
                                        .text_color(rgb(0xe7e7e7))
                                })
                                .when(!selected, |this| {
                                    this.border_color(rgb(0x292b2b))
                                        .bg(rgb(0x0e0f0f))
                                        .text_color(rgb(0x858989))
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
                                .child(Input::new(&self.branch_input)),
                        )
                        .child(
                            Button::new("sync-checkout-branch")
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
                    this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
                })
                .when_some(self.branch_notice.clone(), |this, notice| {
                    this.child(div().text_sm().text_color(rgb(0x858989)).child(notice))
                }),
            )
            .child(
                group(
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
                                        this.border_color(rgb(0x2f81f7))
                                            .bg(rgb(0x0e1a2b))
                                            .text_color(rgb(0xe7e7e7))
                                    })
                                    .when(!selected, |this| {
                                        this.border_color(rgb(0x292b2b))
                                            .bg(rgb(0x0e0f0f))
                                            .text_color(rgb(0x858989))
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
                    this.child(div().text_xs().text_color(rgb(0x858989)).child(note))
                })
                .when_some(self.interval_error.clone(), |this, error| {
                    this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
                }),
            )
            .child(
                group(
                    "Manual sync",
                    Some("Stage, commit, fetch, rebase, and push portable data now."),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            Button::new("sync-now")
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
                                    rgb(0xf87171)
                                } else {
                                    rgb(0x858989)
                                })
                                .child(status_text),
                        ),
                ),
            )
    }

    /// Current-branch line for the Branch group: the checked-out branch plus
    /// what sync will push to, or the honest degraded states.
    fn render_branch_current(&self) -> impl IntoElement {
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
            .child(div().text_sm().text_color(rgb(0xe7e7e7)).child(current))
            .when(!tracking.is_empty(), |this| {
                this.child(div().text_xs().text_color(rgb(0x858989)).child(tracking))
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
            "Agents",
            Some("Choose which harnesses are available. The default and New session… only offer enabled agents."),
        )
        .child(
            v_flex()
                .gap_2()
                .children(AgentKind::ALL.into_iter().enumerate().map(|(index, agent)| {
                    let is_enabled = enabled.contains(&agent);
                    let icon = agent_icons::agent_icon(agent, &tiles, agent_icons::ICON_PX);
                    let toggle_view = view.clone();
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x292b2b))
                        .bg(rgb(0x0e0f0f))
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
                                                        .text_color(rgb(0xe7e7e7))
                                                        .child(agent.label().to_owned()),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(0x737878))
                                                        .child(agent.description().to_owned()),
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
                })),
        )
        .when_some(self.enabled_agents_error.clone(), |this, error| {
            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
        });

        let dropdown_view = view.clone();
        let dropdown_enabled = enabled.clone();
        let dropdown_tiles = tiles.clone();
        let current_icon = agent_icons::agent_icon(current, &tiles, agent_icons::ICON_PX);
        let default = group(
            "Default agent",
            Some("Launched when the Agent tab opens without history, and pre-selected in New session…. Open sessions keep running."),
        )
        .child(
            h_flex().gap_2().items_center().child(
                Button::new("default-agent")
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
            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
        });

        let limit = self.session_limit;
        let is_default = limit == DEFAULT_SIDEBAR_LIMIT;
        let sessions = group(
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
                                .text_color(rgb(0xe7e7e7))
                                .child(format!("{limit}")),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x555a5a))
                                .child(format!(
                                    "{MIN_SIDEBAR_LIMIT}–{MAX_SIDEBAR_LIMIT} · step {SIDEBAR_LIMIT_STEP}"
                                )),
                        )
                        .when(!is_default, |this| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(0x858989))
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
            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
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
                        .text_color(rgb(0xe7e7e7))
                        .child("Devcroft skill".to_owned()),
                )
                .child(
                    div().flex_none().child(
                        Button::new("skill-status")
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
                .text_color(rgb(0x737878))
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
                    .border_color(rgb(0x292b2b))
                    .bg(rgb(0x0e0f0f))
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
                                                    .text_color(rgb(0xe7e7e7))
                                                    .child(target.label().to_owned()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x737878))
                                                    .child(target.description().to_owned()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(0x555a5a))
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
                                            .text_color(rgb(0x858989))
                                            .child(status_line),
                                    )
                                    .when(!path_line.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x555a5a))
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
                    .text_color(rgb(0x858989))
                    .child(self.skill_environment.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .child("Updates and removal preserve modified or unmanaged skill folders."),
            );
        v_flex()
            .gap_4()
            .child(agents)
            .child(default)
            .child(sessions)
            .child(skill)
    }

    fn render_keybindings(&self) -> impl IntoElement {
        v_flex()
            .gap_4()
            .child(
                group(
                    "Keyboard",
                    Some("These shortcuts work everywhere, including inside terminals."),
                )
                .child(live_row(
                    "Toggle actions palette",
                    "Search tabs, settings, and sync.",
                    if cfg!(target_os = "macos") {
                        kbd("⌘K")
                    } else {
                        kbd("Ctrl+K")
                    },
                ))
                .child(live_row(
                    "Toggle projects palette",
                    "Switch between recent repositories.",
                    if cfg!(target_os = "macos") {
                        kbd("⌘P")
                    } else {
                        kbd("Ctrl+P")
                    },
                ))
                .child(live_row(
                    "Close palette or dialog",
                    "Dismisses the topmost overlay.",
                    kbd("Esc"),
                )),
            )
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

fn group(title: &str, description: Option<&str>) -> gpui_kit::Div {
    v_flex().gap_3().child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(rgb(0xe7e7e7))
                    .child(title.to_owned()),
            )
            .when_some(description, |this, text| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x737878))
                        .child(text.to_owned()),
                )
            }),
    )
}

/// A live (interactive) setting row at full opacity.
fn live_row(
    title: impl Into<String>,
    description: impl Into<String>,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .child(
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xe7e7e7))
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x737878))
                        .child(description.into()),
                ),
        )
        .child(control)
}

fn step_button(
    label: &str,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .w(px(28.))
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(0x292b2b))
        .bg(rgb(0x0e0f0f))
        .text_sm()
        .text_center()
        .text_color(rgb(0xe7e7e7))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label.to_owned())
}

fn kbd(label: &str) -> impl IntoElement {
    div()
        .px_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(0x292b2b))
        .bg(rgb(0x0e0f0f))
        .text_xs()
        .text_color(rgb(0x858989))
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
            .bg(rgb(0x090a0a))
            .text_color(rgb(0xe7e7e7))
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

    #[test]
    fn sections_cover_the_requested_set_in_order() {
        let labels: Vec<_> = SettingsSection::ALL
            .into_iter()
            .map(|section| section.label())
            .collect();
        assert_eq!(labels, vec!["General", "Sync", "Agent", "Keybindings"]);
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
        for stored in [
            None,
            Some(vec![]),
            Some(vec!["gemini".to_owned()]),
        ] {
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
