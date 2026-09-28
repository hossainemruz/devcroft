//! Behavior tests for the tool dialog, rendered in a real (headless) window.

use super::*;
use crate::tools::ToolStore;
use gpui_kit::test::TestWindowExt as _;

/// A data root in a temp dir plus the store pointed at it. The temp dir must
/// outlive the test, so it is returned alongside the root.
fn fixture() -> (tempfile::TempDir, DataRoot, ToolStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let store = ToolStore::new(&root);
    (dir, root, store)
}

/// The JSON formatter's single input slot.
fn slot() -> ToolInput {
    ToolKind::JsonFormatter.inputs()[0]
}

/// The diff tool's old and new slots, in display order.
fn diff_slots() -> [ToolInput; 2] {
    let [old, new] = ToolKind::DiffChecker.inputs() else {
        panic!("the diff tool must declare exactly two inputs");
    };
    [*old, *new]
}

/// The tool view as the window root, for tests that need nothing but the
/// dialog body.
fn open_tool(
    cx: &mut gpui_kit::TestAppContext,
    tool: ToolKind,
    root: DataRoot,
) -> (Entity<ToolView>, &mut gpui_kit::VisualTestContext) {
    cx.update(gpui_kit::init);
    cx.add_window_view(|window, cx| ToolView::new(window, cx, tool, Some(root)))
}

/// Like [`open_tool`], but under gpui-kit's `Root`, which hosts dialogs and
/// notifications automatically.
fn open_with_root(
    cx: &mut gpui_kit::TestAppContext,
    tool: ToolKind,
    root: DataRoot,
) -> (Entity<ToolView>, &mut gpui_kit::VisualTestContext) {
    cx.update(gpui_kit::init);
    let mut view = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let tool_view = cx.new(|cx| ToolView::new(window, cx, tool, Some(root)));
        view = Some(tool_view.clone());
        gpui_kit::component::Root::new(tool_view, window, cx)
    });
    (view.unwrap(), cx)
}

/// Programmatic replacement, like loading a document: deliberately silent, so
/// it does not arm the autosave or record undo history.
fn set_input(view: &Entity<ToolView>, index: usize, text: &str, window: &mut Window, cx: &mut App) {
    view.update(cx, |view, cx| {
        view.inputs[index]
            .editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
    });
}

/// Programmatic replacement that keeps undo history, mirroring what the
/// primary action itself does. It is a real edit, so it emits a change.
fn replace_input(
    view: &Entity<ToolView>,
    index: usize,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) {
    view.update(cx, |view, cx| {
        view.inputs[index]
            .editor
            .update(cx, |editor, cx| editor.replace_all(text, window, cx));
    });
}

fn input_text(view: &ToolView, index: usize, cx: &App) -> String {
    view.inputs[index].editor.read(cx).value().to_string()
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
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    view.read_with(cx, |view, cx| {
        // Opening never rewrites: the tool shows what was left there, and
        // formatting stays an explicit action.
        assert_eq!(input_text(view, 0, cx), r#"{"b":1,"a":[1,2]}"#);
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.load_error.is_none(), "{:?}", view.load_error);
    });
}

#[gpui_kit::test]
fn first_render_focuses_the_input(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
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
    let (view, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        set_input(&view, 0, r#"{"nested":{"x":1}}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    let formatted = "{\n  \"nested\": {\n    \"x\": 1\n  }\n}";
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), formatted);
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
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        set_input(&view, 0, r#"{"a":}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    view.read_with(cx, |view, cx| {
        // A failed parse must never destroy the document being edited.
        assert_eq!(input_text(view, 0, cx), r#"{"a":}"#);
        let error = view.error.clone().expect("a parse error");
        assert!(error.contains("line 1 column 6"), "{error}");
    });
}

#[gpui_kit::test]
fn formatting_is_undoable_and_the_undo_is_saved(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        replace_input(&view, 0, r#"{"a":1}"#, window, cx);
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    let formatted = "{\n  \"a\": 1\n}";
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), formatted)
    });
    // Formatting keeps undo history, so an accidental format is one undo
    // away — and the undo is a real edit that autosaves.
    let undo = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    cx.simulate_keystrokes(undo);
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), r#"{"a":1}"#)
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
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-clear", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "");
        assert!(view.error.is_none());
    });
    assert_eq!(store.load(ToolKind::JsonFormatter, &slot()).unwrap(), "");
}

