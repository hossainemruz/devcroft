//! The Settings dialog: app-wide preferences grouped into sections.
//!
//! General hosts the live app-wide font size — persisted to `device.json`
//! and applied to the Agent/Editor/Terminal/Review panes without a restart.
//! Sync hosts the portable-data Git remote, the automatic sync schedule, and
//! a manual Sync-now action with live status. Every other section is an
//! honest placeholder (disabled controls with a `Soon` badge) until its
//! backend exists.
//!
//! [`SettingsView`] is a long-lived [`Workspace`](crate::workspace::Workspace)
//! entity rendered inside a dialog (`window.open_dialog`): the dialog owns
//! open/close while the view keeps the selected section and edits across
//! reopenings.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, WeakEntity, Window,
    div, px, rgb,
};

use crate::agent_sessions::{
    DEFAULT_SIDEBAR_LIMIT, MAX_SIDEBAR_LIMIT, MIN_SIDEBAR_LIMIT, clamp_sidebar_limit,
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
use crate::workspace::{Workspace, WorkspaceTab};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Sync,
    Editor,
    Agent,
    Terminal,
    Keybindings,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 6] = [
        Self::General,
        Self::Sync,
        Self::Editor,
        Self::Agent,
        Self::Terminal,
        Self::Keybindings,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Sync => "Sync",
            Self::Editor => "Editor",
            Self::Agent => "Agent",
            Self::Terminal => "Terminal",
            Self::Keybindings => "Keybindings",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => "App-wide appearance.",
            Self::Sync => "Portable data Git sync.",
            Self::Editor => "Editor pane preferences.",
            Self::Agent => "Agent pane preferences.",
            Self::Terminal => "Terminal pane preferences.",
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
    skill_report: String,
    skill_environment: String,
    session_limit: usize,
    session_limit_error: Option<String>,
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
        let session_limit = data_root
            .as_ref()
            .and_then(|root| DeviceStore::new(root).load().ok())
            .map(|state| state.recent_sessions_limit_or_default())
            .unwrap_or(DEFAULT_SIDEBAR_LIMIT);
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
            skill_report: crate::agent_skill::perform(crate::agent_skill::Action::Status, None)
                .text,
            skill_environment: crate::agent_skill::environment(),
            session_limit,
            session_limit_error: None,
        }
    }

    /// Link the owning workspace for Sync-now delegation. Set by the
    /// workspace before the dialog opens; `None` until then.
    pub(crate) fn set_workspace(&mut self, workspace: WeakEntity<Workspace>) {
        self.workspace = Some(workspace);
    }

    /// Re-read persisted Sync state plus the Agent sidebar limit so the
    /// dialog never shows stale state (e.g. after external git or CLI
    /// edits). Called before the dialog opens.
    pub(crate) fn refresh_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.data_root.clone() {
            self.sync_interval = DeviceStore::new(&root)
                .load()
                .ok()
                .and_then(|state| state.sync_interval_minutes);
            self.saved_origin = get_origin(&root).unwrap_or(None);
            self.session_limit = DeviceStore::new(&root)
                .load()
                .ok()
                .map(|state| state.recent_sessions_limit_or_default())
                .unwrap_or(DEFAULT_SIDEBAR_LIMIT);
        } else {
            self.sync_interval = None;
            self.saved_origin = None;
            self.session_limit = DEFAULT_SIDEBAR_LIMIT;
        }
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
            self.skill_report =
                crate::agent_skill::perform(crate::agent_skill::Action::Status, None).text;
            self.skill_environment = crate::agent_skill::environment();
        }
        self.session_limit_error = None;
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

    /// Step the Agent sidebar limit in fives, clamped into range. Pure so
    /// the bounds stay unit-testable without a window.
    fn stepped_session_limit(current: usize, delta: i32) -> usize {
        clamp_sidebar_limit(current.saturating_add_signed(delta as isize))
    }

    /// Persist the Agent sidebar limit and push it to the owning workspace
    /// so the sidebar re-projects immediately. The live value applies even
    /// when the write fails (same spirit as the font size); the error line
    /// says persistence is what broke.
    fn set_session_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        let limit = clamp_sidebar_limit(limit);
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
                                SettingsSection::Editor => self.render_editor().into_any_element(),
                                SettingsSection::Agent => self.render_agent(cx).into_any_element(),
                                SettingsSection::Terminal => {
                                    self.render_terminal().into_any_element()
                                }
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

    fn render_editor(&self) -> impl IntoElement {
        v_flex().gap_4().child(
            dummy_group("Editor", "Coming soon — these controls are placeholders.")
                .child(dummy_row(
                    "Default command",
                    "Launched when the Editor tab opens.",
                    dummy_value(editor_command_label()),
                ))
                .child(font_size_note_row(self.font_size, "Editor panes"))
                .child(dummy_row(
                    "Tab width",
                    "Spaces per indent in the TUI editor.",
                    dummy_value("4"),
                ))
                .child(dummy_row(
                    "Word wrap",
                    "Wrap long lines in the TUI editor.",
                    dummy_switch(false),
                )),
        )
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
            let (report, environment) = cx
                .background_spawn(async move {
                    let mut report = crate::agent_skill::perform(action, target);
                    if action != crate::agent_skill::Action::Status {
                        let status =
                            crate::agent_skill::perform(crate::agent_skill::Action::Status, None);
                        report.text = format!("{}\n\n{}", report.text, status.text);
                    }
                    (report, crate::agent_skill::environment())
                })
                .await;
            let _ = cx.update(|_, cx| {
                view.update(cx, |this, cx| {
                    this.skill_busy = false;
                    this.skill_report = report.text;
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
        let limit = self.session_limit;
        let is_default = limit == DEFAULT_SIDEBAR_LIMIT;
        let sessions = group(
            "Sessions",
            Some("How many recent sessions the Agent sidebar lists per repository."),
        )
        .child(live_row(
            "Recent sessions in sidebar",
            "Applies immediately; open sessions always stay reachable.",
            h_flex()
                .gap_2()
                .items_center()
                .child(step_button(
                    "-",
                    cx.listener(move |this, _, _, cx| {
                        this.set_session_limit(SettingsView::stepped_session_limit(limit, -5), cx)
                    }),
                ))
                .child(
                    div()
                        .w(px(48.))
                        .text_center()
                        .text_sm()
                        .text_color(rgb(0xe7e7e7))
                        .child(format!("{limit}")),
                )
                .child(step_button(
                    "+",
                    cx.listener(move |this, _, _, cx| {
                        this.set_session_limit(SettingsView::stepped_session_limit(limit, 5), cx)
                    }),
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x555a5a))
                        .child(format!("{MIN_SIDEBAR_LIMIT}–{MAX_SIDEBAR_LIMIT}")),
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
                                cx.listener(|this, _, _, cx| {
                                    this.set_session_limit(DEFAULT_SIDEBAR_LIMIT, cx)
                                }),
                            )
                            .child(format!("Reset to {DEFAULT_SIDEBAR_LIMIT}")),
                    )
                }),
        ))
        .when_some(self.session_limit_error.clone(), |this, error| {
            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
        });
        let mut skill = group(
            "Devcroft skill",
            Some(
                "Teach agents to use repository artifacts and artifact/review comments. OpenCode also reads these skill locations.",
            ),
        );
        for (index, target) in Target::ALL.into_iter().enumerate() {
            skill = skill.child(live_row(
                target.label(),
                "Install for all projects on this machine.",
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(("skill-install", index))
                            .label("Install / update")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.manage_skill(Action::Install, Some(target), window, cx);
                            })),
                    )
                    .child(
                        Button::new(("skill-remove", index))
                            .label("Remove")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.manage_skill(Action::Uninstall, Some(target), window, cx);
                            })),
                    ),
            ));
        }
        skill = skill
            .child(
                Button::new("skill-status")
                    .label(if busy { "Working…" } else { "Refresh status" })
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.manage_skill(Action::Status, None, window, cx);
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xe7e7e7))
                    .child(self.skill_report.clone()),
            )
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
        v_flex().gap_4().child(sessions).child(skill).child(
            dummy_group("Agent", "Coming soon — these controls are placeholders.")
                .child(dummy_row(
                    "Default command",
                    "Launched when the Agent tab opens.",
                    dummy_value(agent_command_label()),
                ))
                .child(font_size_note_row(self.font_size, "Agent panes"))
                .child(dummy_row(
                    "Provider",
                    "Used for activity detection.",
                    dummy_dropdown("Auto"),
                ))
                .child(dummy_row(
                    "Approval mode",
                    "When the agent may edit without asking.",
                    dummy_dropdown("Ask before edits"),
                )),
        )
    }

    fn render_terminal(&self) -> impl IntoElement {
        v_flex().gap_4().child(
            dummy_group("Terminal", "Coming soon — these controls are placeholders.")
                .child(dummy_row(
                    "Shell",
                    "Login shell launched in new terminals.",
                    dummy_value(default_shell_label()),
                ))
                .child(font_size_note_row(self.font_size, "Terminal panes"))
                .child(dummy_row(
                    "Scrollback",
                    "Lines kept per terminal.",
                    dummy_value("10,000 lines"),
                ))
                .child(dummy_row(
                    "Cursor blink",
                    "Blink the terminal cursor.",
                    dummy_switch(true),
                ))
                .child(dummy_row(
                    "Audible bell",
                    "Play a sound on the terminal bell.",
                    dummy_switch(false),
                )),
        )
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
            .child(
                h_flex().gap_2().items_center().child(soon_badge()).child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x737878))
                        .child("Custom keybindings are coming soon."),
                ),
            )
    }
}

