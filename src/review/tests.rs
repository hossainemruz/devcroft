use super::*;
use std::{fs, process::Command};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
}

pub(super) fn setup(cx: &mut gpui_kit::TestAppContext) -> (tempfile::TempDir, Entity<ReviewView>) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    for path in ["a.txt", "b.txt", "manual.txt"] {
        fs::write(dir.path().join(path), "original\n").unwrap();
    }
    commit(dir.path());
    git(dir.path(), &["checkout", "-b", "review"]);
    for path in ["a.txt", "b.txt", "manual.txt"] {
        fs::write(dir.path().join(path), "reviewed\n").unwrap();
    }
    cx.update(gpui_kit::init);
    let view = cx.new(|cx| ReviewView::new(dir.path(), cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(matches!(view.state, ReviewState::Loaded(_)))
    });
    (dir, view)
}

fn commit(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "fixture"]);
}

fn assert_progress(view: &ReviewView, path: &str, viewed: bool, collapsed: bool) {
    assert_eq!(
        view.viewed.borrow().contains(path),
        viewed,
        "viewed: {path}"
    );
    assert_eq!(
        view.collapsed.borrow().contains(path),
        collapsed,
        "collapsed: {path}"
    );
    let ReviewState::Loaded(loaded) = &view.state else {
        panic!("expected loaded review");
    };
    let file_ix = loaded
        .diff
        .files
        .iter()
        .position(|file| file.path == path)
        .unwrap();
    let rows = loaded
        .rows
        .iter()
        .filter(|row| row.file() == file_ix)
        .count();
    if collapsed {
        assert_eq!(rows, 1, "collapsed file has only a header");
    } else {
        assert!(rows > 1, "expanded file includes its diff");
    }
}

#[gpui_kit::test]
fn reload_reopens_changed_viewed_files_and_preserves_other_progress(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (dir, view) = setup(cx);
    view.update(cx, |view, cx| {
        view.toggle_viewed("a.txt", cx);
        view.toggle_viewed("b.txt", cx);
        view.toggle_collapsed("manual.txt", cx);
    });
    // Same addition/deletion counts, different source text.
    fs::write(dir.path().join("a.txt"), "updated!\n").unwrap();
    fs::write(dir.path().join("manual.txt"), "updated!\n").unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_progress(view, "a.txt", false, false);
        assert_progress(view, "b.txt", true, true);
        assert_progress(view, "manual.txt", false, true);
    });
    // A file can be reviewed again at its new version.
    view.update(cx, |view, cx| {
        view.toggle_viewed("a.txt", cx);
        view.reload(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "a.txt", true, true));
}

#[gpui_kit::test]
fn background_refresh_keeps_unchanged_rows_and_reopens_changed_files(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (dir, view) = setup(cx);
    let before = view.update(cx, |view, cx| {
        view.toggle_viewed("a.txt", cx);
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        loaded.clone()
    });
    view.update(cx, |view, cx| {
        view.refresh(cx);
        let generation = view.generation;
        view.refresh(cx);
        assert_eq!(
            view.generation, generation,
            "background loads do not overlap"
        );
        assert!(matches!(view.state, ReviewState::Loaded(_)));
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        assert!(
            Rc::ptr_eq(&before, loaded),
            "unchanged refresh leaves UI state intact"
        );
        assert_progress(view, "a.txt", true, true);
    });
    fs::write(dir.path().join("a.txt"), "updated!\n").unwrap();
    view.update(cx, |view, cx| view.refresh(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "a.txt", false, false));
}

#[gpui_kit::test]
fn rendered_review_reopens_changed_files_without_moving_the_current_file(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::test::TestWindowExt as _;
    let (dir, view) = setup(cx);
    fs::write(dir.path().join("b.txt"), "reviewed\n".repeat(100)).unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    let render_view = view.clone();
    let (_, cx) = cx
        .add_window_view(move |window, cx| gpui_kit::component::Root::new(render_view, window, cx));
    cx.update(|window, cx| {
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            view.toggle_viewed("a.txt", cx);
            let ReviewState::Loaded(loaded) = &view.state else {
                unreachable!()
            };
            view.list_handle.scroll_to(ListOffset {
                item_ix: loaded.file_row_start[1] + 10,
                offset_in_item: px(0.),
            });
        });
        window.render_frame(cx);
    });
    let previous_row = view.read_with(cx, |view, _| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        assert_eq!(view.last_scrolled.as_deref(), Some("b.txt"));
        view.list_handle.logical_scroll_top().item_ix - loaded.file_row_start[1]
    });
    fs::write(dir.path().join("a.txt"), "updated!\n").unwrap();
    view.update(cx, |view, cx| view.refresh(cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));
    view.read_with(cx, |view, _| {
        assert_progress(view, "a.txt", false, false);
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        assert_eq!(view.last_scrolled.as_deref(), Some("b.txt"));
        assert_eq!(
            view.list_handle.logical_scroll_top().item_ix - loaded.file_row_start[1],
            previous_row,
            "expanding an earlier file preserves the current viewport"
        );
    });
}

