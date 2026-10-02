use super::*;
fn fixture() -> (tempfile::TempDir, ArtifactStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(&DataRoot::new(dir.path().to_owned()));
    for key in ["repo", "other"] {
        let path = store.root.portable_dir().join("repositories").join(key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("repository.json"), "{}").unwrap();
    }
    (dir, store)
}

/// Fixture whose repositories carry spaces: `repo` in Work, `other` in
/// Personal, seeded through the portable catalog.
fn spaced_fixture() -> (tempfile::TempDir, ArtifactStore) {
    let (dir, store) = fixture();
    for (key, space) in [("repo", "Work"), ("other", "Personal")] {
        let path = store.root.portable_dir().join("repositories").join(key);
        fs::write(
            path.join("repository.json"),
            serde_json::json!({ "key": key, "space": space }).to_string(),
        )
        .unwrap();
    }
    crate::data::ensure_spaces(&store.root).unwrap();
    (dir, store)
}

#[test]
fn artifact_space_follows_its_repository_and_survives_removal() {
    let (_dir, store) = spaced_fixture();
    let created = store.create(input("repo")).unwrap();
    assert_eq!(created.artifact.space, "Work");
    assert_eq!(
        store.get(&created.artifact.id).unwrap().artifact.space,
        "Work"
    );

    // Moving the repository moves its artifacts without touching the file.
    let path = store
        .root
        .portable_dir()
        .join("repositories/repo/repository.json");
    fs::write(
        &path,
        serde_json::json!({ "key": "repo", "space": "Personal" }).to_string(),
    )
    .unwrap();
    let moved = store.get(&created.artifact.id).unwrap();
    assert_eq!(moved.artifact.space, "Personal");
    assert_eq!(
        moved.revision, created.revision,
        "reads never rewrite the record"
    );

    // Removing the repository leaves the space captured when the artifact
    // was written (Work) as the fallback: the later repository edit only
    // ever lived in the repository record.
    fs::remove_dir_all(store.root.portable_dir().join("repositories/repo")).unwrap();
    assert_eq!(
        store.get(&created.artifact.id).unwrap().artifact.space,
        "Work"
    );
    let list = store.list(&ListOptions::default()).unwrap();
    assert_eq!(list.artifacts.len(), 1);
    assert_eq!(list.artifacts[0].artifact.space, "Work");
}

#[test]
fn legacy_artifacts_without_a_space_resolve_from_their_repository() {
    let (_dir, store) = spaced_fixture();
    let created = store.create(input("repo")).unwrap();
    // Rewrite the record the way a pre-spaces build would have: no `space`.
    let path = store
        .checked_path(Some(&created.artifact.id))
        .unwrap()
        .join("artifact.md");
    let text = fs::read_to_string(&path).unwrap();
    let (metadata, content) = text
        .trim_start_matches("---\n")
        .split_once("\n---\n")
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(metadata).unwrap();
    value.as_object_mut().unwrap().remove("space");
    fs::write(
        &path,
        format!(
            "---\n{}\n---\n{content}",
            serde_json::to_string_pretty(&value).unwrap()
        ),
    )
    .unwrap();

    let resolved = store.get(&created.artifact.id).unwrap();
    assert_eq!(resolved.artifact.space, "Work");
}

#[test]
fn list_filters_by_space_before_the_page_limit() {
    let (_dir, store) = spaced_fixture();
    store.create(input("other")).unwrap();
    store.create(input("repo")).unwrap();
    let options = ListOptions {
        space: Some("Work".into()),
        limit: Some(1),
        ..Default::default()
    };
    let list = store.list(&options).unwrap();
    // The Personal artifact must not consume the one-slot page.
    assert_eq!(list.artifacts.len(), 1);
    assert_eq!(list.artifacts[0].artifact.space, "Work");
    assert!(!list.truncated);

    let all = store.list(&ListOptions::default()).unwrap();
    assert_eq!(all.artifacts.len(), 2);
}

fn input(repository: &str) -> NewArtifact {
    NewArtifact {
        repository: Some(repository.into()),
        sessions: vec![],
        title: "Plan".into(),
        kind: Kind::Plan,
        content: "# Plan\n\n- [ ] Ship λ\n".into(),
    }
}
#[test]
fn markdown_roundtrip_and_external_edit_conflict() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let path = store
        .checked_path(Some(&first.artifact.id))
        .unwrap()
        .join("artifact.md");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\n{"));
    assert!(text.ends_with(&first.artifact.content));
    assert_eq!(store.get(&first.artifact.id).unwrap(), first);
    fs::write(&path, text.replace("Ship λ", "Ship β")).unwrap();
    assert!(
        store
            .update(
                &first.artifact.id,
                &first.revision,
                ArtifactPatch::default()
            )
            .is_err()
    );
    let edited = store.get(&first.artifact.id).unwrap();
    assert!(edited.artifact.content.contains('β'));
}