#[gpui_kit::test]
fn typed_edits_autosave_after_the_debounce(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        // Real typing, not `set_value`: programmatic sets are deliberately
        // silent, and only user edits should arm the autosave.
        window.render_frame(cx);
        window.input("123", cx);
    });
    view.read_with(cx, |view, cx| assert_eq!(input_text(view, 0, cx), "123"));
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
    let (view, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
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
        assert_eq!(input_text(view, 0, cx), "{\n  \"a\": 1\n}")
    });
}

#[gpui_kit::test]
fn base64_encoder_keeps_input_separate_and_copies_only_current_output(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (_dir, root, store) = fixture();
    let tool = ToolKind::Base64Encoder;
    let slot = tool.inputs()[0];
    let (view, cx) = open_with_root(cx, tool, root);
    cx.update(|window, cx| {
        replace_input(&view, 0, "Hello, 🌍", window, cx);
        window.render_frame(cx);
        window.click("tool-encode", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "Hello, 🌍");
        assert_eq!(
            view.output.as_ref().unwrap().read(cx).value().to_string(),
            "SGVsbG8sIPCfjI0="
        );
    });
    cx.update(|window, cx| {
        window.render_frame(cx);
        let input = window.find(("tool-input", 0usize)).bounds();
        let output = window.find("tool-output").bounds();
        let copy = window.find("tool-copy").bounds();
        assert!(input.bottom() <= output.top());
        assert!(output.bottom() <= copy.top());
    });
    assert_eq!(store.load(tool, &slot).unwrap(), "Hello, 🌍");
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-copy", cx);
    });
    assert_eq!(clipboard_text(cx), Some("SGVsbG8sIPCfjI0=".to_owned()));

    cx.update(|window, cx| replace_input(&view, 0, "changed", window, cx));
    view.read_with(cx, |view, _| assert!(view.output.is_none()));
    // An edit must not leave the old result available to Copy.
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-copy", cx);
    });
    assert_eq!(clipboard_text(cx), Some("SGVsbG8sIPCfjI0=".to_owned()));
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-clear", cx);
    });
    assert_eq!(store.load(tool, &slot).unwrap(), "");
    view.read_with(cx, |view, _| assert!(view.output.is_none()));
}

#[gpui_kit::test]
fn base64_decoder_shows_text_and_rejects_invalid_input_without_stale_output(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (_dir, root, store) = fixture();
    let tool = ToolKind::Base64Decoder;
    let slot = tool.inputs()[0];
    let (view, cx) = open_with_root(cx, tool, root);
    cx.update(|window, cx| {
        replace_input(&view, 0, "SGVs\nbG8=", window, cx);
        window.render_frame(cx);
        window.click("tool-decode", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "SGVs\nbG8=");
        assert_eq!(
            view.output.as_ref().unwrap().read(cx).value().to_string(),
            "Hello"
        );
    });
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-copy", cx);
    });
    assert_eq!(clipboard_text(cx), Some("Hello".to_owned()));

    cx.update(|window, cx| {
        replace_input(&view, 0, "???", window, cx);
        window.render_frame(cx);
        window.click("tool-decode", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "???");
        assert!(view.output.is_none());
        assert!(
            view.error
                .as_deref()
                .unwrap()
                .starts_with("Invalid Base64:")
        );
    });
    assert_eq!(store.load(tool, &slot).unwrap(), "???");
}

