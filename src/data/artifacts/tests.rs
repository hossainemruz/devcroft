use super::*;
use crate::data::tasks::{NewSubtask, NewTask, SubtaskPatch, TaskPatch, TaskStore};

fn fixture() -> (tempfile::TempDir, ArtifactStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(&DataRoot::new(dir.path().to_owned()));
    (dir, store)
}

fn input() -> NewArtifact {
    NewArtifact {
        title: " RFC ".into(),
        kind: Kind::Rfc,
        content: "# Proposal\n\nUnicode: λ\n".into(),
    }
}

fn path(store: &ArtifactStore, id: &str) -> PathBuf {
    store.checked_path(Some(id)).unwrap().join("artifact.json")
}

#[test]
fn standalone_patch_archive_and_external_markdown_revision() {
    let (_dir, store) = fixture();
    let first = store.create(input()).unwrap();
    let id = &first.artifact.id;
    assert_eq!(store.get(id).unwrap(), first);
    assert_eq!(first.artifact.title, "RFC");
    let second = store
        .update(
            id,
            &first.revision,
            ArtifactPatch {
                title: Some("Plan".into()),
                kind: Some(Kind::Plan),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(second.artifact.content, first.artifact.content);
    assert_eq!(second.artifact.created_at, first.artifact.created_at);
    assert!(second.artifact.updated_at > first.artifact.updated_at);
    assert!(
        store
            .update(id, &first.revision, ArtifactPatch::default())
            .unwrap_err()
            .to_string()
            .contains("artifact_changed")
    );
    let third = store
        .update(
            id,
            &second.revision,
            ArtifactPatch {
                content: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(third.artifact.title, "Plan");
    assert!(third.artifact.content.is_empty());
    let mut value: Value = serde_json::from_slice(&fs::read(path(&store, id)).unwrap()).unwrap();
    value["content"] = Value::String("# Externally edited Markdown".into());
    value["future"] = serde_json::json!({"preserve": true});
    fs::write(path(&store, id), serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        store
            .set_archived(id, &third.revision, true)
            .unwrap_err()
            .to_string()
            .contains("artifact_changed")
    );
    let external = store.get(id).unwrap();
    let archived = store.set_archived(id, &external.revision, true).unwrap();
    assert_eq!(archived.artifact.extra["future"], value["future"]);
    assert!(
        store
            .list(&ListOptions::default())
            .unwrap()
            .artifacts
            .is_empty()
    );
    assert_eq!(store.get(id).unwrap(), archived);
    assert_eq!(
        store
            .list(&ListOptions {
                include_archived: true,
                limit: None
            })
            .unwrap()
            .artifacts
            .len(),
        1
    );
    assert!(
        !store
            .set_archived(id, &archived.revision, false)
            .unwrap()
            .artifact
            .archived
    );
}

#[test]
fn shared_live_links_archive_missing_and_clearing() {
    let (_dir, store) = fixture();
    let artifact = store.create(input()).unwrap();
    let id = &artifact.artifact.id;
    let tasks = TaskStore::new(&store.root);
    let a = tasks
        .create(NewTask {
            title: "Idea".into(),
            artifacts: vec![id.clone()],
            ..Default::default()
        })
        .unwrap();
    let b = tasks
        .create(NewTask {
            title: "Shared".into(),
            artifacts: vec![id.clone()],
            ..Default::default()
        })
        .unwrap();
    let repo = store.root.portable_dir().join("repositories/backend");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("repository.json"), "{}").unwrap();
    let a = tasks
        .create_subtask(
            &a.task.id,
            &a.revision,
            NewSubtask {
                title: "Work".into(),
                repository: "backend".into(),
                artifacts: vec![id.clone()],
                ..Default::default()
            },
        )
        .unwrap();
    let revised = store
        .update(
            id,
            &artifact.revision,
            ArtifactPatch {
                content: Some("Current content".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(tasks.get(&a.task.id).unwrap().revision, a.revision);
    assert_eq!(
        store
            .get(&a.task.subtasks[0].artifacts[0])
            .unwrap()
            .artifact
            .content,
        "Current content"
    );
    let archived = store.set_archived(id, &revised.revision, true).unwrap();
    assert!(tasks.get(&a.task.id).unwrap().warnings.is_empty());
    assert!(store.get(&b.task.artifacts[0]).unwrap().artifact.archived);
    tasks.set_archived(&b.task.id, &b.revision, true).unwrap();
    assert_eq!(store.get(id).unwrap(), archived);
    fs::remove_file(path(&store, id)).unwrap();
    let missing = tasks.get(&a.task.id).unwrap();
    assert_eq!(missing.warnings.len(), 1);
    assert_eq!(missing.task.artifacts, vec![id.clone()]);
    let preserved = tasks
        .update(
            &a.task.id,
            &a.revision,
            TaskPatch {
                title: Some("Still readable".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        tasks
            .create(NewTask {
                title: "Bad".into(),
                artifacts: vec![id.clone()],
                ..Default::default()
            })
            .is_err()
    );
    let cleared = tasks
        .update_subtask(
            &a.task.id,
            &preserved.revision,
            "s1",
            SubtaskPatch {
                artifacts: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    let cleared = tasks
        .update(
            &a.task.id,
            &cleared.revision,
            TaskPatch {
                artifacts: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(cleared.warnings.is_empty());
}

#[test]
fn failures_preserve_whole_record_and_interrupted_temps_are_not_adopted() {
    let (_dir, store) = fixture();
    let first = store.create(input()).unwrap();
    let id = &first.artifact.id;
    let original = fs::read(path(&store, id)).unwrap();
    let mut candidate = first.artifact.clone();
    candidate.title = "New metadata".into();
    candidate.content = "New Markdown".into();
    assert!(
        store
            .write(&candidate, || bail!("injected after flush, before rename"))
            .is_err()
    );
    assert_eq!(fs::read(path(&store, id)).unwrap(), original);
    let tmp = path(&store, id).with_file_name(".task-interrupted.tmp");
    fs::write(&tmp, serde_json::to_vec(&candidate).unwrap()).unwrap();
    assert_eq!(store.get(id).unwrap(), first);
    let committed = store
        .update(
            id,
            &first.revision,
            ArtifactPatch {
                title: Some(candidate.title),
                content: Some(candidate.content),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(store.get(id).unwrap(), committed);
    assert!(tmp.exists());
    assert!(
        store
            .update(
                id,
                &committed.revision,
                ArtifactPatch {
                    content: Some("x".repeat(MAX_BYTES as usize)),
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(store.get(id).unwrap(), committed);
}

#[test]
fn collision_malformed_unsupported_and_path_validation() {
    let (_dir, store) = fixture();
    let first = store
        .create_with_ids(input(), || "art-22222222".into())
        .unwrap();
    let mut ids = ["art-22222222", "art-33333333"].into_iter();
    let second = store
        .create_with_ids(input(), || ids.next().unwrap().into())
        .unwrap();
    assert_ne!(first.artifact.id, second.artifact.id);
    let incomplete = store.checked_path(Some("art-44444444")).unwrap();
    fs::create_dir(&incomplete).unwrap();
    fs::write(path(&store, &second.artifact.id), "not JSON").unwrap();
    let legacy = store.checked_path(Some("art-55555555")).unwrap();
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("artifact.json"), "{\"schemaVersion\":1}").unwrap();
    assert!(
        store
            .update("art-55555555", "anything", ArtifactPatch::default())
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
    let list = store.list(&ListOptions::default()).unwrap();
    assert_eq!(list.artifacts.len(), 1);
    assert_eq!(list.errors.len(), 3);
    assert!(
        store
            .list(&ListOptions {
                limit: Some(0),
                ..Default::default()
            })
            .unwrap()
            .truncated
    );
    for id in [
        "../escape",
        "art-../../bad",
        "art-iiiiiiii",
        "art-222222222",
        "task-22222222",
    ] {
        assert!(store.get(id).is_err());
        assert!(
            store
                .update(id, "revision", ArtifactPatch::default())
                .is_err()
        );
    }
    fs::write(
        path(&store, &first.artifact.id).with_file_name("content.md"),
        "ambiguous legacy content",
    )
    .unwrap();
    assert!(
        store
            .get(&first.artifact.id)
            .unwrap_err()
            .to_string()
            .contains("split")
    );
}

#[cfg(unix)]
#[test]
fn symlink_record_is_rejected() {
    let (_dir, store) = fixture();
    let first = store.create(input()).unwrap();
    let record = path(&store, &first.artifact.id);
    let target = store.root.root().join("target.json");
    fs::rename(&record, &target).unwrap();
    std::os::unix::fs::symlink(&target, &record).unwrap();
    assert!(store.get(&first.artifact.id).is_err());
    assert!(
        store
            .update(
                &first.artifact.id,
                &first.revision,
                ArtifactPatch::default()
            )
            .is_err()
    );
}

#[test]
fn concurrent_writers_have_one_winner_and_readers_see_complete_pairs() {
    let (_dir, store) = fixture();
    let first = store.create(input()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|n| {
            let store = store.clone();
            let first = first.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.update(
                    &first.artifact.id,
                    &first.revision,
                    ArtifactPatch {
                        title: Some(format!("pair{n}")),
                        content: Some(format!("pair{n}")),
                        ..Default::default()
                    },
                )
            })
        })
        .collect();
    barrier.wait();
    for _ in 0..20 {
        let read = store.get(&first.artifact.id).unwrap();
        assert!(read == first || read.artifact.title == read.artifact.content);
    }
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
}

#[test]
fn interrupted_process_keeps_old_record_and_releases_locks() {
    let (_dir, store) = fixture();
    let first = store.create(input()).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data::artifacts::tests::interruption_child",
            "--ignored",
        ])
        .env("DEVCROFT_ARTIFACT_TEST_ROOT", store.root.root())
        .env("DEVCROFT_ARTIFACT_TEST_ID", &first.artifact.id)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    assert_eq!(store.get(&first.artifact.id).unwrap(), first);
    assert_eq!(
        fs::read_dir(store.checked_path(Some(&first.artifact.id)).unwrap())
            .unwrap()
            .count(),
        2
    );
    let next = store
        .update(
            &first.artifact.id,
            &first.revision,
            ArtifactPatch {
                kind: Some(Kind::Note),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(next.artifact.kind, Kind::Note);
}

#[test]
#[ignore = "subprocess helper invoked by interrupted_process_keeps_old_record_and_releases_locks"]
fn interruption_child() {
    let root = DataRoot::new(
        std::env::var_os("DEVCROFT_ARTIFACT_TEST_ROOT")
            .unwrap()
            .into(),
    );
    let id = std::env::var("DEVCROFT_ARTIFACT_TEST_ID").unwrap();
    validate_id(&id).unwrap();
    let store = ArtifactStore::new(&root);
    let _gate = portable_gate(&root, false).unwrap();
    let _record = artifact_lock(&root, &id, true).unwrap();
    let mut artifact = store.read(&id).unwrap().artifact;
    artifact.title = "Interrupted metadata".into();
    artifact.content = "Interrupted Markdown".into();
    store.write(&artifact, || std::process::exit(73)).unwrap();
    panic!("failure injection was not reached");
}

#[test]
fn new_links_validate_per_association_without_partial_mutations() {
    let (_dir, store) = fixture();
    let artifact = store.create(input()).unwrap();
    let tasks = TaskStore::new(&store.root);
    let first = tasks
        .create(NewTask {
            title: "Idea".into(),
            artifacts: vec![artifact.artifact.id.clone()],
            ..Default::default()
        })
        .unwrap();
    for links in [
        vec!["../escape".into()],
        vec!["art-22222222".into()],
        vec![artifact.artifact.id.clone(); 2],
    ] {
        assert!(
            tasks
                .update(
                    &first.task.id,
                    &first.revision,
                    TaskPatch {
                        artifacts: Some(links),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert_eq!(tasks.get(&first.task.id).unwrap(), first);
    }
    let repo = store.root.portable_dir().join("repositories/backend");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("repository.json"), "{}").unwrap();
    fs::remove_file(path(&store, &artifact.artifact.id)).unwrap();
    assert!(
        tasks
            .create_subtask(
                &first.task.id,
                &first.revision,
                NewSubtask {
                    title: "Work".into(),
                    repository: "backend".into(),
                    artifacts: vec![artifact.artifact.id],
                    ..Default::default()
                }
            )
            .is_err()
    );
    let current = tasks.get(&first.task.id).unwrap();
    assert_eq!(current.revision, first.revision);
    assert!(current.task.subtasks.is_empty());
}
