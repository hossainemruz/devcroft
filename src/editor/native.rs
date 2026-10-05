//! A deliberately small checkout editor: one UTF-8 file, explicit save, and
//! protection against overwriting a file changed by an agent on disk.

mod file_finder;

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fs,
    hash::{Hash as _, Hasher as _},
    io::{self, Write as _},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use super::drafts::{self, Draft, Journal};
use super::finder::{self, Finder};
use super::lsp::{Client, DiagnosticEvent, LspProviders, discover_rust_analyzer, file_uri};
use super::project::{self, ProjectFile, TextMatch};
use crate::editor::{ExternalEditor, ExternalEditorKind};
use anyhow::{Context as _, Result, bail};
use gpui_kit::component::{
    Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{Confirm, DialogFooter},
    h_flex,
    input::{Editor, EditorState, Input, InputEvent, InputState, MoveDown, MoveUp, Position},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::img;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, px, rgb,
};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// How often the open file is compared with its on-disk contents. A poll
/// keeps concurrent agent edits visible without a platform file-watcher
/// dependency; reads are bounded by `MAX_FILE_BYTES` and page-cached.
const DISK_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct NativeEditor {
    root: PathBuf,
    canonical_root: PathBuf,
    editor: Entity<EditorState>,
    path: Option<PathBuf>,
    saved: Option<String>,
    dirty: bool,
    error: Option<String>,
    pending_open: Option<(PathBuf, Option<usize>)>,
    pending_line: Option<usize>,
    pending_lsp_position: Option<lsp_types::Position>,
    external_editors: HashMap<String, bool>,
    lsp: Option<LspSession>,
    workspace_lsp: Option<Arc<Client>>,
    lsp_starting: bool,
    lsp_trusted: bool,
    lsp_generation: u64,
    lsp_status: String,
    /// Origins of follow-definition jumps (engine positions), newest last.
    /// Pushed by the `show_document` hook before the engine jumps.
    jump_back: Rc<RefCell<Vec<Position>>>,
    /// Last disk observation; prevents the poll from re-reporting the same
    /// external change on every tick.
    disk_seen: Option<DiskSeen>,
    /// Set when the file changed on disk while the buffer had unsaved
    /// changes. Cleared by an explicit reload, a save, or when the disk
    /// returns to the saved contents.
    conflict: bool,
    /// One-line status about disk activity (reload, deletion, format).
    disk_notice: Option<String>,
    tabs: Vec<PathBuf>,
    inactive_documents: HashMap<PathBuf, StashedDocument>,
    close_pending: Option<PathBuf>,
    files: Vec<ProjectFile>,
    icon_tiles: crate::review::icons::IconTiles,
    file_selection: usize,
    finder: Option<Arc<Mutex<Finder>>>,
    finder_error: Option<String>,
    buffer_finder: Option<Arc<Mutex<Finder>>>,
    buffer_finder_error: Option<String>,
    buffer_index_generation: u64,
    file_matches: Vec<ProjectFile>,
    file_total: usize,
    file_searching: bool,
    file_generation: Arc<AtomicU64>,
    preview_generation: u64,
    preview_text: Option<finder::Preview>,
    preview_first_line: usize,
    preview_editor: Entity<EditorState>,
    finder_previous: BrowserMode,
    finder_scroll: gpui_kit::UniformListScrollHandle,
    index_generation: u64,
    file_query: Entity<InputState>,
    text_query: Entity<InputState>,
    line_query: Entity<InputState>,
    search_results: Vec<TextMatch>,
    browser: BrowserMode,
    indexing: bool,
    search_truncated: bool,
    search_error: Option<String>,
    expanded_dirs: HashSet<String>,
    tree_limits: HashMap<String, usize>,
    sidebar_focus: FocusHandle,
    tree_scroll: ScrollHandle,
    navigation_active: bool,
    tree_cursor: Option<String>,
    disk_comparison: Option<String>,
    back_locations: Vec<HistoryLocation>,
    forward_locations: Vec<HistoryLocation>,
    navigating_history: bool,
    journal_path: Option<PathBuf>,
    journal_lock: Arc<Mutex<()>>,
    journal_digest: Option<u64>,
}

#[derive(Clone)]
struct HistoryLocation {
    path: PathBuf,
    position: Position,
}

