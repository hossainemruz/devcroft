//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header, tab bar, status bar) hosting the active terminal pane.

use std::{env, path::PathBuf};

use gpui_kit::component::{
    StyledExt as _, h_flex,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px, rgb,
};

use crate::pane::TerminalPane;
use crate::review::ReviewView;

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

pub(crate) struct Workspace {
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    project_name: SharedString,
    project_path: SharedString,
}

impl Workspace {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project_name = working_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned();
        let project_path = working_directory.display().to_string();

        let tabs = WorkspaceTab::ALL
            .into_iter()
            .map(|tab| {
                if !tab.has_terminal() {
                    return None;
                }
                let cwd = working_directory.clone();
                Some(cx.new(|cx| TerminalPane::new(tab, &cwd, cx)))
            })
            .collect::<Vec<_>>();
        if let Some(Some(initial)) = tabs.first() {
            initial.read(cx).focus_handle.clone().focus(window, cx);
        }
        let review_cwd = working_directory.clone();
        let review = cx.new(|cx| ReviewView::new(&review_cwd, cx));

        Self {
            active_tab: WorkspaceTab::Agent,
            tabs,
            review,
            project_name: project_name.into(),
            project_path: project_path.into(),
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
        if let Some(Some(pane)) = self.tabs.get(index) {
            let focus_handle = pane.read(cx).focus_handle.clone();
            focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn render_active_content(&self) -> AnyElement {
        if self.active_tab == WorkspaceTab::Review {
            return self.review.clone().into_any_element();
        }
        match self.tabs.get(self.active_tab as usize).and_then(|tab| tab.clone()) {
            Some(pane) => pane.into_any_element(),
            None => div().size_full().into_any_element(),
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_index = self.active_tab as usize;
        let active_content = self.render_active_content();

        v_flex()
            .size_full()
            .bg(rgb(0x080909))
            .text_color(rgb(0xe7e7e7))
            .child(
                h_flex()
                    .h(px(58.))
                    .px_4()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(0x292b2b))
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(div().text_color(rgb(0x8e9494)).child("‹"))
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(self.project_name.clone()),
                            )
                            .child(div().text_color(rgb(0x555a5a)).child("/"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x858989))
                                    .child(self.project_path.clone()),
                            ),
                    )
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
                    ),
            )
            .child(
                div().flex_1().min_h_0().p_2().child(
                    div()
                        .size_full()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x292b2b))
                        .bg(rgb(0x090a0a))
                        .child(active_content),
                ),
            )
            .child(
                h_flex()
                    .h(px(24.))
                    .px_4()
                    .justify_between()
                    .text_xs()
                    .text_color(rgb(0x737878))
                    .child(format!(
                        "{} · {}",
                        self.project_name,
                        self.active_tab.label()
                    ))
                    .child("Devcroft · libghostty-vt"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_contract_matches_requested_commands() {
        assert_eq!(WorkspaceTab::Agent.command(), Some("opencode"));
        assert_eq!(WorkspaceTab::Editor.command(), Some("nvim ."));
        assert_eq!(WorkspaceTab::Terminal.command(), None);
        assert_eq!(WorkspaceTab::Review.command(), None);
        assert_eq!(WorkspaceTab::Review.label(), "Review");
        assert!(!WorkspaceTab::Review.has_terminal());
    }
}
