//! The tool dialog: persisted inputs, a Format action that rewrites them in
//! place, and the actions around that.
//!
//! [`ToolView`] is created once per tool by the workspace and kept alive
//! across dialog openings, so reopening a tool shows exactly what was left
//! there. Inputs autosave on a debounce and flush immediately when the
//! dialog closes or Format runs; a failed read blocks saving until the user
//! clears the input, so a transient read failure can never overwrite content
//! this session never saw.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px, relative, rgb,
};

use super::{ToolInput, ToolKind, ToolStore};
use crate::data::DataRoot;

/// Smallest useful editing surface. The editor grows to fill the dialog, and
/// the body scrolls instead of squeezing it below this.
const EDITOR_MIN_HEIGHT: f32 = 200.;
/// How long after the last edit a save runs: long enough that typing a
/// document costs one write, short enough that a crash loses almost nothing.
/// Closing the dialog flushes immediately regardless.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

const ERROR_COLOR: u32 = 0xf87171;

pub(crate) struct ToolView {
    tool: ToolKind,
    store: Option<ToolStore>,
    inputs: Vec<InputSlot>,
    /// First failed read of a persisted input. While set, saving is blocked.
    load_error: Option<String>,
    /// Failed write, shown with the actions until the next successful one.
    save_error: Option<String>,
    /// Formatting failure for the current input, shown with the actions.
    error: Option<String>,
    /// Last text written to disk per input, so saves skip unchanged content.
    saved: Vec<String>,
    /// Bumped on every input change and on every flush; a debounced save
    /// only writes when its snapshot is still current, so a burst of edits
    /// costs one write.
    save_generation: u64,
    /// One-shot: focus the first input on the next render. The dialog layer
    /// takes focus when it opens, so focusing at construction would be lost.
    focus_on_render: bool,
    focus_handle: FocusHandle,
}

struct InputSlot {
    slot: ToolInput,
    editor: Entity<EditorState>,
    /// Kept alive so the change subscription outlives construction.
    _subscription: Subscription,
}