#[gpui_kit::test]
fn failed_load_blocks_saving_until_cleared(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    // A directory where the input file belongs makes the read fail.
    let path = store.path(ToolKind::JsonFormatter, &slot());
    std::fs::create_dir_all(&path).unwrap();
    let (view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    assert!(
        view.read_with(cx, |view, _| view.load_error.is_some()),
        "an unreadable input must surface instead of showing empty"
    );
    cx.update(|window, cx| {
        set_input(&view, 0, r#"{"a":1}"#, window, cx);
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
    let (tool, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
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
    let (view, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-format", cx);
    });
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "");
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
    let (_view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
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
    let (tool, cx) = open_with_root(cx, ToolKind::JsonFormatter, root);
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

/// End-to-end check of the editor highlighting path: with a grammar linked
/// in, the highlighter splits the document into styled spans. Without it
/// `SyntaxHighlighter::new` silently falls back to an inert highlighter that
/// returns one default span — which is exactly what a missing Cargo feature
/// would look like, so this covers both engines the tool uses.
#[gpui_kit::test]
fn editor_highlighting_splits_documents_into_styled_spans(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    for (language, source) in [
        ("json", r#"{"key": "value", "n": 1}"#),
        ("rust", "fn main() { let answer: u32 = 42; }"),
    ] {
        let mut highlighter = gpui_kit::component::highlighter::SyntaxHighlighter::new(language);
        let text = gpui_kit::base::input::Rope::from_str(source);
        assert!(
            highlighter.update(None, &text, None),
            "the {language} highlighter must parse the document"
        );
        let styles = cx.update(|cx| {
            let theme = cx.theme().highlight_theme.clone();
            highlighter.styles(&(0..text.len()), &*theme)
        });
        assert!(
            styles.len() > 1,
            "{language} must produce distinct styled spans, got {styles:?}"
        );
        assert!(
            styles.iter().any(|(_, style)| style.color.is_some()),
            "at least one {language} span must carry a syntax color, got {styles:?}"
        );
    }
}

#[gpui_kit::test]
fn the_actions_are_compact_and_right_aligned(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (_view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
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

#[gpui_kit::test]
fn diff_compares_the_two_inputs_into_the_result_pane(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    view.read_with(cx, |view, _| {
        assert!(view.diff.is_none(), "nothing is computed before Diff runs");
    });
    cx.update(|window, cx| {
        set_input(&view, 0, "one\ntwo\nthree\n", window, cx);
        set_input(&view, 1, "one\nTWO\nthree\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        assert_eq!((pane.result.additions, pane.result.deletions), (1, 1));
        assert!(!pane.stale, "a fresh comparison is not stale");
        // The pane pairs the listing into side-by-side rows: every line of
        // both documents is on a row, with nothing collapsed away.
        let old_lines = pane
            .rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Lines(row) if row.old.is_some()))
            .count();
        let new_lines = pane
            .rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Lines(row) if row.new.is_some()))
            .count();
        assert_eq!((old_lines, new_lines), (3, 3));
        assert_eq!(pane.list.item_count(), pane.rows.len());
    });
    // The rows are actually laid out: the pane renders the stream, not just
    // holds the model.
    cx.update(|window, cx| window.render_frame(cx));
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        let first = pane
            .list
            .bounds_for_item(0)
            .expect("the first diff row must be laid out");
        assert_eq!(first.size.height, px(stream::ROW_H));
    });
}

#[gpui_kit::test]
fn diff_of_identical_texts_says_there_are_no_changes(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "same\n", window, cx);
        set_input(&view, 1, "same\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        assert!(pane.result.hunks.is_empty());
        assert_eq!(*pane.rows, vec![DiffRow::NoChanges]);
    });
}

/// The listing is what a reader references: unchanged lines stay in place,
/// with their absolute numbers, so a change can be located in the pasted
/// text instead of only inside a hunk.
#[gpui_kit::test]
fn the_diff_pane_lists_the_whole_merged_document(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    let old = (1..=40).map(|n| format!("line {n}\n")).collect::<String>();
    let new = old.replacen("line 38", "line THIRTYEIGHT", 1);
    cx.update(|window, cx| {
        set_input(&view, 0, &old, window, cx);
        set_input(&view, 1, &new, window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        let hunk = &pane.result.hunks[0];
        assert_eq!((hunk.old_lines, hunk.new_lines), (40, 40));
        assert_eq!(hunk.lines.len(), 41, "40 lines plus the replaced one");
        // The changed line keeps its absolute number on both sides, and the
        // lines around it are present as context rather than collapsed away.
        use crate::review::model::LineTag;
        let removed = hunk
            .lines
            .iter()
            .find(|line| line.tag == LineTag::Deletion)
            .expect("the removed line");
        assert_eq!(removed.old_no, Some(38));
        let added = hunk
            .lines
            .iter()
            .find(|line| line.tag == LineTag::Addition)
            .expect("the added line");
        assert_eq!(added.new_no, Some(38));
        let context_numbers: Vec<_> = hunk
            .lines
            .iter()
            .filter(|line| line.tag == LineTag::Context)
            .filter_map(|line| line.new_no)
            .collect();
        assert!(context_numbers.contains(&37), "{context_numbers:?}");
        assert!(context_numbers.contains(&39), "{context_numbers:?}");
    });
}

/// The comparison is old against new, not a unified patch: two columns, each
/// with its own line numbers, and a blank cell where a side has nothing.
#[gpui_kit::test]
fn the_comparison_shows_old_and_new_side_by_side(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        // A removal with no matching addition, so the new side needs a blank
        // cell to keep the columns aligned.
        set_input(&view, 0, "one\ntwo\nthree\n", window, cx);
        set_input(&view, 1, "one\nthree\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.render_frame(cx);
        let old_label = window.find("tool-diff-old-label").bounds();
        let new_label = window.find("tool-diff-new-label").bounds();
        // The header names each column over its own half.
        assert!(
            old_label.right() <= new_label.left(),
            "the column labels must sit side by side: {old_label:?} {new_label:?}"
        );
        let pane = window.find("tool-diff-result").bounds();
        assert!(
            (old_label.size.width - new_label.size.width).abs() < px(2.),
            "the columns must share the pane evenly: {old_label:?} {new_label:?}"
        );
        assert!(old_label.left() >= pane.left() && new_label.right() <= pane.right());
    });
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed comparison");
        let rows = &pane.rows;
        // "one" and "three" are context rows; "two" is removed with nothing
        // opposite it.
        assert_eq!(rows.len(), 3, "{rows:?}");
        let removed = rows
            .iter()
            .find_map(|row| match row {
                DiffRow::Lines(row) if row.old.is_some() && row.new.is_none() => Some(row),
                _ => None,
            })
            .expect("a removal with a blank new cell");
        let line = &pane.result.hunks[removed.old.unwrap().hunk].lines[removed.old.unwrap().line];
        assert_eq!((line.text.as_str(), line.old_no), ("two", Some(2)));
    });
}

