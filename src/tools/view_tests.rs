//! Behavior tests for the tool dialog, rendered in a real (headless) window.

use super::*;
use crate::tools::ToolStore;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::test::TestWindowExt as _;

/// Window root for dialog and notification tests: both the dialog layer and
/// notifications only exist under gpui-kit's `Root`.
struct Harness {
    tool: Entity<ToolView>,
}

impl Render for Harness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.tool.clone())
            // `Root` stores dialogs but never paints them; the layer has to
            // be rendered by the app, exactly like `Workspace` does.
            .children(gpui_kit::component::Root::render_dialog_layer(window, cx))
    }
}

/// A data root in a temp dir plus the store pointed at it. The temp dir must
/// outlive the test, so it is returned alongside the root.
fn fixture() -> (tempfile::TempDir, DataRoot, ToolStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let store = ToolStore::new(&root);
    (dir, root, store)
}

fn slot() -> ToolInput {
    ToolKind::JsonFormatter.inputs()[0]
}

/// The tool view as the window root, for tests that need nothing but the
/// dialog body.
fn open_tool(
    cx: &mut gpui_kit::TestAppContext,
    root: DataRoot,
) -> (Entity<ToolView>, &mut gpui_kit::VisualTestContext) {
    cx.update(gpui_kit::init);
    cx.add_window_view(|window, cx| ToolView::new(window, cx, ToolKind::JsonFormatter, Some(root)))
}

/// Like [`open_tool`], but under gpui-kit's `Root` so notifications and the
/// dialog layer exist (both require that window root).
fn open_with_root(
    cx: &mut gpui_kit::TestAppContext,
    root: DataRoot,
) -> (Entity<ToolView>, &mut gpui_kit::VisualTestContext) {
    cx.update(gpui_kit::init);
    let mut tool = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| ToolView::new(window, cx, ToolKind::JsonFormatter, Some(root)));
        tool = Some(view.clone());
        let harness = cx.new(|_| Harness { tool: view });
        gpui_kit::component::Root::new(harness, window, cx)
    });
    (tool.unwrap(), cx)
}

/// Programmatic replacement, like loading a document: deliberately silent, so
/// it does not arm the autosave or record undo history.
fn set_input(view: &Entity<ToolView>, text: &str, window: &mut Window, cx: &mut App) {
    view.update(cx, |view, cx| {
        view.inputs[0]
            .editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
    });
}

/// Programmatic replacement that keeps undo history, mirroring what the
/// Format action itself does.
fn replace_input(view: &Entity<ToolView>, text: &str, window: &mut Window, cx: &mut App) {
    view.update(cx, |view, cx| {
        view.inputs[0]
            .editor
            .update(cx, |editor, cx| editor.replace_all(text, window, cx));
    });
}

fn input_text(view: &ToolView, cx: &App) -> String {
    view.inputs[0].editor.read(cx).value().to_string()
}

fn clipboard_text(cx: &gpui_kit::VisualTestContext) -> Option<String> {
    cx.read(|cx| cx.read_from_clipboard())
        .and_then(|item| item.text())
}

#[gpui_kit::test]
fn opens_with_the_persisted_input_untouched(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    store
        .save(ToolKind::JsonFormatter, &slot(), r#"{"b":1,"a":[1,2]}"#)
        .unwrap();
    let (view, cx) = open_tool(cx, root);
    view.read_with(cx, |view, cx| {
        // Opening never rewrites: the tool shows what was left there, and
        // formatting stays an explicit action.
        assert_eq!(input_text(view, cx), r#"{"b":1,"a":[1,2]}"#);
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.load_error.is_none(), "{:?}", view.load_error);
    });
}

#[gpui_kit::test]
fn first_render_focuses_the_input(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        let focused = window.focused(cx).expect("a focused handle after opening");
        let input = view.read(cx).inputs[0].editor.read(cx).focus_handle(cx);
        assert_eq!(focused, input, "the JSON input must own focus on open");
    });
}

#[gpui_kit::test]
fn format_rewrites_the_input_in_place_and_persists(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_with_root(cx, root);
    cx.update(|window, cx| {
        set_input(&view, r#"{"nested":{"x":1}}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    let formatted = "{\n  \"nested\": {\n    \"x\": 1\n  }\n}";
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, cx), formatted);
        assert!(view.error.is_none(), "{:?}", view.error);
    });
    // Formatting always reports that it ran, even when the text was already
    // laid out.
    assert!(!cx.update(|window, cx| window.notifications(cx)).is_empty());
    // The formatted document is what persists, so the next opening is
    // already laid out.
    assert_eq!(
        store.load(ToolKind::JsonFormatter, &slot()).unwrap(),
        formatted
    );
}

