use super::*;
use gpui_kit::test::TestWindowExt as _;
#[test]
fn refresh_coalesces_requests_and_discards_superseded_results() {
    let mut refresh = Refresh::default();
    let first = refresh.request().unwrap();
    assert!(refresh.request().is_none());
    assert!(refresh.request().is_none());
    assert!(refresh.pending);
    assert!(!refresh.finish(first));
    let latest = refresh.request().unwrap();
    assert!(!refresh.pending);
    assert!(refresh.finish(latest));
    assert!(!refresh.busy);
}

#[test]
fn kind_labels_cover_every_filter_option() {
    assert_eq!(Kind::ALL.len(), 5);
    assert_eq!(Kind::Rfc.label(), "RFC");
    assert_eq!(Kind::Plan.label(), "Plan");
    assert_eq!(Kind::Note.label(), "Note");
    assert_eq!(Kind::Review.label(), "Review");
    assert_eq!(Kind::Tutorial.label(), "Tutorial");
}

#[gpui_kit::test]
fn tutorial_selection_swaps_the_markdown_reader_for_the_sandboxed_viewer(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/repo/repository.json"),
        &serde_json::json!({"key": "repo"}),
    )
    .unwrap();
    let store = ArtifactStore::new(&root);
    let tutorial = store
        .create(NewArtifact {
            repository: Some("repo".into()),
            sessions: vec![],
            title: "Two records".into(),
            kind: Kind::Tutorial,
            content: "<!doctype html><html><body><p>Two records</p></body></html>".into(),
        })
        .unwrap();
    let note = store
        .create(NewArtifact {
            repository: Some("repo".into()),
            sessions: vec![],
            title: "Note".into(),
            kind: Kind::Note,
            content: "# Note".into(),
        })
        .unwrap();
    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    browser.update(cx, |browser, cx| {
        browser.select(Some(tutorial.clone()), cx);
        assert!(
            browser.tutorial.is_some(),
            "tutorial uses the sandboxed viewer"
        );
        assert!(
            browser.preview.is_none(),
            "tutorial never builds a Markdown reader"
        );
        assert!(browser.toc.is_empty(), "tutorials do not feed the outline");
        assert!(
            !browser.navigation_state().editable,
            "tutorials expose no edit or comment rows"
        );

        // Leaving the tab hides the native view but keeps it for re-entry.
        browser.set_active(false, cx);
        assert!(browser.tutorial.is_some());
        browser.set_active(true, cx);
        assert!(browser.tutorial.is_some());

        // A new revision refreshes the existing viewer instead of rebuilding it.
        let mut updated = tutorial.clone();
        updated.artifact.content = "<!doctype html><p>Updated</p>".into();
        browser.select(Some(updated), cx);
        assert!(browser.tutorial.is_some());
        assert!(browser.preview.is_none());

        browser.select(Some(note), cx);
        assert!(
            browser.tutorial.is_none(),
            "Markdown selection drops the tutorial viewer"
        );
        assert!(
            browser.preview.is_some(),
            "Markdown selection uses the reader"
        );
        assert!(
            browser.navigation_state().editable,
            "Markdown resources keep edit and comment rows"
        );
    });
}

#[gpui_kit::test]
fn tutorials_reject_edit_and_comment_commands(cx: &mut gpui_kit::TestAppContext) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/repo/repository.json"),
        &serde_json::json!({"key": "repo"}),
    )
    .unwrap();
    let store = ArtifactStore::new(&root);
    let tutorial = store
        .create(NewArtifact {
            repository: Some("repo".into()),
            sessions: vec![],
            title: "Two records".into(),
            kind: Kind::Tutorial,
            content: "<!doctype html><html><body><p>Two records</p></body></html>".into(),
        })
        .unwrap();
    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    browser.update(cx, |browser, cx| browser.select(Some(tutorial), cx));
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            browser.begin_markdown_edit(window, cx);
            browser.begin_comment(window, cx);
            assert!(
                browser.draft.is_none(),
                "tutorials never open a draft editor"
            );
        });
    });
}

#[test]
fn updated_label_buckets_like_relative_durations() {
    let now_secs = 1_700_000_000;
    let ms = |secs_ago: i64| ((now_secs - secs_ago) as u64).saturating_mul(1_000);
    assert_eq!(updated_label(ms(0), now_secs), "Updated just now");
    assert_eq!(updated_label(ms(30), now_secs), "Updated just now");
    assert_eq!(updated_label(ms(300), now_secs), "Updated 5m ago");
    assert_eq!(updated_label(ms(7_200), now_secs), "Updated 2h ago");
    assert_eq!(updated_label(ms(259_200), now_secs), "Updated 3d ago");
    // Future timestamps never render a negative duration.
    assert_eq!(
        updated_label(((now_secs + 600) as u64).saturating_mul(1_000), now_secs),
        "Updated just now"
    );
}

