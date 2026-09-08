//! The per-workspace settings sheet: default agent harness.
//!
//! Opened from the gear button at the right of the workspace tab bar (see
//! [`Workspace`](crate::workspace::Workspace)). Like the Add Repository
//! dialog, the view is created fresh on every opening so it always reflects
//! the current checkout — repository switches never leave stale state
//! behind. Picking a harness persists it immediately to machine-local
//! `device.json` (see [`crate::data`]) and restarts the Agent tab with it.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, Styled, WeakEntity, Window, div, rgb,
};
use std::path::PathBuf;

use crate::agent::AgentKind;
use crate::command_palette::{
    GoToAgent, GoToEditor, GoToTerminal, PaletteMode, ToggleActionsPalette, ToggleProjectsPalette,
    is_go_to_agent_shortcut, is_go_to_editor_shortcut, is_go_to_terminal_shortcut,
    palette_mode_for_shortcut,
};
use crate::workspace::Workspace;

pub(crate) struct WorkspaceSettingsView {
    focus_handle: FocusHandle,
    checkout: PathBuf,
    project_name: String,
    selected: AgentKind,
    workspace: WeakEntity<Workspace>,
}

impl WorkspaceSettingsView {
    pub(crate) fn new(
        checkout: PathBuf,
        project_name: String,
        default_agent: AgentKind,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            checkout,
            project_name,
            selected: default_agent,
            workspace,
        }
    }

    /// Persist a pick through the workspace (single path for state,
    /// persistence, and feedback) and mirror the applied value locally. A
    /// failed persistence leaves the previous selection shown: the workspace
    /// notifies on failure instead.
    fn pick(&mut self, agent: AgentKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == agent {
            return;
        }
        let applied = self
            .workspace
            .update(cx, |this, cx| {
                this.set_default_agent(agent, window, cx);
                this.default_agent()
            })
            .ok()
            .unwrap_or(self.selected);
        self.selected = applied;
        cx.notify();
    }

    /// Mirror Settings/terminal panes: the palette toggles and the
    /// go-to-tab shortcuts keep working while the sheet has focus;
    /// everything else bubbles normally.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if is_go_to_agent_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToAgent), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_go_to_editor_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToEditor), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_go_to_terminal_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToTerminal), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
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

    /// Agent selector dropdown: the trigger shows the current harness and the
    /// menu lists every harness with its command and description, so adding a
    /// variant later is one more menu item instead of one more card. Picks go
    /// through [`Self::pick`] so state, persistence, and feedback stay on the
    /// single workspace path. The menu builder is `Fn` (it may re-run after
    /// dismiss), so it captures a [`WeakEntity`] and re-enters through
    /// `update` on click rather than borrowing `cx`.
    fn render_agent_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;
        let view = cx.entity().downgrade();
        Button::new("workspace-settings-agent")
            .label(selected.label())
            .accessibility_label(format!("Agent, {}", selected.label()))
            .dropdown_caret(true)
            .outline()
            .w_full()
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                let mut menu = menu;
                for agent in AgentKind::ALL {
                    let view = view.clone();
                    let checked = agent == selected;
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            v_flex()
                                .w_full()
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
                                                .child(agent.label().to_owned()),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(rgb(0x292b2b))
                                                .bg(rgb(0x080909))
                                                .text_xs()
                                                .text_color(rgb(0x858989))
                                                .child(format!("`{}`", agent.command())),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x737878))
                                        .child(agent.description().to_owned()),
                                )
                        })
                        .checked(checked)
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.pick(agent, window, cx));
                        }),
                    );
                }
                menu.scrollable(true)
            })
    }
}

impl Focusable for WorkspaceSettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let track = self.focus_handle.clone();
        let checkout_text = self.checkout.to_string_lossy().into_owned();
        let dropdown = self.render_agent_dropdown(cx).into_any_element();
        v_flex()
            .size_full()
            .px_6()
            .py_5()
            .gap_4()
            .bg(rgb(0x090a0a))
            .text_color(rgb(0xe7e7e7))
            .track_focus(&track)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| focus.focus(window, cx)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(rgb(0xe7e7e7))
                            .child(self.project_name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x737878))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(checkout_text),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(rgb(0xe7e7e7))
                            .child("Agent"),
                    )
                    .child(dropdown)
                    .child(div().text_xs().text_color(rgb(0x737878)).child(
                        "Changing agent will terminate current session and restart the tab.",
                    )),
            )
            .child(
                h_flex().justify_end().pt_2().child(
                    Button::new("workspace-settings-done")
                        .label("Done")
                        .primary()
                        .on_click(|_, window, cx| {
                            window.close_sheet(cx);
                        }),
                ),
            )
    }
}
