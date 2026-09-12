//! The Review tab: live diffs over the working tree.
//!
//! [`git`] is the only module that talks to `gix`; [`model`] is the
//! UI-ready diff model; [`tree`] builds the file-tree items; [`stream`]
//! renders diff rows; [`icons`] resolves file-type icon art. [`ReviewView`]
//! owns loading, scope selection, the tree/stream layout, viewed-file state,
//! and refresh.

pub(crate) mod comments;
mod feedback;
pub(crate) mod git;
mod icons;
pub(crate) mod model;
mod stream;
mod syntax;
mod tree;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex,
    list::ListItem,
    tab::{Tab, TabBar},
    tree::{TreeState, tree},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ListAlignment, ListOffset, ListState, MouseButton, ParentElement,
    Render, ScrollStrategy, SharedString, Styled, Window, div, img, list, px, rgb, svg,
};

use crate::command_palette::{
    GoToAgent, GoToEditor, GoToReview, GoToResources, GoToTerminal, NewAgentSession, PaletteMode,
    ToggleActionsPalette, ToggleProjectsPalette, ToggleSessionsPalette, is_go_to_agent_shortcut,
    is_go_to_editor_shortcut, is_go_to_review_shortcut, is_go_to_resources_shortcut,
    is_go_to_terminal_shortcut, is_new_session_shortcut, palette_mode_for_shortcut,
};

use self::git::{ReviewScope, load_review, suggest_base_branch};
use self::icons::{FALLBACK, ICON_PX, IconTiles, ensure_tiles, icon_key};
use self::model::ReviewDiff;
use self::stream::{file_header_row, flatten, render_row, status_color};
use self::tree::{TreeRowMeta, build_file_tree, file_item_id, file_path_from_id};

/// Lucide `message-square` outline, inlined because `gpui-kit-assets`
/// ships no comment glyph. `stroke="currentColor"` lets the toolbar
/// `text_color` tint it like the neighboring `Icon` glyphs.
const COMMENT_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeTab {
    FullDiff = 0,
    Uncommitted = 1,
}

enum ReviewState {
    Loading,
    Loaded(Rc<LoadedReview>),
    Failed(SharedString),
}

/// A loaded diff plus its virtualized stream rows.
///
/// `rows` flattens every file section once per load (or per collapse toggle);
/// `file_row_start[file]` is that file's first row (its header), so
/// tree↔stream scrolling maps between file indices and row indices. `diff`
/// and `syntax` are shared across collapse rebuilds so toggling a file never
/// re-clones file text or highlight spans.
pub(crate) struct LoadedReview {
    pub(crate) diff: Rc<ReviewDiff>,
    pub(crate) rows: Vec<stream::StreamRow>,
    pub(crate) file_row_start: Vec<usize>,
    pub(crate) syntax: Rc<syntax::SyntaxHighlights>,
}

pub(crate) struct ReviewView {
    pub(crate) focus_handle: FocusHandle,
    cwd: PathBuf,
    scope_tab: ScopeTab,
    base_branch: String,
    remote: String,
    state: ReviewState,
    tree_state: Entity<TreeState>,
    tree_metas: Rc<HashMap<String, TreeRowMeta>>,
    viewed: Rc<RefCell<HashSet<String>>>,
    /// Collapsed files by repo-relative path, GitHub PR style. Scoped to the
    /// current review (see `review_key`): survives refreshes of the same
    /// branch pair, resets on branch/scope change, and re-flattens `rows` on
    /// toggle without re-running git or syntax highlighting.
    collapsed: Rc<RefCell<HashSet<String>>>,
    /// Identifies the review that `viewed`/`collapsed` belong to
    /// (`scope|base|head`). A branch or scope switch is a different review,
    /// so stale marks must not leak across it.
    review_key: Option<String>,
    /// Full-color raster tiles per icon key, filled at load and shared by
    /// every row render. Rasterizing here (not per frame) keeps scrolling
    /// smooth; the `Rc` lets the virtualized row closures share the cache.
    icon_tiles: Rc<IconTiles>,
    list_handle: ListState,
    editor_row: Cell<Option<usize>>,
    comment_layout: RefCell<Vec<(String, u64)>>,
    /// First visible logical row from ListState, used for tree scroll sync.
    stream_top: Rc<Cell<usize>>,
    last_scrolled: Option<String>,
    /// Monotonic load id; stale background loads never overwrite newer state.
    generation: u64,
    feedback: feedback::Feedback,
    show_comments: bool,
}