#[gpui_kit::test]
fn direct_reference_opens_archived_resource_and_preserves_drafts(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/repo/repository.json"),
        &serde_json::json!({"key": "repo"}),
    )
    .unwrap();
    let store = ArtifactStore::new(&root);
    for index in 0..3 {
        let target = store
            .create(NewArtifact {
                repository: Some("repo".into()),
                sessions: vec![],
                title: format!("Target {index}"),
                kind: Kind::Note,
                content: "# Target".into(),
            })
            .unwrap();
        store
            .set_archived(&target.artifact.id, &target.revision, true)
            .unwrap();
    }
    let all = store
        .list(&ListOptions {
            include_archived: true,
            ..Default::default()
        })
        .unwrap();
    // This target is both archived and outside the one-record page used below.
    let id = all.artifacts.last().unwrap().artifact.id.clone();
    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    browser.update(cx, |browser, cx| {
        browser.kind_filter = Some(Kind::Plan);
        browser.limit = 1;
        browser.open_by_id(id.clone(), cx).unwrap();
    });
    cx.run_until_parked();
    browser.update(cx, |browser, cx| {
        assert_eq!(browser.selected_id.as_ref(), Some(&id));
        assert!(browser.include_archived);
        assert!(browser.kind_filter.is_none());
        assert!(browser.visible_artifacts().any(|s| s.artifact.id == id));
        browser.refresh(cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            assert_eq!(browser.selected_id.as_ref(), Some(&id));
            browser.begin_markdown_edit(window, cx);
            assert!(browser.open_by_id(id.clone(), cx).is_err());
            assert!(browser.draft.is_some());
            assert_eq!(browser.selected_id.as_ref(), Some(&id));
        })
    });
}

#[gpui_kit::test]
fn block_comment_editor_keeps_reader_and_persists_anchor(cx: &mut gpui_kit::TestAppContext) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/repo/repository.json"),
        &serde_json::json!({"key":"repo"}),
    )
    .unwrap();
    let store = ArtifactStore::new(&root);
    let snapshot = store.create(NewArtifact {
        repository: Some("repo".into()), sessions: vec![], title: "Block feedback".into(), kind: Kind::Note,
        content: "# Example\n\nParagraph with **bold** and `code`.\n\n- First\n- Second\n\n```rs\nlet x = 1;\n```\n".into(),
    }).unwrap();
    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    browser.update(cx, |browser, cx| browser.select(Some(snapshot.clone()), cx));
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }
    cx.update(|window, cx| {
        window.click(("block-comments", 1usize), cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            assert!(browser.show_comments);
            let draft = browser.draft.as_ref().unwrap();
            assert!(!draft.document);
            assert_eq!(draft.block, Some(1));
            assert!(browser.preview.is_some());
            draft
                .input
                .update(cx, |input, cx| input.set_value("Explain this", window, cx));
            browser.save_draft(window, cx);
        });
    });
    cx.run_until_parked();
    browser.update(cx, |browser, _| {
        assert!(browser.draft.is_none(), "{:?}", browser.error);
        let saved = browser.selected.as_ref().unwrap();
        assert_eq!(saved.artifact.comments.len(), 1);
        let anchor = saved.artifact.comments[0]
            .anchor
            .as_ref()
            .unwrap()
            .location();
        assert_eq!(anchor.source, "Paragraph with **bold** and `code`.");
    });
    for _ in 0..2 {
        cx.update(|window, cx| window.render_frame(cx));
        cx.run_until_parked();
    }
    assert_eq!(
        store
            .get(&snapshot.artifact.id)
            .unwrap()
            .artifact
            .comments
            .len(),
        1
    );
}