struct StashedDocument {
    editor: Entity<EditorState>,
    saved: String,
    dirty: bool,
    disk_seen: Option<DiskSeen>,
    conflict: bool,
    disk_notice: Option<String>,
    jump_back: Rc<RefCell<Vec<Position>>>,
    subscribed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserMode {
    Closed,
    Files,
    Buffers,
    Tree,
    Text,
    Line,
}

#[derive(Clone, Copy)]
enum CloseChoice {
    Save,
    Discard,
    Cancel,
}

/// What the last disk check observed. `Content` hashes the bytes so an
/// unchanged external change is reported once, not every tick.
#[derive(Clone, PartialEq, Eq)]
enum DiskSeen {
    Missing,
    Unreadable(String),
    Content(u64),
}

/// A difference between the open document and its file. `Reload` carries
/// text that already passed the format checks.
enum DiskChange {
    /// The buffer was clean and the file changed: adopt the disk contents.
    Reload(String),
    /// The disk now matches the unsaved draft: treat it as saved.
    Adopted,
    /// The buffer has unsaved changes and the file changed underneath it.
    Conflict,
    /// The file is gone from disk.
    Deleted,
    /// The file changed into something the editor cannot represent.
    Unreadable(String),
    /// Disk matches the saved contents again: clear stale notices.
    Settled,
}

/// The active document binding; its client belongs to the checkout workspace.
struct LspSession {
    client: Arc<Client>,
    uri: String,
}

struct TreeRow {
    key: String,
    label: String,
    depth: usize,
    file: Option<PathBuf>,
    more: bool,
}

impl TreeRow {
    fn id(&self) -> String {
        let kind = if self.more {
            "more"
        } else if self.file.is_some() {
            "file"
        } else {
            "dir"
        };
        format!("native-tree-{kind}-{}", self.key)
    }
}

const TREE_PAGE_SIZE: usize = 100;

/// Sort the index once in the background, before rendering or paginating
/// the tree. At each level, directories precede files and siblings sort by
/// name; a directory's descendants stay together.
fn sort_tree_files(files: &mut [ProjectFile]) {
    files.sort_unstable_by(|a, b| {
        let mut a = a.label.split('/').peekable();
        let mut b = b.label.split('/').peekable();
        while let (Some(a_name), Some(b_name)) = (a.next(), b.next()) {
            let order = a
                .peek()
                .is_none()
                .cmp(&b.peek().is_none())
                .then_with(|| a_name.cmp(b_name));
            if !order.is_eq() {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// `files` must be ordered by `sort_tree_files`.
fn tree_rows(
    files: &[ProjectFile],
    expanded: &HashSet<String>,
    limits: &HashMap<String, usize>,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut child_counts: HashMap<String, usize> = HashMap::new();
    let mut more_rows = HashSet::new();
    for file in files {
        let parts: Vec<_> = file.label.split('/').collect();
        let mut parent = String::new();
        for (depth, part) in parts.iter().enumerate() {
            if !expanded.contains(&parent) {
                break;
            }
            let key = if parent.is_empty() {
                (*part).to_owned()
            } else {
                format!("{parent}/{part}")
            };
            let is_file = depth + 1 == parts.len();
            if !is_file && seen_dirs.contains(&key) {
                parent = key;
                continue;
            }
            let count = child_counts.entry(parent.clone()).or_default();
            if *count >= limits.get(&parent).copied().unwrap_or(TREE_PAGE_SIZE) {
                if more_rows.insert(parent.clone()) {
                    rows.push(TreeRow {
                        key: parent.clone(),
                        label: "Show more".into(),
                        depth,
                        file: None,
                        more: true,
                    });
                }
                break;
            }
            *count += 1;
            if is_file {
                rows.push(TreeRow {
                    key: key.clone(),
                    label: (*part).to_owned(),
                    depth,
                    file: Some(file.path.clone()),
                    more: false,
                });
            } else if seen_dirs.insert(key.clone()) {
                rows.push(TreeRow {
                    key: key.clone(),
                    label: (*part).to_owned(),
                    depth,
                    file: None,
                    more: false,
                });
            }
            parent = key;
        }
    }
    rows
}

impl NativeEditor {
    pub(crate) fn new(
        root: &Path,
        external_editors: HashMap<String, bool>,
        data_root: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let journal_path = data_root.map(|data_root| drafts::journal_path(data_root, root));
        let recovered = journal_path
            .as_ref()
            .map_or_else(Vec::new, |path| drafts::read(path, root));
        let mut inactive_documents = HashMap::new();
        let mut tabs = Vec::new();
        for mut draft in recovered {
            draft.path = draft.path.canonicalize().unwrap_or_else(|_| {
                draft
                    .path
                    .parent()
                    .and_then(|parent| parent.canonicalize().ok())
                    .zip(draft.path.file_name())
                    .map(|(parent, name)| parent.join(name))
                    .unwrap_or_else(|| draft.path.clone())
            });
            let document = cx.new(|cx| {
                let mut editor = EditorState::new(window, cx);
                editor.set_highlighter(language_for(&draft.path), cx);
                editor.set_value(draft.text.clone(), window, cx);
                editor
            });
            if document.read(cx).value().as_ref() != draft.text {
                continue;
            }
            tabs.push(draft.path.clone());
            inactive_documents.insert(
                draft.path,
                StashedDocument {
                    editor: document,
                    saved: draft.saved,
                    dirty: true,
                    disk_seen: None,
                    conflict: false,
                    disk_notice: Some("Recovered unsaved draft from the last session.".into()),
                    jump_back: Rc::new(RefCell::new(Vec::new())),
                    subscribed: false,
                },
            );
        }
        tabs.sort();
        let editor = cx.new(|cx| EditorState::new(window, cx));
        cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dirty = this.saved.as_ref().is_some_and(|saved| {
                    file_bytes_for_buffer(saved, editor.read(cx).value().as_ref()) != saved.as_str()
                });
                this.push_text_to_server(cx);
                cx.notify();
            }
        })
        .detach();
        let file_query = cx.new(|cx| InputState::new(window, cx).placeholder("Find a file…"));
        cx.subscribe(&file_query, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.search_files(cx),
            InputEvent::PressEnter { .. } => this.accept_file(cx),
            _ => {}
        })
        .detach();
        let preview_editor = cx.new(|cx| EditorState::new(window, cx));
        let text_query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search project text…"));
        cx.subscribe(&text_query, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.search_project(cx),
            InputEvent::PressEnter { .. } => this.accept_file(cx),
            _ => {}
        })
        .detach();
        let line_query = cx.new(|cx| InputState::new(window, cx).placeholder("Line number…"));
        cx.subscribe(&line_query, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.pending_line = this.line_query.read(cx).value().parse().ok();
                cx.notify();
            }
        })
        .detach();
        let scan_root = root.to_owned();
        cx.spawn_in(window, async move |view, cx| {
            let (files, finder) = cx
                .background_spawn(async move {
                    let mut files = project::scan(&scan_root);
                    let finder = Finder::new(&files);
                    sort_tree_files(&mut files);
                    (files, finder)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.index_generation == 0 {
                    crate::review::icons::ensure_tiles(
                        files.iter().map(|file| file.label.as_str()),
                        &mut this.icon_tiles,
                        cx,
                    );
                    this.files = files;
                    this.indexing = false;
                    this.install_finder(finder, cx);
                    cx.notify();
                }
            });
        })
        .detach();
        // Watch the open file for external edits (agents write on save). The
        // loop lives with the entity and reads only the current document.
        cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(DISK_POLL_INTERVAL).await;
                let change = view.update(cx, |this, cx| this.detect_disk_change(cx));
                let Ok(change) = change else {
                    break;
                };
                if let Some(change) = change {
                    let applied = cx.update(|window, cx| {
                        view.update(cx, |this, cx| this.apply_disk_change(change, window, cx))
                    });
                    if !matches!(applied, Ok(Ok(()))) {
                        break;
                    }
                }
                let write = view.update(cx, |this, cx| this.journal_if_changed(cx));
                if let Ok(Some((path, journal, lock))) = write {
                    let result = cx
                        .background_spawn(async move {
                            let _guard = lock.lock().expect("draft journal lock poisoned");
                            drafts::write(&path, &journal)
                        })
                        .await;
                    if let Err(error) = result {
                        let _ = view.update(cx, |this, cx| {
                            this.journal_digest = None;
                            this.error = Some(format!("Could not save recovery draft: {error:#}"));
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
        cx.on_app_quit(|this, cx| {
            let snapshot = this.journal_path.clone().map(|path| {
                let journal = this.draft_snapshot(cx);
                let lock = Arc::clone(&this.journal_lock);
                cx.background_spawn(async move {
                    let _guard = lock.lock().expect("draft journal lock poisoned");
                    let _ = drafts::write(&path, &journal);
                })
            });
            async move {
                if let Some(task) = snapshot {
                    task.await;
                }
            }
        })
        .detach();
        cx.on_release(|this, cx| {
            if let Some(path) = this.journal_path.as_ref() {
                let journal = this.draft_snapshot(cx);
                let _guard = this
                    .journal_lock
                    .lock()
                    .expect("draft journal lock poisoned");
                let _ = drafts::write(path, &journal);
            }
        })
        .detach();
        Self {
            root: root.to_owned(),
            canonical_root: root.canonicalize().unwrap_or_else(|_| root.to_owned()),
            editor,
            path: None,
            saved: None,
            dirty: false,
            error: None,
            pending_open: None,
            pending_line: None,
            pending_lsp_position: None,
            external_editors,
            lsp: None,
            workspace_lsp: None,
            lsp_starting: false,
            lsp_trusted: false,
            lsp_generation: 0,
            lsp_status: "Rust: disabled — click to trust this checkout and enable tooling".into(),
            jump_back: Rc::new(RefCell::new(Vec::new())),
            disk_seen: None,
            conflict: false,
            disk_notice: None,
            tabs,
            inactive_documents,
            close_pending: None,
            files: Vec::new(),
            icon_tiles: HashMap::new(),
            file_selection: 0,
            finder: None,
            finder_error: None,
            buffer_finder: None,
            buffer_finder_error: None,
            buffer_index_generation: 0,
            file_matches: Vec::new(),
            file_total: 0,
            file_searching: false,
            file_generation: Arc::new(AtomicU64::new(0)),
            preview_generation: 0,
            preview_text: None,
            preview_first_line: 1,
            preview_editor,
            finder_previous: BrowserMode::Tree,
            finder_scroll: gpui_kit::UniformListScrollHandle::new(),
            index_generation: 0,
            file_query,
            text_query,
            line_query,
            search_results: Vec::new(),
            browser: BrowserMode::Tree,
            indexing: true,
            search_truncated: false,
            search_error: None,
            expanded_dirs: HashSet::from([String::new()]),
            tree_limits: HashMap::new(),
            sidebar_focus: cx.focus_handle().tab_stop(true),
            tree_scroll: ScrollHandle::new(),
            navigation_active: false,
            tree_cursor: None,
            disk_comparison: None,
            back_locations: Vec::new(),
            forward_locations: Vec::new(),
            navigating_history: false,
            journal_path,
            journal_lock: Arc::new(Mutex::new(())),
            journal_digest: None,
        }
    }

    pub(crate) fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub(crate) fn navigation_panes(&self, cx: &App) -> Vec<(&'static str, FocusHandle)> {
        let mut panes = Vec::new();
        let mode = if self.finder_open() {
            self.finder_previous
        } else {
            self.browser
        };
        if mode != BrowserMode::Closed {
            panes.push(("Files", self.sidebar_focus.clone()));
        }
        panes.push(("Editor", self.editor_focus(cx)));
        panes
    }

    pub(crate) fn navigation_in_tree(&self, pane: usize) -> bool {
        pane == 0 && self.browser == BrowserMode::Tree && !self.finder_open()
    }

    fn tree_cursor_index(&self, rows: &[TreeRow]) -> Option<usize> {
        rows.iter()
            .position(|row| self.tree_cursor.as_ref() == Some(&row.id()))
            .or_else(|| {
                rows.iter().position(|row| {
                    row.file.as_ref().is_some_and(|path| {
                        self.path.as_ref().is_some_and(|active| {
                            active == path || active == &self.canonical_root.join(&row.key)
                        })
                    })
                })
            })
            .or_else(|| (!rows.is_empty()).then_some(0))
    }

    pub(crate) fn set_navigation_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.navigation_active = active;
        self.tree_cursor = None;
        if active {
            // Completion/code-action menus own Up/Down while open. Cancel
            // pending requests as well so navigation always moves the code.
            self.editor
                .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
            let rows = tree_rows(&self.files, &self.expanded_dirs, &self.tree_limits);
            if let Some(index) = self.tree_cursor_index(&rows) {
                self.tree_cursor = Some(rows[index].id());
                self.tree_scroll.scroll_to_item(index);
            }
        }
        cx.notify();
    }

    pub(crate) fn move_navigation_item(
        &mut self,
        pane: usize,
        down: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.navigation_in_tree(pane) {
            let rows = tree_rows(&self.files, &self.expanded_dirs, &self.tree_limits);
            if let Some(index) = self.tree_cursor_index(&rows) {
                let index = crate::navigation::move_index(index, rows.len(), down);
                self.tree_cursor = Some(rows[index].id());
                self.tree_scroll.scroll_to_item(index);
                self.sidebar_focus.focus(window, cx);
                cx.notify();
            }
        } else if self
            .navigation_panes(cx)
            .get(pane)
            .is_some_and(|(label, _)| *label == "Editor")
        {
            self.editor_focus(cx).focus(window, cx);
            // Use the editor's own vertical motion so wrapped lines, folds,
            // preferred columns and scrolling behave like the arrow keys.
            if down {
                window.dispatch_action(Box::new(MoveDown), cx);
            } else {
                window.dispatch_action(Box::new(MoveUp), cx);
            }
        }
    }

    /// Returns true after toggling a folder so navigation can continue in
    /// the tree with the same row highlighted.
    pub(crate) fn activate_navigation_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let rows = tree_rows(&self.files, &self.expanded_dirs, &self.tree_limits);
        if let Some(index) = self.tree_cursor_index(&rows) {
            let row = &rows[index];
            let toggled_folder = row.file.is_none() && !row.more;
            self.tree_cursor = Some(row.id());
            self.activate_tree_row(row, window, cx);
            if toggled_folder {
                self.sidebar_focus.focus(window, cx);
            }
            return toggled_folder;
        }
        false
    }

    fn activate_tree_row(&mut self, row: &TreeRow, window: &mut Window, cx: &mut Context<Self>) {
        if row.more {
            *self
                .tree_limits
                .entry(row.key.clone())
                .or_insert(TREE_PAGE_SIZE) += TREE_PAGE_SIZE;
        } else if let Some(path) = &row.file {
            self.error = self
                .open(path, None, window, cx)
                .err()
                .map(|error| format!("{error:#}"));
        } else if !self.expanded_dirs.insert(row.key.clone()) {
            self.expanded_dirs.remove(&row.key);
        }
        cx.notify();
    }

    pub(crate) fn has_jump_history(&self) -> bool {
        !self.jump_back.borrow().is_empty() || !self.back_locations.is_empty()
    }

    pub(crate) fn open_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finder_open() {
            self.finder_previous = self.browser;
        }
        self.browser = BrowserMode::Files;
        self.file_query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.search_files(cx);
        self.file_query.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn open_live_grep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finder_open() {
            self.finder_previous = self.browser;
        }
        self.browser = BrowserMode::Text;
        self.text_query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.search_project(cx);
        self.text_query.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn open_buffer_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finder_open() {
            self.finder_previous = self.browser;
        }
        self.browser = BrowserMode::Buffers;
        self.file_query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refresh_buffer_finder(cx);
        self.file_query.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn show_browser(&mut self, mode: BrowserMode, window: &mut Window, cx: &mut Context<Self>) {
        if mode == BrowserMode::Files {
            self.open_file_finder(window, cx);
            return;
        }
        if mode == BrowserMode::Text {
            self.open_live_grep(window, cx);
            return;
        }
        if mode == BrowserMode::Buffers {
            self.open_buffer_finder(window, cx);
            return;
        }
        self.browser = if self.browser == mode {
            BrowserMode::Closed
        } else {
            mode
        };
        let input = match self.browser {
            BrowserMode::Files | BrowserMode::Buffers => Some(&self.file_query),
            BrowserMode::Text => Some(&self.text_query),
            BrowserMode::Line => Some(&self.line_query),
            BrowserMode::Closed | BrowserMode::Tree => None,
        };
        if let Some(input) = input {
            input.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn refresh_files(&mut self, cx: &mut Context<Self>) {
        if self.browser == BrowserMode::Buffers {
            self.refresh_buffer_finder(cx);
            return;
        }
        self.index_generation += 1;
        let generation = self.index_generation;
        self.indexing = true;
        self.finder = None;
        self.reset_finder_results();
        let root = self.root.clone();
        cx.spawn(async move |view, cx| {
            let (files, finder) = cx
                .background_spawn(async move {
                    let mut files = project::scan(&root);
                    let finder = Finder::new(&files);
                    sort_tree_files(&mut files);
                    (files, finder)
                })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.index_generation == generation {
                    crate::review::icons::ensure_tiles(
                        files.iter().map(|file| file.label.as_str()),
                        &mut this.icon_tiles,
                        cx,
                    );
                    this.files = files;
                    this.indexing = false;
                    this.install_finder(finder, cx);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn compare_with_disk(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        match read_text_file(path) {
            Ok(disk) => {
                let draft = self.editor.read(cx).value().to_string();
                let diff = similar::TextDiff::from_lines(&disk, &draft);
                let comparison = diff.unified_diff().header("disk", "draft").to_string();
                let preview: String = comparison.chars().take(20_000).collect();
                self.disk_comparison = Some(if preview.len() < comparison.len() {
                    format!(
                        "{preview}\n… diff truncated; use Review or an external editor for the full file"
                    )
                } else {
                    preview
                });
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Could not compare: {error:#}")),
        }
        cx.notify();
    }

    fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let picker =
            cx.prompt_for_new_path(parent, path.file_name().and_then(|name| name.to_str()));
        let draft = self.editor.read(cx).value().to_string();
        let copy = self
            .saved
            .as_deref()
            .map_or_else(
                || Cow::Borrowed(draft.as_str()),
                |saved| file_bytes_for_buffer(saved, &draft),
            )
            .into_owned();
        cx.spawn_in(window, async move |view, cx| {
            let Some(target) = picker.await.ok().into_iter().flatten().flatten().next() else {
                return;
            };
            let result = (|| -> Result<()> {
                let parent = target.parent().context("Save location has no parent")?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                if copy.len() as u64 > MAX_FILE_BYTES {
                    bail!("The draft is larger than 2 MiB. Open it in Zed or VS Code.");
                }
                temporary.write_all(copy.as_bytes())?;
                temporary.as_file().sync_all()?;
                temporary
                    .persist_noclobber(&target)
                    .context("Could not create the copy")?;
                Ok(())
            })();
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.disk_notice = Some(format!(
                            "Saved a copy to {}. The original draft is still open.",
                            target.display()
                        ));
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("Could not save a copy: {error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stash_active(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.take() else {
            return;
        };
        self.disk_comparison = None;
        self.clear_lsp_providers(cx);
        self.lsp.take();
        self.inactive_documents.insert(
            path,
            StashedDocument {
                editor: self.editor.clone(),
                saved: self.saved.take().unwrap_or_default(),
                dirty: self.dirty,
                disk_seen: self.disk_seen.take(),
                conflict: self.conflict,
                disk_notice: self.disk_notice.take(),
                jump_back: Rc::clone(&self.jump_back),
                subscribed: true,
            },
        );
    }

    fn draft_snapshot(&self, cx: &App) -> Journal {
        let mut drafts = Vec::new();
        if self.dirty
            && let (Some(path), Some(saved)) = (&self.path, &self.saved)
        {
            drafts.push(Draft {
                path: path.clone(),
                saved: saved.clone(),
                text: self.editor.read(cx).value().to_string(),
            });
        }
        for (path, document) in &self.inactive_documents {
            if document.dirty {
                drafts.push(Draft {
                    path: path.clone(),
                    saved: document.saved.clone(),
                    text: document.editor.read(cx).value().to_string(),
                });
            }
        }
        drafts.sort_by(|a, b| a.path.cmp(&b.path));
        Journal {
            root: self.root.clone(),
            drafts,
        }
    }

    fn journal_if_changed(&mut self, cx: &App) -> Option<(PathBuf, Journal, Arc<Mutex<()>>)> {
        let path = self.journal_path.clone()?;
        let journal = self.draft_snapshot(cx);
        let digest = content_hash(&serde_json::to_vec(&journal).ok()?);
        if self.journal_digest == Some(digest) {
            return None;
        }
        self.journal_digest = Some(digest);
        Some((path, journal, Arc::clone(&self.journal_lock)))
    }

    fn subscribe_changes(&self, editor: &Entity<EditorState>, cx: &mut Context<Self>) {
        cx.subscribe(editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && this.editor.entity_id() == editor.entity_id()
            {
                this.dirty = this.saved.as_ref().is_some_and(|saved| {
                    file_bytes_for_buffer(saved, editor.read(cx).value().as_ref()) != saved.as_str()
                });
                this.push_text_to_server(cx);
                cx.notify();
            }
        })
        .detach();
    }

    /// A Review event has no Window handle. Apply it on the next render, when
    /// the editor can safely set text and place the caret.
    pub(crate) fn request_open(
        &mut self,
        path: PathBuf,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.pending_open = Some((path, line));
        cx.notify();
    }

    fn open(
        &mut self,
        path: &Path,
        line: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let canonical_root = self
            .root
            .canonicalize()
            .context("Checkout is unavailable")?;
        let canonical =
            if self.path.as_deref() == Some(path) || self.inactive_documents.contains_key(path) {
                path.to_owned()
            } else {
                path.canonicalize()
                    .with_context(|| format!("Could not open {}", path.display()))?
            };
        if !canonical.starts_with(&canonical_root) {
            bail!("Choose a file inside this checkout.");
        }
        let origin = self.path.as_ref().map(|path| HistoryLocation {
            path: path.clone(),
            position: self.editor.read(cx).cursor_position(),
        });
        if self.path.as_deref() == Some(canonical.as_path()) {
            self.place_cursor(line, window, cx);
            if self
                .lsp
                .as_ref()
                .is_none_or(|session| !session.client.is_alive())
            {
                self.restart_lsp(&canonical, language_for(&canonical), window, cx);
            }
            let moved = line.is_some_and(|line| {
                origin
                    .as_ref()
                    .is_some_and(|origin| origin.position.line != line.saturating_sub(1) as u32)
            });
            self.record_origin(origin, moved, cx);
            return Ok(());
        }
        let opened_new_tab = !self.inactive_documents.contains_key(&canonical);
        if let Some(stashed) = self.inactive_documents.remove(&canonical) {
            self.stash_active(cx);
            let subscribed = stashed.subscribed;
            self.editor = stashed.editor;
            if !subscribed {
                self.subscribe_changes(&self.editor, cx);
            }
            self.saved = Some(stashed.saved);
            self.dirty = stashed.dirty;
            self.disk_seen = stashed.disk_seen;
            self.conflict = stashed.conflict;
            self.disk_notice = stashed.disk_notice;
            self.jump_back = stashed.jump_back;
        } else {
            let contents = read_text_file(&canonical)?;
            let language = language_for(&canonical);
            let next_editor = cx.new(|cx| {
                let mut editor = EditorState::new(window, cx);
                editor.set_highlighter(language, cx);
                editor.set_value(contents.clone(), window, cx);
                editor
            });
            if next_editor.read(cx).value().as_ref() != contents {
                bail!(
                    "This file's text format cannot be preserved by the built-in editor. Open it in Zed or VS Code."
                );
            }
            self.stash_active(cx);
            self.editor = next_editor;
            self.subscribe_changes(&self.editor, cx);
            self.saved = Some(contents);
            self.dirty = false;
            self.conflict = false;
            self.disk_notice = None;
            self.disk_seen = None;
            self.jump_back = Rc::new(RefCell::new(Vec::new()));
            self.tabs.push(canonical.clone());
        }
        self.path = Some(canonical.clone());
        self.place_cursor(line, window, cx);
        self.check_disk(window, cx);
        self.restart_lsp(&canonical, language_for(&canonical), window, cx);
        self.record_origin(origin, true, cx);
        if opened_new_tab && self.browser == BrowserMode::Buffers {
            self.refresh_buffer_finder(cx);
        }
        Ok(())
    }

    fn record_origin(
        &mut self,
        origin: Option<HistoryLocation>,
        moved: bool,
        cx: &mut Context<Self>,
    ) {
        if moved
            && !self.navigating_history
            && let Some(origin) = origin
        {
            if self.back_locations.len() >= 100 {
                self.back_locations.remove(0);
            }
            self.back_locations.push(origin);
            self.forward_locations.clear();
            cx.notify();
        }
    }

    fn place_cursor(&mut self, line: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            if let Some(line) = line.filter(|line| *line > 0) {
                editor.set_cursor_position(
                    Position::new((line - 1).min(u32::MAX as usize) as u32, 0),
                    window,
                    cx,
                );
            }
            editor.focus(window, cx);
        });
    }

    fn close_tab(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = if self.path.as_deref() == Some(path) {
            self.dirty
        } else {
            self.inactive_documents
                .get(path)
                .is_some_and(|document| document.dirty)
        };
        if dirty {
            self.close_pending = Some(path.to_owned());
            cx.notify();
        } else {
            self.finish_close(path, window, cx);
        }
    }

    fn confirm_close(&mut self, choice: CloseChoice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.close_pending.clone() else {
            return;
        };
        match choice {
            CloseChoice::Cancel => {
                self.close_pending = None;
                cx.notify();
            }
            CloseChoice::Discard => {
                self.close_pending = None;
                self.finish_close(&path, window, cx);
            }
            CloseChoice::Save => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.save(window, cx);
                    if self.dirty {
                        return;
                    }
                } else if let Some(document) = self.inactive_documents.get_mut(&path) {
                    let draft = document.editor.read(cx).value().to_string();
                    let replacement = file_bytes_for_buffer(&document.saved, &draft);
                    if let Err(error) =
                        save_if_unchanged(&path, document.saved.as_bytes(), replacement.as_bytes())
                    {
                        self.error = Some(format!("Could not save: {error:#}"));
                        cx.notify();
                        return;
                    }
                }
                if self.path.as_ref() != Some(&path)
                    && let Some(client) = &self.workspace_lsp
                    && let Ok(uri) = file_uri(&path)
                {
                    let _ = client.did_save(uri.as_str());
                }
                self.close_pending = None;
                self.finish_close(&path, window, cx);
            }
        }
    }

    fn finish_close(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(client) = &self.workspace_lsp
            && let Ok(uri) = file_uri(path)
        {
            client.did_close(uri.as_str());
        }
        self.tabs.retain(|tab| tab != path);
        if self.path.as_deref() == Some(path) {
            self.disk_comparison = None;
            self.lsp.take();
            self.path = None;
            self.saved = None;
            self.dirty = false;
            self.conflict = false;
            self.disk_seen = None;
            self.disk_notice = None;
            self.jump_back.borrow_mut().clear();
            if let Some(next) = self.tabs.last().cloned() {
                self.error = self
                    .open(&next, None, window, cx)
                    .err()
                    .map(|error| format!("{error:#}"));
            } else {
                let editor = cx.new(|cx| EditorState::new(window, cx));
                self.editor = editor;
                self.subscribe_changes(&self.editor, cx);
            }
        } else {
            self.inactive_documents.remove(path);
        }
        if self.browser == BrowserMode::Buffers {
            self.refresh_buffer_finder(cx);
        }
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(saved) = self.saved.clone() else {
            return;
        };
        let draft = self.editor.read(cx).value().to_string();
        let contents = file_bytes_for_buffer(&saved, &draft).into_owned();
        match save_if_unchanged(&path, saved.as_bytes(), contents.as_bytes()) {
            Ok(()) => {
                if let Some(session) = &self.lsp {
                    let _ = session.client.did_save(&session.uri);
                }
                self.saved = Some(contents);
                self.dirty = false;
                self.error = None;
                self.conflict = false;
                self.disk_notice = None;
                self.disk_seen = None;
            }
            Err(error) => {
                self.error = Some(format!("Could not save: {error:#}"));
                // A failed save caused by a concurrent write should surface
                // the conflict banner immediately, not on the next poll tick.
                if let Some(change) = self.detect_disk_change(cx) {
                    self.apply_disk_change(change, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(saved) = self.saved.clone() {
            self.editor
                .update(cx, |editor, cx| editor.set_value(saved, window, cx));
            self.dirty = false;
            self.error = None;
            cx.notify();
            // The server saw the draft, so restore its copy of the saved
            // text before the disk check possibly replaces it again.
            self.push_text_to_server(cx);
            // Discarding means returning to the disk state, so pick up an
            // external change now instead of waiting for the next tick.
            self.check_disk(window, cx);
        }
    }

    /// One disk-poll step: detect an external change and apply it. The timer
    /// loop calls this every `DISK_POLL_INTERVAL`; tests call it directly.
    fn check_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(client) = &self.workspace_lsp {
            let status = if !client.is_alive() {
                "Rust: server stopped — Restart language server".into()
            } else if let Some(error) = client.last_error() {
                format!("Rust: {error} — Restart language server")
            } else {
                "rust-analyzer: ready".into()
            };
            if self.lsp_status != status {
                self.lsp_status = status;
                cx.notify();
            }
        }
        if let Some(change) = self.detect_disk_change(cx) {
            self.apply_disk_change(change, window, cx);
        }
    }

    /// Compare the open document with the file on disk and report what
    /// changed. Reads at most one bounded file per call.
    fn detect_disk_change(&mut self, cx: &mut Context<Self>) -> Option<DiskChange> {
        let path = self.path.clone()?;
        self.saved.as_ref()?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return self.missing_once();
            }
            Err(error) => return self.unreadable_once(format!("{error:#}")),
        };
        if !metadata.is_file() {
            return self.unreadable_once("The path is no longer a regular file.".into());
        }
        // Check the size before reading so an oversized replacement never
        // lands in memory.
        if metadata.len() > MAX_FILE_BYTES {
            return self.unreadable_once(
                "This file is larger than 2 MiB. Open it in Zed or VS Code.".into(),
            );
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return self.missing_once();
            }
            Err(error) => return self.unreadable_once(format!("{error:#}")),
        };
        let seen = DiskSeen::Content(content_hash(&bytes));
        let unchanged = self.disk_seen.as_ref() == Some(&seen);
        let matches_saved = self
            .saved
            .as_deref()
            .is_some_and(|saved| bytes == saved.as_bytes());
        if matches_saved {
            self.disk_seen = Some(seen);
            return (self.conflict || self.disk_notice.is_some()).then_some(DiskChange::Settled);
        }
        // Compare against the buffer before the repeat guard: a user action
        // (typing the disk text, or discarding) can make them match even
        // though the disk bytes are the ones already seen.
        if bytes
            == file_bytes_for_buffer(
                self.saved.as_deref().unwrap_or_default(),
                self.editor.read(cx).value().as_ref(),
            )
            .as_bytes()
        {
            self.disk_seen = Some(seen);
            return Some(DiskChange::Adopted);
        }
        if self.dirty {
            // The same conflicting content was already announced; re-reporting
            // every tick would only repaint the banner.
            if unchanged {
                return None;
            }
            self.disk_seen = Some(seen);
            return Some(DiskChange::Conflict);
        }
        // A clean buffer follows the disk. Only suppress a repeat when an
        // earlier reload already failed and left its reason in the notice.
        if unchanged && self.disk_notice.is_some() {
            return None;
        }
        self.disk_seen = Some(seen);
        match text_from_bytes(&bytes) {
            Ok(contents) => Some(DiskChange::Reload(contents)),
            Err(error) => Some(DiskChange::Unreadable(format!("{error:#}"))),
        }
    }

    /// Report a missing file once, until the observation changes.
    fn missing_once(&mut self) -> Option<DiskChange> {
        if self.disk_seen == Some(DiskSeen::Missing) {
            return None;
        }
        self.disk_seen = Some(DiskSeen::Missing);
        Some(DiskChange::Deleted)
    }

    /// Report an unreadable file once per distinct reason.
    fn unreadable_once(&mut self, reason: String) -> Option<DiskChange> {
        if self.disk_seen == Some(DiskSeen::Unreadable(reason.clone())) {
            return None;
        }
        self.disk_seen = Some(DiskSeen::Unreadable(reason.clone()));
        Some(DiskChange::Unreadable(reason))
    }

    /// Apply a detected disk change. A reload that the engine cannot
    /// represent leaves the draft open together with a notice.
    fn apply_disk_change(
        &mut self,
        change: DiskChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match change {
            DiskChange::Reload(contents) => {
                if let Err(error) = self.adopt_disk_contents(contents, window, cx) {
                    self.disk_notice = Some(format!("{error:#}"));
                }
            }
            DiskChange::Adopted => {
                self.saved = Some(self.editor.read(cx).value().to_string());
                self.dirty = false;
                self.conflict = false;
                self.disk_notice = Some("The file on disk now matches your draft.".into());
                self.error = None;
            }
            DiskChange::Conflict => {
                self.conflict = true;
                self.disk_notice = None;
            }
            DiskChange::Deleted => {
                self.conflict = false;
                self.disk_notice = Some(
                    "This file was deleted on disk. The buffer is unchanged; saving will not recreate it silently."
                        .into(),
                );
            }
            DiskChange::Unreadable(reason) => {
                // An unreadable change can only leave a stale conflict banner
                // when the buffer has no unsaved changes of its own.
                self.conflict = self.dirty;
                self.disk_notice = Some(format!(
                    "This file changed on disk but cannot be reloaded: {reason}"
                ));
            }
            DiskChange::Settled => {
                self.disk_notice = if std::mem::take(&mut self.conflict) {
                    Some("The file on disk matches the saved version again.".into())
                } else {
                    None
                };
            }
        }
        cx.notify();
    }

    /// Replace the buffer with disk contents, keeping the draft when the
    /// engine cannot preserve the text format.
    fn adopt_disk_contents(
        &mut self,
        contents: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let previous = self.editor.read(cx).value().to_string();
        self.editor.update(cx, |editor, cx| {
            editor.set_value(contents.clone(), window, cx)
        });
        if self.editor.read(cx).value().as_ref() != contents {
            self.editor
                .update(cx, |editor, cx| editor.set_value(previous, window, cx));
            bail!(
                "This file's text format cannot be preserved by the built-in editor. Open it in Zed or VS Code."
            );
        }
        self.saved = Some(contents);
        self.dirty = false;
        self.conflict = false;
        self.disk_notice = Some("Reloaded changes from disk.".into());
        self.error = None;
        self.push_text_to_server(cx);
        Ok(())
    }

    /// Explicit "Reload from disk": drop the draft for the file's current
    /// contents, or keep it when the disk cannot be read.
    fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let result = match read_text_file(&path) {
            Ok(contents) => {
                // The user asked for the disk state: restore it whenever the
                // buffer is not already showing exactly those bytes. (The
                // conflict may have been resolved by an external revert.)
                if self.editor.read(cx).value().as_ref() == contents {
                    if self.saved.as_deref() != Some(contents.as_str()) {
                        // The buffer already matches disk; only the saved
                        // bookkeeping is stale, so no `set_value` is needed.
                        self.saved = Some(contents);
                        self.dirty = false;
                        self.error = None;
                    }
                    self.conflict = false;
                    self.disk_notice = None;
                    Ok(())
                } else {
                    self.adopt_disk_contents(contents, window, cx)
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => cx.notify(),
            Err(error) => {
                self.error = Some(format!("Could not reload: {error:#}"));
                cx.notify();
            }
        }
    }

    pub(crate) fn set_external_editors(
        &mut self,
        external_editors: HashMap<String, bool>,
        cx: &mut Context<Self>,
    ) {
        self.external_editors = external_editors;
        cx.notify();
    }

    /// Push the current buffer to the server after an edit. Silent on
    /// failure: the client records the failure and the next edit retries,
    /// so a struggling server never interrupts typing.
    fn push_text_to_server(&self, cx: &mut Context<Self>) {
        let Some(session) = self.lsp.as_ref() else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        let _ = session.client.did_change(&session.uri, &text);
    }

    /// Bind a Rust document to the checkout client, starting it asynchronously
    /// after the explicit trust decision. Failures preserve plain editing.
    fn restart_lsp(
        &mut self,
        path: &Path,
        language: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_lsp_providers(cx);
        self.lsp = None;
        if language != "rust" {
            return;
        }
        if !self.lsp_trusted {
            self.lsp_status =
                "Rust: disabled — click to trust this checkout and enable tooling".into();
            return;
        }
        let Ok(uri) = file_uri(path) else {
            return;
        };
        if let Some(client) = self.workspace_lsp.clone().filter(|c| c.is_alive()) {
            if client
                .did_open(&uri, "rust", self.editor.read(cx).value().as_ref())
                .is_ok()
            {
                self.attach_lsp(client, uri, cx);
            }
            return;
        }
        if self.lsp_starting {
            return;
        }
        self.workspace_lsp = None;
        let Some(program) = discover_rust_analyzer(None).filter(|program| {
            program.is_absolute()
                && program
                    .canonicalize()
                    .is_ok_and(|path| !path.starts_with(&self.canonical_root))
        }) else {
            self.lsp_status =
                "Rust: rust-analyzer unavailable — install it, then Restart language server".into();
            return;
        };
        self.lsp_starting = true;
        self.lsp_generation += 1;
        let generation = self.lsp_generation;
        self.lsp_status = "Rust: starting rust-analyzer…".into();
        let root = self.canonical_root.clone();
        let (diagnostics_tx, diagnostics_rx) = async_channel::unbounded();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_spawn(async move { Client::start(&program, &root, diagnostics_tx) })
                .await;
            let accepted = view
                .update_in(cx, |this, _, cx| {
                    if this.lsp_generation != generation {
                        return false;
                    }
                    this.lsp_starting = false;
                    match result {
                        Err(error) => {
                            this.lsp_status = format!("Rust: {error:#} — Restart language server");
                            cx.notify();
                            false
                        }
                        Ok(client) => {
                            // Rehydrate every Rust tab, including unsaved inactive buffers.
                            for (path, doc) in &this.inactive_documents {
                                if language_for(path) == "rust"
                                    && let Ok(uri) = file_uri(path)
                                {
                                    let _ = client.did_open(
                                        &uri,
                                        "rust",
                                        doc.editor.read(cx).value().as_ref(),
                                    );
                                }
                            }
                            this.workspace_lsp = Some(client.clone());
                            this.lsp_status = "rust-analyzer: ready".into();
                            if let Some(path) =
                                this.path.as_ref().filter(|p| language_for(p) == "rust")
                                && let Ok(uri) = file_uri(path)
                            {
                                let _ = client.did_open(
                                    &uri,
                                    "rust",
                                    this.editor.read(cx).value().as_ref(),
                                );
                                this.attach_lsp(client, uri, cx);
                            }
                            cx.notify();
                            true
                        }
                    }
                })
                .unwrap_or(false);
            if !accepted {
                return;
            }
            while let Ok(event) = diagnostics_rx.recv().await {
                let active = view
                    .update(cx, |this, cx| {
                        if this.lsp_generation != generation {
                            return false;
                        }
                        this.apply_diagnostics(&event, cx);
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
    }

    fn attach_lsp(&mut self, client: Arc<Client>, uri: lsp_types::Uri, cx: &mut Context<Self>) {
        debug_assert_eq!(client.position_encoding(), "utf-16");
        let providers = LspProviders::new(Arc::clone(&client), uri.clone());
        let target_view = cx.entity().downgrade();
        self.editor.update(cx, |editor, _| {
            let lsp = editor.lsp_mut();
            lsp.hover_provider = client
                .supports("textDocument/hover")
                .then(|| providers.clone() as Rc<dyn gpui_kit::component::input::HoverProvider>);
            lsp.completion_provider = client.supports("textDocument/completion").then(|| {
                providers.clone() as Rc<dyn gpui_kit::component::input::CompletionProvider>
            });
            lsp.definition_provider = client.supports("textDocument/definition").then(|| {
                providers.clone() as Rc<dyn gpui_kit::component::input::DefinitionProvider>
            });
            lsp.show_document = Some(Rc::new(
                move |params: &lsp_types::ShowDocumentParams,
                      _window: &mut Window,
                      cx: &mut App| {
                    let target = super::lsp::path_from_uri(&params.uri);
                    let position = params.selection.map(|range| range.start);
                    let view = target_view.clone();
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |this, cx| {
                            match target {
                                Ok(path) => {
                                    this.pending_open = Some((path, None));
                                    this.pending_lsp_position = position;
                                }
                                Err(error) => {
                                    this.error =
                                        Some(format!("Could not follow definition: {error:#}"))
                                }
                            }
                            cx.notify();
                        });
                    });
                    true
                },
            ));
        });
        self.lsp = Some(LspSession {
            client,
            uri: uri.to_string(),
        });
    }

    fn clear_lsp_providers(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let lsp = editor.lsp_mut();
            lsp.hover_provider = None;
            lsp.completion_provider = None;
            lsp.definition_provider = None;
            lsp.show_document = None;
            cx.notify();
        });
    }

    fn apply_diagnostics(&mut self, event: &DiagnosticEvent, cx: &mut Context<Self>) {
        let client = self
            .workspace_lsp
            .as_ref()
            .or_else(|| self.lsp.as_ref().map(|s| &s.client));
        let Some(client) = client else {
            return;
        };
        let version = client.doc_version(&event.uri);
        if version == 0 || event.version.is_some_and(|v| v != version) {
            return;
        }
        let target = if self
            .path
            .as_ref()
            .and_then(|p| file_uri(p).ok())
            .is_some_and(|u| u.as_str() == event.uri)
        {
            Some(self.editor.clone())
        } else {
            self.inactive_documents
                .iter()
                .find(|(path, _)| file_uri(path).is_ok_and(|u| u.as_str() == event.uri))
                .map(|(_, doc)| doc.editor.clone())
        };
        let Some(target) = target else {
            return;
        };
        target.update(cx, |editor, cx| {
            let text = editor.text().clone();
            if let Some(set) = editor.diagnostics_mut() {
                set.reset(&text);
                set.extend(event.diagnostics.iter().cloned().map(|mut diagnostic| {
                    diagnostic.range.start = super::lsp::providers::lsp_to_editor_position(
                        &text,
                        diagnostic.range.start,
                    );
                    diagnostic.range.end =
                        super::lsp::providers::lsp_to_editor_position(&text, diagnostic.range.end);
                    diagnostic
                }));
                cx.notify();
            }
        });
    }

    /// Return to the origin of the last follow-definition jump, if any.
    pub(crate) fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.back_locations.pop() else {
            if let Some(position) = self.jump_back.borrow_mut().pop() {
                self.editor.update(cx, |editor, cx| {
                    editor.set_cursor_position(position, window, cx);
                    editor.focus(window, cx);
                });
                cx.notify();
            }
            return;
        };
        let current = self.path.as_ref().map(|path| HistoryLocation {
            path: path.clone(),
            position: self.editor.read(cx).cursor_position(),
        });
        self.navigating_history = true;
        let result = self.open(&target.path, None, window, cx);
        self.navigating_history = false;
        if let Err(error) = result {
            self.back_locations.push(target);
            self.error = Some(format!("Could not go back: {error:#}"));
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_cursor_position(target.position, window, cx)
        });
        if let Some(current) = current {
            self.forward_locations.push(current);
        }
        cx.notify();
    }

    pub(crate) fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.forward_locations.pop() else {
            return;
        };
        let current = self.path.as_ref().map(|path| HistoryLocation {
            path: path.clone(),
            position: self.editor.read(cx).cursor_position(),
        });
        self.navigating_history = true;
        let result = self.open(&target.path, None, window, cx);
        self.navigating_history = false;
        if let Err(error) = result {
            self.forward_locations.push(target);
            self.error = Some(format!("Could not go forward: {error:#}"));
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_cursor_position(target.position, window, cx)
        });
        if let Some(current) = current {
            self.back_locations.push(current);
        }
        cx.notify();
    }

