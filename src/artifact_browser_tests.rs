use super::*;
use crate::data::artifacts::{ArtifactPatch, NewArtifact};

#[test]
fn refresh_coalesces_requests_and_discards_superseded_results() {
    let mut refresh = Refresh::default();
    let first = refresh.request().unwrap();
    // Selection, filter, and manual reload coalesce into one subsequent scan.
    assert!(refresh.request().is_none());
    assert!(refresh.request().is_none());
    assert!(refresh.pending);
    assert!(!refresh.finish(first));
    let latest = refresh.request().unwrap();
    assert!(!refresh.pending);
    assert!(refresh.finish(latest));
    assert!(!refresh.busy);
    // Mutations share the same single-flight slot.
    refresh.busy = true;
    assert!(refresh.request().is_none());
    assert!(refresh.pending);
}

#[test]
fn standalone_viewer_reads_live_and_archived_content_independent_of_list() {
    let temp = tempfile::tempdir().unwrap();
    let root = DataRoot::new(temp.path().to_owned());
    let store = ArtifactStore::new(&root);
    let first = store
        .create(NewArtifact {
            title: "Standalone RFC".into(),
            kind: Kind::Rfc,
            content: "# First".into(),
        })
        .unwrap();
    let id = &first.artifact.id;
    let initial = load(&root, false, PAGE_SIZE, Some(id));
    assert_eq!(initial.list.unwrap().artifacts, vec![first.clone()]);
    assert_eq!(initial.selected.unwrap().unwrap(), first);
    let revised = store
        .update(
            id,
            &first.revision,
            ArtifactPatch {
                content: Some("# Revised\n\nλ".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        load(&root, false, PAGE_SIZE, Some(id))
            .selected
            .unwrap()
            .unwrap(),
        revised
    );
    assert!(store.set_archived(id, &first.revision, true).is_err());
    let archived = store.set_archived(id, &revised.revision, true).unwrap();
    let active = load(&root, false, PAGE_SIZE, Some(id));
    assert!(active.list.unwrap().artifacts.is_empty());
    assert_eq!(active.selected.unwrap().unwrap(), archived);
    assert_eq!(
        load(&root, true, PAGE_SIZE, None).list.unwrap().artifacts,
        vec![archived]
    );
    assert!(!root.portable_dir().join("tasks").exists());
}

#[test]
fn malformed_siblings_missing_selection_and_bounded_browsing_are_visible() {
    let temp = tempfile::tempdir().unwrap();
    let root = DataRoot::new(temp.path().to_owned());
    let store = ArtifactStore::new(&root);
    let valid = store
        .create(NewArtifact {
            title: "Valid".into(),
            kind: Kind::Note,
            content: String::new(),
        })
        .unwrap();
    let broken = store
        .create(NewArtifact {
            title: "Broken".into(),
            kind: Kind::Plan,
            content: "old".into(),
        })
        .unwrap();
    let path = root
        .portable_dir()
        .join("artifacts")
        .join(&broken.artifact.id)
        .join("artifact.json");
    std::fs::write(&path, "not JSON").unwrap();
    let loaded = load(&root, false, PAGE_SIZE, Some(&broken.artifact.id));
    let list = loaded.list.unwrap();
    assert_eq!(list.artifacts, vec![valid]);
    assert_eq!(list.errors.len(), 1);
    assert!(
        loaded
            .selected
            .unwrap()
            .unwrap_err()
            .contains("missing or unreadable")
    );
    std::fs::remove_file(path).unwrap();
    assert!(
        load(&root, false, PAGE_SIZE, Some(&broken.artifact.id))
            .selected
            .unwrap()
            .is_err()
    );
    let bounded = load(&root, false, 0, None).list.unwrap();
    assert!(bounded.truncated);
    assert!(bounded.artifacts.is_empty());
}
