//! The Review tab: live diffs over the working tree.
//!
//! [`git`] is the only module that talks to `gix`; [`model`] is the
//! UI-ready diff model; [`tree`] builds the file-tree items; [`stream`]
//! renders diff rows; [`icons`] resolves file-type icon art. [`ReviewView`]
//! owns loading, scope selection, the tree/stream layout, viewed-file state,
//! and refresh.

pub(crate) mod git;
pub(crate) mod model;
mod icons;
mod stream;
mod tree;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::{
    Icon, IconName, StyledExt as _, h_flex,
    list::ListItem,
    tab::{Tab, TabBar},
    tree::{TreeState, tree},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, ScrollStrategy, SharedString,
    Styled, UniformListScrollHandle, Window, div, img, px, rgb, uniform_list,
};

use crate::command_palette::ToggleCommandPalette;

use self::git::{ReviewScope, load_review, suggest_base_branch};
use self::model::ReviewDiff;
use self::stream::{flatten, render_row, status_color};
use self::icons::{FALLBACK, ICON_PX, IconTiles, ensure_tiles, icon_key};
use self::tree::{TreeRowMeta, build_file_tree, file_item_id, file_path_from_id};

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
/// `rows` flattens every file section into uniform-height rows once per
/// load; `file_row_start[file]` is that file's first row (its header), so
/// tree↔stream scrolling maps between file indices and row indices.
pub(crate) struct LoadedReview {
    pub(crate) diff: ReviewDiff,
    pub(crate) rows: Vec<stream::StreamRow>,
    pub(crate) file_row_start: Vec<usize>,
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
    /// Full-color raster tiles per icon key, filled at load and shared by
    /// every row render. Rasterizing here (not per frame) keeps scrolling
    /// smooth; the `Rc` lets the virtualized row closures share the cache.
    icon_tiles: Rc<IconTiles>,
    list_handle: UniformListScrollHandle,
    /// Absolute index of the first visible stream row, recorded by the
    /// list's row processor on every layout. This is the scroll-spy source:
    /// the list measures only one item height and renders a moving window,
    /// so handle offsets can't be mapped back to rows from outside.
    /// Recorded values lag the latest input by one frame at most; the tree
    /// selection update itself schedules the render that observes the fresh
    /// value, so the two converge instead of fighting.
    stream_top: Rc<Cell<usize>>,
    last_scrolled: Option<String>,
    /// Monotonic load id; stale background loads never overwrite newer state.
    generation: u64,
}

impl ReviewView {
    pub(crate) fn new(cwd: &Path, cx: &mut Context<Self>) -> Self {
        let base_branch =
            suggest_base_branch(cwd, "origin").unwrap_or_else(|| "main".to_owned());
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
            icon_tiles: Rc::default(),
            list_handle: UniformListScrollHandle::new(),
            stream_top: Rc::default(),
            last_scrolled: None,
            generation: 0,
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
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { load_review(&cwd, &scope) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                match loaded {
                    Ok(diff) => view.apply_diff(diff, cx),
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

    fn apply_diff(&mut self, diff: ReviewDiff, cx: &mut Context<Self>) {
        let (items, metas) = build_file_tree(&diff.files);
        let (rows, file_row_start) = flatten(&diff);
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
            diff,
            rows,
            file_row_start,
        }));
        cx.notify();
    }

    fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        let mut viewed = self.viewed.borrow_mut();
        if !viewed.remove(path) {
            viewed.insert(path.to_owned());
        }
        drop(viewed);
        cx.notify();
    }

    /// Forward the command-bar toggle to the workspace, mirroring
    /// `TerminalPane::on_key_down`. The tree/stream children don't swallow
    /// keys today, but without this any future child that stops propagation
    /// would silently break `cmd-k` on this tab again. All other keys bubble
    /// normally (no `prevent_default`/`stop_propagation`) so tree navigation
    /// and list scrolling keep working.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key.eq_ignore_ascii_case("k")
            && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
            && !event.keystroke.modifiers.alt
        {
            window.dispatch_action(Box::new(ToggleCommandPalette), cx);
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
                    loaded.diff.head_branch.as_deref().unwrap_or("detached HEAD")
                ),
                ScopeTab::Uncommitted => format!(
                    "uncommitted · {}",
                    loaded.diff.head_branch.as_deref().unwrap_or("detached HEAD")
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
                format!("{} files · +{additions} −{deletions}", loaded.diff.files.len())
            }
            _ => String::new(),
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let scope_index = self.scope_tab as usize;
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
                    .children([Tab::new().label("Full diff"), Tab::new().label("Uncommitted")]),
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
        tree(&self.tree_state, move |ix, entry, _selected, _window, _cx| {
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
            let is_viewed = file_path_from_id(&id)
                .is_some_and(|path| viewed.borrow().contains(path));
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
                let tile = tiles
                    .get(icon_key(&name))
                    .or_else(|| tiles.get(FALLBACK));
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
        })
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
                self.list_handle
                    .scroll_to_item(loaded.file_row_start[file], ScrollStrategy::Top);
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

    /// The virtualized diff stream: only the visible row window is built.
    ///
    /// The row processor also records the visible window's start, which is
    /// the absolute scroll position `sync_tree_and_stream` reads back.
    fn render_stream(&self, loaded: &Rc<LoadedReview>, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = loaded.rows.len();
        let loaded = loaded.clone();
        let viewed = self.viewed.clone();
        let view = cx.entity();
        let stream_top = self.stream_top.clone();
        uniform_list("review-stream", row_count, move |range, _window, _cx| {
            stream_top.set(range.start);
            let seen = viewed.borrow();
            range
                .map(|ix| render_row(&loaded, loaded.rows[ix], &seen, &view))
                .collect()
        })
        .size_full()
        .track_scroll(&self.list_handle)
        // The list scrolls internally without notifying us; forward wheel
        // activity so the scroll-spy in `sync_tree_and_stream` keeps up.
        // (No scrollbar-drag path exists: the list has no scrollbar
        // decoration, so wheel/trackpad plus programmatic scrolls cover it.)
        .on_scroll_wheel(cx.listener(|_, _, _, cx| {
            cx.notify();
        }))
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Sync scroll before borrowing the diff for the body below.
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
            ReviewState::Loaded(loaded) if loaded.diff.files.is_empty() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgb(0x737878))
                .child("No changes in this scope.")
                .into_any_element(),
            ReviewState::Loaded(loaded) => {
                let file_count = loaded.diff.files.len();
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(280.))
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
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_stream(loaded, cx)),
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
            .child(body)
    }
}

impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