#[gpui_kit::test]
fn the_paste_editors_highlight_the_picked_language(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    // Plain text by default: the editors are not highlighted either.
    view.read_with(cx, |view, cx| {
        for input in &view.inputs {
            assert_eq!(input.editor.read(cx).language_name(), "text");
        }
    });
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.set_language("Rust", cx));
        window.render_frame(cx);
    });
    view.read_with(cx, |view, cx| {
        for input in &view.inputs {
            assert_eq!(input.editor.read(cx).language_name(), "rust");
        }
    });
    // A language the editor grammars do not cover falls back to plain text
    // rather than to a highlighter that would paint nothing.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.set_language("XML", cx));
        window.render_frame(cx);
    });
    view.read_with(cx, |view, cx| {
        for input in &view.inputs {
            assert_eq!(input.editor.read(cx).language_name(), "text");
        }
    });
}

/// Every mapped language must actually have a grammar linked in, or the
/// editor would fall back to an inert highlighter and the picker would lie
/// about what the paste areas can color.
#[test]
fn every_mapped_editor_language_has_a_grammar() {
    let registry = gpui_kit::component::highlighter::LanguageRegistry::singleton();
    for (name, id) in diff::EDITOR_LANGUAGES {
        let config = registry
            .language(id)
            .unwrap_or_else(|| panic!("{name} maps to an unregistered language {id:?}"));
        assert!(
            config.has_grammar(),
            "{name} ({id}) has no linked grammar; add its Cargo feature"
        );
    }
}

