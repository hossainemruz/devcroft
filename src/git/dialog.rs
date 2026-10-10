mod remote_ui;

use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    scroll::ScrollableElement as _,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Styled, Window, div, px,
    rgb,
};

use crate::{diff::viewer::DiffViewer, git_status::GitStatus};

use super::{
    diff::{ChangeArea, DiffTarget, load_change_diff},
    model::{Change, RepositorySnapshot},
    operations::{
        Mutation, TargetFingerprint, capture_restore_target, capture_trash_target, execute,
        trash_available,
    },
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
    Loaded(Box<RepositorySnapshot>),
    NotRepository,
    GitUnavailable,
    Failed(String),
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotChanged {
    pub(crate) checkout: PathBuf,
    pub(crate) status: GitStatus,
}

#[derive(Clone, Debug)]
pub(crate) struct OpenGitFile {
    pub(crate) checkout: PathBuf,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirmation {
    Remote {
        mutation: Mutation,
        title: String,
        detail: String,
    },
    Restore {
        change: Change,
        fingerprint: TargetFingerprint,
    },
    Trash {
        path: PathBuf,
        fingerprint: TargetFingerprint,
    },
}

#[derive(Clone, Debug)]
struct OperationNotice {
    message: String,
    error: bool,
}

pub(crate) struct GitDialog {
    focus_handle: FocusHandle,
    checkout: PathBuf,
    mode: GitMode,
    state: LoadState,
    generation: u64,
    load_in_flight: bool,
    file_changes: FileChanges,
    selection: Option<DiffTarget>,
    diff_generation: u64,
    diff_viewer: Entity<DiffViewer>,
    commit_message: Entity<TextareaState>,
    mutation_queue: VecDeque<Mutation>,
    active_mutation: Option<Mutation>,
    mutation_generation: u64,
    notice: Option<OperationNotice>,
    confirmation: Option<Confirmation>,
    dirty_editor_paths: HashSet<PathBuf>,
    branch_query: Entity<InputState>,
    branch_name: Entity<InputState>,
    push_branch: Entity<InputState>,
    selected_ref: Option<String>,
    selected_remote: Option<String>,
    remote_form: Option<remote_ui::RemoteForm>,
    active_started: Option<Instant>,
    check_conflicts: bool,
}

impl EventEmitter<SnapshotChanged> for GitDialog {}
impl EventEmitter<OpenGitFile> for GitDialog {}

impl GitDialog {
    pub(crate) fn new(
        checkout: PathBuf,
        dirty_editor_paths: HashSet<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let commit_message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 7)
                .placeholder("Commit message")
        });
        cx.observe(&commit_message, |_, _, cx| cx.notify()).detach();
        let branch_query = cx.new(|cx| InputState::new(window, cx).placeholder("Search branches…"));
        let branch_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("New local branch name"));
        let push_branch =
            cx.new(|cx| InputState::new(window, cx).placeholder("Remote branch name"));
        for input in [&branch_query, &branch_name, &push_branch] {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }
        let mut dialog = Self {
            focus_handle: cx.focus_handle(),
            file_changes: FileChanges::new(&checkout),
            checkout,
            mode: GitMode::Changes,
            state: LoadState::Loading,
            generation: 0,
            load_in_flight: false,
            selection: None,
            diff_generation: 0,
            diff_viewer: cx.new(|_| DiffViewer::new()),
            commit_message,
            mutation_queue: VecDeque::new(),
            active_mutation: None,
            mutation_generation: 0,
            notice: None,
            confirmation: None,
            dirty_editor_paths,
            branch_query,
            branch_name,
            push_branch,
            selected_ref: None,
            selected_remote: None,
            remote_form: None,
            active_started: None,
            check_conflicts: false,
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
        if self.active_mutation.is_some() {
            cx.notify();
            return;
        }
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
                        let changed = !matches!(&this.state, LoadState::Loaded(current) if current.as_ref() == &snapshot);
                        let status = snapshot.header_status();
                        if this.check_conflicts {
                            this.check_conflicts = false;
                            if !snapshot.conflicts.is_empty() || snapshot.operation.is_some() {
                                this.mode = GitMode::Changes;
                                this.selection = None;
                            }
                        }
                        if this.selected_ref.as_ref().is_some_and(|reference| !snapshot.branches.iter().any(|branch| &branch.reference == reference)) {
                            this.selected_ref = None;
                        }
                        this.selection = reconcile_selection(this.selection.take(), &snapshot);
                        this.state = LoadState::Loaded(Box::new(snapshot));
                        this.load_selected_diff(cx);
                        if changed {
                            cx.emit(SnapshotChanged {
                                checkout: this.checkout.clone(),
                                status,
                            });
                        }
                    }
                    Err(LoadError::NotRepository) => this.fail_load(LoadState::NotRepository, cx),
                    Err(LoadError::GitUnavailable) => {
                        this.fail_load(LoadState::GitUnavailable, cx)
                    }
                    Err(LoadError::Failed(message)) => {
                        this.fail_load(LoadState::Failed(message), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fail_load(&mut self, state: LoadState, cx: &mut Context<Self>) {
        self.state = state;
        self.selection = None;
        self.diff_generation += 1;
        self.diff_viewer.update(cx, |viewer, cx| viewer.clear(cx));
    }

    fn select_change(&mut self, target: DiffTarget, cx: &mut Context<Self>) {
        if self.selection.as_ref() == Some(&target) {
            return;
        }
        self.selection = Some(target);
        self.confirmation = None;
        self.load_selected_diff(cx);
        cx.notify();
    }

    fn load_selected_diff(&mut self, cx: &mut Context<Self>) {
        self.diff_generation += 1;
        let generation = self.diff_generation;
        let Some(target) = self.selection.clone() else {
            self.diff_viewer.update(cx, |viewer, cx| viewer.clear(cx));
            return;
        };
        let label = target.label();
        self.diff_viewer
            .update(cx, |viewer, cx| viewer.set_loading(label.clone(), cx));
        let checkout = self.checkout.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_change_diff(&checkout, &target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.diff_generation != generation {
                    return;
                }
                match result {
                    Ok(diff) => this
                        .diff_viewer
                        .update(cx, |viewer, cx| viewer.set_loaded(diff, cx)),
                    Err(message) => this.diff_viewer.update(cx, |viewer, cx| {
                        viewer.set_error(label.clone(), message, cx)
                    }),
                }
            });
        })
        .detach();
    }

    fn enqueue_mutation(
        &mut self,
        mutation: Mutation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Mutation::Remote { action, .. } = &mutation
            && action.changes_worktree()
            && !self.dirty_editor_paths.is_empty()
        {
            self.notice = Some(OperationNotice {
                message: "Save or discard unsaved editor drafts before changing the working tree."
                    .into(),
                error: true,
            });
            cx.notify();
            return;
        }
        self.confirmation = None;
        self.mutation_queue.push_back(mutation);
        self.start_next_mutation(window, cx);
    }

    fn start_next_mutation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_mutation.is_some() {
            return;
        }
        let Some(mutation) = self.mutation_queue.pop_front() else {
            return;
        };
        self.mutation_generation += 1;
        let generation = self.mutation_generation;
        self.notice = Some(OperationNotice {
            message: mutation.progress(),
            error: false,
        });
        self.active_mutation = Some(mutation.clone());
        self.active_started = Some(Instant::now());
        let checkout = self.checkout.clone();
        cx.spawn_in(window, async move |this, cx| {
            let task_mutation = mutation.clone();
            let result = cx
                .background_spawn(async move { execute(&checkout, &task_mutation) })
                .await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if this.mutation_generation != generation {
                        return;
                    }
                    this.active_mutation = None;
                    this.active_started = None;
                    this.check_conflicts = matches!(mutation, Mutation::Remote { .. });
                    match result {
                        Ok(message) => {
                            if matches!(mutation, Mutation::Commit { .. }) {
                                this.commit_message
                                    .update(cx, |input, cx| input.set_value("", window, cx));
                            }
                            this.notice = Some(OperationNotice {
                                message,
                                error: false,
                            });
                            this.remote_form = None;
                        }
                        Err(message) => {
                            this.notice = Some(OperationNotice {
                                message,
                                error: true,
                            });
                        }
                    }
                    // A failed pull/rebase may still fetch refs or enter conflict state.
                    this.file_changes.mark_changed();
                    this.reload(true, cx);
                    this.start_next_mutation(window, cx);
                    cx.notify();
                })
            });
        })
        .detach();
        cx.notify();
    }

    fn stage_change(&mut self, change: &Change, window: &mut Window, cx: &mut Context<Self>) {
        self.enqueue_mutation(
            Mutation::Stage {
                paths: change_paths(change),
            },
            window,
            cx,
        );
    }

    fn unstage_change(&mut self, change: &Change, window: &mut Window, cx: &mut Context<Self>) {
        let unborn = matches!(&self.state, LoadState::Loaded(snapshot) if snapshot.unborn);
        self.enqueue_mutation(
            Mutation::Unstage {
                paths: change_paths(change),
                unborn,
            },
            window,
            cx,
        );
    }

    fn request_restore(&mut self, change: Change, cx: &mut Context<Self>) {
        let dirty = change_paths(&change)
            .into_iter()
            .find(|path| self.is_dirty_in_editor(path));
        if let Some(path) = dirty {
            self.notice = Some(OperationNotice {
                message: format!(
                    "{} has an unsaved editor draft. Save or discard that draft before replacing the working-tree file.",
                    path.display()
                ),
                error: true,
            });
        } else {
            match capture_restore_target(&self.checkout, &change.path) {
                Ok(fingerprint) => {
                    self.confirmation = Some(Confirmation::Restore {
                        change,
                        fingerprint,
                    });
                }
                Err(message) => {
                    self.notice = Some(OperationNotice {
                        message,
                        error: true,
                    });
                }
            }
        }
        cx.notify();
    }

    fn request_trash(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.is_dirty_in_editor(&path) {
            self.notice = Some(OperationNotice {
                message: format!(
                    "{} has an unsaved editor draft. Save or discard that draft before moving the file to Trash.",
                    path.display()
                ),
                error: true,
            });
        } else {
            match capture_trash_target(&self.checkout, &path) {
                Ok(fingerprint) => {
                    self.confirmation = Some(Confirmation::Trash { path, fingerprint });
                }
                Err(message) => {
                    self.notice = Some(OperationNotice {
                        message,
                        error: true,
                    });
                }
            }
        }
        cx.notify();
    }

    fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirmation) = self.confirmation.take() else {
            return;
        };
        let mutation = match confirmation {
            Confirmation::Remote { mutation, .. } => mutation,
            Confirmation::Restore {
                change,
                fingerprint,
            } => Mutation::Restore {
                change,
                fingerprint,
            },
            Confirmation::Trash { path, fingerprint } => Mutation::Trash { path, fingerprint },
        };
        self.enqueue_mutation(mutation, window, cx);
    }

    fn is_dirty_in_editor(&self, path: &Path) -> bool {
        let absolute = self.checkout.join(path);
        self.dirty_editor_paths.contains(&absolute)
            || absolute
                .canonicalize()
                .is_ok_and(|canonical| self.dirty_editor_paths.contains(&canonical))
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.checkout.join(&path).is_file() {
            self.notice = Some(OperationNotice {
                message: format!("{} is unavailable in the working tree.", path.display()),
                error: true,
            });
            cx.notify();
            return;
        }
        cx.emit(OpenGitFile {
            checkout: self.checkout.clone(),
            path,
        });
        window.close_dialog(cx);
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
                    .disabled(self.active_mutation.is_some())
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
        let busy = self.active_mutation.is_some();
        let has_conflicts = !snapshot.conflicts.is_empty();
        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                v_flex()
                    .w(px(390.))
                    .h_full()
                    .min_h_0()
                    .flex_none()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .h(px(40.))
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_semibold()
                                    .child(format!("Changes · {}", snapshot.change_count())),
                            )
                            .child(
                                Button::new("git-stage-all")
                                    .small()
                                    .ghost()
                                    .label("Stage All")
                                    .disabled(
                                        busy
                                            || has_conflicts
                                            || (snapshot.unstaged.is_empty()
                                                && snapshot.untracked.is_empty()),
                                    )
                                    .tooltip(if has_conflicts {
                                        "Resolve merge changes before staging all files"
                                    } else {
                                        "Stage every changed file"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.enqueue_mutation(Mutation::StageAll, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("git-unstage-all")
                                    .small()
                                    .ghost()
                                    .label("Unstage All")
                                    .disabled(
                                        busy || has_conflicts || snapshot.staged.is_empty(),
                                    )
                                    .tooltip(if has_conflicts {
                                        "Resolve merge changes before unstaging all files"
                                    } else {
                                        "Unstage every staged file"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let unborn = matches!(&this.state, LoadState::Loaded(snapshot) if snapshot.unborn);
                                        this.enqueue_mutation(
                                            Mutation::UnstageAll { unborn },
                                            window,
                                            cx,
                                        )
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(
                                v_flex()
                                    .w_full()
                                    .p_2()
                                    .gap_3()
                                    .child(self.render_change_section(
                                        "Merge Changes",
                                        ChangeArea::Conflict,
                                        &snapshot.conflicts,
                                        0xf87171,
                                        busy,
                                        cx,
                                    ))
                                    .child(self.render_change_section(
                                        "Staged Changes",
                                        ChangeArea::Staged,
                                        &snapshot.staged,
                                        0x4ade80,
                                        busy,
                                        cx,
                                    ))
                                    .child(self.render_change_section(
                                        "Changes",
                                        ChangeArea::Unstaged,
                                        &snapshot.unstaged,
                                        0xfbbf24,
                                        busy,
                                        cx,
                                    ))
                                    .child(self.render_change_section(
                                        "Untracked Files",
                                        ChangeArea::Untracked,
                                        &snapshot.untracked,
                                        0x60a5fa,
                                        busy,
                                        cx,
                                    )),
                            ),
                    )
                    .child(self.render_commit_panel(snapshot, cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .min_h_0()
                    .child(self.render_diff_header(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(self.diff_viewer.clone()),
                    ),
            )
            .into_any_element()
    }

    fn render_change_section(
        &self,
        title: &str,
        area: ChangeArea,
        changes: &[Change],
        color: u32,
        busy: bool,
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
                    .px_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(title.to_owned())
                    .child(changes.len().to_string()),
            )
            .children(
                changes
                    .iter()
                    .take(visible)
                    .cloned()
                    .enumerate()
                    .map(|(index, change)| {
                        let target = DiffTarget {
                            area,
                            change: change.clone(),
                        };
                        let selected = self.selection.as_ref() == Some(&target);
                        let select_target = target.clone();
                        let path = change.path.to_string_lossy().into_owned();
                        let mut row = h_flex()
                            .id((
                                "git-change-row",
                                area as usize * MAX_VISIBLE_CHANGES + index,
                            ))
                            .w_full()
                            .min_h(px(40.))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(selected, |row| row.bg(rgb(0x203442)))
                            .hover(|style| style.bg(rgb(0x202222)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.select_change(select_target.clone(), cx)
                                }),
                            )
                            .child(
                                div()
                                    .w(px(16.))
                                    .flex_none()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(rgb(color))
                                    .child(change.kind.label()),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().child(path))
                                    .when_some(
                                        change.original_path.as_ref(),
                                        |column, original| {
                                            column.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(format!(
                                                        "from {}",
                                                        original.to_string_lossy()
                                                    )),
                                            )
                                        },
                                    ),
                            );
                        row = match area {
                            ChangeArea::Staged => {
                                let action = change.clone();
                                row.child(
                                    Button::new(("git-unstage-file", index))
                                        .xsmall()
                                        .ghost()
                                        .label("Unstage")
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.unstage_change(&action, window, cx)
                                        })),
                                )
                            }
                            ChangeArea::Unstaged => {
                                let stage = change.clone();
                                let discard = change.clone();
                                row.child(
                                    Button::new(("git-stage-file", index))
                                        .xsmall()
                                        .ghost()
                                        .label("Stage")
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.stage_change(&stage, window, cx)
                                        })),
                                )
                                .when(
                                    discard_supported(&discard),
                                    |row| {
                                        row.child(
                                            Button::new(("git-discard-file", index))
                                                .xsmall()
                                                .ghost()
                                                .label("Discard")
                                                .disabled(busy)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.request_restore(discard.clone(), cx)
                                                })),
                                        )
                                    },
                                )
                            }
                            ChangeArea::Untracked => {
                                let stage = change.clone();
                                let trash = change.path.clone();
                                row.child(
                                    Button::new(("git-stage-untracked", index))
                                        .xsmall()
                                        .ghost()
                                        .label("Stage")
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.stage_change(&stage, window, cx)
                                        })),
                                )
                                .when(trash_available(), |row| {
                                    row.child(
                                        Button::new(("git-trash-file", index))
                                            .xsmall()
                                            .ghost()
                                            .label("Trash")
                                            .disabled(busy)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.request_trash(trash.clone(), cx)
                                            })),
                                    )
                                })
                            }
                            ChangeArea::Conflict => row,
                        };
                        row
                    }),
            )
            .when(changes.is_empty(), |view| {
                view.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(0x555a5a))
                        .child("No files"),
                )
            })
            .when(changes.len() > visible, |view| {
                view.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} more files", changes.len() - visible)),
                )
            })
            .into_any_element()
    }

    fn render_commit_panel(
        &self,
        snapshot: &RepositorySnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = self.commit_message.read(cx).value().to_string();
        let blank = message.trim().is_empty();
        let no_staged = snapshot.staged.is_empty();
        let conflicts = !snapshot.conflicts.is_empty();
        let disabled = self.active_mutation.is_some() || blank || no_staged || conflicts;
        let reason = if conflicts {
            "Resolve merge changes before committing"
        } else if no_staged {
            "Stage at least one file before committing"
        } else if blank {
            "Enter a commit message"
        } else {
            "Create commit"
        };
        v_flex()
            .flex_none()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child("Commit"),
            )
            .child(Textarea::new(&self.commit_message))
            .child(
                Button::new("git-commit")
                    .w_full()
                    .primary()
                    .label(if self.active_mutation.is_some() {
                        "Working…"
                    } else {
                        "Commit"
                    })
                    .tooltip(reason)
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let message = this.commit_message.read(cx).value().to_string();
                        this.enqueue_mutation(Mutation::Commit { message }, window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_confirmation(
        &self,
        confirmation: &Confirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, detail, action) = match confirmation {
            Confirmation::Remote { title, detail, .. } => {
                (title.clone(), detail.as_str(), "Confirm")
            }
            Confirmation::Restore { change, .. } => {
                let renamed = change.original_path.is_some();
                (
                    format!("Discard changes to {}?", change.path.display()),
                    if renamed {
                        "The renamed file will move to Trash and its original path will be restored from the index."
                    } else {
                        "The working-tree file will be replaced with the staged index version. This cannot be undone from Devcroft."
                    },
                    "Discard",
                )
            }
            Confirmation::Trash { path, .. } => (
                format!("Move {} to Trash?", path.display()),
                "This untracked file will leave the checkout and can be recovered from macOS Trash.",
                "Move to Trash",
            ),
        };
        h_flex()
            .flex_none()
            .min_h(px(58.))
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(rgb(0x332a16))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_semibold().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(detail.to_owned()),
                    ),
            )
            .child(
                Button::new("git-cancel-confirmation")
                    .small()
                    .ghost()
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirmation = None;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("git-confirm-destructive")
                    .small()
                    .danger()
                    .label(action)
                    .disabled(self.active_mutation.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_pending(window, cx))),
            )
            .into_any_element()
    }

    fn render_diff_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(target) = &self.selection else {
            return div().h(px(0.)).into_any_element();
        };
        let path = target.change.path.clone();
        let label = path.to_string_lossy().into_owned();
        let can_open = self.checkout.join(&path).is_file();
        h_flex()
            .h(px(40.))
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .font_semibold()
                    .child(label),
            )
            .child(
                Button::new("git-open-selected-file")
                    .small()
                    .ghost()
                    .label("Open in Editor")
                    .disabled(!can_open)
                    .tooltip(if can_open {
                        "Open this working-tree file in the built-in editor"
                    } else {
                        "This path is not a regular working-tree file"
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file(path.clone(), window, cx)
                    })),
            )
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
            .when(matches!(self.state, LoadState::Loaded(_)), |view| {
                view.child(self.render_remote_toolbar(cx))
            })
            .when_some(self.confirmation.as_ref(), |view, confirmation| {
                view.child(self.render_confirmation(confirmation, cx))
            })
            .when_some(self.notice.as_ref(), |view, notice| {
                let elapsed = self
                    .active_started
                    .map(|started| format!(" · {}s", started.elapsed().as_secs()))
                    .unwrap_or_default();
                view.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(if notice.error {
                            rgb(0xf87171).into()
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(format!("{}{elapsed}", notice.message)),
                )
            })
            .when(self.remote_form.is_some(), |view| {
                view.child(self.render_remote_form(cx))
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
    }
}

fn change_paths(change: &Change) -> Vec<PathBuf> {
    let mut paths = vec![change.path.clone()];
    if let Some(original) = &change.original_path
        && original != &change.path
    {
        paths.push(original.clone());
    }
    paths
}

fn discard_supported(change: &Change) -> bool {
    change.original_path.is_none() || trash_available()
}

fn reconcile_selection(
    current: Option<DiffTarget>,
    snapshot: &RepositorySnapshot,
) -> Option<DiffTarget> {
    current
        .and_then(|target| {
            changes(snapshot, target.area)
                .iter()
                .find(|change| change.path == target.change.path)
                .cloned()
                .map(|change| DiffTarget {
                    area: target.area,
                    change,
                })
        })
        .or_else(|| {
            [
                ChangeArea::Conflict,
                ChangeArea::Staged,
                ChangeArea::Unstaged,
                ChangeArea::Untracked,
            ]
            .into_iter()
            .find_map(|area| {
                changes(snapshot, area)
                    .first()
                    .cloned()
                    .map(|change| DiffTarget { area, change })
            })
        })
}

fn changes(snapshot: &RepositorySnapshot, area: ChangeArea) -> &[Change] {
    match area {
        ChangeArea::Conflict => &snapshot.conflicts,
        ChangeArea::Staged => &snapshot.staged,
        ChangeArea::Unstaged => &snapshot.unstaged,
        ChangeArea::Untracked => &snapshot.untracked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::model::ChangeKind;

    fn change(path: &str) -> Change {
        Change {
            path: path.into(),
            original_path: None,
            kind: ChangeKind::Modified,
            index_mode: None,
            index_oid: None,
        }
    }

    #[test]
    fn selection_survives_refresh_and_falls_back_in_stable_section_order() {
        let snapshot = RepositorySnapshot {
            staged: vec![change("staged")],
            unstaged: vec![change("worktree")],
            ..RepositorySnapshot::default()
        };
        let current = DiffTarget {
            area: ChangeArea::Unstaged,
            change: change("worktree"),
        };
        assert_eq!(
            reconcile_selection(Some(current.clone()), &snapshot),
            Some(current)
        );

        let fallback = reconcile_selection(
            Some(DiffTarget {
                area: ChangeArea::Untracked,
                change: change("gone"),
            }),
            &snapshot,
        )
        .unwrap();
        assert_eq!(fallback.area, ChangeArea::Staged);
        assert_eq!(fallback.change.path, PathBuf::from("staged"));
    }

    #[test]
    fn selection_keeps_its_path_and_refreshes_index_identity() {
        let mut current_change = change("tracked.txt");
        current_change.index_oid = Some("old".into());
        let mut refreshed_change = current_change.clone();
        refreshed_change.index_oid = Some("new".into());
        let snapshot = RepositorySnapshot {
            unstaged: vec![refreshed_change.clone()],
            ..RepositorySnapshot::default()
        };

        let selected = reconcile_selection(
            Some(DiffTarget {
                area: ChangeArea::Unstaged,
                change: current_change,
            }),
            &snapshot,
        )
        .unwrap();

        assert_eq!(selected.area, ChangeArea::Unstaged);
        assert_eq!(selected.change, refreshed_change);
    }

    #[test]
    fn rename_actions_include_both_repository_paths() {
        let change = Change {
            path: "new name.txt".into(),
            original_path: Some("old name.txt".into()),
            kind: ChangeKind::Renamed,
            index_mode: None,
            index_oid: None,
        };
        assert_eq!(
            change_paths(&change),
            [PathBuf::from("new name.txt"), PathBuf::from("old name.txt")]
        );
    }
}