    fn open_external(&mut self, kind: ExternalEditorKind, cx: &mut Context<Self>) {
        if !self
            .external_editors
            .get(kind.id())
            .copied()
            .unwrap_or(true)
        {
            return;
        }
        match ExternalEditor::new(kind, None).launch(&self.root, None, None) {
            Ok(()) => self.error = None,
            Err(error) => {
                self.error = Some(format!(
                    "Could not launch {}: {error:#}. Install the application or its command-line launcher, then choose Check again in Settings → Editor.",
                    kind.label()
                ));
            }
        }
        cx.notify();
    }

    fn file_icon(&self, name: &str) -> AnyElement {
        use crate::review::icons::{FALLBACK, icon_key};
        match self
            .icon_tiles
            .get(icon_key(name))
            .or_else(|| self.icon_tiles.get(FALLBACK))
        {
            Some(tile) => img(tile.clone())
                .size(px(16.))
                .flex_shrink_0()
                .into_any_element(),
            None => Icon::new(IconName::File)
                .size(px(16.))
                .text_color(rgb(0x858989))
                .into_any_element(),
        }
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = h_flex()
            .debug_selector(|| "native-tab-strip".into())
            .h(px(43.))
            .flex_1()
            .min_w_0()
            .gap_1()
            .px_2()
            .overflow_x_scrollbar();
        for path in &self.tabs {
            let selected = self.path.as_deref() == Some(path.as_path());
            let dirty = if selected {
                self.dirty
            } else {
                self.inactive_documents
                    .get(path)
                    .is_some_and(|document| document.dirty)
            };
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let tab_path = path.clone();
            let close_path = path.clone();
            row = row.child(
                h_flex()
                    .h(px(32.))
                    .flex_shrink_0()
                    .gap_0()
                    .pr_1()
                    .rounded_md()
                    .when(selected, |row| row.bg(rgb(0x1c1e22)))
                    .child(
                        Button::new(format!("native-tab-{}", path.display()))
                            .small()
                            .ghost()
                            .accessibility_label(name.clone())
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_color(rgb(if selected { 0xe0e2e5 } else { 0x8b9099 }))
                                    .child(self.file_icon(&name))
                                    .child(name),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.error = this
                                    .open(&tab_path, None, window, cx)
                                    .err()
                                    .map(|e| format!("{e:#}"));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("native-close-{}", path.display()))
                            .accessibility_label("Close file")
                            .label(if dirty { "●" } else { "×" })
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(&close_path, window, cx);
                            })),
                    ),
            );
        }
        row.into_any_element()
    }

    /// Grant trust for this checkout and start Rust tooling. Only reachable
    /// through the status-bar trust dialog: trusting may run project tools,
    /// so it asks for explicit confirmation instead of living in the menu.
    fn trust_checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lsp_trusted {
            return;
        }
        self.lsp_trusted = true;
        self.restart_language_server(window, cx);
    }

    /// Drop the current language-server state and start over. Used by the
    /// editor menu's Restart entry; restarting never changes trust.
    fn restart_language_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lsp_generation += 1;
        self.lsp_starting = false;
        self.workspace_lsp = None;
        self.lsp = None;
        if let Some(path) = self.path.clone() {
            self.restart_lsp(&path, language_for(&path), window, cx);
        }
        cx.notify();
    }

    /// Confirm checkout trust in a dialog. Opened by clicking the disabled
    /// Rust status indicator in the footer.
    fn open_trust_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lsp_trusted {
            return;
        }
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("Trust this checkout?")
                .child("Trusting enables Rust tooling with rust-analyzer and may run project tools in this checkout. This lasts for the editor workspace's lifetime.")
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel-trust-checkout")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-trust-checkout")
                                .primary()
                                .label("Trust checkout")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |this, cx| this.trust_checkout(window, cx))
                        .is_ok()
                })
        });
    }

    fn render_editor_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let has_file = self.path.is_some();
        let dirty = self.dirty;
        let external_editors = self.external_editors.clone();
        let can_go_back = self.has_jump_history();
        let can_go_forward = !self.forward_locations.is_empty();
        Button::new("native-editor-actions")
            .label("⋯")
            .small()
            .ghost()
            .accessibility_label("Editor actions")
            .dropdown_menu(move |mut menu, _, _| {
                for (label, action, enabled) in [
                    ("Find in file", 0, has_file),
                    ("Replace in file", 1, has_file),
                    ("Go to line", 2, has_file),
                    ("Go back", 12, can_go_back),
                    ("Go forward", 13, can_go_forward),
                    ("Save", 3, dirty),
                    ("Save as", 4, has_file),
                    ("Discard changes", 5, dirty),
                    ("Find file", 6, true),
                    ("Search project", 7, true),
                    ("Show file tree", 8, true),
                    ("Refresh files", 9, true),
                    ("Restart language server", 10, true),
                ] {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).disabled(!enabled).on_click(
                        move |_, window, cx| {
                            view.update(cx, |this, cx| match action {
                                0 | 1 => this
                                    .editor
                                    .update(cx, |editor, cx| editor.open_search(action == 1, cx)),
                                2 => this.show_browser(BrowserMode::Line, window, cx),
                                3 => this.save(window, cx),
                                4 => this.save_as(window, cx),
                                5 => this.discard(window, cx),
                                6 => this.open_file_finder(window, cx),
                                7 => this.show_browser(BrowserMode::Text, window, cx),
                                8 => {
                                    this.browser = BrowserMode::Tree;
                                    cx.notify();
                                }
                                9 => this.refresh_files(cx),
                                12 => this.go_back(window, cx),
                                13 => this.go_forward(window, cx),
                                10 => this.restart_language_server(window, cx),
                                _ => {}
                            })
                            .ok();
                        },
                    ));
                }
                menu = menu.separator();
                for kind in ExternalEditorKind::ALL {
                    if !external_editors.get(kind.id()).copied().unwrap_or(true)
                        || kind.installed_executable().is_none()
                    {
                        continue;
                    }
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("Open in {}", kind.label())).on_click(
                            move |_, _, cx| {
                                view.update(cx, |this, cx| this.open_external(kind, cx))
                                    .ok();
                            },
                        ),
                    );
                }
                menu
            })
    }

    fn render_browser(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.browser == BrowserMode::Closed {
            return None;
        }
        let mut panel = v_flex()
            .id("native-tree-rows")
            .flex_1()
            .min_h_0()
            .gap_0()
            .px_2()
            .py_2()
            .track_scroll(&self.tree_scroll)
            .overflow_y_scroll()
            .vertical_scrollbar(&self.tree_scroll);
        let mode = if self.finder_open() {
            self.finder_previous
        } else {
            self.browser
        };
        if mode == BrowserMode::Closed {
            return None;
        }
        match mode {
            BrowserMode::Files | BrowserMode::Buffers | BrowserMode::Text => unreachable!(),
            BrowserMode::Tree => {
                let rows = tree_rows(&self.files, &self.expanded_dirs, &self.tree_limits);
                let cursor = self
                    .navigation_active
                    .then(|| self.tree_cursor_index(&rows))
                    .flatten();
                for (index, item) in rows.into_iter().enumerate() {
                    let expanded = self.expanded_dirs.contains(&item.key);
                    let selected = item.file.is_some()
                        && self
                            .path
                            .as_ref()
                            .and_then(|path| path.strip_prefix(&self.canonical_root).ok())
                            .is_some_and(|path| path == Path::new(&item.key));
                    let icon = if item.file.is_some() {
                        self.file_icon(&item.label)
                    } else {
                        h_flex()
                            .gap_1()
                            .flex_shrink_0()
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(px(16.))
                                .text_color(rgb(0x737983)),
                            )
                            .when(!item.more, |row| {
                                row.child(
                                    Icon::new(if expanded {
                                        IconName::FolderOpen
                                    } else {
                                        IconName::FolderClosed
                                    })
                                    .size(px(16.))
                                    .text_color(rgb(0xa4a9b2)),
                                )
                            })
                            .into_any_element()
                    };
                    let id = item.id();
                    panel = panel.child(
                        Button::new(id)
                            .ghost()
                            .small()
                            .h(px(28.))
                            .w_full()
                            .p_0()
                            .accessibility_label(item.label.clone())
                            .when(selected, |row| row.bg(rgb(0x222b36)))
                            .when(cursor == Some(index), |row| {
                                row.border_1().border_color(rgb(0x61afef))
                            })
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .pl(px(10. + item.depth as f32 * 16.))
                                    .pr_2()
                                    .text_color(rgb(if selected { 0x61afef } else { 0xa4a9b2 }))
                                    .child(icon)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_ellipsis()
                                            .child(item.label.clone()),
                                    )
                                    .when(selected && self.dirty, |row| row.child("●")),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.activate_tree_row(&item, window, cx);
                            })),
                    );
                }
            }
            BrowserMode::Line => {
                panel = panel.child(Input::new(&self.line_query).small()).child(
                    Button::new("native-go-line")
                        .label("Go to line")
                        .outline()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let line = this.line_query.read(cx).value().parse().ok();
                            if let Some(path) = this.path.clone() {
                                this.error = this
                                    .open(&path, line, window, cx)
                                    .err()
                                    .map(|error| format!("{error:#}"));
                            }
                            this.browser = BrowserMode::Tree;
                            cx.notify();
                        })),
                );
            }
            BrowserMode::Closed => unreachable!(),
        }
        let project_name = self
            .root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Some(
            v_flex()
                .debug_selector(|| "native-sidebar".into())
                .id("native-sidebar")
                .track_focus(&self.sidebar_focus)
                .w(px(260.))
                .h_full()
                .flex_shrink_0()
                .min_h_0()
                .bg(rgb(0x121416))
                .border_r_1()
                .border_color(rgb(if self.sidebar_focus.contains_focused(window, cx) {
                    0x61afef
                } else {
                    0x26292e
                }))
                .child(
                    h_flex()
                        .h(px(44.))
                        .flex_shrink_0()
                        .px_3()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_sm()
                                .text_color(rgb(0xd3d6dc))
                                .child(project_name),
                        )
                        .child(
                            Button::new("native-open-file")
                                .icon(IconName::Search)
                                .small()
                                .ghost()
                                .accessibility_label("Find file")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_file_finder(window, cx)
                                })),
                        )
                        .child(
                            Button::new("native-file-tree")
                                .icon(IconName::FolderClosed)
                                .small()
                                .ghost()
                                .accessibility_label("Show file tree")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.browser = BrowserMode::Tree;
                                    cx.notify();
                                })),
                        ),
                )
                .child(panel)
                .into_any_element(),
        )
    }
}