#[gpui_kit::test]
fn the_diff_pane_highlights_the_picked_language(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "{\n  \"a\": 1\n}\n", window, cx);
        set_input(&view, 1, "{\n  \"a\": 2\n}\n", window, cx);
        view.update(cx, |view, cx| view.set_language("JSON", cx));
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison and its highlight both run on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        let colored = (0..pane.result.hunks[0].lines.len())
            .any(|line| !pane.highlights.document_line(true, 0, line).is_empty());
        assert!(colored, "the pane must carry spans for JSON lines");
    });
    // Plain text paints nothing, and switching back to it repaints the pane
    // that is already shown.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.set_language("Plain Text", cx));
        window.render_frame(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("the pane stays up");
        for line in 0..pane.result.hunks[0].lines.len() {
            assert!(pane.highlights.document_line(true, 0, line).is_empty());
        }
    });
}

#[gpui_kit::test]
fn the_language_picker_starts_plain_and_restores_the_persisted_choice(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (_dir, root, store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root.clone());
    // A fresh tool highlights nothing.
    assert_eq!(view.read_with(cx, |view, _| view.language), "Plain Text");

    // A picked language persists and is restored on the next opening.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.set_language("Rust", cx));
        window.render_frame(cx);
    });
    assert_eq!(
        store
            .load_setting(ToolKind::DiffChecker, "language")
            .unwrap(),
        "Rust"
    );
    let (reopened, cx) = open_tool(cx, ToolKind::DiffChecker, root.clone());
    assert_eq!(reopened.read_with(cx, |view, _| view.language), "Rust");

    // A name the bundle no longer knows falls back to plain text.
    store
        .save_setting(ToolKind::DiffChecker, "language", "No Such Language")
        .unwrap();
    let (stale, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    assert_eq!(stale.read_with(cx, |view, _| view.language), "Plain Text");
}

#[gpui_kit::test]
fn switching_the_language_keeps_the_diff_and_its_scroll(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "{\n  \"a\": 1\n}\n", window, cx);
        set_input(&view, 1, "{\n  \"a\": 2\n}\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    cx.run_until_parked();
    let before = view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a computed diff");
        (pane.result.clone(), pane.list.logical_scroll_top().item_ix)
    });
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.set_language("JSON", cx));
        window.render_frame(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("the pane stays up");
        // Recoloring repaints the same comparison in place: same hunks, same
        // rows, same viewport.
        assert_eq!(*pane.result, *before.0);
        assert_eq!(pane.list.logical_scroll_top().item_ix, before.1);
        let colored = (0..pane.result.hunks[0].lines.len())
            .any(|line| !pane.highlights.document_line(true, 0, line).is_empty());
        assert!(colored, "the new language must be painted");
    });
}

#[gpui_kit::test]
fn clearing_supersedes_a_comparison_still_computing(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "one\ntwo\n", window, cx);
        set_input(&view, 1, "one\nTWO\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
        // Clear before the background comparison lands: the stale result
        // must not reappear on the cleared pane.
        window.click("tool-clear", cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert!(view.diff.is_none(), "a superseded comparison must not land");
        assert_eq!(input_text(view, 0, cx), "");
    });
}

#[gpui_kit::test]
fn an_empty_pair_does_not_compute_a_result(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(
            view.diff.is_none(),
            "an empty pair must not claim a comparison"
        );
        assert_eq!(view.mode, ToolMode::Edit, "and must not leave the editors");
        assert!(view.error.is_none(), "{:?}", view.error);
    });
}

#[gpui_kit::test]
fn the_toggle_recomputes_only_what_it_must(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "one\ntwo\n", window, cx);
        set_input(&view, 1, "one\nTWO\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.mode), ToolMode::Diff);
    let first = view.read_with(cx, |view, _| view.diff.as_ref().unwrap().result.clone());

    // Back to the editors, then straight back: nothing was edited, so the
    // same comparison is revealed instead of being computed again.
    for _ in 0..2 {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("tool-diff", cx);
        });
        cx.run_until_parked();
    }
    assert_eq!(view.read_with(cx, |view, _| view.mode), ToolMode::Diff);
    let again = view.read_with(cx, |view, _| view.diff.as_ref().unwrap().result.clone());
    assert!(
        Rc::ptr_eq(&first, &again),
        "an unchanged toggle must reuse the comparison"
    );

    // Editing invalidates it, and the next toggle recomputes rather than
    // revealing the old one.
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-diff", cx);
        replace_input(&view, 0, "one\nCHANGED\n", window, cx);
    });
    assert_eq!(view.read_with(cx, |view, _| view.mode), ToolMode::Edit);
    assert!(view.read_with(cx, |view, _| view.diff.as_ref().unwrap().stale));
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let pane = view.diff.as_ref().expect("a refreshed comparison");
        assert_eq!(view.mode, ToolMode::Diff);
        assert!(!pane.stale);
        assert_eq!((pane.result.additions, pane.result.deletions), (1, 1));
    });
}