impl ReviewView {
    pub(crate) fn new(cwd: &Path, cx: &mut Context<Self>) -> Self {
        let base_branch = suggest_base_branch(cwd, "origin").unwrap_or_else(|| "main".to_owned());
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            cwd: cwd.to_owned(),
            scope_tab: ScopeTab::FullDiff,
            base_branch,
            remote: "origin".to_owned(),
            state: ReviewState::Loading,
            tree_state: cx.new(|cx| TreeState::new(cx)),
            tree_metas: Rc::new(HashMap::new()),
            viewed: Rc::default(),
            collapsed: Rc::default(),
            review_key: None,
            icon_tiles: Rc::default(),
            list_handle: ListState::new(0, ListAlignment::Top, px(300.)),
            editor_row: Cell::new(None),
            comment_layout: RefCell::default(),
            stream_top: Rc::default(),
            last_scrolled: None,
            generation: 0,
            feedback: feedback::Feedback::default(),
            show_comments: true,
        };
        view.reload(cx);
        view
    }

    fn scope(&self) -> ReviewScope {
        match self.scope_tab {
            ScopeTab::FullDiff => {
                ReviewScope::full_diff(self.base_branch.clone(), self.remote.clone())
            }
            ScopeTab::Uncommitted => ReviewScope::UncommittedChanges,
        }
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = ReviewState::Loading;
        self.last_scrolled = None;
        self.generation += 1;
        let generation = self.generation;
        cx.notify();
        let cwd = self.cwd.clone();
        let scope = self.scope();
        let base = self.base_branch.clone();
        let remote = self.remote.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let diff = load_review(&cwd, &scope)?;
                    let syntax = syntax::highlight(&diff);
                    let feedback = comments::Store::open(&cwd, &base, &remote, &scope, &diff)
                        .and_then(|store| {
                            store.refresh(&cwd, &diff).map(|comments| (store, comments))
                        });
                    anyhow::Ok((diff, syntax, feedback))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                match loaded {
                    Ok((diff, syntax, feedback)) => {
                        view.load_comments(feedback);
                        view.apply_diff(diff, syntax, cx);
                    }
                    Err(error) => {
                        view.state = ReviewState::Failed(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Reload when the tab becomes visible, unless a load is in flight.
    /// Freshness beats cached rows here: agents change the worktree under us.
    pub(crate) fn activate(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ReviewState::Loading) {
            return;
        }
        self.reload(cx);
    }

    fn review_identity(&self, diff: &ReviewDiff) -> String {
        let scope = match self.scope_tab {
            ScopeTab::FullDiff => format!("full:{}:{}", self.remote, self.base_branch),
            ScopeTab::Uncommitted => "uncommitted".to_owned(),
        };
        format!(
            "{scope}|{}|{}|{}|{}",
            diff.base_commit,
            diff.head_commit,
            diff.head_branch.as_deref().unwrap_or(""),
            diff.base_ref.as_deref().unwrap_or(""),
        )
    }

    fn apply_diff(
        &mut self,
        diff: ReviewDiff,
        syntax: syntax::SyntaxHighlights,
        cx: &mut Context<Self>,
    ) {
        let key = self.review_identity(&diff);
        if self.review_key.as_deref() == Some(&key) {
            // Same review, fresh worktree content: keep progress, drop marks
            // for files that no longer exist in the diff.
            self.collapsed
                .borrow_mut()
                .retain(|path| diff.files.iter().any(|file| &file.path == path));
            self.viewed
                .borrow_mut()
                .retain(|path| diff.files.iter().any(|file| &file.path == path));
        } else {
            // Branch, base, or scope changed: a different review, so stale
            // viewed/collapsed marks must not leak across it.
            self.collapsed.borrow_mut().clear();
            self.viewed.borrow_mut().clear();
            self.review_key = Some(key);
        }
        let (items, metas) = build_file_tree(&diff.files);
        let (rows, file_row_start) = flatten(&diff, &self.collapsed.borrow());
        self.list_handle.reset(rows.len() + 1);
        self.editor_row.set(None);
        let mut tiles = (*self.icon_tiles).clone();
        ensure_tiles(
            diff.files.iter().map(|file| file.path.as_str()),
            &mut tiles,
            cx,
        );
        self.icon_tiles = Rc::new(tiles);
        self.tree_metas = Rc::new(metas);
        self.tree_state.update(cx, |state, cx| {
            state.set_items(items, cx);
        });
        self.stream_top.set(0);
        self.state = ReviewState::Loaded(Rc::new(LoadedReview {
            diff: Rc::new(diff),
            rows,
            file_row_start,
            syntax: Rc::new(syntax),
        }));
        cx.notify();
    }

    fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        let now_viewed = {
            let mut viewed = self.viewed.borrow_mut();
            if viewed.remove(path) {
                false
            } else {
                viewed.insert(path.to_owned());
                true
            }
        };
        // GitHub PR behavior: marking a file viewed collapses it;
        // unmarking expands it again. A manual chevron toggle stays
        // independent (it only touches `collapsed`, never `viewed`).
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if now_viewed {
                collapsed.insert(path.to_owned());
            } else {
                collapsed.remove(path);
            }
        }
        self.rebuild_rows_around(path, cx);
    }

    pub(crate) fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if !collapsed.remove(path) {
                collapsed.insert(path.to_owned());
            }
        }
        self.rebuild_rows_around(path, cx);
    }

    /// Re-flatten `rows` from the loaded diff plus the current `collapsed`
    /// set, then restore the viewport on the given file header.
    fn rebuild_rows_around(&mut self, path: &str, cx: &mut Context<Self>) {
        let ReviewState::Loaded(loaded) = &self.state else {
            cx.notify();
            return;
        };
        let collapsed = self.collapsed.borrow().clone();
        let (rows, file_row_start) = flatten(&loaded.diff, &collapsed);
        let scroll_to = loaded
            .diff
            .files
            .iter()
            .position(|file| file.path == path)
            .map(|file_ix| file_row_start[file_ix]);
        self.state = ReviewState::Loaded(Rc::new(LoadedReview {
            diff: loaded.diff.clone(),
            rows,
            file_row_start,
            syntax: loaded.syntax.clone(),
        }));
        // The row count changed; reset virtualization then restore context on
        // the toggled file header so the viewport doesn't jump elsewhere.
        self.editor_row.set(None);
        if let ReviewState::Loaded(loaded) = &self.state {
            self.list_handle.reset(loaded.rows.len() + 1);
            if let Some(item_ix) = scroll_to {
                self.list_handle.scroll_to(ListOffset {
                    item_ix,
                    offset_in_item: px(0.),
                });
                self.stream_top.set(item_ix);
                self.last_scrolled = Some(path.to_owned());
            }
        }
        cx.notify();
    }

    /// Forward the command-bar toggles and the go-to-tab shortcuts to
    /// the workspace, mirroring `TerminalPane::on_key_down`. The tree/stream
    /// children don't swallow keys today, but without this any future child
    /// that stops propagation would silently break `cmd-k`/`cmd-p`/`cmd-a`/
    /// `cmd-e`/`cmd-/`/`cmd-d`/`cmd-t` on this tab again. All other keys bubble normally (no
    /// `prevent_default`/`stop_propagation`) so tree navigation and list
    /// scrolling keep working.
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
        if is_go_to_review_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToReview), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_go_to_resources_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToResources), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_new_session_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(NewAgentSession), cx);
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
                PaletteMode::Sessions => {
                    window.dispatch_action(Box::new(ToggleSessionsPalette), cx);
                }
            }
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn base_label(&self) -> String {
        match &self.state {
            ReviewState::Loaded(loaded) => match self.scope_tab {
                ScopeTab::FullDiff => format!(
                    "base {} · {}",
                    loaded.diff.base_ref.as_deref().unwrap_or(&self.base_branch),
                    loaded
                        .diff
                        .head_branch
                        .as_deref()
                        .unwrap_or("detached HEAD")
                ),
                ScopeTab::Uncommitted => format!(
                    "uncommitted · {}",
                    loaded
                        .diff
                        .head_branch
                        .as_deref()
                        .unwrap_or("detached HEAD")
                ),
            },
            _ => match self.scope_tab {
                ScopeTab::FullDiff => format!("base {}/{}", self.remote, self.base_branch),
                ScopeTab::Uncommitted => "uncommitted changes".to_owned(),
            },
        }
    }

    fn summary(&self) -> String {
        match &self.state {
            ReviewState::Loaded(loaded) => {
                let additions: u32 = loaded.diff.files.iter().map(|file| file.additions).sum();
                let deletions: u32 = loaded.diff.files.iter().map(|file| file.deletions).sum();
                format!(
                    "{} files · +{additions} −{deletions}",
                    loaded.diff.files.len()
                )
            }
            _ => String::new(),
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let scope_index = self.scope_tab as usize;
        let comment_tint = rgb(if self.show_comments {
            0xe7e7e7
        } else {
            0x737878
        });
        h_flex()
            .h(px(40.))
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(rgb(0x292b2b))
            .child(
                TabBar::new("review-scope")
                    .segmented()
                    .selected_index(scope_index)
                    .on_click(cx.listener(|this, index: &usize, _window, cx| {
                        let next = if *index == 0 {
                            ScopeTab::FullDiff
                        } else {
                            ScopeTab::Uncommitted
                        };
                        if this.scope_tab != next {
                            this.scope_tab = next;
                            this.reload(cx);
                        }
                    }))
                    .children([
                        Tab::new().label("Full diff"),
                        Tab::new().label("Uncommitted"),
                    ]),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .child(self.base_label()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x737878))
                    .child(self.summary()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .gap_1()
                    .items_center()
                    .border_color(rgb(if self.show_comments {
                        0x7dd3fc
                    } else {
                        0x292b2b
                    }))
                    .text_xs()
                    .text_color(comment_tint)
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.show_comments = !this.show_comments;
                            cx.notify();
                        }),
                    )
                    .child(
                        svg()
                            .data(COMMENT_ICON)
                            .size(px(14.))
                            .text_color(comment_tint),
                    )
                    .child(format!("{}", self.comment_count())),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x292b2b))
                    .text_xs()
                    .text_color(rgb(0xe7e7e7))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.reload(cx)),
                    )
                    .child("Refresh"),
            )
    }

    fn render_tree(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let metas = self.tree_metas.clone();
        let viewed = self.viewed.clone();
        let tiles = self.icon_tiles.clone();
        tree(
            &self.tree_state,
            move |ix, entry, _selected, _window, _cx| {
                let id = entry.item().id.to_string();
                let fallback = entry.item().label.to_string();
                let meta = metas.get(&id);
                let name = meta.map(|meta| meta.name.clone()).unwrap_or(fallback);
                let is_folder = meta.is_some_and(|meta| meta.is_folder);
                let chevron = if is_folder {
                    if entry.is_expanded() { "▾ " } else { "▸ " }
                } else {
                    ""
                };
                let is_viewed =
                    file_path_from_id(&id).is_some_and(|path| viewed.borrow().contains(path));
                // Files show full-color Material tiles; folders keep neutral
                // gpui-kit glyphs. The colored A/M/D/R/T letter (not the icon)
                // carries git status. Viewed files dim to match their names.
                let icon: AnyElement = if is_folder {
                    Icon::new(if entry.is_expanded() {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    })
                    .size(px(ICON_PX))
                    .text_color(rgb(0x858989))
                    .into_any_element()
                } else {
                    let tile = tiles.get(icon_key(&name)).or_else(|| tiles.get(FALLBACK));
                    match tile {
                        Some(tile) => img(tile.clone())
                            .size(px(ICON_PX))
                            .when(is_viewed, |this| this.opacity(0.45))
                            .into_any_element(),
                        // Unreachable: apply_diff pre-rasterizes every key plus
                        // the fallback. An empty slot beats a panic on skew.
                        None => div().size(px(ICON_PX)).into_any_element(),
                    }
                };
                let status_cue = match meta.and_then(|meta| meta.status) {
                    // Single-letter status cue, right-aligned like VS Code's
                    // explorer. The per-file +/- counts live on the diff headers.
                    Some(status) if !is_folder => div()
                        .text_xs()
                        .font_semibold()
                        .text_color(rgb(status_color(status)))
                        .child(status.abbrev()),
                    _ => div(),
                };
                ListItem::new(ix).child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .pl(px(entry.depth() as f32 * 14.0 + 8.0))
                        .text_sm()
                        .when(is_viewed && !is_folder, |this| {
                            this.text_color(rgb(0x737878))
                        })
                        .child(div().text_color(rgb(0x555a5a)).child(chevron))
                        .child(icon)
                        .child(div().child(name))
                        .child(div().flex_1())
                        .child(status_cue)
                        .when(is_viewed && !is_folder, |this| {
                            this.child(div().text_xs().text_color(rgb(0x4ade80)).child("✓"))
                        }),
                )
            },
        )
        .size_full()
    }

    /// Keep tree selection and stream scroll in sync, in both directions.
    ///
    /// A fresh tree selection (click/keyboard) wins and scrolls the stream
    /// to the file's first row. Otherwise the viewport wins: the file owning
    /// the recorded top row becomes the tree selection (scroll-spy), and the
    /// stream branch claims `last_scrolled` immediately — pinning the stream
    /// to a lagging selection would yank the viewport backwards mid-scroll,
    /// so the tree branch only ever fires for genuine tree interaction. The
    /// two branches converge within one extra frame because each only fires
    /// on a change.
    fn sync_tree_and_stream(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .tree_state
            .read(cx)
            .selected_entry()
            .and_then(|entry| file_path_from_id(&entry.item().id).map(str::to_owned));
        if selected != self.last_scrolled {
            if let ReviewState::Loaded(loaded) = &self.state
                && let Some(path) = &selected
                && let Some(file) = loaded.diff.files.iter().position(|file| &file.path == path)
            {
                self.list_handle.scroll_to(ListOffset {
                    item_ix: loaded.file_row_start[file],
                    offset_in_item: px(0.),
                });
            }
            self.last_scrolled = selected;
            return;
        }
        if let ReviewState::Loaded(loaded) = &self.state {
            let top = loaded
                .rows
                .get(self.stream_top.get())
                .map(|row| loaded.diff.files[row.file()].path.clone());
            if top != selected
                && let Some(path) = top
            {
                // The viewport is already showing this file: claim it without
                // scrolling so fast flings never fight the user's direction.
                self.last_scrolled = Some(path.clone());
                let id: SharedString = file_item_id(&path).into();
                self.tree_state.update(cx, |state, cx| {
                    state.set_selected_index(state.index_of(&id), cx);
                    state.reveal_item(&id, ScrollStrategy::Center, cx);
                });
            }
        }
    }

    /// File whose header should stick to the top of the diff pane, if any.
    ///
    /// Returns `None` when the stream is empty or when the viewport sits
    /// exactly on a file header (the in-flow header is fully visible, so a
    /// duplicate sticky bar would just double-render). Otherwise returns the
    /// file owning the top visible row, GitHub PR style.
    fn sticky_file_ix(&self, loaded: &LoadedReview) -> Option<usize> {
        let top = self.list_handle.logical_scroll_top();
        let row = loaded.rows.get(top.item_ix)?;
        let file = row.file();
        let at_header_top =
            matches!(row, stream::StreamRow::FileHeader { .. }) && top.offset_in_item == px(0.);
        if at_header_top { None } else { Some(file) }
    }

    fn render_sticky_header(
        &self,
        loaded: &Rc<LoadedReview>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let file_ix = self.sticky_file_ix(loaded)?;
        let file = &loaded.diff.files[file_ix];
        let view = cx.entity();
        Some(
            div()
                .flex_none()
                .w_full()
                .child(file_header_row(
                    file,
                    self.viewed.borrow().contains(&file.path),
                    self.collapsed.borrow().contains(&file.path),
                    &view,
                ))
                .into_any_element(),
        )
    }

    /// The virtualized diff stream: only the visible row window is built.
    ///
    /// Variable-height virtualization lets the editor live below its anchor.
    fn render_stream(&self, loaded: &Rc<LoadedReview>, cx: &mut Context<Self>) -> impl IntoElement {
        let revisions = self.comment_revisions();
        if *self.comment_layout.borrow() != revisions {
            *self.comment_layout.borrow_mut() = revisions;
            let top = self.list_handle.logical_scroll_top();
            self.list_handle.reset(loaded.rows.len() + 1);
            self.list_handle.scroll_to(top);
        }
        let editor = self.inline_editor_row();
        let previous = self.editor_row.replace(editor);
        if previous != editor {
            for ix in [previous, editor].into_iter().flatten() {
                if ix <= loaded.rows.len() {
                    self.list_handle.splice(ix..ix + 1, 1);
                }
            }
            if let Some(ix) = editor {
                self.list_handle.scroll_to(ListOffset {
                    item_ix: ix.saturating_sub(3),
                    offset_in_item: px(0.),
                });
            }
        }
        let loaded = loaded.clone();
        let dark = cx.theme().is_dark();
        let viewed = self.viewed.clone();
        let collapsed = self.collapsed.clone();
        let thread_groups = self.inline_thread_groups();
        let view = cx.entity();
        let weak = view.downgrade();
        self.list_handle.set_scroll_handler(move |_, _, cx| {
            let _ = weak.update(cx, |_, cx| cx.notify());
        });
        list(self.list_handle.clone(), move |ix, _window, cx| {
            if ix == loaded.rows.len() {
                return view.update(cx, |this, cx| {
                    v_flex()
                        .w_full()
                        .p_3()
                        .child(div().text_xs().text_color(rgb(0x858989)).child(
                            if loaded.rows.is_empty() {
                                "No changes in this scope. Saved review comments:"
                            } else {
                                "Comments outside the current diff"
                            },
                        ))
                        .child(
                            this.render_inline_threads(
                                thread_groups
                                    .get(&ix)
                                    .map(Vec::as_slice)
                                    .unwrap_or_default(),
                                cx,
                            ),
                        )
                        .into_any_element()
                });
            }
            let row = loaded.rows[ix];
            let (selected, count) = view.read(cx).line_feedback(row);
            let target = view.clone();
            let row_h = match row {
                stream::StreamRow::FileHeader { .. } => stream::FILE_HEADER_H,
                _ => stream::ROW_H,
            };
            let line = h_flex()
                .h(px(row_h))
                .flex_none()
                .w_full()
                .when(selected, |d| d.bg(rgb(0x203442)))
                .border_l_2()
                .border_color(rgb(if selected {
                    0x7dd3fc
                } else if count > 0 {
                    0xfbbf24
                } else {
                    0x090a0a
                }))
                .child(div().flex_1().min_w_0().child(render_row(
                    &loaded,
                    row,
                    &viewed.borrow(),
                    &collapsed.borrow(),
                    &view,
                    selected,
                    dark,
                )))
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    if matches!(row, stream::StreamRow::Line { .. }) {
                        cx.stop_propagation();
                        target.update(cx, |this, cx| {
                            this.select_line(row, event.modifiers.shift, window, cx)
                        });
                    }
                });
            let mut item = v_flex().w_full().child(line);
            let threads = view.update(cx, |this, cx| {
                this.render_inline_threads(
                    thread_groups
                        .get(&ix)
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    cx,
                )
            });
            item = item.child(threads);
            if editor == Some(ix) && view.read(cx).is_new_comment() {
                let editor = view.update(cx, |this, cx| this.render_comment_editor(cx));
                item = item.child(editor);
            }
            item.into_any_element()
        })
        .size_full()
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Sync scroll before borrowing the diff for the body below.
        self.stream_top
            .set(self.list_handle.logical_scroll_top().item_ix);
        self.sync_tree_and_stream(cx);
        let focus = self.focus_handle.clone();
        let track = self.focus_handle.clone();
        let body: AnyElement = match &self.state {
            ReviewState::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgb(0x737878))
                .child("Loading review…")
                .into_any_element(),
            ReviewState::Failed(message) => {
                let retry = div()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x292b2b))
                    .text_sm()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.reload(cx)),
                    )
                    .child("Retry");
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_6()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xf87171))
                            .child(message.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x737878))
                            .child("Check that the base branch exists and this is a git checkout."),
                    )
                    .child(retry)
                    .into_any_element()
            }
            ReviewState::Loaded(loaded) => {
                let file_count = loaded.diff.files.len();
                let sticky = self.render_sticky_header(loaded, cx);
                h_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(320.))
                            .flex_none()
                            .h_full()
                            .border_r_1()
                            .border_color(rgb(0x292b2b))
                            .child(
                                div()
                                    .h(px(32.))
                                    .flex_none()
                                    .px_3()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(rgb(0x858989))
                                    .border_b_1()
                                    .border_color(rgb(0x1d1f1f))
                                    .child(format!("Files · {file_count}")),
                            )
                            .child(div().flex_1().min_h_0().child(self.render_tree(cx))),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .min_h_0()
                            .when_some(sticky, |this, header| this.child(header))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .min_w_0()
                                    .w_full()
                                    .child(self.render_stream(loaded, cx)),
                            ),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .bg(rgb(0x090a0a))
            .text_color(rgb(0xe7e7e7))
            .track_focus(&track)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| focus.focus(window, cx)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    // h_flex centers by default; the nested tree/diff pane
                    // needs the entire cross axis for its virtualized lists.
                    .items_stretch()
                    .min_w_0()
                    .min_h_0()
                    .child(body)
                    .when(self.show_comments, |d| d.child(self.render_comments(cx))),
            )
    }
}

impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