/// `nvim .` today; read from the workspace contract so the label cannot drift.
fn editor_command_label() -> String {
    WorkspaceTab::Editor
        .command()
        .unwrap_or("nvim .")
        .to_owned()
}

/// `opencode` today; read from the workspace contract so the label cannot drift.
fn agent_command_label() -> String {
    WorkspaceTab::Agent
        .command()
        .unwrap_or("opencode")
        .to_owned()
}

fn default_shell_label() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned())
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

/// A placeholder row: dimmed, non-interactive, with a `Soon` badge.
fn dummy_row(
    title: impl Into<String>,
    description: impl Into<String>,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .opacity(0.7)
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
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(control)
                .child(soon_badge()),
        )
}

/// A placeholder group: title with a `Soon` badge plus dimmed rows.
fn dummy_group(title: &str, description: &str) -> gpui_kit::Div {
    v_flex().gap_3().child(
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(rgb(0xe7e7e7))
                            .child(title.to_owned()),
                    )
                    .child(soon_badge()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x737878))
                    .child(description.to_owned()),
            ),
    )
}

/// Read-only note pointing a pane section at the General size. Live in the
/// sense that it shows the current value; the control itself lives in General.
fn font_size_note_row(current: f32, panes: &str) -> impl IntoElement {
    live_row(
        "Font size",
        format!("{panes} follow the app-wide size in General."),
        div()
            .text_sm()
            .text_color(rgb(0x858989))
            .child(format_font_size(current)),
    )
}

