//! A deliberately small checkout editor: one UTF-8 file, explicit save, and
//! protection against overwriting a file changed by an agent on disk.

use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

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
        }
    }

    pub(crate) fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
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
        self.path = Some(canonical);
        self.saved = Some(contents);
        self.dirty = false;
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
