use std::{path::PathBuf, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Window, div, px, rgb,
};

use crate::git_status::GitStatus;

use super::{
    model::{Change, RepositorySnapshot},
    repository::{LoadError, load_snapshot},
    watch::FileChanges,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_VISIBLE_CHANGES: usize = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitMode {
    Changes,
    Branches,
    History,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LoadState {
    Loading,
    Loaded(RepositorySnapshot),
    NotRepository,
    GitUnavailable,
    Failed(String),
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotChanged {
    pub(crate) checkout: PathBuf,
    pub(crate) status: GitStatus,
}

pub(crate) struct GitDialog {
    focus_handle: FocusHandle,
    checkout: PathBuf,
    mode: GitMode,
    state: LoadState,
    generation: u64,
    load_in_flight: bool,
    file_changes: FileChanges,
}

impl EventEmitter<SnapshotChanged> for GitDialog {}

impl GitDialog {
    pub(crate) fn new(checkout: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut dialog = Self {
            focus_handle: cx.focus_handle(),
            file_changes: FileChanges::new(&checkout),
            checkout,
            mode: GitMode::Changes,
            state: LoadState::Loading,
            generation: 0,
            load_in_flight: false,
        };
        dialog.reload(false, cx);
        dialog.start_refresh_loop(cx);
        dialog
    }

    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.refresh_if_changed(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh_if_changed(&mut self, cx: &mut Context<Self>) {
        if !self.load_in_flight && self.file_changes.take_changed() {
            self.reload(true, cx);
        }
    }

    fn reload(&mut self, background: bool, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.load_in_flight = true;
        self.file_changes.take_changed();
        if !background || !matches!(self.state, LoadState::Loaded(_)) {
            self.state = LoadState::Loading;
            cx.notify();
        }
        let checkout = self.checkout.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_snapshot(&checkout) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.load_in_flight = false;
                match result {
                    Ok(snapshot) => {
                        let changed = !matches!(&this.state, LoadState::Loaded(current) if current == &snapshot);
                        let status = snapshot.header_status();
                        this.state = LoadState::Loaded(snapshot);
                        if changed {
                            cx.emit(SnapshotChanged {
                                checkout: this.checkout.clone(),
                                status,
                            });
                        }
                    }
                    Err(LoadError::NotRepository) => this.state = LoadState::NotRepository,
                    Err(LoadError::GitUnavailable) => this.state = LoadState::GitUnavailable,
                    Err(LoadError::Failed(message)) => this.state = LoadState::Failed(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn summary(&self) -> String {
        match &self.state {
            LoadState::Loaded(snapshot) => {
                let mut parts = vec![snapshot.branch_label()];
                if snapshot.ahead > 0 {
                    parts.push(format!("↑{}", snapshot.ahead));
                }
                if snapshot.behind > 0 {
                    parts.push(format!("↓{}", snapshot.behind));
                }
                if let Some(operation) = snapshot.operation {
                    parts.push(operation.label().to_owned());
                }
                parts.join("  ·  ")
            }
            _ => String::new(),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("git-dialog-mode")
                    .segmented()
                    .selected_index(self.mode as usize)
                    .on_click(cx.listener(|this, index: &usize, _, cx| {
                        this.mode = match *index {
                            1 => GitMode::Branches,
                            2 => GitMode::History,
                            _ => GitMode::Changes,
                        };
                        cx.notify();
                    }))
                    .children([
                        Tab::new().label("Changes"),
                        Tab::new().label("Branches"),
                        Tab::new().label("History"),
                    ]),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.summary()),
            )
            .child(
                Button::new("git-dialog-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RotateCw)
                    .tooltip("Refresh repository")
                    .accessibility_label("Refresh repository")
                    .loading(self.load_in_flight)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.file_changes.mark_changed();
                        this.reload(true, cx);
                    })),
            )
    }

    fn render_loaded(&self, snapshot: &RepositorySnapshot, cx: &mut Context<Self>) -> AnyElement {
        match self.mode {
            GitMode::Changes => self.render_changes(snapshot, cx),
            GitMode::Branches => self.render_branches(snapshot, cx),
            GitMode::History => self.render_history(snapshot, cx),
        }
    }

    fn render_changes(&self, snapshot: &RepositorySnapshot, cx: &mut Context<Self>) -> AnyElement {
        if snapshot.change_count() == 0 {
            return self
                .render_empty(
                    "Working tree clean",
                    "There are no staged, unstaged, untracked, or conflicted files.",
                    cx,
                )
                .into_any_element();
        }

        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .when_some(snapshot.operation, |view, operation| {
                view.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x332a16))
                        .text_sm()
                        .child(operation.label()),
                )
            })
            .when(!snapshot.conflicts.is_empty(), |view| {
                view.child(self.render_change_section(
                    "Conflicts",
                    &snapshot.conflicts,
                    0xf87171,
                    cx,
                ))
            })
            .when(!snapshot.staged.is_empty(), |view| {
                view.child(self.render_change_section(
                    "Staged changes",
                    &snapshot.staged,
                    0x4ade80,
                    cx,
                ))
            })
            .when(!snapshot.unstaged.is_empty(), |view| {
                view.child(self.render_change_section("Changes", &snapshot.unstaged, 0xfbbf24, cx))
            })
            .when(!snapshot.untracked.is_empty(), |view| {
                view.child(self.render_change_section(
                    "Untracked files",
                    &snapshot.untracked,
                    0x60a5fa,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_change_section(
        &self,
        title: &str,
        changes: &[Change],
        color: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible = changes.len().min(MAX_VISIBLE_CHANGES);
        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .pb_1()
                    .text_sm()
                    .font_semibold()
                    .child(title.to_owned())
                    .child(
                        div()
                            .text_xs()
                            .font_normal()
                            .text_color(cx.theme().muted_foreground)
                            .child(changes.len().to_string()),
                    ),
            )
            .children(changes.iter().take(visible).map(|change| {
                h_flex()
                    .h(px(28.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded_sm()
                    .hover(|style| style.bg(rgb(0x202222)))
                    .child(
                        div()
                            .w(px(16.))
                            .text_xs()
                            .font_semibold()
                            .text_color(rgb(color))
                            .child(change.kind.label()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(change.path.to_string_lossy().into_owned()),
                    )
                    .when_some(change.original_path.as_ref(), |row, original| {
                        row.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("from {}", original.to_string_lossy())),
                        )
                    })
            }))
            .when(changes.len() > visible, |view| {
                view.child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} more files", changes.len() - visible)),
                )
            })
            .into_any_element()
    }

    fn render_branches(&self, snapshot: &RepositorySnapshot, cx: &mut Context<Self>) -> AnyElement {
        let upstream = snapshot
            .upstream
            .as_deref()
            .unwrap_or("No upstream configured");
        let remotes = if snapshot.remotes.is_empty() {
            "No remotes configured".to_owned()
        } else {
            snapshot.remotes.join(", ")
        };
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .child(self.render_detail("Current branch", snapshot.branch_label(), cx))
            .child(self.render_detail("Upstream", upstream.to_owned(), cx))
            .child(self.render_detail("Remotes", remotes, cx))
            .into_any_element()
    }

    fn render_history(&self, snapshot: &RepositorySnapshot, cx: &mut Context<Self>) -> AnyElement {
        let head = snapshot
            .oid
            .as_deref()
            .map(|oid| oid[..oid.len().min(12)].to_owned())
            .unwrap_or_else(|| "No commits yet".to_owned());
        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .child(self.render_detail("HEAD", head, cx))
            .into_any_element()
    }

    fn render_detail(&self, label: &str, value: String, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_owned()),
            )
            .child(div().text_sm().child(value))
            .into_any_element()
    }

    fn render_empty(&self, title: &str, detail: &str, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .child(div().text_sm().font_semibold().child(title.to_owned()))
            .child(
                div()
                    .max_w(px(520.))
                    .text_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(detail.to_owned()),
            )
    }
}

impl Focusable for GitDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            LoadState::Loading => self
                .render_empty(
                    "Loading repository…",
                    "Reading Git status for this checkout.",
                    cx,
                )
                .into_any_element(),
            LoadState::Loaded(snapshot) => self.render_loaded(snapshot, cx),
            LoadState::NotRepository => self
                .render_empty(
                    "This folder is not a Git repository",
                    "Open a repository checkout, then open Git again.",
                    cx,
                )
                .into_any_element(),
            LoadState::GitUnavailable => self
                .render_empty(
                    "Git is unavailable",
                    "Install Git or make it available on the application PATH.",
                    cx,
                )
                .into_any_element(),
            LoadState::Failed(message) => self
                .render_empty("Could not load repository", message, cx)
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .tab_group()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(body),
            )
    }
}