fn soon_badge() -> impl IntoElement {
    div()
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(0x292b2b))
        .text_xs()
        .text_color(rgb(0x737878))
        .child("Soon")
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

fn dummy_value(text: impl Into<String>) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(0x292b2b))
        .bg(rgb(0x0e0f0f))
        .text_xs()
        .text_color(rgb(0x858989))
        .child(text.into())
}

fn dummy_dropdown(label: &str) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(0x292b2b))
        .bg(rgb(0x0e0f0f))
        .text_xs()
        .text_color(rgb(0x858989))
        .child(label.to_owned())
        .child(div().text_color(rgb(0x555a5a)).child("▾"))
}

fn dummy_switch(on: bool) -> impl IntoElement {
    h_flex()
        .w(px(38.))
        .h(px(22.))
        .px(px(3.))
        .rounded_md()
        .items_center()
        .when(on, |this| this.justify_end().bg(rgb(0x2f5f46)))
        .when(!on, |this| {
            this.justify_start()
                .bg(rgb(0x0e0f0f))
                .border_1()
                .border_color(rgb(0x292b2b))
        })
        .child(
            div()
                .size(px(15.))
                .rounded_md()
                .when(on, |this| this.bg(rgb(0xe7e7e7)))
                .when(!on, |this| this.bg(rgb(0x555a5a))),
        )
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
        assert_eq!(
            labels,
            vec![
                "General",
                "Sync",
                "Editor",
                "Agent",
                "Terminal",
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
    fn session_limit_steps_stay_in_bounds() {
        assert_eq!(SettingsView::stepped_session_limit(25, 5), 30);
        assert_eq!(SettingsView::stepped_session_limit(25, -5), 20);
        assert_eq!(
            SettingsView::stepped_session_limit(MAX_SIDEBAR_LIMIT, 5),
            MAX_SIDEBAR_LIMIT
        );
        assert_eq!(
            SettingsView::stepped_session_limit(MIN_SIDEBAR_LIMIT, -50),
            MIN_SIDEBAR_LIMIT
        );
        assert_eq!(
            SettingsView::stepped_session_limit(usize::MAX, 5),
            MAX_SIDEBAR_LIMIT
        );
    }
}