#[gpui_kit::test]
fn returning_to_review_keeps_unchanged_rows(cx: &mut gpui_kit::TestAppContext) {
    let (_dir, view) = setup(cx);
    let before = view.read_with(cx, |view, _| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        loaded.clone()
    });
    view.update(cx, |view, cx| {
        view.activate(cx);
        assert!(matches!(view.state, ReviewState::Loaded(_)));
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        assert!(Rc::ptr_eq(&before, loaded));
    });
}

fn top_line(view: &ReviewView) -> (String, String, gpui_kit::Pixels) {
    let ReviewState::Loaded(loaded) = &view.state else {
        panic!("expected loaded review");
    };
    let top = view.list_handle.logical_scroll_top();
    let stream::StreamRow::Line { file, hunk, line } = loaded.rows[top.item_ix] else {
        panic!("expected visible code line: {:?}", loaded.rows[top.item_ix]);
    };
    let model::FileContent::Text { hunks, .. } = &loaded.diff.files[file].content else {
        unreachable!()
    };
    (
        loaded.diff.files[file].path.clone(),
        hunks[hunk].lines[line].text.clone(),
        top.offset_in_item,
    )
}

#[gpui_kit::test]
fn refresh_preserves_visible_code_when_lines_and_files_move(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    let (dir, view) = setup(cx);
    let content = (0..150)
        .map(|ix| format!("line {ix}\n"))
        .collect::<String>();
    fs::write(dir.path().join("b.txt"), &content).unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    let render_view = view.clone();
    let (_, cx) = cx
        .add_window_view(move |window, cx| gpui_kit::component::Root::new(render_view, window, cx));
    cx.update(|window, cx| {
        window.render_frame(cx);
        view.update(cx, |view, _| {
            let ReviewState::Loaded(loaded) = &view.state else {
                unreachable!()
            };
            view.list_handle.scroll_to(ListOffset {
                item_ix: loaded.file_row_start[1] + 40,
                offset_in_item: px(7.),
            });
        });
        window.render_frame(cx);
    });
    let before = view.read_with(cx, |view, _| top_line(view));
    for (path, value) in [
        ("aa.txt", "new file\n".to_owned()),
        ("a.txt", "original\n".to_owned()),
        ("b.txt", format!("inserted\n{content}")),
    ] {
        fs::write(dir.path().join(path), value).unwrap();
        view.update(cx, |view, cx| view.refresh(cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(top_line(view), before, "after editing {path}");
            assert_eq!(view.last_scrolled.as_deref(), Some("b.txt"));
            let selected = view.tree_state.read(cx).selected_entry().unwrap();
            assert_eq!(file_path_from_id(&selected.item().id), Some("b.txt"));
        });
    }
}

#[gpui_kit::test]
fn commit_only_refresh_does_not_invalidate_the_rendered_list(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    let (dir, view) = setup(cx);
    fs::write(dir.path().join("b.txt"), "reviewed\n".repeat(150)).unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    let render_view = view.clone();
    let (_, cx) = cx
        .add_window_view(move |window, cx| gpui_kit::component::Root::new(render_view, window, cx));
    cx.update(|window, cx| {
        window.render_frame(cx);
        view.update(cx, |view, _| {
            let ReviewState::Loaded(loaded) = &view.state else {
                unreachable!()
            };
            view.list_handle.scroll_to(ListOffset {
                item_ix: loaded.file_row_start[1] + 40,
                offset_in_item: px(7.),
            });
        });
        window.render_frame(cx);
    });
    let (syntax, before, bounds, selection) = view.read_with(cx, |view, cx| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        let top = view.list_handle.logical_scroll_top();
        (
            loaded.syntax.clone(),
            top_line(view),
            view.list_handle.bounds_for_item(top.item_ix).unwrap(),
            view.tree_state.read(cx).selected_index(),
        )
    });
    commit(dir.path());
    view.update(cx, |view, cx| view.refresh(cx));
    cx.run_until_parked();
    // Check before rendering: a list reset would discard measured item bounds.
    view.read_with(cx, |view, cx| {
        let ReviewState::Loaded(loaded) = &view.state else {
            unreachable!()
        };
        assert!(Rc::ptr_eq(&syntax, &loaded.syntax));
        assert_eq!(top_line(view), before);
        assert_eq!(view.tree_state.read(cx).selected_index(), selection);
        assert_eq!(
            view.list_handle
                .bounds_for_item(view.list_handle.logical_scroll_top().item_ix),
            Some(bounds),
        );
    });
    cx.update(|window, cx| window.render_frame(cx));
    view.read_with(cx, |view, _| assert_eq!(top_line(view), before));
}