impl ToolView {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        tool: ToolKind,
        root: Option<DataRoot>,
    ) -> Self {
        let store = root.map(|root| ToolStore::new(&root));
        let mut inputs = Vec::new();
        let mut saved = Vec::new();
        let mut load_error = None;
        for slot in tool.inputs() {
            let text = match store.as_ref().map(|store| store.load(tool, slot)) {
                Some(Ok(text)) => text,
                Some(Err(error)) => {
                    load_error.get_or_insert_with(|| {
                        format!(
                            "Could not read the saved input, so it will not be overwritten: \
                             {error:#}. Use Clear to start over."
                        )
                    });
                    String::new()
                }
                None => String::new(),
            };
            let editor = cx.new(|cx| {
                let mut editor = editor_state(window, cx, tool, Some(slot.placeholder));
                if !text.is_empty() {
                    editor.set_value(text.clone(), window, cx);
                }
                editor
            });
            let subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_save(cx);
                }
            });
            inputs.push(InputSlot {
                slot: *slot,
                editor,
                _subscription: subscription,
            });
            saved.push(text);
        }
        Self {
            tool,
            store,
            inputs,
            load_error,
            save_error: None,
            error: None,
            saved,
            save_generation: 0,
            focus_on_render: true,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Open `view` as a modal dialog. Esc, backdrop click, and the close
    /// button all dismiss through the dialog layer, and the close handler
    /// flushes pending edits to disk, so dismissing never loses the last
    /// keystrokes. Opening always re-arms the input focus: the dialog layer
    /// takes focus when it opens, so it has to be reclaimed on the first
    /// render.
    pub(crate) fn open_dialog(view: Entity<Self>, window: &mut Window, cx: &mut App) {
        view.update(cx, |view, cx| view.prepare_for_open(cx));
        let tool = view.read(cx).tool;
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(tool.label())
                // Large on purpose: the dialog layer clamps these to the
                // window, so the tool takes the biggest surface the screen
                // offers and the editor below fills it.
                .w(px(1760.))
                .h(px(1400.))
                .on_close({
                    let view = view.clone();
                    move |_, window, cx| {
                        view.update(cx, |view, cx| view.flush_save(window, cx));
                    }
                })
                .child(view.clone().into_any_element())
        });
    }

    /// Arm the one-shot input focus for the next render.
    fn prepare_for_open(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    /// Persist immediately, superseding any pending debounced write. Called
    /// when the dialog closes and when Format runs, so the last edits always
    /// land on disk.
    pub(crate) fn flush_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_generation = self.save_generation.wrapping_add(1);
        match self.save_now(cx) {
            Some(error) => {
                window.push_notification(format!("Could not save tool input: {error}"), cx);
            }
            None => self.save_error = None,
        }
    }

    /// Format in place: the tool's result replaces the first input, which is
    /// the tool's document. The replacement keeps the editor's undo history,
    /// so formatting by accident is one undo away, and it is persisted right
    /// away — an already-formatted input stays formatted next time. A failed
    /// parse leaves the text untouched and shows why.
    fn apply_format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs: Vec<String> = self.texts(cx);
        // An empty document is not a formatting event: leave it, and its
        // confirmation, alone.
        if inputs.first().is_none_or(|input| input.trim().is_empty()) {
            return;
        }
        match self.tool.compute(&inputs) {
            Ok(formatted) => {
                self.error = None;
                if let Some(input) = self.inputs.first() {
                    input
                        .editor
                        .update(cx, |editor, cx| editor.replace_all(formatted, window, cx));
                }
                // Formatting an already-laid-out document changes nothing on
                // screen, so the action still reports that it ran.
                window.push_notification("Formatted", cx);
                self.flush_save(window, cx);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn copy_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.inputs.first() else {
            return;
        };
        let text = input.editor.read(cx).value().to_string();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification("Copied to clipboard", cx);
    }

    /// Clear every input and persist the clear. An explicit clear is a
    /// deliberate replace, so it also lifts a failed read's save block: the
    /// content that read could not show is being discarded anyway.
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_error = None;
        self.error = None;
        for input in &self.inputs {
            input
                .editor
                .update(cx, |editor, cx| editor.set_value("", window, cx));
        }
        self.flush_save(window, cx);
        cx.notify();
    }

    fn texts(&self, cx: &App) -> Vec<String> {
        self.inputs
            .iter()
            .map(|input| input.editor.read(cx).value().to_string())
            .collect()
    }

    /// Save every input whose text changed since the last write, returning
    /// the first failure. Blocked while a load error is unresolved, so a
    /// transient read failure cannot overwrite content never shown.
    fn save_now(&mut self, cx: &mut Context<Self>) -> Option<String> {
        if self.load_error.is_some() {
            return None;
        }
        let store = self.store.clone()?;
        let texts = self.texts(cx);
        let mut failure = None;
        for (index, text) in texts.into_iter().enumerate() {
            if self.saved.get(index).is_some_and(|saved| *saved == text) {
                continue;
            }
            let slot = self.inputs[index].slot;
            match store.save(self.tool, &slot, &text) {
                Ok(()) => {
                    if let Some(saved) = self.saved.get_mut(index) {
                        *saved = text;
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(format!("{error:#}"));
                    }
                }
            }
        }
        failure
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_generation = self.save_generation.wrapping_add(1);
        let generation = self.save_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                if this.save_generation == generation {
                    this.save_error = this.save_now(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

/// A JSON-highlighted editor for tools that declare a language, plain
/// otherwise. Every input shares the construction so their styling cannot
/// drift.
fn editor_state(
    window: &mut Window,
    cx: &mut Context<EditorState>,
    tool: ToolKind,
    placeholder: Option<&'static str>,
) -> EditorState {
    let mut state = EditorState::new(window, cx);
    if let Some(language) = tool.editor_language() {
        state = state.language(language);
    }
    if let Some(placeholder) = placeholder {
        state = state.placeholder(placeholder);
    }
    state
}

impl Focusable for ToolView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ToolView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_on_render {
            self.focus_on_render = false;
            if let Some(input) = self.inputs.first() {
                input.editor.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        let copy_disabled = self
            .inputs
            .first()
            .is_none_or(|input| input.editor.read(cx).value().is_empty());
        let mut body = v_flex().size_full().gap_2();
        if let Some(error) = self.load_error.clone() {
            body = body.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error));
        }
        for input in &self.inputs {
            // The editor takes every pixel the dialog has left, so a bigger
            // dialog is a bigger editing surface and nothing is wasted below
            // the actions.
            body = body.child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(EDITOR_MIN_HEIGHT))
                    .child(Editor::new(&input.editor).h(relative(1.))),
            );
        }
        // The status message holds the free space so the actions stay right,
        // matching the action rows of the other dialogs.
        let actions = h_flex()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .gap_2()
                    .items_center()
                    .when_some(self.error.clone(), |row, error| {
                        row.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error))
                    })
                    .when_some(self.save_error.clone(), |row, error| {
                        row.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error))
                    }),
            )
            .child(
                Button::new("tool-clear")
                    .label("Clear")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
            )
            .child(
                Button::new("tool-copy")
                    .label("Copy")
                    .ghost()
                    .small()
                    .disabled(copy_disabled)
                    .on_click(cx.listener(|this, _, window, cx| this.copy_input(window, cx))),
            )
            .child(
                Button::new("tool-format")
                    .label("Format")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.apply_format(window, cx))),
            );
        body.child(actions)
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
