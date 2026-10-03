//! A deliberately small checkout editor: one UTF-8 file, explicit save, and
//! protection against overwriting a file changed by an agent on disk.

use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::DefaultHasher},
    fs,
    hash::{Hash as _, Hasher as _},
    io::{self, Write as _},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use super::lsp::{Client, DiagnosticEvent, LspProviders, discover_rust_analyzer, file_uri};
use crate::editor::{ExternalEditor, ExternalEditorKind};
use anyhow::{Context as _, Result, bail};
use gpui_kit::component::{
    Disableable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent, Position},
    menu::{DropdownMenu as _, PopupMenuItem},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement,
    PathPromptOptions, Render, Styled, Window, div, px, rgb,
};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// How often the open file is compared with its on-disk contents. A poll
/// keeps concurrent agent edits visible without a platform file-watcher
/// dependency; reads are bounded by `MAX_FILE_BYTES` and page-cached.
const DISK_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct NativeEditor {
    root: PathBuf,
    editor: Entity<EditorState>,
    path: Option<PathBuf>,
    saved: Option<String>,
    dirty: bool,
    error: Option<String>,
    pending_open: Option<(PathBuf, Option<usize>)>,
    executables: HashMap<String, String>,
    lsp: Option<LspSession>,
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

/// One live language session: exactly the document currently open, at the
/// URI the server knows. Replaced on every successful open; dropped (and
/// the server killed) when the editor moves to another file.
struct LspSession {
    client: Arc<Client>,
    uri: String,
}

impl NativeEditor {
    pub(crate) fn new(
        root: &Path,
        executables: HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| EditorState::new(window, cx));
        cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dirty = this
                    .saved
                    .as_ref()
                    .is_some_and(|saved| editor.read(cx).value().as_ref() != saved.as_str());
                this.push_text_to_server(cx);
                cx.notify();
            }
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
                let Some(change) = change else {
                    continue;
                };
                let applied = cx.update(|window, cx| {
                    view.update(cx, |this, cx| this.apply_disk_change(change, window, cx))
                });
                if !matches!(applied, Ok(Ok(()))) {
                    break;
                }
            }
        })
        .detach();
        Self {
            root: root.to_owned(),
            editor,
            path: None,
            saved: None,
            dirty: false,
            error: None,
            pending_open: None,
            executables,
            lsp: None,
            jump_back: Rc::new(RefCell::new(Vec::new())),
            disk_seen: None,
            conflict: false,
            disk_notice: None,
        }
    }

    pub(crate) fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub(crate) fn has_jump_history(&self) -> bool {
        !self.jump_back.borrow().is_empty()
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
        let canonical = path
            .canonicalize()
            .with_context(|| format!("Could not open {}", path.display()))?;
        if !canonical.starts_with(&canonical_root) {
            bail!("Choose a file inside this checkout.");
        }
        if self.dirty {
            if self.path.as_deref() == Some(canonical.as_path()) {
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
                return Ok(());
            }
            bail!("Save or discard the current changes before opening another file.");
        }
        let contents = read_text_file(&canonical)?;
        let language = language_for(&canonical);
        let old_text = self.saved.clone().unwrap_or_default();
        let old_language = self
            .path
            .as_ref()
            .map(|path| language_for(path))
            .unwrap_or("");
        self.editor.update(cx, |editor, cx| {
            editor.set_highlighter(language, cx);
            editor.set_value(contents.clone(), window, cx);
            if let Some(line) = line.filter(|line| *line > 0) {
                editor.set_cursor_position(
                    Position::new((line - 1).min(u32::MAX as usize) as u32, 0),
                    window,
                    cx,
                );
            }
            editor.focus(window, cx);
        });
        if self.editor.read(cx).value().as_ref() != contents {
            self.editor.update(cx, |editor, cx| {
                editor.set_highlighter(old_language, cx);
                editor.set_value(old_text, window, cx);
            });
            bail!(
                "This file's text format cannot be preserved by the built-in editor. Open it in Zed or VS Code."
            );
        }
        if self.path.as_deref() != Some(canonical.as_path()) {
            // A different file: origins recorded in the previous document
            // would jump to meaningless offsets here.
            self.jump_back.borrow_mut().clear();
        }
        self.path = Some(canonical.clone());
        self.saved = Some(contents.clone());
        self.dirty = false;
        self.conflict = false;
        self.disk_notice = None;
        self.disk_seen = None;
        self.restart_lsp(&canonical, language, window, cx);
        Ok(())
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            self.error =
                Some("Save or discard the current changes before opening another file.".into());
            cx.notify();
            return;
        }
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open a file in this checkout".into()),
        });
        cx.spawn_in(window, async move |view, cx| match picker.await {
            Ok(Ok(Some(mut paths))) => {
                if let Some(path) = paths.pop() {
                    let _ = cx.update(|window, cx| {
                        view.update(cx, |this, cx| {
                            this.error = this
                                .open(&path, None, window, cx)
                                .err()
                                .map(|e| format!("{e:#}"));
                            cx.notify();
                        })
                        .ok();
                    });
                }
            }
            Ok(Ok(None)) | Err(_) => {}
            Ok(Err(error)) => {
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.error = Some(format!("Could not open file picker: {error:#}"));
                        cx.notify();
                    })
                    .ok();
                });
            }
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(saved) = self.saved.clone() else {
            return;
        };
        let contents = self.editor.read(cx).value().to_string();
        match save_if_unchanged(&path, saved.as_bytes(), contents.as_bytes()) {
            Ok(()) => {
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
        if bytes == self.editor.read(cx).value().as_bytes() {
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

    pub(crate) fn set_executables(
        &mut self,
        executables: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.executables = executables;
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
        let _ = session.client.did_change(&text);
    }

    /// Restart the language session for a newly opened file. Any failure
    /// (no server binary, failed handshake) leaves `lsp` empty and the file
    /// perfectly editable without language features.
    fn restart_lsp(
        &mut self,
        path: &Path,
        language: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The same live document keeps its session: edits already flowed to
        // the server through Change events, so reopening must not respawn it
        // (every Review line-click would otherwise restart rust-analyzer).
        // A dead server is the exception: reopening revives it.
        let Some(uri) = file_uri(path).ok() else {
            return;
        };
        if self
            .lsp
            .as_ref()
            .is_some_and(|session| session.uri == uri.as_str() && session.client.is_alive())
        {
            return;
        }
        self.lsp.take();
        self.clear_lsp_providers(cx);
        if language != "rust" {
            return;
        }
        let Some(program) = discover_rust_analyzer(None) else {
            return;
        };
        let (diagnostics_tx, diagnostics_rx) = async_channel::unbounded();
        let client = match Client::start(&program, &self.root, diagnostics_tx) {
            Ok(client) => client,
            Err(_) => return,
        };
        if client.position_encoding() != "utf-16" {
            // The offset math assumes UTF-16 code units; never wire
            // providers against a server speaking anything else.
            return;
        }
        if client
            .did_open(&uri, "rust", self.editor.read(cx).value().as_ref())
            .is_err()
        {
            return;
        }
        let providers = LspProviders::new(Arc::clone(&client), uri.clone());
        let jumps = Rc::clone(&self.jump_back);
        let editor_handle = self.editor.downgrade();
        self.editor.update(cx, |editor, _| {
            let lsp = editor.lsp_mut();
            lsp.hover_provider = Some(providers.clone());
            lsp.completion_provider = Some(providers.clone());
            lsp.definition_provider = Some(providers.clone());
            lsp.show_document = Some(Rc::new(
                move |params: &lsp_types::ShowDocumentParams,
                      _window: &mut Window,
                      cx: &mut App| {
                    if params.external == Some(true) {
                        return false;
                    }
                    if let Ok(origin) =
                        editor_handle.update(cx, |editor, _| editor.cursor_position())
                    {
                        let mut jumps = jumps.borrow_mut();
                        if jumps.len() >= 100 {
                            jumps.remove(0);
                        }
                        jumps.push(origin);
                    }
                    false
                },
            ));
        });
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = diagnostics_rx.recv().await {
                let _ = view.update(cx, |this, cx| this.apply_diagnostics(&event, cx));
            }
        })
        .detach();
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
        let Some(session) = self.lsp.as_ref() else {
            return;
        };
        if session.uri != event.uri {
            return;
        }
        if event
            .version
            .is_some_and(|version| version < session.client.doc_version())
        {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            let text = editor.text().clone();
            if let Some(set) = editor.diagnostics_mut() {
                set.reset(&text);
                set.extend(event.diagnostics.iter().cloned());
                cx.notify();
            }
        });
    }

    /// Return to the origin of the last follow-definition jump, if any.
    pub(crate) fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.jump_back.borrow_mut().pop() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_cursor_position(position, window, cx);
            editor.focus(window, cx);
        });
        cx.notify();
    }

    fn open_external(&mut self, kind: ExternalEditorKind, cx: &mut Context<Self>) {
        let executable = self.executables.get(kind.id()).cloned();
        match ExternalEditor::new(kind, executable).launch(&self.root, None, None) {
            Ok(()) => self.error = None,
            Err(error) => {
                self.error = Some(format!(
                    "Could not launch {}: {error:#}. Configure its launcher in Settings → General.",
                    kind.label()
                ));
            }
        }
        cx.notify();
    }
}