#[gpui_kit::test]
fn space_switching_stashes_drafts_and_never_shows_another_spaces_editor(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    for (key, space) in [("work", "Work"), ("home", "Personal")] {
        crate::data::write_json_atomic(
            &root
                .portable_dir()
                .join(format!("repositories/{key}/repository.json")),
            &serde_json::json!({"key": key, "space": space}),
        )
        .unwrap();
    }
    crate::data::ensure_spaces(&root).unwrap();
    let store = ArtifactStore::new(&root);
    let new = |repository: &str, title: &str| {
        store
            .create(NewArtifact {
                repository: Some(repository.into()),
                sessions: vec![],
                title: title.into(),
                kind: Kind::Note,
                content: format!("# {title}\n"),
            })
            .unwrap()
    };
    let work = new("work", "Work doc");
    let home = new("home", "Home doc");

    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    browser.update(cx, |browser, cx| {
        browser.set_space("Work".into(), cx);
        browser.set_active(true, cx);
    });
    cx.run_until_parked();
    browser.update(cx, |browser, _| {
        assert_eq!(browser.selected_id.as_ref(), Some(&work.artifact.id));
    });
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            browser.begin_markdown_edit(window, cx);
            assert!(browser.draft.is_some());
            assert!(
                browser.has_draft(),
                "Settings must refuse catalog edits while a draft is unsaved"
            );
        })
    });
    cx.run_until_parked();

    browser.update(cx, |browser, cx| browser.set_space("Personal".into(), cx));
    cx.run_until_parked();
    browser.update(cx, |browser, _| {
        assert!(
            browser.draft.is_none(),
            "another space's draft must not be shown"
        );
        assert_eq!(browser.selected_id.as_ref(), Some(&home.artifact.id));
        assert!(
            browser.has_draft(),
            "a stashed draft still blocks catalog edits"
        );
    });

    browser.update(cx, |browser, cx| browser.set_space("Work".into(), cx));
    cx.run_until_parked();
    browser.update(cx, |browser, _| {
        assert_eq!(
            browser
                .draft
                .as_ref()
                .map(|draft| draft.snapshot.artifact.id.clone()),
            Some(work.artifact.id.clone()),
            "the stashed draft comes back with its space"
        );
    });
}

#[gpui_kit::test]
fn repository_scoped_drafts_report_their_resolved_space(cx: &mut gpui_kit::TestAppContext) {
    use crate::data::artifacts::NewArtifact;
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/work/repository.json"),
        &serde_json::json!({"key": "work", "space": "Work"}),
    )
    .unwrap();
    crate::data::ensure_spaces(&root).unwrap();
    let store = ArtifactStore::new(&root);
    store
        .create(NewArtifact {
            repository: Some("work".into()),
            sessions: vec![],
            title: "Work doc".into(),
            kind: Kind::Note,
            content: "# Work\n".into(),
        })
        .unwrap();

    cx.update(gpui_kit::init);
    // The repository Resources browser has no space filter, so its drafts
    // must resolve their space from the artifact snapshot instead.
    let browser = cx.new(|cx| {
        ArtifactBrowser::scoped(Some(root), Scope::Repository(Some("work".to_owned())), cx)
    });
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    browser.update(cx, |browser, cx| browser.set_active(true, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            browser.begin_markdown_edit(window, cx);
            assert!(browser.draft.is_some());
            assert!(
                browser.has_draft(),
                "repository-scoped drafts block catalog edits too"
            );
        })
    });
}

#[gpui_kit::test]
fn selected_text_draft_saves_selection_instead_of_containing_block(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::data::artifacts::{CommentAnchor, NewArtifact};
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    crate::data::write_json_atomic(
        &root
            .portable_dir()
            .join("repositories/repo/repository.json"),
        &serde_json::json!({"key":"repo"}),
    )
    .unwrap();
    let store = ArtifactStore::new(&root);
    let content = "# Title\n\nFirst **word**, second **word**.";
    let start = content.rfind("word").unwrap();
    let snapshot = store
        .create(NewArtifact {
            repository: Some("repo".into()),
            sessions: vec![],
            title: "Selection feedback".into(),
            kind: Kind::Note,
            content: content.into(),
        })
        .unwrap();
    cx.update(gpui_kit::init);
    let browser = cx.new(|cx| ArtifactBrowser::new(Some(root), cx));
    browser.update(cx, |browser, cx| browser.select(Some(snapshot.clone()), cx));
    let view = browser.clone();
    let (_, cx) =
        cx.add_window_view(move |window, cx| gpui_kit::component::Root::new(view, window, cx));
    cx.update(|window, cx| {
        browser.update(cx, |browser, cx| {
            browser.begin_comment(window, cx);
            let draft = browser.draft.as_mut().unwrap();
            draft.block = Some(1);
            draft.selection = Some(start..start + 4);
            draft.quote = Some("word".into());
            draft.input.update(cx, |input, cx| {
                input.set_value("Explain the second word", window, cx)
            });
            browser.save_draft(window, cx);
        });
    });
    cx.run_until_parked();
    browser.update(cx, |browser, _| {
        assert!(browser.draft.is_none(), "{:?}", browser.error);
    });
    let saved = store.get(&snapshot.artifact.id).unwrap();
    let anchor = saved.artifact.comments[0].anchor.as_ref().unwrap();
    assert!(matches!(anchor, CommentAnchor::Selection(_)));
    assert_eq!(anchor.location().start, start);
    assert_eq!(anchor.location().source, "word");
    assert_eq!(anchor.location().start_line, 3);
}