fn language_for(path: &Path) -> &'static str {
    if path.file_name().is_some_and(|name| name == "Cargo.lock") {
        return "toml";
    }
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "rs" => "rust",
        "toml" => "toml",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "sh" => "bash",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "json" => "json",
        "yml" | "yaml" => "yaml",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        _ => "",
    }
}

/// Read a file the editor can represent: a regular file, at most
/// `MAX_FILE_BYTES`, UTF-8, and free of NUL bytes.
fn read_text_file(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        bail!("Choose a regular file.");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("This file is larger than 2 MiB. Open it in Zed or VS Code.");
    }
    text_from_bytes(&fs::read(path)?)
}

fn text_from_bytes(bytes: &[u8]) -> Result<String> {
    if bytes.contains(&0) {
        bail!("This appears to be a binary file. Open it in Zed or VS Code.");
    }
    String::from_utf8(bytes.to_vec()).context("This file is not UTF-8; open it in Zed or VS Code")
}

/// Keep a file's consistent newline convention when the editor inserts a
/// newline or text is pasted. Mixed-newline files are left byte-for-byte as
/// edited because there is no unambiguous style to apply.
fn file_bytes_for_buffer<'a>(saved: &str, draft: &'a str) -> Cow<'a, str> {
    let has_crlf = saved.contains("\r\n");
    let has_lone_lf = saved.as_bytes().iter().enumerate().any(|(index, byte)| {
        *byte == b'\n' && (index == 0 || saved.as_bytes()[index - 1] != b'\r')
    });
    if has_crlf && !has_lone_lf {
        let lone = draft.as_bytes().iter().enumerate().any(|(index, byte)| {
            *byte == b'\n' && (index == 0 || draft.as_bytes()[index - 1] != b'\r')
        });
        if lone {
            return Cow::Owned(draft.replace("\r\n", "\n").replace('\n', "\r\n"));
        }
    } else if !has_crlf && has_lone_lf && draft.contains("\r\n") {
        return Cow::Owned(draft.replace("\r\n", "\n"));
    }
    Cow::Borrowed(draft)
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn save_if_unchanged(path: &Path, original: &[u8], replacement: &[u8]) -> Result<()> {
    if replacement.len() as u64 > MAX_FILE_BYTES {
        bail!("The draft is larger than 2 MiB. Open it in Zed or VS Code.");
    }
    let current =
        fs::read(path).with_context(|| format!("Could not re-read {}", path.display()))?;
    if current != original {
        bail!(
            "The file changed on disk. Your draft is still open; copy it before reloading the file."
        );
    }
    let metadata = fs::metadata(path)?;
    let parent = path.parent().context("File has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(replacement)?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    temporary.as_file().sync_all()?;
    // Check again immediately before replacement to catch ordinary concurrent
    // agent edits. The draft stays open on any failure.
    if fs::read(path)? != original {
        bail!("The file changed on disk while saving. Your draft is still open.");
    }
    temporary
        .persist(path)
        .context("Could not replace the file")?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

impl Focusable for NativeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor_focus(cx)
    }
}