fn tutorial(repository: &str, html: &str) -> NewArtifact {
    NewArtifact {
        repository: Some(repository.into()),
        sessions: vec![],
        title: "Change tutorial".into(),
        kind: Kind::Tutorial,
        content: html.into(),
    }
}

#[test]
fn tutorial_roundtrip_keeps_html_and_rejects_comments_and_format_changes() {
    let (_dir, store) = fixture();
    // A body line that is exactly the metadata delimiter must survive the
    // front-matter envelope, and script text is stored verbatim.
    let html = "<!doctype html>\n<html><body>\n<h1>Two records</h1>\n---\n<script>const tag = \"</script>\";</script>\n</body></html>\n";
    let first = store.create(tutorial("repo", html)).unwrap();
    assert_eq!(first.artifact.kind, Kind::Tutorial);
    assert_eq!(first.artifact.content, html);
    assert_eq!(store.get(&first.artifact.id).unwrap(), first);

    // Comments are rejected without changing the record.
    assert!(
        store
            .comment(
                &first.artifact.id,
                &first.revision,
                CommentChange::Create("Feedback".into()),
            )
            .is_err()
    );
    assert!(
        store
            .comment(
                &first.artifact.id,
                &first.revision,
                CommentChange::CreateBlock {
                    body: "Feedback".into(),
                    block: 0,
                    quote: None,
                },
            )
            .is_err()
    );
    assert_eq!(store.get(&first.artifact.id).unwrap(), first);

    // Cross-format kind changes are rejected in both directions.
    assert!(
        store
            .update(
                &first.artifact.id,
                &first.revision,
                ArtifactPatch {
                    kind: Some(Kind::Plan),
                    ..Default::default()
                },
            )
            .is_err()
    );
    let plan = store.create(input("repo")).unwrap();
    assert!(
        store
            .update(
                &plan.artifact.id,
                &plan.revision,
                ArtifactPatch {
                    kind: Some(Kind::Tutorial),
                    ..Default::default()
                },
            )
            .is_err()
    );

    // Markdown-to-Markdown renames remain allowed.
    let renamed = store
        .update(
            &plan.artifact.id,
            &plan.revision,
            ArtifactPatch {
                kind: Some(Kind::Note),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(renamed.artifact.kind, Kind::Note);

    // Updates keep the revision check and replace the HTML body.
    let updated = store
        .update(
            &first.artifact.id,
            &first.revision,
            ArtifactPatch {
                content: Some("<p>Updated</p>".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_ne!(updated.revision, first.revision);
    assert_eq!(updated.artifact.content, "<p>Updated</p>");
    assert_eq!(store.get(&first.artifact.id).unwrap(), updated);
}

#[test]
fn tutorial_content_must_be_a_nonempty_document_while_markdown_may_be_empty() {
    let (_dir, store) = fixture();
    assert!(store.create(tutorial("repo", "  \n")).is_err());
    let html = store.create(tutorial("repo", "<p>Hello</p>")).unwrap();
    assert!(
        store
            .update(
                &html.artifact.id,
                &html.revision,
                ArtifactPatch {
                    content: Some(String::new()),
                    ..Default::default()
                },
            )
            .is_err()
    );
    let empty = store
        .create(NewArtifact {
            content: String::new(),
            ..input("repo")
        })
        .unwrap();
    assert_eq!(empty.artifact.content, "");
}

#[test]
fn repository_filter_order_archive_and_partial_errors() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let second = store.create(input("repo")).unwrap();
    store.create(input("other")).unwrap();
    let updated = store
        .update(
            &first.artifact.id,
            &first.revision,
            ArtifactPatch::default(),
        )
        .unwrap();
    let options = ListOptions {
        repository: Some("repo".into()),
        ..Default::default()
    };
    let list = store.list(&options).unwrap();
    assert_eq!(list.artifacts.len(), 2);
    assert_eq!(list.artifacts[0].artifact.id, first.artifact.id);
    store
        .set_archived(&first.artifact.id, &updated.revision, true)
        .unwrap();
    assert_eq!(
        store.list(&options).unwrap().artifacts[0].artifact.id,
        second.artifact.id
    );
    fs::create_dir(store.checked_path(None).unwrap().join("broken")).unwrap();
    let list = store.list(&options).unwrap();
    assert_eq!(list.artifacts.len(), 1);
    assert_eq!(list.errors.len(), 1);
}
#[test]
fn comments_lifecycle_preserves_document_and_sessions_and_rejects_stale_writes() {
    let (_dir, store) = fixture();
    let mut data = input("repo");
    data.sessions.push(OriginSession {
        repository: "other".into(),
        key: crate::agent_sessions::SessionKey {
            provider: "codex".into(),
            store: "/tmp/sessions".into(),
            id: "session-1".into(),
        },
        title: "Clarification".into(),
    });
    let first = store.create(data).unwrap();
    let id = &first.artifact.id;
    let added = store
        .comment(
            id,
            &first.revision,
            CommentChange::Create("Clarify scope".into()),
        )
        .unwrap();
    assert!(
        store
            .comment(id, &first.revision, CommentChange::Create("Stale".into()))
            .is_err()
    );
    let comment = added.artifact.comments[0].id.clone();
    let edited = store
        .comment(
            id,
            &added.revision,
            CommentChange::Edit(comment.clone(), "Updated feedback".into()),
        )
        .unwrap();
    let resolved = store
        .comment(
            id,
            &edited.revision,
            CommentChange::Resolve(comment.clone(), true),
        )
        .unwrap();
    assert!(resolved.artifact.comments[0].resolved);
    let reopened = store
        .comment(
            id,
            &resolved.revision,
            CommentChange::Resolve(comment.clone(), false),
        )
        .unwrap();
    assert!(!reopened.artifact.comments[0].resolved);
    let deleted = store
        .comment(id, &reopened.revision, CommentChange::Delete(comment))
        .unwrap();
    assert!(deleted.artifact.comments.is_empty());
    assert_eq!(deleted.artifact.content, first.artifact.content);
    assert_eq!(deleted.artifact.sessions, first.artifact.sessions);
    assert!(
        store
            .comment(id, &deleted.revision, CommentChange::Create("  ".into()))
            .is_err()
    );
    assert!(
        store
            .comment(
                id,
                &deleted.revision,
                CommentChange::Delete("missing".into())
            )
            .is_err()
    );
    assert_eq!(store.get(id).unwrap(), deleted);
}
#[test]
fn legacy_json_is_readable_and_retained_after_conversion() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let dir = store.checked_path(Some(&first.artifact.id)).unwrap();
    let mut value = serde_json::to_value(&first.artifact).unwrap();
    value["schemaVersion"] = Value::from(3);
    value.as_object_mut().unwrap().remove("repository");
    value["future"] = serde_json::json!({"preserve": true});
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(dir.join("artifact.json"), &bytes).unwrap();
    fs::remove_file(dir.join("artifact.md")).unwrap();
    let legacy = store.get(&first.artifact.id).unwrap();
    assert!(legacy.artifact.repository.is_none());
    let converted = store
        .update(
            &first.artifact.id,
            &legacy.revision,
            ArtifactPatch {
                repository: Some("repo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(converted.artifact.extra["future"], value["future"]);
    assert_eq!(fs::read(dir.join("artifact.json")).unwrap(), bytes);
    assert_eq!(store.get(&first.artifact.id).unwrap(), converted);
}
#[test]
fn delete_removes_directory_and_rejects_stale_revisions() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let id = first.artifact.id.clone();
    assert!(store.delete(&id, "stale-revision").is_err());
    assert!(store.checked_path(Some(&id)).unwrap().exists());
    store.delete(&id, &first.revision).unwrap();
    assert!(!store.checked_path(Some(&id)).unwrap().exists());
    assert!(store.get(&id).is_err());
    assert!(store.delete(&id, &first.revision).is_err());
}
#[test]
fn failed_atomic_write_does_not_replace_document() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let mut artifact = first.artifact.clone();
    artifact.content = "changed".into();
    assert!(
        store
            .write(&artifact, &std::collections::HashMap::new(), || bail!(
                "interrupted"
            ))
            .is_err()
    );
    assert_eq!(store.get(&first.artifact.id).unwrap(), first);
}
#[cfg(unix)]
#[test]
fn symlink_document_is_rejected_without_touching_target() {
    let (dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let path = store
        .checked_path(Some(&first.artifact.id))
        .unwrap()
        .join("artifact.md");
    let target = dir.path().join("target");
    fs::write(&target, "unchanged").unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(store.get(&first.artifact.id).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "unchanged");
}

#[test]
fn block_comments_roundtrip_relocate_and_keep_original_text_when_outdated() {
    let (_dir, store) = fixture();
    let first = store.create(input("repo")).unwrap();
    let id = &first.artifact.id;
    let added = store
        .comment(
            id,
            &first.revision,
            CommentChange::CreateBlock {
                body: "Clarify this step".into(),
                block: 1,
                quote: Some("Ship λ".into()),
            },
        )
        .unwrap();
    assert_eq!(store.get(id).unwrap(), added);
    let anchor = added.artifact.comments[0]
        .anchor
        .as_ref()
        .unwrap()
        .location();
    assert_eq!(anchor.source, "- [ ] Ship λ");
    assert_eq!(anchor.start_line, 3);
    assert_eq!(anchor.quote.as_deref(), Some("Ship λ"));
    assert_eq!(added.artifact.content, first.artifact.content);
    assert!(
        store
            .comment(
                id,
                &added.revision,
                CommentChange::CreateBlock {
                    body: "Invalid".into(),
                    block: 99,
                    quote: None,
                }
            )
            .is_err()
    );
    let moved = store
        .update(
            id,
            &added.revision,
            ArtifactPatch {
                content: Some(format!("Intro.\n\n{}", first.artifact.content)),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        moved.artifact.comments[0]
            .anchor
            .as_ref()
            .unwrap()
            .location()
            .start_line,
        5
    );
    let edited = store
        .update(
            id,
            &moved.revision,
            ArtifactPatch {
                content: Some(moved.artifact.content.replace("Ship λ", "Ship β")),
                ..Default::default()
            },
        )
        .unwrap();
    let anchor = edited.artifact.comments[0]
        .anchor
        .as_ref()
        .unwrap()
        .location();
    assert!(anchor.outdated);
    assert_eq!(anchor.source, "- [ ] Ship λ");
    let comment_id = &edited.artifact.comments[0].id;
    assert!(
        store
            .comment(
                id,
                &moved.revision,
                CommentChange::Resolve(comment_id.clone(), true)
            )
            .is_err()
    );
    let resolved = store
        .comment(
            id,
            &edited.revision,
            CommentChange::Resolve(comment_id.clone(), true),
        )
        .unwrap();
    assert!(resolved.artifact.comments[0].resolved);
    assert!(
        resolved.artifact.comments[0]
            .anchor
            .as_ref()
            .unwrap()
            .location()
            .outdated
    );
    // Raw body changes from an external editor get relocation on read, without writing.
    let path = store.checked_path(Some(id)).unwrap().join("artifact.md");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("Ship β", "Ship λ");
    fs::write(&path, &text).unwrap();
    let restored = store.get(id).unwrap();
    assert!(
        !restored.artifact.comments[0]
            .anchor
            .as_ref()
            .unwrap()
            .location()
            .outdated
    );
    assert_eq!(fs::read_to_string(path).unwrap(), text);
}

#[test]
fn selection_comments_roundtrip_and_reject_invalid_or_stale_ranges() {
    let (_dir, store) = fixture();
    let mut input = input("repo");
    input.content = "# Header\n\nFirst λ and second λ.\nMore text.".into();
    let first = store.create(input).unwrap();
    let id = &first.artifact.id;
    let start = first.artifact.content.rfind('λ').unwrap();
    let range = start..start + 2;
    let added = store
        .comment(
            id,
            &first.revision,
            CommentChange::CreateSelection {
                body: "Second occurrence".into(),
                range: range.clone(),
                quote: Some("λ".into()),
            },
        )
        .unwrap();
    assert_eq!(store.get(id).unwrap(), added);
    assert!(matches!(
        added.artifact.comments[0].anchor,
        Some(CommentAnchor::Selection(_))
    ));
    let anchor = added.artifact.comments[0]
        .anchor
        .as_ref()
        .unwrap()
        .location();
    assert_eq!(
        (anchor.start, anchor.end, anchor.start_line),
        (start, start + 2, 3)
    );
    for range in [start + 1..start + 2, 0..0, 0..usize::MAX] {
        assert!(
            store
                .comment(
                    id,
                    &added.revision,
                    CommentChange::CreateSelection {
                        body: "Invalid".into(),
                        range,
                        quote: None,
                    }
                )
                .is_err()
        );
        assert_eq!(store.get(id).unwrap(), added);
    }
    assert!(
        store
            .comment(
                id,
                &first.revision,
                CommentChange::CreateSelection {
                    body: "Stale".into(),
                    range,
                    quote: None,
                }
            )
            .is_err()
    );
    assert_eq!(store.get(id).unwrap(), added);
}
