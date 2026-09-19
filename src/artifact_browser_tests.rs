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
    assert_eq!(Kind::ALL.len(), 3);
    assert_eq!(Kind::Rfc.label(), "RFC");
    assert_eq!(Kind::Plan.label(), "Plan");
    assert_eq!(Kind::Note.label(), "Note");
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
        let anchor = saved.artifact.comments[0].anchor.as_ref().unwrap().block();
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