#[gpui_kit::test]
fn clear_drops_the_diff_with_the_text(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let slots = diff_slots();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&view, 0, "one\ntwo\n", window, cx);
        set_input(&view, 1, "one\nTWO\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
        window.render_frame(cx);
        window.click("tool-clear", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "");
        assert_eq!(input_text(view, 1, cx), "");
        assert!(
            view.diff.is_none(),
            "clearing the text must drop the diff computed from it"
        );
        // Nothing left to look at: the dialog returns to the paste editors.
        assert_eq!(view.mode, ToolMode::Edit);
    });
    assert_eq!(store.load(ToolKind::DiffChecker, &slots[0]).unwrap(), "");
    assert_eq!(store.load(ToolKind::DiffChecker, &slots[1]).unwrap(), "");
}

#[gpui_kit::test]
fn diff_inputs_persist_under_their_own_slots(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, store) = fixture();
    let slots = diff_slots();
    store
        .save(ToolKind::DiffChecker, &slots[0], "old text")
        .unwrap();
    store
        .save(ToolKind::DiffChecker, &slots[1], "new text")
        .unwrap();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    view.read_with(cx, |view, cx| {
        assert_eq!(input_text(view, 0, cx), "old text");
        assert_eq!(input_text(view, 1, cx), "new text");
    });
    // Typing in the first editor autosaves that slot only.
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.input("!", cx);
    });
    cx.background_executor.advance_clock(SAVE_DEBOUNCE * 2);
    cx.run_until_parked();
    let typed = view.read_with(cx, |view, cx| input_text(view, 0, cx));
    assert!(typed.contains('!'), "{typed:?}");
    assert_eq!(store.load(ToolKind::DiffChecker, &slots[0]).unwrap(), typed);
    assert_eq!(
        store.load(ToolKind::DiffChecker, &slots[1]).unwrap(),
        "new text"
    );
    // The two sides are separate files, not one shared slot.
    assert_ne!(
        store.path(ToolKind::DiffChecker, &slots[0]),
        store.path(ToolKind::DiffChecker, &slots[1])
    );
}

#[gpui_kit::test]
fn the_edit_mode_gives_the_editors_the_whole_dialog(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        let old = window.find(("tool-input", 0usize)).bounds();
        let new = window.find(("tool-input", 1usize)).bounds();
        // Old and new are the two halves of one row...
        assert!(
            old.right() <= new.left(),
            "the paste areas must be side by side: {old:?} {new:?}"
        );
        assert!(
            (old.size.height - new.size.height).abs() < px(1.),
            "the paste areas must share one height: {old:?} {new:?}"
        );
        // ...and with no comparison sharing the dialog they take all of it:
        // only the actions and the dialog chrome sit above and below them.
        let viewport = window.viewport_size();
        assert!(
            old.size.height > viewport.height / 2.,
            "the editors should own the dialog: {old:?} in {viewport:?}"
        );
        assert!(
            viewport.height - old.bottom() < px(80.),
            "the editors should reach the actions: {old:?} in {viewport:?}"
        );
    });
    // The dialog opens on the paste editors.
    assert_eq!(view.read_with(cx, |view, _| view.mode), ToolMode::Edit);
}

