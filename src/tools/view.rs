//! The tool dialog: persisted inputs, a primary action that derives the
//! tool's result, and the actions around it.
//!
//! [`ToolView`] is created once per tool by the workspace and kept alive
//! across dialog openings, so reopening a tool shows exactly what was left
//! there. Inputs autosave on a debounce and flush immediately when the
//! dialog closes or the primary action runs; a failed read blocks saving
//! until the user clears the input, so a transient read failure can never
//! overwrite content this session never saw.
//!
//! How the result is presented comes from [`ToolKind::result_kind`]: an
//! in-place tool has one editor and rewrites its text, while a diff tool has
//! two modes the primary button toggles between — `Edit` (the old and new
//! paste editors, side by side) and `Diff` (the comparison, old against new
//! side by side). Each mode owns the whole dialog, and one global language
//! picker colors both surfaces: the comparison with the Review tab's syntaxes,
//! the paste editors with the editor's own grammars.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::searchable_list::{SearchableListDelegate as _, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement, Render, Styled,
    StyledText, Subscription, TestSupportExt as _, Window, div, list, px, relative, rgb,
};

use super::diff::{self, DiffResult, LineRef, SplitRow};
use super::{ToolInput, ToolKind, ToolOutput, ToolResultKind, ToolStore};
use crate::data::DataRoot;
use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::metrics::review_font_size;
use crate::review::model::LineTag;
use crate::review::stream;
use crate::review::syntax::{SyntaxHighlights, styled_highlights};

/// The language picker's state: a searchable list of language names.
type LanguageSelect = SelectState<SearchableVec<&'static str>>;

/// Storage key of the highlight-language setting under `tools/<tool-id>/`.
const LANGUAGE_SETTING: &str = "language";

/// Smallest useful editing surface. The editor grows to fill the dialog, and
/// the body scrolls instead of squeezing it below this.
const EDITOR_MIN_HEIGHT: f32 = 200.;
/// How long after the last edit a save runs: long enough that typing a
/// document costs one write, short enough that a crash loses almost nothing.
/// Closing the dialog flushes immediately regardless.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

const ERROR_COLOR: u32 = 0xf87171;
/// Secondary text: editor labels, the language picker's label, and the
/// comparison's header, matching the Review tab's diff surface.
const MUTED_COLOR: u32 = 0x858989;
/// Line numbers, dimmer than the surrounding chrome, as in the review rows.
const NUMBER_COLOR: u32 = 0x555a5a;
const PANE_BORDER: u32 = 0x292b2b;
const PANE_BG: u32 = 0x090a0a;
const PANE_HEADER_BG: u32 = 0x151a1a;
/// A cell whose side has no line at this row: it keeps the columns aligned
/// and reads as "nothing here".
const FILLER_BG: u32 = 0x151a1a;
/// Smallest useful comparison surface, so a short window still shows rows.
const RESULT_MIN_HEIGHT: f32 = 200.;
/// Height of the comparison's header bar.
const RESULT_HEADER_H: f32 = 32.;
/// Width of a line-number gutter, matching the review rows.
const NUMBER_WIDTH: f32 = 44.;

pub(crate) struct ToolView {
    tool: ToolKind,
    store: Option<ToolStore>,
    inputs: Vec<InputSlot>,
    /// First failed read of a persisted input. While set, saving is blocked.
    load_error: Option<String>,
    /// Failed write, shown with the actions until the next successful one.
    save_error: Option<String>,
    /// Primary-action failure for the current input, shown with the actions.
    error: Option<String>,
    /// Last text written to disk per input, so saves skip unchanged content.
    saved: Vec<String>,
    /// Bumped on every input change and on every flush; a debounced save
    /// only writes when its snapshot is still current, so a burst of edits
    /// costs one write.
    save_generation: u64,
    /// Which surface a diff tool is showing. In-place tools only ever edit.
    mode: ToolMode,
    /// The last computed comparison and its virtualized rows. Diff tools
    /// only; it survives closing the dialog so reopening shows the same diff.
    diff: Option<DiffPane>,
    /// Bumped on every comparison run; a background result only lands when
    /// its run is still the newest, so a slow paste cannot overwrite a newer
    /// diff (or one cleared while it was computing).
    diff_generation: u64,
    /// Bumped on every recolor; a background highlight only lands when its
    /// run is still the newest, so repainting never fights a newer language.
    highlight_generation: u64,
    /// The language both surfaces highlight with. Diff tools only.
    language: &'static str,
    /// The language picker. Diff tools only.
    language_select: Option<Entity<LanguageSelect>>,
    /// Kept alive so the picker's confirm subscription outlives construction.
    _language_subscription: Option<Subscription>,
    /// One-shot: focus the first input on the next render. The dialog layer
    /// takes focus when it opens, so focusing at construction would be lost.
    focus_on_render: bool,
    focus_handle: FocusHandle,
}

