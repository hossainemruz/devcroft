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
        self.restart_lsp(&canonical, language, &contents, window, cx);
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
        contents: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lsp.take();
        self.clear_lsp_providers(cx);
        if language != "rust" {
            return;
        }
        let Some(program) = discover_rust_analyzer(None) else {
            return;
        };
        let uri = match file_uri(path) {
            Ok(uri) => uri,
            Err(_) => return,
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
        if client.did_open(&uri, "rust", contents).is_err() {
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
}
