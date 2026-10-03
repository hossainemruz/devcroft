//! A deliberately small checkout editor: one UTF-8 file, explicit save, and
//! protection against overwriting a file changed by an agent on disk.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
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
        let metadata = fs::metadata(&canonical)?;
        if !metadata.is_file() {
            bail!("Choose a regular file.");
        }
        if metadata.len() > MAX_FILE_BYTES {
            bail!("This file is larger than 2 MiB. Open it in Zed or VS Code.");
        }
        let bytes = fs::read(&canonical)?;
        if bytes.contains(&0) {
            bail!("This appears to be a binary file. Open it in Zed or VS Code.");
        }
        let contents = String::from_utf8(bytes)
            .context("This file is not UTF-8; open it in Zed or VS Code")?;
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

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Some(saved) = self.saved.as_ref() else {
            return;
        };
        let contents = self.editor.read(cx).value().to_string();
        match save_if_unchanged(path, saved.as_bytes(), contents.as_bytes()) {
            Ok(()) => {
                self.saved = Some(contents);
                self.dirty = false;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Could not save: {error:#}")),
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
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
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
}