/// Which surface a diff tool's dialog is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolMode {
    /// The old and new paste editors.
    Edit,
    /// The comparison.
    Diff,
}

/// A computed comparison plus the rows its pane renders.
struct DiffPane {
    result: Rc<DiffResult>,
    rows: Rc<Vec<DiffRow>>,
    /// Highlight spans for the listing's lines, empty until the background
    /// highlight lands (and for plain text, which has none by definition).
    highlights: Rc<SyntaxHighlights>,
    list: ListState,
    /// The comparison no longer matches the inputs. Editing is only possible
    /// in [`ToolMode::Edit`], so this is never shown as stale: it tells the
    /// toggle to recompute instead of revealing an out-of-date comparison.
    stale: bool,
}

/// One row of the comparison pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffRow {
    /// A paired listing row: the old side's line and the new side's line,
    /// either of which may be missing (see [`diff::split_rows`]).
    Lines(SplitRow),
    Truncated,
    NoChanges,
}

/// Flatten a computed comparison into rows. The listing is one hunk holding
/// every line of both documents, so there is no collapsed-run row: a reader
/// can reference any line of the pasted text. An empty comparison still gets
/// one row so the pane can say there are no changes.
fn flatten(result: &DiffResult) -> Vec<DiffRow> {
    let mut rows: Vec<DiffRow> = diff::split_rows(result)
        .into_iter()
        .map(DiffRow::Lines)
        .collect();
    if result.truncated {
        rows.push(DiffRow::Truncated);
    }
    if rows.is_empty() {
        rows.push(DiffRow::NoChanges);
    }
    rows
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
        let diff_tool = tool.result_kind() == ToolResultKind::Diff;
        // A failed read of a preference is not worth blocking the dialog:
        // the picker opens on plain text instead.
        let language = if diff_tool {
            store
                .as_ref()
                .and_then(|store| store.load_setting(tool, LANGUAGE_SETTING).ok())
                .map(|name| diff::resolve_language(name.trim()))
                .unwrap_or(diff::PLAIN_LANGUAGE)
        } else {
            diff::PLAIN_LANGUAGE
        };
        // The paste editors color with the picked language too, through the
        // editor's own highlighter; in-place tools color with the one
        // language their editor declares.
        let editor_language = if diff_tool {
            diff::editor_language(language)
        } else {
            tool.editor_language()
                .unwrap_or(diff::PLAIN_EDITOR_LANGUAGE)
        };
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
                let mut editor = editor_state(window, cx, editor_language, Some(slot.placeholder));
                if !text.is_empty() {
                    editor.set_value(text.clone(), window, cx);
                }
                editor
            });
            let subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.mark_result_stale();
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
        let (language_select, language_subscription) = if diff_tool {
            let delegate = SearchableVec::new(diff::languages());
            let selected = delegate.position(&language);
            let select =
                cx.new(|cx| SelectState::new(delegate, selected, window, cx).searchable(true));
            let subscription = cx.subscribe(
                &select,
                |this, _, event: &SelectEvent<SearchableVec<&'static str>>, cx| {
                    if let SelectEvent::Confirm(Some(language)) = event {
                        this.set_language(language, cx);
                    }
                },
            );
            (Some(select), Some(subscription))
        } else {
            (None, None)
        };
        Self {
            tool,
            store,
            inputs,
            load_error,
            save_error: None,
            error: None,
            saved,
            save_generation: 0,
            mode: ToolMode::Edit,
            diff: None,
            diff_generation: 0,
            highlight_generation: 0,
            language,
            language_select,
            _language_subscription: language_subscription,
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
                // offers and the panes below fill it.
                .w(px(1760.))
                .h(px(1400.))
                // The layer leaves a tenth of the viewport above a dialog by
                // default, which pushes a full-height tool far down the
                // screen; a small top margin keeps it near the top and hands
                // the reclaimed height to the panes.
                .margin_top(px(24.))
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
    /// when the dialog closes and when the primary action runs, so the last
    /// edits always land on disk.
    pub(crate) fn flush_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_generation = self.save_generation.wrapping_add(1);
        match self.save_now(cx) {
            Some(error) => {
                window.push_notification(format!("Could not save tool input: {error}"), cx);
            }
            None => self.save_error = None,
        }
    }

    /// The dialog's primary action. For a diff tool this is also the mode
    /// toggle, so its label names the destination rather than the state.
    fn primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tool.result_kind() {
            ToolResultKind::InPlaceText => self.apply_in_place(window, cx),
            ToolResultKind::Diff => self.toggle_diff(cx),
        }
    }

    /// The primary button's label: the action itself while editing, and the
    /// way back while reading the comparison.
    fn primary_label(&self) -> &'static str {
        match self.tool.result_kind() {
            ToolResultKind::InPlaceText => self.tool.action_label(),
            ToolResultKind::Diff if self.mode == ToolMode::Diff => "Edit",
            ToolResultKind::Diff => self.tool.action_label(),
        }
    }

    /// Switch between the paste editors and the comparison. Leaving the
    /// comparison costs nothing — editing is the only thing that can make it
    /// out of date, and editing is not possible while it is shown — so only
    /// entering it can need work: a missing or stale comparison is recomputed.
    fn toggle_diff(&mut self, cx: &mut Context<Self>) {
        if self.mode == ToolMode::Diff {
            self.mode = ToolMode::Edit;
            // The editors come back into view, so hand them the focus.
            self.focus_on_render = true;
            cx.notify();
            return;
        }
        if self.diff.as_ref().is_some_and(|pane| !pane.stale) {
            self.mode = ToolMode::Diff;
            cx.notify();
            return;
        }
        let inputs: Vec<String> = self.texts(cx);
        // An empty pair is not an action: leave it, and its confirmation,
        // alone.
        if inputs.iter().all(|input| input.trim().is_empty()) {
            return;
        }
        self.apply_diff(inputs, cx);
    }

    /// Rewrite the tool's document with the computed text: the first input is
    /// the document, and the replacement keeps the editor's undo history, so
    /// formatting by accident is one undo away. The result is persisted right
    /// away, so an already-formatted input stays formatted next time, and a
    /// failed parse leaves the text untouched and shows why.
    fn apply_in_place(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs: Vec<String> = self.texts(cx);
        if inputs.first().is_none_or(|input| input.trim().is_empty()) {
            return;
        }
        match self.tool.compute(&inputs) {
            Ok(ToolOutput::Text(formatted)) => {
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
            // Unreachable: `compute` returns the kind `result_kind` declares,
            // which the registry tests pin. Reported rather than panicking so
            // a registry mistake cannot take the dialog down.
            Ok(ToolOutput::Diff(_)) => {
                self.error = Some("This tool does not format text".to_owned())
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    /// Compare the two inputs, reveal the comparison, and highlight it.
    /// Diffing a large paste takes long enough to be visible, so it runs on
    /// the background executor like the review's load does; the editors stay
    /// up until the comparison lands.
    fn apply_diff(&mut self, inputs: Vec<String>, cx: &mut Context<Self>) {
        self.error = None;
        // The comparison takes a moment: clear a previous error on screen now
        // rather than when the result lands.
        cx.notify();
        self.diff_generation = self.diff_generation.wrapping_add(1);
        // A pending recolor belongs to the comparison this run replaces.
        self.highlight_generation = self.highlight_generation.wrapping_add(1);
        let generation = self.diff_generation;
        let tool = self.tool;
        cx.spawn(async move |this, cx| {
            let compared = cx
                .background_spawn(async move {
                    match tool.compute(&inputs) {
                        Ok(ToolOutput::Diff(result)) => Ok(result),
                        // Unreachable, as above: the registry pins the kind.
                        Ok(ToolOutput::Text(_)) => {
                            Err("This tool does not compute a diff".to_owned())
                        }
                        Err(error) => Err(error),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.diff_generation != generation {
                    return;
                }
                match compared {
                    Ok(result) => {
                        this.show_diff(result);
                        // The rows are up now; the colors follow as soon as
                        // the background highlight lands.
                        this.recolor(cx);
                        this.mode = ToolMode::Diff;
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Replace the comparison pane with a freshly computed one, uncolored:
    /// [`Self::recolor`] resolves the spans right after. A new list state
    /// means a new comparison starts at its top, however the previous one was
    /// scrolled.
    fn show_diff(&mut self, result: DiffResult) {
        let rows = flatten(&result);
        let list = ListState::new(rows.len(), ListAlignment::Top, px(300.));
        self.diff = Some(DiffPane {
            result: Rc::new(result),
            rows: Rc::new(rows),
            highlights: Rc::new(SyntaxHighlights::default()),
            list,
            stale: false,
        });
    }

    /// Resolve the highlight spans of the shown comparison for the current
    /// language, off the UI thread: highlighting is the expensive half of a
    /// comparison, and switching languages must not freeze the dialog while
    /// it repaints. The listing, the rows, and the scroll position stay
    /// exactly as they are; only the painting changes.
    fn recolor(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = &self.diff else {
            return;
        };
        let hunks = pane.result.hunks.clone();
        let language = self.language;
        self.highlight_generation = self.highlight_generation.wrapping_add(1);
        let generation = self.highlight_generation;
        cx.spawn(async move |this, cx| {
            let spans = cx
                .background_spawn(async move { diff::highlight(&hunks, language) })
                .await;
            this.update(cx, |this, cx| {
                if this.highlight_generation != generation {
                    return;
                }
                if let Some(pane) = this.diff.as_mut() {
                    pane.highlights = Rc::new(spans);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switch the highlight language: persist the choice and repaint both
    /// surfaces. The comparison itself does not change, so its pane keeps the
    /// listing and the scroll position.
    fn set_language(&mut self, language: &'static str, cx: &mut Context<Self>) {
        let language = diff::resolve_language(language);
        if self.language == language {
            return;
        }
        self.language = language;
        // The paste editors color with the same language, through the
        // editor's own highlighter: a different engine with a narrower
        // language set, which falls back to plain text for the rest.
        let editor_language = diff::editor_language(language);
        for input in &self.inputs {
            input
                .editor
                .update(cx, |editor, cx| editor.set_highlighter(editor_language, cx));
        }
        if let Some(store) = self.store.clone() {
            match store.save_setting(self.tool, LANGUAGE_SETTING, language) {
                Ok(()) => self.save_error = None,
                Err(error) => self.save_error = Some(format!("{error:#}")),
            }
        }
        self.recolor(cx);
        cx.notify();
    }

    /// Note that the shown comparison no longer matches the inputs. Nothing
    /// on screen changes — the comparison is not visible while editing — so
    /// this only marks it: the toggle recomputes before revealing it again.
    fn mark_result_stale(&mut self) {
        if let Some(pane) = self.diff.as_mut() {
            pane.stale = true;
        }
    }

    /// Copy the tool's document. Only in-place tools offer this: their result
    /// *is* the document, so there is one obvious thing to hand out. A diff
    /// tool has two documents and a comparison, and none of them is "the"
    /// text, so it does not offer the action at all.
    fn copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self
            .inputs
            .first()
            .map(|input| input.editor.read(cx).value().to_string())
        else {
            return;
        };
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification("Copied to clipboard", cx);
    }

    /// Clear every input and persist the clear. An explicit clear is a
    /// deliberate replace, so it also lifts a failed read's save block: the
    /// content that read could not show is being discarded anyway. A shown
    /// comparison goes with the text it was computed from, and the dialog
    /// returns to the paste editors, which is where the next one starts.
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_error = None;
        self.error = None;
        // Supersede anything still computing: it would otherwise land on top
        // of the cleared pane.
        self.diff_generation = self.diff_generation.wrapping_add(1);
        self.highlight_generation = self.highlight_generation.wrapping_add(1);
        self.diff = None;
        self.mode = ToolMode::Edit;
        for input in &self.inputs {
            input
                .editor
                .update(cx, |editor, cx| editor.set_value("", window, cx));
        }
        self.focus_on_render = true;
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

    /// The editing surface: one document for an in-place tool, the old and
    /// new pastes side by side for a diff tool.
    fn render_edit_mode(&self) -> AnyElement {
        match self.tool.result_kind() {
            ToolResultKind::InPlaceText => self.render_single_editor(),
            ToolResultKind::Diff => h_flex()
                .w_full()
                .flex_1()
                .min_h(px(EDITOR_MIN_HEIGHT))
                .items_stretch()
                .gap_2()
                .child(self.render_labelled_editor(0, "Old text"))
                .child(self.render_labelled_editor(1, "New text"))
                .into_any_element(),
        }
    }

    /// One editor filling the dialog, for tools whose result rewrites it.
    fn render_single_editor(&self) -> AnyElement {
        let Some(input) = self.inputs.first() else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .flex_1()
            .min_h(px(EDITOR_MIN_HEIGHT))
            .child(Editor::new(&input.editor).h(relative(1.)))
            .into_any_element()
    }

    /// One side of the editing surface: a labelled paste editor. The dialog
    /// is wide, so the two sides read as old and new at a glance.
    fn render_labelled_editor(&self, index: usize, label: &'static str) -> AnyElement {
        let Some(input) = self.inputs.get(index) else {
            return div().into_any_element();
        };
        v_flex()
            .id(("tool-input", index))
            .test_support()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(div().text_xs().text_color(rgb(MUTED_COLOR)).child(label))
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(Editor::new(&input.editor).h(relative(1.))),
            )
            .into_any_element()
    }

    /// The comparison pane: a header naming each column, then the paired
    /// rows. Only reachable with a comparison in hand, since the toggle
    /// computes one before revealing it.
    fn render_comparison(&self, cx: &App) -> AnyElement {
        let Some(pane) = &self.diff else {
            return div().into_any_element();
        };
        let stats = if pane.result.additions > 0 || pane.result.deletions > 0 {
            format!("+{} −{}", pane.result.additions, pane.result.deletions)
        } else {
            String::new()
        };
        // Each half names its column, and the stats close the row. The
        // hairline between the halves lines up with the one between cells.
        let header = h_flex()
            .h(px(RESULT_HEADER_H))
            .flex_none()
            .w_full()
            .items_stretch()
            .bg(rgb(PANE_HEADER_BG))
            .border_b_1()
            .border_color(rgb(PANE_BORDER))
            .text_xs()
            .font_semibold()
            .text_color(rgb(MUTED_COLOR))
            .child(
                h_flex()
                    .id("tool-diff-old-label")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .items_center()
                    .child("Old text"),
            )
            .child(div().w(px(1.)).flex_none().bg(rgb(PANE_BORDER)))
            .child(
                h_flex()
                    .id("tool-diff-new-label")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .child("New text")
                    .child(div().flex_1())
                    .child(stats),
            );
        let result = pane.result.clone();
        let rows = pane.rows.clone();
        let highlights = pane.highlights.clone();
        let dark = cx.theme().is_dark();
        let list = list(pane.list.clone(), move |ix, _window, _cx| match rows[ix] {
            DiffRow::Lines(row) => split_row(row, &result, &highlights, dark),
            DiffRow::Truncated => stream::truncated_row(),
            DiffRow::NoChanges => stream::no_changes_row(),
        })
        .size_full();
        v_flex()
            .id("tool-diff-result")
            .test_support()
            .w_full()
            .flex_1()
            .min_w_0()
            .min_h(px(RESULT_MIN_HEIGHT))
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(rgb(PANE_BORDER))
            .bg(rgb(PANE_BG))
            .child(header)
            .child(div().w_full().flex_1().min_h_0().child(list))
            .into_any_element()
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let copy_disabled = self
            .inputs
            .first()
            .is_none_or(|input| input.editor.read(cx).value().is_empty());
        // The status message holds the free space so the actions stay right,
        // matching the action rows of the other dialogs.
        h_flex()
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
            .when_some(self.language_select.clone(), |row, select| {
                // The picker is global to the tool, not per mode, so the
                // language can be changed without leaving the comparison.
                row.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED_COLOR))
                                .child("Language"),
                        )
                        .child(
                            Select::new(&select)
                                .small()
                                .menu_width(px(280.))
                                .search_placeholder("Search languages")
                                .placeholder(diff::PLAIN_LANGUAGE),
                        ),
                )
            })
            .child(
                Button::new("tool-clear")
                    .label("Clear")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
            )
            .when(
                self.tool.result_kind() == ToolResultKind::InPlaceText,
                |row| {
                    row.child(
                        Button::new("tool-copy")
                            .label("Copy")
                            .ghost()
                            .small()
                            .disabled(copy_disabled)
                            .on_click(cx.listener(|this, _, window, cx| this.copy(window, cx))),
                    )
                },
            )
            .child(
                Button::new(self.tool.action_id())
                    .label(self.primary_label())
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.primary(window, cx))),
            )
    }
}

/// Which column a comparison cell belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// One side-by-side row: both cells with a hairline between them, so the
/// columns read as two documents rather than one wide table.
fn split_row(
    row: SplitRow,
    result: &Rc<DiffResult>,
    highlights: &Rc<SyntaxHighlights>,
    dark: bool,
) -> AnyElement {
    h_flex()
        .w_full()
        .flex_none()
        .items_stretch()
        .child(split_cell(row.old, Side::Old, result, highlights, dark))
        .child(div().w(px(1.)).flex_none().bg(rgb(PANE_BORDER)))
        .child(split_cell(row.new, Side::New, result, highlights, dark))
        .into_any_element()
}

/// One cell of a comparison row: a line number and the line's text, tinted
/// when the line is a removal (old side) or an addition (new side). A side
/// with no line gets a blank cell instead, so the columns stay aligned across
/// a change.
fn split_cell(
    line: Option<LineRef>,
    side: Side,
    result: &Rc<DiffResult>,
    highlights: &Rc<SyntaxHighlights>,
    dark: bool,
) -> AnyElement {
    let Some(reference) = line else {
        return div()
            .flex_1()
            .min_w_0()
            .h(px(stream::ROW_H))
            .bg(rgb(FILLER_BG))
            .into_any_element();
    };
    let line = &result.hunks[reference.hunk].lines[reference.line];
    let (number, background) = match side {
        Side::Old => (
            line.old_no,
            (line.tag == LineTag::Deletion).then_some(stream::DELETION_BG),
        ),
        Side::New => (
            line.new_no,
            (line.tag == LineTag::Addition).then_some(stream::ADDITION_BG),
        ),
    };
    let number = match number {
        Some(number) => format!("{number:>5}"),
        None => " ".repeat(5),
    };
    div()
        .flex_1()
        .min_w_0()
        .h(px(stream::ROW_H))
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(TERMINAL_FONT_FAMILY)
        .text_size(px(review_font_size()))
        .line_height(px(stream::ROW_H))
        .when_some(background, |this, color| this.bg(rgb(color)))
        .child(
            div()
                .w(px(NUMBER_WIDTH))
                .flex_none()
                .text_color(rgb(NUMBER_COLOR))
                .child(number),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    StyledText::new(line.text.clone()).with_highlights(styled_highlights(
                        highlights.document_line(dark, reference.hunk, reference.line),
                    )),
                ),
        )
        .into_any_element()
}

/// An editor highlighting `language`, or plain when it is the editor's plain
/// text name. Every input shares the construction so their styling cannot
/// drift.
fn editor_state(
    window: &mut Window,
    cx: &mut Context<EditorState>,
    language: &str,
    placeholder: Option<&'static str>,
) -> EditorState {
    let mut state = EditorState::new(window, cx).language(language);
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
            // Only an editor that is actually on screen takes the focus; the
            // comparison has nothing to type into.
            if self.mode == ToolMode::Edit
                && let Some(input) = self.inputs.first()
            {
                input.editor.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        let mut body = v_flex().size_full().gap_2();
        if let Some(error) = self.load_error.clone() {
            body = body.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error));
        }
        body = body.child(match self.mode {
            ToolMode::Edit => self.render_edit_mode(),
            ToolMode::Diff => self.render_comparison(cx),
        });
        body.child(self.render_actions(cx))
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
