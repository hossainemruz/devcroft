use super::*;

#[test]
fn portable_urls_round_trip_without_device_paths() {
    for reference in [
        Reference::Repository("backend".into()),
        Reference::Artifact("art-23456789".into()),
        Reference::Session {
            repository: "backend".into(),
            provider: "codex".into(),
            id: "conversation:hello world+✓".into(),
        },
    ] {
        assert_eq!(reference.url().parse::<Reference>().unwrap(), reference);
    }
    for url in [
        "https://example.com",
        "devcroft:repository/../secrets",
        "devcroft:repository/%2Ftmp",
        "devcroft:artifact/unknown",
        "devcroft:session/backend/omp/id",
        "devcroft:session/backend/codex/-option",
        "devcroft:session/backend/codex/%00",
        "devcroft:session/backend/codex/%GG",
        "devcroft:session/backend/codex/%FF",
        "devcroft:session/backend/codex/id/extra",
        "devcroft:session/backend/codex/id?store=/tmp",
        "devcroft:repository/backend#section",
    ] {
        assert!(url.parse::<Reference>().is_err(), "accepted {url}");
    }
}

#[test]
fn metadata_scan_ignores_code_and_external_links() {
    let source = "See [**Backend** `API`](devcroft:repository/backend).";
    let ast = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap();
    let link = &ast.children().unwrap()[0].children().unwrap()[1];
    assert_eq!(plain_text(link), "Backend API");
    let markdown_ast::Node::Link(link) = link else {
        panic!("expected a link")
    };
    assert_eq!(
        link.url.parse::<Reference>().unwrap(),
        Reference::Repository("backend".into())
    );
    let details = load_details(
        "`[Backend](devcroft:repository/backend)`\n\n```md\n[Backend](devcroft:repository/backend)\n```\n\n[Web](https://example.com)",
        None,
    );
    assert!(details.is_empty());
    let details = load_details(source, None);
    assert_eq!(details.len(), 1);
    assert!(
        details
            .values()
            .next()
            .unwrap()
            .description
            .contains("unavailable")
    );
}

#[test]
fn session_resolution_requires_repository_and_unique_local_store() {
    let checkout = std::path::PathBuf::from("/tmp/reference-test-repository");
    let mut session = SessionSummary {
        key: SessionKey {
            provider: "codex".into(),
            store: "/tmp/store-one".into(),
            id: "id".into(),
        },
        cwd: checkout.clone(),
        checkout: checkout.clone(),
        title: "Discussion".into(),
        updated: 0,
        timestamp_source: "test".into(),
    };
    assert_eq!(
        session_key_for(&[session.clone()], &checkout, "codex", "id").unwrap(),
        session.key
    );
    assert!(
        session_key_for(
            &[session.clone()],
            std::path::Path::new("/tmp/another-repository"),
            "codex",
            "id"
        )
        .is_err()
    );
    let original = session.clone();
    session.key.store = "/tmp/store-two".into();
    assert!(
        session_key_for(&[original, session], &checkout, "codex", "id")
            .unwrap_err()
            .to_string()
            .contains("More than one")
    );
    assert!(session_key_for(&[], &checkout, "codex", "id").is_err());
}

#[test]
fn metadata_lookup_is_bounded_and_deduplicated() {
    let source = (0..200)
        .map(|i| format!("[Repo](devcroft:repository/repo-{i}) "))
        .collect::<String>();
    assert_eq!(load_details(&source, None).len(), MAX_REFERENCES);
    assert_eq!(
        load_details(
            "[A](devcroft:repository/backend) [B](devcroft:repository/backend)",
            None
        )
        .len(),
        1
    );
}

#[gpui_kit::test]
fn inline_references_render_copy_and_activate(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::component::{
        Root,
        text::{SelectionFormat, TextView, TextViewState},
    };
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Context, Entity, IntoElement, Render};
    use std::sync::Mutex;
    struct Harness {
        text: Entity<TextViewState>,
        opened: Arc<Mutex<Vec<Reference>>>,
    }
    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let opened = self.opened.clone();
            TextView::new(&self.text).plugin(ReferencePlugin {
                details: Default::default(),
                open: Arc::new(move |reference, _, _| opened.lock().unwrap().push(reference)),
            })
        }
    }
    cx.update(gpui_kit::init);
    let source = "Before [Backend API](devcroft:repository/backend) after.";
    let text = cx.new(|cx| TextViewState::markdown(source, cx));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = text.clone();
    let opened = captured.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let view = cx.new(|_| Harness {
            text: state,
            opened,
        });
        Root::new(view, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.render_frame(cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.render_frame(cx);
        text.update(cx, |state, cx| {
            state.select_all(cx);
            assert_eq!(
                state.selected_text().trim_end(),
                "Before Backend API after."
            );
            state.set_selection_format(SelectionFormat::Source, cx);
            assert_eq!(state.selected_text(), source);
        });
        window.click("reference-open", cx);
    });
    assert_eq!(
        *captured.lock().unwrap(),
        vec![Reference::Repository("backend".into())]
    );
}