#[gpui_kit::test]
fn filesystem_events_gate_refreshes_and_queue_changes_during_a_load(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (dir, view) = setup(cx);
    let watched = tempfile::tempdir().unwrap();
    view.update(cx, |view, cx| {
        // Keep OS delivery separate from the deterministic queue interleaving.
        view.file_changes = watch::FileChanges::new(watched.path());
        assert!(
            view.file_changes.is_watching(),
            "fixture watcher is available"
        );
        view.file_changes.take_changed();
        let generation = view.generation;
        view.refresh_if_changed(cx);
        assert_eq!(view.generation, generation, "idle ticks do not reload");
        view.file_changes.mark_changed();
        view.refresh_if_changed(cx);
        assert_eq!(view.generation, generation + 1);
        view.file_changes.mark_changed();
        view.refresh_if_changed(cx);
        assert_eq!(
            view.generation,
            generation + 1,
            "writes during a load stay queued"
        );
    });
    cx.run_until_parked();
    fs::write(dir.path().join("a.txt"), "updated!\n").unwrap();
    view.update(cx, |view, cx| {
        view.toggle_viewed("a.txt", cx);
        view.refresh_if_changed(cx);
        assert!(view.load_in_flight, "queued event starts the next refresh");
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "a.txt", false, false));
}

#[gpui_kit::test]
fn commits_preserve_unchanged_viewed_files_but_branch_and_scope_switches_reset_them(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (dir, view) = setup(cx);
    view.update(cx, |view, cx| view.toggle_viewed("b.txt", cx));
    commit(dir.path());
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "b.txt", true, true));
    git(dir.path(), &["checkout", "-b", "another-review"]);
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "b.txt", false, false));
    view.update(cx, |view, cx| {
        view.toggle_viewed("b.txt", cx);
        view.scope_tab = ScopeTab::Uncommitted;
        view.reload(cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(view.viewed.borrow().is_empty());
        assert!(view.collapsed.borrow().is_empty());
        assert!(view.viewed_versions.is_empty());
    });
}

#[gpui_kit::test]
fn removed_and_renamed_files_do_not_keep_stale_marks(cx: &mut gpui_kit::TestAppContext) {
    let (dir, view) = setup(cx);
    view.update(cx, |view, cx| {
        view.toggle_viewed("a.txt", cx);
        view.toggle_viewed("b.txt", cx);
    });
    fs::write(dir.path().join("a.txt"), "original\n").unwrap();
    fs::rename(dir.path().join("b.txt"), dir.path().join("renamed.txt")).unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(!view.viewed.borrow().contains("a.txt"));
        assert!(!view.collapsed.borrow().contains("a.txt"));
        assert_progress(view, "b.txt", false, false);
        assert_progress(view, "renamed.txt", false, false);
        assert!(view.viewed_versions.is_empty());
    });
}

#[gpui_kit::test]
fn a_failed_refresh_preserves_progress_until_a_successful_load(cx: &mut gpui_kit::TestAppContext) {
    let (dir, view) = setup(cx);
    view.update(cx, |view, cx| view.toggle_viewed("a.txt", cx));
    git(dir.path(), &["branch", "-m", "main", "base-unavailable"]);
    view.update(cx, |view, cx| view.refresh(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_progress(view, "a.txt", true, true);
        assert!(!view.load_in_flight);
    });
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert!(matches!(view.state, ReviewState::Failed(_)))
    });
    git(dir.path(), &["branch", "-m", "base-unavailable", "main"]);
    fs::write(dir.path().join("a.txt"), "updated!\n").unwrap();
    view.update(cx, |view, cx| view.reload(cx));
    cx.run_until_parked();
    view.read_with(cx, |view, _| assert_progress(view, "a.txt", false, false));
}
