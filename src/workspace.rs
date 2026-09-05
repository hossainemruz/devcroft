//! The workspace shell: tab definitions plus the surrounding chrome
//! (project header and tab bar) hosting the active terminal pane.

use std::{path::Path, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, StyledExt as _, WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, IntoElement,
    InteractiveElement, MouseButton, ParentElement, Render, SharedString, Styled, Window, div, px,
    rgb,
};

use crate::command_palette::{GROUPS, PaletteCommand, ToggleCommandPalette, command_at};
use crate::data::{DataRoot, SyncStatus, SyncTracker, sync_portable_with_tracker};
use crate::git_status::{GitStatus, load_git_status};
use crate::metrics::WORKSPACE_HEADER_HEIGHT;
use crate::pane::TerminalPane;
use crate::review::ReviewView;

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

pub(crate) struct Workspace {
    active_tab: WorkspaceTab,
    tabs: Vec<Option<Entity<TerminalPane>>>,
    review: Entity<ReviewView>,
    project_name: SharedString,
    git_status: GitStatus,
    command_open: bool,
    command_state: Entity<CommandState>,
    data_root: Option<DataRoot>,
    sync_tracker: SyncTracker,
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
        // open), then repeats on the interval. The loop ends with the
        // entity: `update` fails once the workspace is dropped.
        let poll_dir = working_directory.to_path_buf();
        cx.spawn(async move |this, cx| {
            loop {
                let path = poll_dir.clone();
                let status =
                    cx.background_spawn(async move { load_git_status(&path) }).await;
                let dropped = this
                    .update(cx, |this, cx| {
                        if this.git_status != status {
                            this.git_status = status;
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

        Self {
            active_tab: WorkspaceTab::Agent,
            tabs,
            review,
            project_name: project_name.into(),
            git_status: GitStatus::default(),
            command_open: false,
            command_state,
            data_root,
            sync_tracker: SyncTracker::default(),
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

    /// Run the confirmed palette entry. `index` addresses the model installed
    /// by the latest `Command` render (before filtering), so it maps back
    /// through [`command_at`] however the query narrowed the list.
    fn on_palette_confirm(
        &mut self,
        index: IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = command_at(index.section, index.row) else {
            self.close_command_palette(window, cx);
            return;
        };
        // Every path below leaves the bar closed; `select_tab` focuses and
        // notifies itself, the other arms do it explicitly.
        self.command_open = false;
        match command {
            PaletteCommand::GoAgent => self.select_tab(0, window, cx),
            PaletteCommand::GoEditor => self.select_tab(1, window, cx),
            PaletteCommand::GoTerminal => self.select_tab(2, window, cx),
            PaletteCommand::GoReview => self.select_tab(3, window, cx),
            // Placeholders until their views exist: visible and searchable so
            // the bar advertises the roadmap, honest about doing nothing yet.
            PaletteCommand::GoHome
            | PaletteCommand::SwitchRepository
            | PaletteCommand::OpenSettings => {
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
        }
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

    fn render_command_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        for (heading, items) in GROUPS {
            command = command.group(
                CommandGroup::new().label(heading).items(
                    items
                        .iter()
                        .copied()
                        .map(|item| {
                            CommandItem::new()
                                .label(item.label())
                                .keywords(item.keywords().iter().copied())
                                .icon(palette_icon(item))
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
        match self.tabs.get(self.active_tab as usize).and_then(|tab| tab.clone()) {
            Some(pane) => pane.into_any_element(),
            None => div().size_full().into_any_element(),
        }
    }

    /// Branch pill for the header: `⎇ <branch>` on a branch, `➦ <short-sha>`
    /// on a detached HEAD. Long branch names truncate instead of pushing the
    /// command bar aside.
    fn render_branch_pill(&self, branch: SharedString) -> impl IntoElement {
        let glyph = if self.git_status.detached { "➦" } else { "⎇" };
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
        PaletteCommand::SwitchRepository => IconName::Folder,
        PaletteCommand::OpenSettings => IconName::Settings,
        PaletteCommand::SyncPortable => IconName::RotateCw,
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_index = self.active_tab as usize;
        let active_content = self.render_active_content();

        v_flex()
            .relative()
            .size_full()
            .bg(rgb(0x080909))
            .text_color(rgb(0xe7e7e7))
            .on_action(cx.listener(
                |this, _: &ToggleCommandPalette, window, cx| {
                    this.toggle_command_palette(window, cx);
                },
            ))
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
                            .when_some(
                                self.git_status.branch.clone(),
                                |this, branch| {
                                    this.child(self.render_branch_pill(branch.into()))
                                },
                            )
                            // Amber dot while staged, unstaged, or untracked
                            // changes exist; hidden when clean so the steady
                            // state stays quiet.
                            .when(self.git_status.dirty, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0xeab308))
                                        .child("●"),
                                )
                            })
                            .when_some(self.git_status.ahead_label(), |this, ahead| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x858989))
                                        .child(ahead),
                                )
                            })
                            .when_some(self.git_status.behind_label(), |this, behind| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x858989))
                                        .child(behind),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .justify_center()
                            .child(
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
                        .child(
                            div()
                                .absolute()
                                .size_full()
                                .bg(rgb(0x000000))
                                .opacity(0.4),
                        )
                        .child(
                            h_flex()
                                .justify_center()
                                .pt(px(8.))
                                .child(
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