impl Render for NativeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let definition_origin = self.pending_lsp_position.and_then(|_| {
            self.path.as_ref().map(|path| HistoryLocation {
                path: path.clone(),
                position: self.editor.read(cx).cursor_position(),
            })
        });
        if let Some((path, line)) = self.pending_open.take() {
            self.error = self
                .open(&path, line, window, cx)
                .err()
                .map(|e| format!("{e:#}"));
        }
        if let Some(position) = self.pending_lsp_position.take()
            && self.error.is_none()
        {
            self.editor.update(cx, |editor, cx| {
                let position =
                    super::lsp::providers::lsp_to_editor_position(editor.text(), position);
                editor.set_cursor_position(position, window, cx);
            });
            if let Some(origin) = definition_origin
                && self.path.as_ref() == Some(&origin.path)
                && self.editor.read(cx).cursor_position() != origin.position
            {
                self.record_origin(Some(origin), true, cx);
            }
        }
        if let Some(line) = self.pending_line.take() {
            if let Some(path) = self.path.clone() {
                self.error = self
                    .open(&path, Some(line), window, cx)
                    .err()
                    .map(|error| format!("{error:#}"));
            }
            self.browser = BrowserMode::Tree;
        }
        if let Some(preview) = self.preview_text.take() {
            self.preview_first_line = preview.first_line;
            let selected = self.selected_finder_file();
            let language = selected
                .as_ref()
                .map(|(file, _)| language_for(&file.path))
                .unwrap_or("text");
            let target = selected.and_then(|(_, line)| line);
            let query = self.text_query.read(cx).value().to_string();
            let match_range = target.and_then(|line| preview.match_range(line, &query));
            // Highlight the actual matched spelling so Unicode case folding
            // also works with the editor's ASCII-insensitive search painter.
            let matched_text = match_range
                .as_ref()
                .map(|range| preview.text[range.clone()].to_owned());
            self.preview_editor.update(cx, |editor, cx| {
                editor.set_highlighter(language, cx);
                editor.set_line_number(target.is_none(), window, cx);
                editor.set_value(preview.text, window, cx);
                editor.close_search(cx);
                if let Some(text) = matched_text {
                    editor.set_search_query(text, true, cx);
                }
                if let Some(line) = target {
                    editor.set_cursor_position(
                        Position::new(line.saturating_sub(preview.first_line) as u32, 0),
                        window,
                        cx,
                    );
                }
                if let Some(range) = match_range {
                    editor.set_selected_range(range, cx);
                }
            });
            if self.finder_open() {
                let query = if self.browser == BrowserMode::Text {
                    &self.text_query
                } else {
                    &self.file_query
                };
                query.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        let label = self
            .path
            .as_ref()
            .and_then(|path| path.strip_prefix(&self.canonical_root).ok())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No file open".into());
        let browser = self.render_browser(window, cx);
        let tabs = self.render_tabs(cx);
        let actions = self.render_editor_menu(cx).into_any_element();
        let code = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x0c0e10))
            .child(
                h_flex()
                    .debug_selector(|| "native-toolbar".into())
                    .h(px(44.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(0x202328))
                    .child(tabs)
                    .child(actions),
            )
            .when_some(self.close_pending.clone(), |view, path| {
                view.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_sm().child(format!(
                            "Save changes to {} before closing?",
                            path.display()
                        )))
                        .child(
                            Button::new("native-close-save")
                                .label("Save")
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_close(CloseChoice::Save, window, cx)
                                })),
                        )
                        .child(
                            Button::new("native-close-discard")
                                .label("Discard")
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_close(CloseChoice::Discard, window, cx)
                                })),
                        )
                        .child(
                            Button::new("native-close-cancel")
                                .label("Cancel")
                                .ghost()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_close(CloseChoice::Cancel, window, cx)
                                })),
                        ),
                )
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
            })
            .when_some(self.disk_notice.clone(), |view, notice| {
                view.child(div().text_sm().text_color(rgb(0x60a5fa)).child(notice))
            })
            .when(self.conflict, |view| {
                view.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div().text_sm().text_color(rgb(0xfbbf24)).child(
                                "This file changed on disk and the buffer has unsaved changes.",
                            ),
                        )
                        .child(
                            Button::new("native-reload-file")
                                .label("Reload from disk")
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.reload_from_disk(window, cx)
                                })),
                        )
                        .child(
                            Button::new("native-compare-file")
                                .label("Compare")
                                .outline()
                                .on_click(cx.listener(|this, _, _, cx| this.compare_with_disk(cx))),
                        ),
                )
            })
            .when_some(self.disk_comparison.clone(), |view, comparison| {
                view.child(
                    v_flex()
                        .max_h(px(220.))
                        .overflow_y_scrollbar()
                        .p_2()
                        .border_1()
                        .border_color(rgb(0x454545))
                        .child(div().text_sm().child("Disk → draft"))
                        .child(div().text_xs().child(comparison)),
                )
            })
            .child(
                div()
                    .debug_selector(|| "native-code-pane".into())
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .pt_2()
                    .child(Editor::new(&self.editor).h_full().appearance(false)),
            )
            .child(
                h_flex()
                    .h(px(24.))
                    .flex_shrink_0()
                    .px_3()
                    .gap_2()
                    .text_xs()
                    .text_color(rgb(0x727985))
                    .border_t_1()
                    .border_color(rgb(0x202328))
                    .child(div().flex_1().min_w_0().overflow_hidden().child(label))
                    .when(self.dirty, |row| row.child("Modified"))
                    .child(
                        div()
                            .max_w(px(420.))
                            .text_ellipsis()
                            .when(!self.lsp_trusted, |status| {
                                status.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        this.open_trust_dialog(window, cx)
                                    }),
                                )
                            })
                            .child(self.lsp_status.clone()),
                    )
                    .child("UTF-8"),
            );
        let finder = self.render_file_finder(cx);
        h_flex()
            .relative()
            .size_full()
            .min_h_0()
            .items_stretch()
            .overflow_hidden()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && (matches!(
                        this.browser,
                        BrowserMode::Files
                            | BrowserMode::Buffers
                            | BrowserMode::Text
                            | BrowserMode::Line
                    ) || this.disk_comparison.is_some())
                {
                    if this.finder_open() {
                        this.close_file_finder(window, cx);
                    } else {
                        this.browser = BrowserMode::Tree;
                    }
                    this.disk_comparison = None;
                    this.editor_focus(cx).focus(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    cx.notify();
                } else if event.keystroke.key.eq_ignore_ascii_case("s")
                    && crate::command_palette::is_primary_modifier(
                        event.keystroke.modifiers.platform,
                        event.keystroke.modifiers.control,
                    )
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.shift
                    && this.path.is_some()
                {
                    this.save(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .when_some(browser, |row, browser| row.child(browser))
            .child(code)
            .when_some(finder, |row, finder| row.child(finder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    #[test]
    fn tree_sorts_folders_before_files_at_each_level() {
        let mut files: Vec<_> = [
            "a.txt",
            "zfolder/a.txt",
            "bfolder/x.txt",
            ".gitignore",
            "zfolder/zfolder/c.txt",
            "bfolder/zfolder/file.txt",
        ]
        .into_iter()
        .map(|label| ProjectFile {
            path: PathBuf::from(label),
            label: label.into(),
        })
        .collect();
        sort_tree_files(&mut files);
        let expanded = [
            "",
            "bfolder",
            "bfolder/zfolder",
            "zfolder",
            "zfolder/zfolder",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let rows = tree_rows(&files, &expanded, &HashMap::new());
        assert_eq!(
            rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
            [
                "bfolder",
                "bfolder/zfolder",
                "bfolder/zfolder/file.txt",
                "bfolder/x.txt",
                "zfolder",
                "zfolder/zfolder",
                "zfolder/zfolder/c.txt",
                "zfolder/a.txt",
                ".gitignore",
                "a.txt",
            ]
        );
    }

    #[test]
    fn tree_paginates_after_sorting_folders_first() {
        let mut files: Vec<_> = (0..105)
            .map(|index| {
                let label = format!("a-{index:03}.txt");
                ProjectFile {
                    path: PathBuf::from(&label),
                    label,
                }
            })
            .collect();
        files.push(ProjectFile {
            path: "zfolder/child.txt".into(),
            label: "zfolder/child.txt".into(),
        });
        sort_tree_files(&mut files);
        let rows = tree_rows(&files, &HashSet::from([String::new()]), &HashMap::new());
        assert_eq!(rows[0].key, "zfolder", "folders must be on the first page");
        assert_eq!(rows[1].key, "a-000.txt");
        assert_eq!(
            rows.iter().filter(|row| row.file.is_some()).count(),
            TREE_PAGE_SIZE - 1
        );
        assert!(
            rows.last().unwrap().more,
            "Show more follows the sorted siblings"
        );
        let rows = tree_rows(
            &files,
            &HashSet::from([String::new()]),
            &HashMap::from([(String::new(), 200)]),
        );
        assert_eq!(rows[0].key, "zfolder");
        assert_eq!(rows.last().unwrap().key, "a-104.txt");
        assert!(!rows.iter().any(|row| row.more));
    }

    #[test]
    fn expanded_tree_keeps_later_top_level_folders_visible() {
        let mut files: Vec<ProjectFile> = (0..350)
            .map(|index| ProjectFile {
                path: PathBuf::from(format!("a/{index:03}.txt")),
                label: format!("a/{index:03}.txt"),
            })
            .collect();
        files.push(ProjectFile {
            path: PathBuf::from("z/last.txt"),
            label: "z/last.txt".into(),
        });
        let expanded = HashSet::from([String::new(), "a".into()]);
        let rows = tree_rows(&files, &expanded, &HashMap::new());
        assert!(rows.iter().any(|row| row.key == "z"));
        assert!(rows.iter().any(|row| row.more && row.key == "a"));
        assert_eq!(
            rows.iter().filter(|row| row.file.is_some()).count(),
            TREE_PAGE_SIZE
        );
    }

    #[test]
    fn save_preserves_draft_when_disk_changed_and_keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("বাংলা file.rs");
        fs::write(&path, "before\n").unwrap();
        fs::write(&path, "agent edit\n").unwrap();
        assert!(save_if_unchanged(&path, b"before\n", b"human edit\n").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "agent edit\n");
        save_if_unchanged(&path, b"agent edit\n", b"human edit\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "human edit\n");
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_posix_mode_and_symlink_target() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "before\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();
        save_if_unchanged(&target, b"before\n", b"after\n").unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&link).unwrap(), "after\n");
    }

    /// Test host: a real window plus a bare `NativeEditor`, without entering
    /// any repository workspace.
    fn test_editor<'a>(
        cx: &'a mut gpui_kit::TestAppContext,
        dir: &std::path::Path,
    ) -> (Entity<NativeEditor>, &'a mut gpui_kit::VisualTestContext) {
        test_editor_with_data(cx, dir, None)
    }

    fn test_editor_with_data<'a>(
        cx: &'a mut gpui_kit::TestAppContext,
        dir: &std::path::Path,
        data_root: Option<PathBuf>,
    ) -> (Entity<NativeEditor>, &'a mut gpui_kit::VisualTestContext) {
        use std::{cell::RefCell, rc::Rc};
        let holder: Rc<RefCell<Option<Entity<NativeEditor>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let root = dir.to_owned();
        let (_, test_cx) = cx.add_window_view(move |window, cx| {
            let editor = cx.new(|cx| {
                NativeEditor::new(&root, HashMap::new(), data_root.as_deref(), window, cx)
            });
            *holder_for_window.borrow_mut() = Some(editor.clone());
            gpui_kit::component::Root::new(editor, window, cx)
        });
        (holder.borrow().clone().unwrap(), test_cx)
    }

    #[gpui_kit::test]
    fn navigation_tree_moves_without_opening_and_activates_folders_files_and_more(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let files: Vec<_> = (0..105)
            .map(|index| {
                let label = format!("src/file-{index:03}.txt");
                let path = dir.path().join(&label);
                fs::write(&path, "hello\nworld\n").unwrap();
                ProjectFile { path, label }
            })
            .collect();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            view.files = files;
            view.set_navigation_active(true, cx);
            assert_eq!(view.navigation_panes(cx).len(), 2);
            view.move_navigation_item(0, false, window, cx);
            assert_eq!(view.tree_cursor.as_deref(), Some("native-tree-dir-src"));
            assert!(view.path.is_none());
            assert!(view.activate_navigation_cursor(window, cx));
            assert!(view.expanded_dirs.contains("src"));
            view.move_navigation_item(0, true, window, cx);
            assert_eq!(
                view.tree_cursor.as_deref(),
                Some("native-tree-file-src/file-000.txt")
            );
            assert!(view.path.is_none(), "movement only highlights the row");
            view.activate_navigation_cursor(window, cx);
            assert_eq!(
                view.path,
                Some(view.canonical_root.join("src/file-000.txt"))
            );
            view.editor
                .update(cx, |editor, cx| editor.set_value("draft", window, cx));
            view.move_navigation_item(0, true, window, cx);
            view.activate_navigation_cursor(window, cx);
            assert!(
                view.inactive_documents
                    .values()
                    .any(|doc| doc.editor.read(cx).text().to_string() == "draft")
            );
            for _ in 0..110 {
                view.move_navigation_item(0, true, window, cx);
            }
            assert_eq!(view.tree_cursor.as_deref(), Some("native-tree-more-src"));
            view.activate_navigation_cursor(window, cx);
            assert_eq!(view.tree_limits.get("src"), Some(&200));
            assert!(
                tree_rows(&view.files, &view.expanded_dirs, &view.tree_limits)
                    .iter()
                    .any(|row| row.key == "src/file-104.txt")
            );
            view.set_navigation_active(false, cx);
            assert!(!view.navigation_active);
            assert!(view.tree_cursor.is_none());
            view.browser = BrowserMode::Closed;
            assert_eq!(view.navigation_panes(cx).len(), 1);
            assert!(!view.navigation_in_tree(0));
            view.files.clear();
            view.browser = BrowserMode::Tree;
            view.set_navigation_active(true, cx);
            view.move_navigation_item(0, true, window, cx);
            view.activate_navigation_cursor(window, cx);
            assert!(
                view.tree_cursor.is_none(),
                "empty trees consume movement safely"
            );
        });
    }

    #[gpui_kit::test]
    fn navigation_tree_scrolls_highlighted_rows_into_view(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update(test_cx, |view, cx| {
            view.files = (0..80)
                .map(|index| {
                    let label = format!("file-{index:03}.txt");
                    ProjectFile {
                        path: dir.path().join(&label),
                        label,
                    }
                })
                .collect();
            view.set_navigation_active(true, cx);
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        view.update_in(test_cx, |view, window, cx| {
            for _ in 0..79 {
                view.move_navigation_item(0, true, window, cx);
            }
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, _| {
            assert!(view.tree_scroll.offset().y < px(0.));
            let viewport = view.tree_scroll.bounds();
            let row = view.tree_scroll.bounds_for_item(79).unwrap();
            let bottom = row.bottom() + view.tree_scroll.offset().y;
            assert!(
                bottom <= viewport.bottom(),
                "highlighted row must be visible"
            );
        });
    }

    #[gpui_kit::test]
    fn navigation_editor_moves_vertically_without_changing_text(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        let text = "hello\nx\nworld\n";
        fs::write(&file, text).unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.update_in(test_cx, |view, window, cx| {
            view.open(&file, None, window, cx).unwrap();
            view.editor.update(cx, |editor, cx| {
                editor.set_cursor_position(Position::new(0, 4), window, cx)
            });
            view.editor.update(cx, |editor, cx| {
                editor.present_completion_items(
                    0,
                    "",
                    vec![lsp_types::CompletionItem {
                        label: "hello".into(),
                        ..Default::default()
                    }],
                    cx,
                );
                editor.set_overlay_action_handler(|_, _, _, _| true);
                assert!(editor.completion_menu_state().open);
            });
            view.set_navigation_active(true, cx);
            assert!(!view.editor.read(cx).completion_menu_state().open);
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        for (down, row, column) in [
            (true, 1, 1),
            (true, 2, 4),
            (false, 1, 1),
            (false, 0, 4),
            (false, 0, 4),
        ] {
            view.update_in(test_cx, |view, window, cx| {
                view.move_navigation_item(1, down, window, cx)
            });
            test_cx.run_until_parked();
            test_cx.update(|window, cx| window.render_frame(cx));
            view.read_with(test_cx, |view, cx| {
                assert_eq!(
                    view.editor.read(cx).cursor_position(),
                    Position::new(row, column)
                );
                assert_eq!(view.editor.read(cx).text().to_string(), text);
                assert!(!view.dirty);
            });
        }
        view.update_in(test_cx, |view, window, cx| {
            view.browser = BrowserMode::Closed;
            view.move_navigation_item(0, true, window, cx);
        });
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.editor.read(cx).cursor_position().line, 1)
        });
    }

    #[gpui_kit::test]
    fn file_finder_controls_open_a_checkout_file(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello world.txt");
        fs::write(&file, "hello\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-open-file").visible());
            window.click("native-open-file", cx);
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-finder-close").visible());
            assert!(window.find("native-file-0").visible());
            window.click("native-file-0", cx);
            window.render_frame(cx);
        });
        view.read_with(test_cx, |view, cx| {
            assert_eq!(
                view.path.as_deref(),
                Some(file.canonicalize().unwrap().as_path())
            );
            assert_eq!(view.editor.read(cx).value().as_ref(), "hello\n");
            assert_eq!(view.browser, BrowserMode::Tree);
        });
    }

    #[gpui_kit::test]
    fn buffer_finder_searches_only_open_tabs_and_keeps_unsaved_documents(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("navigation.txt");
        let second = dir.path().join("settings.txt");
        fs::write(&first, "first\nsecond\n").unwrap();
        fs::write(&second, "settings\n").unwrap();
        fs::write(dir.path().join("navigation-closed.txt"), "closed\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "navigation.txt\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        let document_id = view.update_in(test_cx, |view, window, cx| {
            view.open(&first, None, window, cx).unwrap();
            view.editor.update(cx, |editor, cx| {
                editor.set_value("unsaved buffer\nsecond\n", window, cx);
                editor.set_cursor_position(Position::new(1, 3), window, cx);
            });
            view.dirty = true;
            let id = view.editor.entity_id();
            view.open(&second, None, window, cx).unwrap();
            view.open_buffer_finder(window, cx);
            id
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.browser, BrowserMode::Buffers);
            assert_eq!(view.file_total, 2);
            assert_eq!(view.file_matches.len(), 2);
            assert!(
                view.file_matches
                    .iter()
                    .all(|file| view.tabs.contains(&file.path))
            );
            assert_eq!(
                view.preview_editor.read(cx).value().as_ref(),
                "unsaved buffer\nsecond\n"
            );
            assert!(!view.files.iter().any(|file| file.label == "navigation.txt"));
        });
        test_cx.simulate_keystrokes("n a v i g a t o n");
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.file_matches.len(), 1);
            assert_eq!(view.file_matches[0].label, "navigation.txt");
        });
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-file-0").visible());
        });
        test_cx.simulate_keystrokes("enter");
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.path.as_ref(), Some(&first.canonicalize().unwrap()));
            assert_eq!(view.editor.entity_id(), document_id);
            assert_eq!(
                view.editor.read(cx).value().as_ref(),
                "unsaved buffer\nsecond\n"
            );
            assert_eq!(view.editor.read(cx).cursor_position(), Position::new(1, 3));
            assert!(view.dirty);
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.browser, BrowserMode::Tree);
        });
        assert_eq!(fs::read_to_string(first).unwrap(), "first\nsecond\n");
        view.update_in(test_cx, |view, window, cx| {
            view.open_buffer_finder(window, cx);
        });
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("down");
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.file_selection, 1);
            assert_eq!(view.preview_editor.read(cx).value().as_ref(), "settings\n");
        });
        test_cx.simulate_keystrokes("escape");
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.editor.entity_id(), document_id);
            assert_eq!(view.browser, BrowserMode::Tree);
        });
    }

    #[gpui_kit::test]
    fn buffer_finder_previews_large_unicode_drafts_and_switches_deleted_tabs(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("deleted.txt");
        let second = dir.path().join("keep.txt");
        fs::write(&first, "original\n").unwrap();
        fs::write(&second, "keep\n").unwrap();
        let canonical = first.canonicalize().unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        let document_id = view.update_in(test_cx, |view, window, cx| {
            view.open(&first, None, window, cx).unwrap();
            view.editor.update(cx, |editor, cx| {
                editor.set_value("界".repeat(20_000), window, cx)
            });
            view.dirty = true;
            let id = view.editor.entity_id();
            fs::remove_file(&first).unwrap();
            view.open(&second, None, window, cx).unwrap();
            view.open_buffer_finder(window, cx);
            id
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-file-0").visible());
        });
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.file_matches[0].path, canonical);
            let text = view.preview_editor.read(cx).value();
            assert!(text.starts_with("界"));
            assert!(text.len() < 40_100);
            assert!(text.ends_with("… Preview truncated\n"));
        });
        test_cx.update(|window, cx| {
            window.click("native-file-0", cx);
            window.render_frame(cx);
        });
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.path.as_ref(), Some(&canonical));
            assert_eq!(view.editor.entity_id(), document_id);
            assert_eq!(view.editor.read(cx).text().len(), 60_000);
            assert!(view.dirty);
            assert!(view.error.is_none());
        });
        // Accepting the already-active deleted buffer must also keep it open.
        view.update_in(test_cx, |view, window, cx| {
            view.open_buffer_finder(window, cx)
        });
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("enter");
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.path.as_ref(), Some(&canonical));
            assert_eq!(view.editor.entity_id(), document_id);
            assert!(view.error.is_none());
            assert_eq!(view.tabs.len(), 2);
        });
    }

    #[gpui_kit::test]
    fn buffer_finder_handles_empty_tabs_and_rebuilds_after_closing_a_tab(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            view.browser = BrowserMode::Closed;
            view.open_buffer_finder(window, cx);
        });
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("enter");
        view.read_with(test_cx, |view, _| {
            assert!(view.file_matches.is_empty());
            assert_eq!(view.browser, BrowserMode::Buffers);
            assert!(view.path.is_none());
        });
        test_cx.simulate_keystrokes("escape");
        view.update_in(test_cx, |view, window, cx| {
            assert_eq!(view.browser, BrowserMode::Closed);
            view.open_buffer_finder(window, cx);
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            // External opens must appear while the picker is already visible.
            view.open(&file, None, window, cx).unwrap();
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            assert_eq!(view.file_matches.len(), 1);
            view.close_tab(&file.canonicalize().unwrap(), window, cx);
        });
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, _| {
            assert!(view.tabs.is_empty());
            assert!(view.file_matches.is_empty());
            assert!(view.pending_open.is_none());
            assert_eq!(view.browser, BrowserMode::Buffers);
        });
        view.update_in(test_cx, |view, _, cx| {
            // A stale result arriving after close must never reopen the tab.
            view.file_matches.push(ProjectFile {
                path: file.canonicalize().unwrap(),
                label: "hello.txt".into(),
            });
            view.accept_file(cx);
            assert!(view.pending_open.is_none());
        });
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("escape");
        view.update_in(test_cx, |view, window, cx| {
            view.open_buffer_finder(window, cx);
            view.open_file_finder(window, cx);
        });
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.browser, BrowserMode::Files);
            assert_eq!(view.file_matches.len(), 1);
            assert_eq!(view.file_matches[0].label, "hello.txt");
        });
    }

    fn settle_live_grep(cx: &mut gpui_kit::VisualTestContext) {
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    #[gpui_kit::test]
    fn live_grep_keeps_alphabetical_priority_when_tree_is_folder_first(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("zfolder")).unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        fs::write(dir.path().join("zfolder/child.txt"), "needle\n".repeat(201)).unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            assert_eq!(view.files[0].label, "zfolder/child.txt");
            view.open_live_grep(window, cx);
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        test_cx.simulate_keystrokes("n e e d l e");
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, _| {
            assert!(view.search_truncated);
            assert_eq!(view.search_results.len(), 200);
            assert_eq!(view.search_results[0].label, "a.txt");
            assert_eq!(view.search_results[1].label, "zfolder/child.txt");
        });
    }

    #[gpui_kit::test]
    fn live_grep_updates_on_typing_previews_and_opens_the_matching_line(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        fs::write(
            &file,
            format!(
                "{}let needle = 1;\nlet NEEDLE = 2;\n",
                "// context\n".repeat(350)
            ),
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "// needle\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            view.browser = BrowserMode::Closed;
            view.open_live_grep(window, cx);
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        test_cx.simulate_keystrokes("n e e d l e");
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.browser, BrowserMode::Text);
            assert_eq!(view.search_results.len(), 3);
            assert_eq!(view.search_results[0].line, 351);
            assert_eq!(view.preview_first_line, 331);
            assert!(
                view.preview_editor
                    .read(cx)
                    .value()
                    .contains("let needle = 1;")
            );
            let preview = view.preview_editor.read(cx);
            assert_eq!(&preview.value()[preview.selected_range()], "needle");
            let search = preview.search_session();
            assert!(search.is_active());
            assert!(
                !search.open,
                "preview highlighting must not open a second search panel"
            );
            assert_eq!(search.matcher.len(), 2);
        });
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-finder-close").visible());
            assert!(window.find("native-search-0").visible());
        });
        test_cx.simulate_keystrokes("down ctrl-n ctrl-p");
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.file_selection, 1);
            let preview = view.preview_editor.read(cx);
            assert_eq!(&preview.value()[preview.selected_range()], "NEEDLE");
            assert_eq!(view.text_query.read(cx).value().as_ref(), "needle");
        });
        test_cx.simulate_keystrokes("enter");
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, cx| {
            assert_eq!(
                view.path.as_deref(),
                Some(file.canonicalize().unwrap().as_path())
            );
            assert_eq!(
                view.editor.read(cx).cursor_position(),
                Position::new(351, 0)
            );
            assert_eq!(view.browser, BrowserMode::Closed);
        });
        view.update_in(test_cx, |view, window, cx| {
            view.open_file_finder(window, cx)
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, cx| {
            assert!(
                !view.preview_editor.read(cx).search_session().is_active(),
                "file previews must clear grep word highlights"
            );
        });
    }

    #[gpui_kit::test]
    fn live_grep_cancels_old_queries_refreshes_and_restores_previous_view(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "old\nnew\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            view.open_file_finder(window, cx);
            view.open_live_grep(window, cx);
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| window.render_frame(cx));
        test_cx.simulate_keystrokes("o l d");
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a n e w"
        } else {
            "ctrl-a n e w"
        });
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.search_results.len(), 1);
            assert_eq!(view.search_results[0].line, 2);
            assert!(!view.file_searching);
        });
        fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        test_cx.update(|window, cx| window.click("native-finder-refresh", cx));
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, _| assert_eq!(view.search_results.len(), 2));
        test_cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a backspace"
        } else {
            "ctrl-a backspace"
        });
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, cx| {
            assert!(!view.preview_editor.read(cx).search_session().is_active())
        });
        test_cx.simulate_keystrokes("enter");
        view.read_with(test_cx, |view, _| {
            assert!(view.search_results.is_empty());
            assert_eq!(view.browser, BrowserMode::Text);
            assert!(view.path.is_none());
        });
        test_cx.simulate_keystrokes("o l d");
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("escape");
        settle_live_grep(test_cx);
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.browser, BrowserMode::Tree);
            assert!(view.search_results.is_empty());
            assert!(view.preview_text.is_none());
        });
        test_cx.update(|window, cx| window.render_frame(cx));
        assert!(test_cx.debug_bounds("native-file-finder").is_none());
    }

    #[gpui_kit::test]
    fn file_finder_keyboard_selection_preview_and_escape(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        for index in 0..80 {
            fs::write(
                dir.path().join(format!("file-{index:03}.rs")),
                format!("fn file_{index}() {{}}\n"),
            )
            .unwrap();
        }
        let (view, test_cx) = test_editor(cx, dir.path());
        test_cx.run_until_parked();
        view.update_in(test_cx, |view, window, cx| {
            view.browser = BrowserMode::Closed;
            view.open_file_finder(window, cx);
        });
        test_cx.run_until_parked();
        test_cx.update(|window, cx| window.render_frame(cx));
        test_cx.simulate_keystrokes("down ctrl-n ctrl-p");
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.file_selection, 1);
            assert_eq!(
                view.preview_editor.read(cx).value().as_ref(),
                "fn file_1() {}\n"
            );
        });
        for _ in 0..40 {
            test_cx.simulate_keystrokes("down");
        }
        test_cx.run_until_parked();
        test_cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("native-file-41").visible());
        });
        test_cx.simulate_keystrokes("escape");
        view.read_with(test_cx, |view, _| {
            assert_eq!(view.browser, BrowserMode::Closed);
            assert!(view.path.is_none());
        });
        view.update_in(test_cx, |view, window, cx| {
            view.open_file_finder(window, cx)
        });
        test_cx.run_until_parked();
        test_cx.simulate_keystrokes("down enter");
        test_cx.update(|window, cx| window.render_frame(cx));
        view.read_with(test_cx, |view, _| {
            assert_eq!(
                view.path.as_deref(),
                Some(
                    dir.path()
                        .join("file-001.rs")
                        .canonicalize()
                        .unwrap()
                        .as_path()
                )
            );
        });
    }

    #[gpui_kit::test]
    fn editor_layout_keeps_chrome_compact_and_code_fills_remaining_height(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello\nworld\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
                view.browser = BrowserMode::Tree;
                cx.notify();
            })
            .unwrap();
        test_cx.update(|window, cx| window.render_frame(cx));
        let toolbar = test_cx.debug_bounds("native-toolbar").unwrap();
        let tabs = test_cx.debug_bounds("native-tab-strip").unwrap();
        let sidebar = test_cx.debug_bounds("native-sidebar").unwrap();
        let code = test_cx.debug_bounds("native-code-pane").unwrap();
        assert_eq!(toolbar.size.height, px(44.));
        assert_eq!(tabs.size.height, px(43.));
        assert_eq!(tabs.top(), toolbar.top());
        assert_eq!(code.top(), toolbar.bottom());
        assert_eq!(toolbar.top(), sidebar.top());
        assert_eq!(code.bottom() + px(24.), sidebar.bottom());
        assert_eq!(code.left(), sidebar.right());
        assert!(
            code.size.height > px(300.),
            "code pane must fill the window"
        );
    }

    #[gpui_kit::test]
    fn disk_comparison_clears_when_switching_tabs(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&first, None, window, cx).unwrap();
                view.compare_with_disk(cx);
                assert!(view.disk_comparison.is_some());
                view.open(&second, None, window, cx).unwrap();
                assert!(view.disk_comparison.is_none());
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn dirty_tabs_keep_separate_buffers_and_close_requires_a_choice(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&first, None, window, cx).unwrap();
                view.editor
                    .update(cx, |editor, cx| editor.set_value("draft\n", window, cx));
                view.dirty = true;
                assert!(view.dirty);
                view.open(&second, None, window, cx).unwrap();
                view.editor.update(cx, |editor, cx| {
                    editor.set_value("other draft\n", window, cx)
                });
                view.dirty = true;
                view.open(&first, None, window, cx).unwrap();
                assert_eq!(view.editor.read(cx).value().as_ref(), "draft\n");
                assert!(view.dirty);
                view.close_tab(&first.canonicalize().unwrap(), window, cx);
                assert!(view.close_pending.is_some());
                view.confirm_close(CloseChoice::Cancel, window, cx);
                assert_eq!(
                    view.path.as_deref(),
                    Some(first.canonicalize().unwrap().as_path())
                );
                view.close_tab(&first.canonicalize().unwrap(), window, cx);
                view.confirm_close(CloseChoice::Discard, window, cx);
                assert_eq!(view.editor.read(cx).value().as_ref(), "other draft\n");
                assert_eq!(fs::read_to_string(&first).unwrap(), "first\n");
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn back_and_forward_restore_file_locations(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        fs::write(&first, "one\ntwo\n").unwrap();
        fs::write(&second, "a\nb\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&first, Some(2), window, cx).unwrap();
                view.open(&second, None, window, cx).unwrap();
                view.go_back(window, cx);
                assert_eq!(
                    view.path.as_deref(),
                    Some(first.canonicalize().unwrap().as_path())
                );
                assert_eq!(view.editor.read(cx).cursor_position().line, 1);
                view.go_forward(window, cx);
                assert_eq!(
                    view.path.as_deref(),
                    Some(second.canonicalize().unwrap().as_path())
                );
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn recovery_opens_a_draft_without_replacing_changed_disk(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let checkout = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let file = checkout.path().join("main.txt");
        fs::write(&file, "agent\n").unwrap();
        let journal_path = drafts::journal_path(data.path(), checkout.path());
        drafts::write(
            &journal_path,
            &Journal {
                root: checkout.path().to_owned(),
                drafts: vec![Draft {
                    path: file.clone(),
                    saved: "before\n".into(),
                    text: "human\n".into(),
                }],
            },
        )
        .unwrap();
        let (view, test_cx) =
            test_editor_with_data(cx, checkout.path(), Some(data.path().to_owned()));
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                assert_eq!(view.tabs.len(), 1);
                view.open(&file, None, window, cx).unwrap();
                assert_eq!(view.editor.read(cx).value().as_ref(), "human\n");
                assert!(view.conflict);
                assert_eq!(fs::read_to_string(&file).unwrap(), "agent\n");
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn crlf_content_survives_open_and_save(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("windows.txt");
        fs::write(&file, b"one\r\ntwo\r\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
                view.editor.update(cx, |editor, cx| {
                    editor.set_value("one\r\nchanged\r\n", window, cx)
                });
                view.dirty = true;
                view.save(window, cx);
                assert_eq!(fs::read(&file).unwrap(), b"one\r\nchanged\r\n");
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn typing_a_newline_keeps_crlf_style(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("windows.txt");
        fs::write(&file, b"one\r\ntwo\r\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
                view.editor.update(cx, |editor, cx| {
                    editor.set_cursor_position(Position::new(1, 3), window, cx)
                });
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("\nnew", cx));
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                assert!(view.dirty);
                view.save(window, cx);
            })
            .unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"one\r\ntwo\r\nnew\r\n");
    }

    /// Non-Rust files never start a session and expose no providers. Runs
    /// identically with or without rust-analyzer installed.
    #[gpui_kit::test]
    fn non_rust_open_leaves_language_features_off(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx)
            })
            .unwrap()
            .unwrap();
        view.downgrade()
            .update_in(test_cx, |view, _, cx| {
                assert!(view.lsp.is_none());
                let lsp = view.editor.read(cx).lsp();
                assert!(lsp.hover_provider.is_none());
                assert!(lsp.completion_provider.is_none());
                assert!(lsp.definition_provider.is_none());
            })
            .unwrap();
    }

    /// Opening a Rust file keeps session and providers consistent however
    /// discovery resolves: either both exist or neither does. Discovery
    /// itself is environment-dependent (covered by the live integration
    /// test); this pins the glue invariant.
    #[gpui_kit::test]
    fn rust_open_keeps_session_and_providers_consistent(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx)
            })
            .unwrap()
            .unwrap();
        view.downgrade()
            .update_in(test_cx, |view, _, cx| {
                let lsp = view.editor.read(cx).lsp();
                let providers = lsp.hover_provider.is_some()
                    || lsp.completion_provider.is_some()
                    || lsp.definition_provider.is_some();
                assert_eq!(
                    view.lsp.is_some(),
                    providers,
                    "session and providers must agree"
                );
                assert_eq!(view.editor.read(cx).value().as_ref(), "fn main() {}\n");
            })
            .unwrap();
    }

    /// A stub-backed session applies diagnostics and restores the jump
    /// origin, exercising the same wiring `restart_lsp` installs without
    /// the environment-dependent discovery step.
    #[gpui_kit::test]
    fn workspace_lsp_reuses_server_and_closes_only_closed_tabs(cx: &mut gpui_kit::TestAppContext) {
        use crate::editor::lsp::transport::{Transport, test_util::stub_server};
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.rs");
        let second = dir.path().join("second.rs");
        fs::write(&first, "fn first() {}\n").unwrap();
        fs::write(&second, "fn second() {}\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        let (reader, writer) = stub_server();
        let (transport, messages) = Transport::new(reader, writer);
        let (tx, _) = async_channel::unbounded();
        let client = Client::handshake(transport, dir.path(), messages, tx).unwrap();
        let first_uri = file_uri(&first.canonicalize().unwrap()).unwrap();
        let second_uri = file_uri(&second.canonicalize().unwrap()).unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&first, None, window, cx).unwrap();
                assert!(!view.lsp_trusted && !view.lsp_starting && view.workspace_lsp.is_none());
                view.lsp_trusted = true;
                view.workspace_lsp = Some(client.clone());
                view.open(&first, None, window, cx).unwrap();
                view.open(&second, None, window, cx).unwrap();
                assert!(Arc::ptr_eq(&view.lsp.as_ref().unwrap().client, &client));
                assert!(client.doc_version(first_uri.as_str()) > 0);
                let version = client.doc_version(first_uri.as_str());
                view.open(&first, None, window, cx).unwrap();
                assert_eq!(client.doc_version(first_uri.as_str()), version);
                view.finish_close(&second.canonicalize().unwrap(), window, cx);
                assert_eq!(client.doc_version(second_uri.as_str()), 0);
                assert!(client.doc_version(first_uri.as_str()) > 0);
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn same_file_definition_preserves_back_history(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        fs::write(&file, "fn one() {}\nfn two() {}\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, Some(1), window, cx).unwrap();
                view.pending_open = Some((file.clone(), None));
                view.pending_lsp_position = Some(lsp_types::Position::new(1, 3));
                cx.notify();
            })
            .unwrap();
        test_cx.update(|window, cx| window.render_frame(cx));
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                assert_eq!(view.editor.read(cx).cursor_position(), Position::new(1, 3));
                view.go_back(window, cx);
                assert_eq!(view.editor.read(cx).cursor_position(), Position::new(0, 0));
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn stub_session_applies_diagnostics_and_goes_back(cx: &mut gpui_kit::TestAppContext) {
        use crate::editor::lsp::transport::{Transport, test_util::stub_server};

        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());

        let (reader, writer) = stub_server();
        let (transport, messages) = Transport::new(reader, writer);
        let (diagnostics_tx, _diagnostics_rx) = async_channel::unbounded();
        let client = Client::handshake(transport, dir.path(), messages, diagnostics_tx).unwrap();
        // `open` canonicalizes (e.g. /var → /private/var on macOS), and the
        // session keeps only the canonical URI: build the stub session from
        // the same path the editor will compute, or the URIs never match.
        let canonical = file.canonicalize().unwrap();
        let uri = file_uri(&canonical).unwrap();
        client.did_open(&uri, "rust", "fn main() {}\n").unwrap();

        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.lsp_trusted = true;
                view.workspace_lsp = Some(client.clone());
                view.lsp = Some(LspSession {
                    client: Arc::clone(&client),
                    uri: uri.to_string(),
                });
                let providers = LspProviders::new(Arc::clone(&client), uri.clone());
                view.editor.update(cx, |editor, _| {
                    let lsp = editor.lsp_mut();
                    lsp.hover_provider = Some(providers.clone());
                    lsp.definition_provider = Some(providers.clone());
                });
                view.open(&file, None, window, cx).unwrap();
                // The same document keeps its session instead of respawning.
                assert!(view.lsp.is_some());
                // Diagnostics for the current version apply into the editor
                // set. (The open's own buffer fill already pushed version 2,
                // so the version-1 event is stale by design — that is the
                // next assertion's probe.)
                view.apply_diagnostics(
                    &DiagnosticEvent {
                        uri: uri.to_string(),
                        version: Some(client.doc_version(uri.as_str())),
                        diagnostics: vec![lsp_types::Diagnostic {
                            range: lsp_types::Range {
                                start: lsp_types::Position::new(0, 0),
                                end: lsp_types::Position::new(0, 2),
                            },
                            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                            message: "stub diagnostic".to_owned(),
                            ..Default::default()
                        }],
                    },
                    cx,
                );
                let count = view.editor.update(cx, |editor, _| {
                    editor.diagnostics_mut().map(|set| set.len())
                });
                // Stale diagnostics (older than the current version) are ignored.
                view.apply_diagnostics(
                    &DiagnosticEvent {
                        uri: uri.to_string(),
                        version: Some(0),
                        diagnostics: vec![],
                    },
                    cx,
                );
                assert!(count.is_some_and(|count| count > 0));
                // A recorded jump origin restores the cursor.
                view.jump_back
                    .borrow_mut()
                    .push(gpui_kit::component::input::Position::new(0, 0));
                view.editor.update(cx, |editor, cx| {
                    editor.set_cursor_position(
                        gpui_kit::component::input::Position::new(0, 11),
                        window,
                        cx,
                    );
                });
                view.go_back(window, cx);
                let cursor = view.editor.read(cx).cursor_position();
                assert_eq!(cursor, gpui_kit::component::input::Position::new(0, 0));
                assert!(!view.has_jump_history());
            })
            .unwrap();
    }

    /// Switching files drops jump origins recorded in the previous
    /// document; they would restore meaningless offsets otherwise.
    #[gpui_kit::test]
    fn file_switch_adds_history_without_moving_local_origins(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        std::fs::write(&first, "aaa\n").unwrap();
        std::fs::write(&second, "bbb\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&first, None, window, cx).unwrap();
                view.jump_back
                    .borrow_mut()
                    .push(gpui_kit::component::input::Position::new(0, 1));
                view.open(&second, None, window, cx).unwrap();
                assert!(view.jump_back.borrow().is_empty());
                assert!(view.has_jump_history());
            })
            .unwrap();
    }

    /// A clean buffer follows the disk; a dirty buffer keeps its draft and
    /// reports the conflict until the user explicitly reloads.
    #[gpui_kit::test]
    fn external_change_reloads_clean_buffer_and_conflicts_with_dirty_buffer(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();

        std::fs::write(&file, "agent edit\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert_eq!(view.editor.read(cx).value().as_ref(), "agent edit\n");
                assert!(!view.dirty);
                assert!(!view.conflict);
                assert!(
                    view.disk_notice
                        .as_deref()
                        .is_some_and(|notice| notice.contains("Reloaded")),
                    "expected a reload notice, got {:?}",
                    view.disk_notice
                );
            })
            .unwrap();

        // Type into the loaded document: the buffer becomes dirty through
        // the same change event as real typing. Rendering requires a window
        // step outside the view update.
        test_cx.update(|window, cx| window.input("draft ", cx));
        view.read_with(test_cx, |view, _| assert!(view.dirty));

        std::fs::write(&file, "agent edit 2\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.dirty);
                assert!(view.conflict);
                assert!(
                    view.editor.read(cx).value().contains("agent edit"),
                    "the buffer must keep the human draft"
                );

                // The explicit action drops the draft for the disk version.
                view.reload_from_disk(window, cx);
                assert_eq!(view.editor.read(cx).value().as_ref(), "agent edit 2\n");
                assert!(!view.dirty);
                assert!(!view.conflict);
            })
            .unwrap();
    }

    /// When the external writer produces exactly the open draft, the draft
    /// is already on disk and the editor can mark it saved.
    #[gpui_kit::test]
    fn external_write_matching_the_draft_is_adopted(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        view.read_with(test_cx, |view, _| assert!(view.dirty));
        let buffer = view.read_with(test_cx, |view, cx| view.editor.read(cx).value().to_string());
        std::fs::write(&file, &buffer).unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(!view.dirty);
                assert!(!view.conflict);
                assert_eq!(view.saved.as_deref(), Some(buffer.as_str()));
            })
            .unwrap();
    }

    /// A conflict clears itself when the disk returns to the saved content,
    /// without touching the unsaved draft.
    #[gpui_kit::test]
    fn conflict_clears_when_disk_reverts_to_saved_content(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        view.read_with(test_cx, |view, _| assert!(view.dirty));

        std::fs::write(&file, "agent\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.conflict);

                std::fs::write(&file, "before\n").unwrap();
                view.check_disk(window, cx);
                assert!(!view.conflict);
                assert!(view.dirty, "the human draft is untouched");
                assert!(view.editor.read(cx).value().contains("draft"));
            })
            .unwrap();
    }

    /// A failed save caused by a concurrent write surfaces the conflict
    /// banner itself, without waiting for a poll tick.
    #[gpui_kit::test]
    fn save_conflict_is_detected_without_a_poll(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        std::fs::write(&file, "agent\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                // No check_disk first: the failed save itself must detect.
                view.save(window, cx);
                assert!(view.error.is_some());
                assert!(view.conflict);
                assert!(view.dirty);
                assert!(view.editor.read(cx).value().contains("draft"));
            })
            .unwrap();
    }

    /// Discard means "return to the disk state": after a conflict it must
    /// pick up the external write, not restore the stale snapshot.
    #[gpui_kit::test]
    fn discard_returns_to_disk_after_conflict(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        std::fs::write(&file, "agent\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.conflict);

                view.discard(window, cx);
                assert_eq!(view.editor.read(cx).value().as_ref(), "agent\n");
                assert!(!view.dirty);
                assert!(!view.conflict);
            })
            .unwrap();
    }

    /// An edit whose result matches the disk bytes is adopted even when that
    /// disk content was already reported as a conflict.
    #[gpui_kit::test]
    fn editing_the_buffer_to_match_disk_is_adopted(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        std::fs::write(&file, "agent\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.conflict);
                // Model a user edit whose result equals the disk bytes.
                view.editor
                    .update(cx, |editor, cx| editor.set_value("agent\n", window, cx));
                view.dirty = true;

                view.check_disk(window, cx);
                assert!(!view.dirty);
                assert!(!view.conflict);
                assert_eq!(view.saved.as_deref(), Some("agent\n"));
            })
            .unwrap();
    }

    /// Reloading when the buffer already shows the disk bytes must still
    /// reconcile the saved/dirty bookkeeping (no `set_value` needed).
    #[gpui_kit::test]
    fn reload_when_buffer_matches_disk_reconciles_bookkeeping(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        std::fs::write(&file, "agent\n").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.conflict);
                // The buffer is edited to the disk bytes, but the change
                // handler's bookkeeping has not run yet.
                view.editor
                    .update(cx, |editor, cx| editor.set_value("agent\n", window, cx));
                view.dirty = true;

                view.reload_from_disk(window, cx);
                assert!(!view.dirty);
                assert!(!view.conflict);
                assert_eq!(view.saved.as_deref(), Some("agent\n"));
            })
            .unwrap();
    }

    /// An unreadable external change must not leave a conflict banner behind
    /// once the buffer has no unsaved changes of its own.
    #[gpui_kit::test]
    fn unreadable_change_clears_conflict_after_discard(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        std::fs::write(&file, b"binary\0bytes").unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.conflict);

                // Discard returns to the saved buffer; the binary file still
                // cannot be loaded, but there is no unsaved draft anymore.
                view.discard(window, cx);
                assert!(!view.dirty);
                assert!(!view.conflict);
                assert!(
                    view.disk_notice
                        .as_deref()
                        .is_some_and(|notice| notice.contains("cannot be reloaded")),
                    "expected an unreadable notice, got {:?}",
                    view.disk_notice
                );
            })
            .unwrap();
    }

    /// Deleting the open file preserves the draft and leaves saving blocked;
    /// nothing is silently recreated.
    #[gpui_kit::test]
    fn deleted_file_keeps_the_draft_and_save_fails(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        test_cx.update(|window, cx| window.input("draft ", cx));
        view.read_with(test_cx, |view, _| assert!(view.dirty));

        std::fs::remove_file(&file).unwrap();
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.check_disk(window, cx);
                assert!(view.dirty);
                assert!(view.editor.read(cx).value().contains("draft"));
                assert!(
                    view.disk_notice
                        .as_deref()
                        .is_some_and(|notice| notice.contains("deleted")),
                    "expected a deletion notice, got {:?}",
                    view.disk_notice
                );

                view.save(window, cx);
                assert!(view.error.is_some());
                assert!(view.dirty);
                assert!(view.editor.read(cx).value().contains("draft"));
            })
            .unwrap();
    }

    /// The background poll performs the same check without manual calls.
    #[gpui_kit::test]
    fn disk_poll_reloads_clean_buffer(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let (view, test_cx) = test_editor(cx, dir.path());
        view.downgrade()
            .update_in(test_cx, |view, window, cx| {
                view.open(&file, None, window, cx).unwrap();
            })
            .unwrap();
        std::fs::write(&file, "agent\n").unwrap();
        test_cx
            .background_executor
            .advance_clock(DISK_POLL_INTERVAL * 2);
        test_cx.run_until_parked();
        view.read_with(test_cx, |view, cx| {
            assert_eq!(view.editor.read(cx).value().as_ref(), "agent\n");
            assert!(!view.dirty);
        });
    }
}
