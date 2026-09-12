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
    assert!(store.write(&artifact, || bail!("interrupted")).is_err());
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
