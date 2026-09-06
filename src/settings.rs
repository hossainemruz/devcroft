//! The Settings dialog: app-wide preferences grouped into sections.
//!
//! General hosts the live app-wide font size — persisted to `device.json`
//! and applied to the Agent/Editor/Terminal/Review panes without a restart.
//! Every other section is an honest placeholder (disabled controls with a
//! `Soon` badge) until its backend exists.
//!
//! [`SettingsView`] is a long-lived [`Workspace`](crate::workspace::Workspace)
//! entity rendered inside a dialog (`window.open_dialog`): the dialog owns
//! open/close while the view keeps the selected section and edits across
//! reopenings.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window, div, px, rgb,
};

use crate::command_palette::{
    GoToTerminal, PaletteMode, ToggleActionsPalette, ToggleProjectsPalette,
    is_go_to_terminal_shortcut, palette_mode_for_shortcut,
};
use crate::data::{DataRoot, DeviceStore};
use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::metrics::{
    DEFAULT_APP_FONT_SIZE, MAX_APP_FONT_SIZE, MIN_APP_FONT_SIZE, clamp_app_font_size,
    review_font_size, set_app_font_size,
};
use crate::workspace::WorkspaceTab;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Editor,
    Agent,
    Terminal,
    Keybindings,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 5] = [
        Self::General,
        Self::Editor,
        Self::Agent,
        Self::Terminal,
        Self::Keybindings,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editor => "Editor",
            Self::Agent => "Agent",
            Self::Terminal => "Terminal",
            Self::Keybindings => "Keybindings",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => "App-wide appearance.",
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
}

impl SettingsView {
    pub(crate) fn new(
        data_root: Option<DataRoot>,
        initial_font_size: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            active_section: SettingsSection::General,
            font_size: clamp_app_font_size(initial_font_size),
            data_root,
        }
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

    fn select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.active_section != section {
            self.active_section = section;
            cx.notify();
        }
    }

    /// Forward the command-bar toggles and the go-to-terminal shortcut to
    /// the workspace, mirroring `TerminalPane::on_key_down` /
    /// `ReviewView::on_key_down`. All other keys bubble normally.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                                SettingsSection::Editor => self.render_editor().into_any_element(),
                                SettingsSection::Agent => self.render_agent().into_any_element(),
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

    fn render_agent(&self) -> impl IntoElement {
        v_flex().gap_4().child(
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
                    "Toggle command palette",
                    "Search tabs, settings, and sync.",
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(kbd("⌘K"))
                        .child(kbd("Ctrl+K")),
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
            vec!["General", "Editor", "Agent", "Terminal", "Keybindings"]
        );
        for section in SettingsSection::ALL {
            assert!(
                !section.description().is_empty(),
                "{section:?} needs a header description"
            );
        }
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
}