#[gpui_kit::test]
fn invalid_input_keeps_the_text_and_shows_the_error(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        set_input(&view, r#"{"a":}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    view.read_with(cx, |view, cx| {
        // A failed parse must never destroy the document being edited.
        assert_eq!(input_text(view, cx), r#"{"a":}"#);
        let error = view.error.clone().expect("a parse error");
        assert!(error.contains("line 1 column 6"), "{error}");
    });
}

#[gpui_kit::test]
fn formatting_is_undoable_and_the_undo_is_saved(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_with_root(cx, root);
    cx.update(|window, cx| {
        replace_input(&view, r#"{"a":1}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    let formatted = "{\n  \"a\": 1\n}";
    view.read_with(cx, |view, cx| assert_eq!(input_text(view, cx), formatted));
    // Formatting keeps undo history, so an accidental format is one undo
    // away — and the undo is a real edit that autosaves.
    let undo = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    cx.simulate_keystrokes(undo);
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, cx), r#"{"a":1}"#)
    });
    cx.background_executor.advance_clock(SAVE_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(
        store.load(ToolKind::JsonFormatter, &slot()).unwrap(),
        r#"{"a":1}"#
    );
}

#[gpui_kit::test]
fn clear_empties_the_input_and_persisted_file(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    store
        .save(ToolKind::JsonFormatter, &slot(), r#"{"a":1}"#)
        .unwrap();
    let (view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-clear", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, cx), "");
        assert!(view.error.is_none());
    });
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "");
}

#[gpui_kit::test]
fn typed_edits_autosave_after_the_debounce(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        // Real typing, not `set_value`: programmatic sets are deliberately
        // silent, and only user edits should arm the autosave.
        window.render_frame(cx);
        window.input("123", cx);
    });
    view.read_with(cx, |view, cx| assert_eq!(input_text(view, cx), "123"));
    // The write is debounced, so nothing has landed yet.
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "");
    cx.background_executor.advance_clock(SAVE_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "123");
    assert!(view.read_with(cx, |view, _| view.save_error.is_none()));
}

#[gpui_kit::test]
fn copy_copies_the_current_input(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    store
        .save(ToolKind::JsonFormatter, &slot(), r#"{"a":1}"#)
        .unwrap();
    let (view, cx) = open_with_root(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-copy", cx);
    });
    assert_eq!(clipboard_text(cx), Some(r#"{"a":1}"#.to_owned()));
    // After formatting, Copy hands out the laid-out document.
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-format", cx);
        window.render_frame(cx);
        window.click("tool-copy", cx);
    });
    assert_eq!(clipboard_text(cx), Some("{\n  \"a\": 1\n}".to_owned()));
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, cx), "{\n  \"a\": 1\n}")
    });
}

#[gpui_kit::test]
fn failed_load_blocks_saving_until_cleared(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    // A directory where the input file belongs makes the read fail.
    let path = store.path(ToolKind::JsonFormatter, &slot());
    std::fs::create_dir_all(&path).unwrap();
    let (view, cx) = open_tool(cx, root);
    assert!(
        view.read_with(cx, |view, _| view.load_error.is_some()),
        "an unreadable input must surface instead of showing empty"
    );
    cx.update(|window, cx| {
        set_input(&view, r#"{"a":1}"#, window, cx);
        view.update(cx, |view, cx| view.flush_save(window, cx));
    });
    // The blocked save left the unreadable input and its directory alone.
    assert!(path.is_dir());
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "a blocked save must not write at all");
    // Clear is an explicit replace, so it lifts the block.
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-clear", cx);
    });
    assert!(view.read_with(cx, |view, _| view.load_error.is_none()));
}

#[gpui_kit::test]
fn dismissing_the_dialog_flushes_pending_edits(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (tool, cx) = open_with_root(cx, root);
    cx.update(|window, cx| ToolView::open_dialog(tool.clone(), window, cx));
    cx.update(|window, cx| {
        assert!(window.has_active_dialog(cx), "the tool dialog must open");
        window.render_frame(cx);
        // The dialog layer takes focus on open; the tool must reclaim it so
        // typing reaches the input.
        let input = tool.read(cx).inputs[0].editor.read(cx).focus_handle(cx);
        assert_eq!(window.focused(cx), Some(input));
        window.input("42", cx);
    });
    // Nothing is written before the debounce...
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "");
    // ...but dismissing the dialog flushes the pending edit immediately.
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "42");
}