#[gpui_kit::test]
fn reopening_the_dialog_keeps_the_shown_diff(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (tool, cx) = open_with_root(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        set_input(&tool, 0, "one\ntwo\n", window, cx);
        set_input(&tool, 1, "one\nTWO\n", window, cx);
        window.render_frame(cx);
        window.click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    cx.update(|window, cx| ToolView::open_dialog(tool.clone(), window, cx));
    cx.update(|window, cx| {
        window.render_frame(cx);
        assert!(window.has_active_dialog(cx), "the tool dialog must open");
    });
    // The view outlives the dialog, so reopening shows the same comparison
    // instead of an empty pane.
    tool.read_with(cx, |view, _| {
        assert!(view.diff.is_some(), "reopening must keep the computed diff");
    });
    cx.simulate_keystrokes("escape");
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    tool.read_with(cx, |view, _| {
        assert!(
            view.diff.is_some(),
            "dismissing must keep the computed diff"
        );
    });
}

/// The comparison has to own the dialog in its mode, with only the actions
/// below it; this pins that the shell gives it the whole clamped height
/// instead of clipping it.
#[gpui_kit::test]
fn the_diff_mode_gives_the_comparison_the_whole_dialog(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (view, cx) = open_with_root(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| ToolView::open_dialog(view.clone(), window, cx));
    cx.update(|window, cx| {
        set_input(&view, 0, "one\ntwo\nthree\n", window, cx);
        set_input(&view, 1, "one\nTWO\nthree\n", window, cx);
        window.render_frame(cx);
        // The harness renders the same view, so scope the click to the dialog.
        window.within("dialog-0").click("tool-diff", cx);
    });
    // The comparison runs on the background executor.
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        // The dialog layer would leave a tenth of the viewport above a dialog
        // by default; the tool asks for a small top margin instead, so the
        // full-height surface sits near the top of the window.
        let dialog_bounds = window.find("dialog-0").bounds();
        assert!(
            dialog_bounds.top() <= px(64.),
            "the dialog should start near the top: {dialog_bounds:?}"
        );
        let dialog = window.within("dialog-0");
        let result = dialog.find("tool-diff-result").bounds();
        let actions = dialog.find("tool-diff").bounds();
        assert!(
            result.bottom() <= actions.top(),
            "the pane must sit above the actions: {result:?} {actions:?}"
        );
        assert!(
            result.size.height > viewport.height / 2.,
            "the pane should own the dialog: {result:?} in {viewport:?}"
        );
        assert!(
            viewport.height - actions.bottom() < px(140.),
            "the actions should sit near the dialog bottom: {actions:?} in {viewport:?}"
        );
    });
    // Running Diff lands on the comparison.
    assert_eq!(view.read_with(cx, |view, _| view.mode), ToolMode::Diff);
}

#[gpui_kit::test]
fn the_language_picker_belongs_to_the_diff_tool_only(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (diff_tool, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    let select = diff_tool.read_with(cx, |view, _| {
        view.language_select.clone().expect("a language picker")
    });
    cx.update(|window, cx| {
        window.render_frame(cx);
        // The picker is a stock Select whose id is its state's entity id.
        let id: gpui_kit::ElementId = ("select", select.entity_id()).into();
        assert!(
            window.try_find(id).is_some(),
            "the language picker must be rendered"
        );
    });

    // An in-place tool has no comparison to color, so it has no picker.
    let (_dir, root, _store) = fixture();
    let (formatter, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    formatter.read_with(cx, |view, _| {
        assert!(view.language_select.is_none());
    });
}

#[gpui_kit::test]
fn the_diff_tool_offers_only_its_own_actions(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, root, _store) = fixture();
    let (_view, cx) = open_tool(cx, ToolKind::DiffChecker, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        // The primary action is Diff...
        assert!(window.try_find("tool-diff").is_some());
        // ...and neither the in-place Format action nor Copy is part of this
        // tool: it has two documents and a comparison, so there is no single
        // "the text" to hand out.
        assert!(window.try_find("tool-format").is_none());
        assert!(window.try_find("tool-copy").is_none());
    });
    // An in-place tool still offers Copy.
    let (_dir, root, _store) = fixture();
    let (_view, cx) = open_tool(cx, ToolKind::JsonFormatter, root);
    cx.update(|window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tool-copy").is_some());
    });
}
