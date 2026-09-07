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
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, Styled, WeakEntity, Window, div, px, rgb,
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

    fn render_option(&self, agent: AgentKind, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected == agent;
        h_flex()
            .gap_3()
            .items_center()
            .w_full()
            .px_4()
            .py_3()
            .rounded_md()
            .border_1()
            .when(selected, |this| {
                this.border_color(rgb(0x2f81f7)).bg(rgb(0x0e1a2b))
            })
            .when(!selected, |this| {
                this.border_color(rgb(0x292b2b))
                    .bg(rgb(0x0e0f0f))
                    .hover(|this| this.border_color(rgb(0x3a3d3d)))
            })
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.pick(agent, window, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .size(px(18.))
                    .rounded_full()
                    .border_1()
                    .items_center()
                    .justify_center()
                    .flex()
                    .border_color(if selected {
                        rgb(0x2f81f7)
                    } else {
                        rgb(0x555a5a)
                    })
                    .when(selected, |this| {
                        this.child(div().size(px(10.)).rounded_full().bg(rgb(0x2f81f7)))
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
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
                    ),
            )
            .when(selected, |this| {
                this.child(div().text_sm().text_color(rgb(0x2f81f7)).child("✓"))
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
        // Built with a loop (not `map`): each row borrows `cx` for its
        // listener, which cannot escape an `FnMut` closure body.
        let mut options = Vec::with_capacity(AgentKind::ALL.len());
        for agent in AgentKind::ALL {
            options.push(self.render_option(agent, cx).into_any_element());
        }
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
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(rgb(0xe7e7e7))
                            .child("Default agent"),
                    )
                    .child(
                        div().text_xs().text_color(rgb(0x737878)).child(
                            "Launched when the Agent tab is first created for this workspace.",
                        ),
                    ),
            )
            .child(v_flex().gap_2().children(options))
            .child(
                div().text_xs().text_color(rgb(0x737878)).child(
                    "Switching restarts the Agent tab with the new harness — the current session is stopped. The choice is saved for this workspace and restored on reopen.",
                ),
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