#[gpui_kit::test]
fn formatting_an_empty_document_does_nothing(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_with_root(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, cx), "");
        assert!(view.error.is_none(), "{:?}", view.error);
    });
    assert!(
        cx.update(|window, cx| window.notifications(cx)).is_empty(),
        "an empty document must not claim it was formatted"
    );
}

/// The editor highlights JSON only when the app enables gpui-kit's
/// `tree-sitter` feature, which registers the grammar. Without it the
/// highlighter factory finds no parser and the editor silently renders plain
/// text, so pin the wiring here rather than trusting the Cargo.toml.
#[test]
fn the_json_grammar_is_registered_for_highlighting() {
    let registry = gpui_kit::component::highlighter::LanguageRegistry::singleton();
    let config = registry
        .language("json")
        .expect("the json language must be registered");
    assert!(
        config.has_grammar(),
        "the JSON grammar must be linked in for the tool editor to highlight"
    );
}

#[gpui_kit::test]
fn the_editor_absorbs_the_space_above_the_actions(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (_view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        let actions = window.find("tool-format").bounds();
        let bottom = window.viewport_size().height;
        // The editor above the actions grows into whatever the dialog has
        // left, so the actions sit at the bottom instead of leaving dead
        // space below them.
        assert!(
            bottom - actions.bottom() < px(80.),
            "actions should sit near the bottom: {actions:?} in a {bottom:?} viewport"
        );
    });
}

#[gpui_kit::test]
fn the_dialog_editor_fills_the_dialog_height(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (tool, cx) = open_with_root(cx, root);
    cx.update(|window, cx| ToolView::open_dialog(tool.clone(), window, cx));
    cx.update(|window, cx| {
        window.render_frame(cx);
        // Scope to the dialog: the harness renders the tool view too, so the
        // plain id would be ambiguous.
        let actions = window.within("dialog-0").find("tool-format").bounds();
        let bottom = window.viewport_size().height;
        // The dialog is clamped to nearly the whole viewport, and the editor
        // inside it absorbs the height the dialog is given: the actions land
        // near the bottom instead of leaving dead space under them.
        assert!(
            bottom - actions.bottom() < px(140.),
            "actions should sit near the dialog bottom: {actions:?} in a {bottom:?} viewport"
        );
    });
}

/// End-to-end check of the highlighting path: with the JSON grammar linked
/// in, the highlighter splits the document into styled spans (keys, strings,
/// numbers). Without it `SyntaxHighlighter::new` silently falls back to an
/// inert highlighter that returns one default span.
#[gpui_kit::test]
fn json_highlighting_splits_the_document_into_styled_spans(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    let mut highlighter = gpui_kit::component::highlighter::SyntaxHighlighter::new("json");
    let text = gpui_kit::base::input::Rope::from_str(r#"{"key": "value", "n": 1}"#);
    assert!(
        highlighter.update(None, &text, None),
        "the highlighter must parse the document"
    );
    let styles = cx.update(|cx| {
        let theme = cx.theme().highlight_theme.clone();
        highlighter.styles(&(0..text.len()), &*theme)
    });
    assert!(
        styles.len() > 1,
        "json must produce distinct styled spans, got {styles:?}"
    );
    assert!(
        styles.iter().any(|(_, style)| style.color.is_some()),
        "at least one span must carry a syntax color, got {styles:?}"
    );
}

#[gpui_kit::test]
fn the_actions_are_compact_and_right_aligned(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (_view, cx) = open_tool(cx, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        let format = window.find("tool-format").bounds();
        let clear = window.find("tool-clear").bounds();
        let viewport = window.viewport_size();
        // Compact: the small button variant (24px) rather than the default
        // medium one (32px), matching the app's panel buttons.
        assert!(
            format.size.height <= px(26.),
            "actions should use the compact button size: {format:?}"
        );
        // Right-aligned: the primary action ends at the right edge of the
        // row, and the whole group stays in its right half.
        assert!(
            viewport.width - format.right() <= px(8.),
            "the primary action should end at the right edge: {format:?} in {viewport:?}"
        );
        assert!(
            clear.left() > viewport.width / 2.,
            "the action group should sit on the right: {clear:?} in {viewport:?}"
        );
    });
}