fn language_for(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
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

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn save_if_unchanged(path: &Path, original: &[u8], replacement: &[u8]) -> Result<()> {
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
    Ok(())
}

impl Focusable for NativeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor_focus(cx)
    }
}

impl Render for NativeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((path, line)) = self.pending_open.take() {
            self.error = self
                .open(&path, line, window, cx)
                .err()
                .map(|e| format!("{e:#}"));
        }
        let label = self
            .path
            .as_ref()
            .and_then(|path| path.strip_prefix(&self.root).ok())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No file open".into());
        let view = cx.entity().downgrade();
        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("native-open-file")
                            .label("Open file…")
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                    )
                    .child(
                        Button::new("native-save-file")
                            .label("Save")
                            .primary()
                            .disabled(!self.dirty)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    )
                    .when(self.dirty, |row| {
                        row.child(
                            Button::new("native-discard-file")
                                .label("Discard changes")
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.discard(window, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("native-open-in")
                            .accessibility_label("Open in")
                            .outline()
                            .dropdown_caret(true)
                            .label("Open in")
                            .dropdown_menu(move |mut menu, _, _| {
                                for kind in ExternalEditorKind::ALL {
                                    let option_view = view.clone();
                                    menu = menu.item(PopupMenuItem::new(kind.label()).on_click(
                                        move |_, _, cx| {
                                            option_view
                                                .update(cx, |this, cx| this.open_external(kind, cx))
                                                .ok();
                                        },
                                    ));
                                }
                                menu
                            }),
                    )
                    .when(!self.jump_back.borrow().is_empty(), |row| {
                        row.child(
                            Button::new("native-go-back")
                                .accessibility_label("Go back to definition origin")
                                .label("Back")
                                .ghost()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.go_back(window, cx)),
                                ),
                        )
                    })
                    .child(div().text_sm().text_color(rgb(0x858989)).child(label))
                    .when(self.dirty, |row| {
                        row.child(div().text_sm().text_color(rgb(0xfbbf24)).child("Unsaved"))
                    }),
            )
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
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(160.))
                    .child(Editor::new(&self.editor).h_full()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

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

    /// Test host: a real window plus a bare `NativeEditor`, without entering
    /// any repository workspace.
    fn test_editor<'a>(
        cx: &'a mut gpui_kit::TestAppContext,
        dir: &std::path::Path,
    ) -> (Entity<NativeEditor>, &'a mut gpui_kit::VisualTestContext) {
        use std::{cell::RefCell, rc::Rc};
        let holder: Rc<RefCell<Option<Entity<NativeEditor>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let root = dir.to_owned();
        let (_, test_cx) = cx.add_window_view(move |window, cx| {
            let editor = cx.new(|cx| NativeEditor::new(&root, HashMap::new(), window, cx));
            *holder_for_window.borrow_mut() = Some(editor.clone());
            gpui_kit::component::Root::new(editor, window, cx)
        });
        (holder.borrow().clone().unwrap(), test_cx)
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
                        version: Some(2),
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
                        version: Some(1),
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
    fn jump_history_clears_on_file_switch(cx: &mut gpui_kit::TestAppContext) {
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
                assert!(!view.has_jump_history());
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
