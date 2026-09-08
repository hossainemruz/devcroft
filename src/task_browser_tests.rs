use super::*;
use crate::data::artifacts::{ArtifactStore, Kind, NewArtifact};
use crate::data::tasks::{NewSubtask, NewTask, SubtaskPatch, TaskPatch};

fn fixture() -> (tempfile::TempDir, DataRoot, TaskStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    for key in ["public-api", "backend", "deployment", "unrelated"] {
        let path = root.portable_dir().join("repositories").join(key);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("repository.json"), "{}").unwrap();
    }
    let store = TaskStore::new(&root);
    (dir, root, store)
}

fn idea(store: &TaskStore) -> Snapshot {
    store
        .create(NewTask {
            title: "API → backend → deployment".into(),
            description: "# Requirements\n\nKeep stable IDs".into(),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn every_repository_lists_membership_but_opens_the_full_breakdown() {
    let (_dir, root, store) = fixture();
    let unplanned = idea(&store);
    assert_eq!(progress_label(&unplanned.task), "Not planned");
    let mut current = idea(&store);
    for (repository, dependencies) in [
        ("public-api", vec![]),
        ("backend", vec!["s1".into()]),
        ("deployment", vec!["s2".into()]),
    ] {
        current = store
            .create_subtask(
                &current.task.id,
                &current.revision,
                NewSubtask {
                    title: repository.into(),
                    repository: repository.into(),
                    dependencies,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    for (id, status) in [
        ("s1", Status::Done),
        ("s2", Status::Doing),
        ("s3", Status::Blocked),
    ] {
        current = store
            .update_subtask(
                &current.task.id,
                &current.revision,
                id,
                SubtaskPatch {
                    status: Some(status),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    assert_eq!(progress_label(&current.task), "1/3 done");
    for repository in ["public-api", "backend", "deployment"] {
        let loaded = load(
            &root,
            &Scope::Repository(Some(repository.into())),
            false,
            PAGE_SIZE,
            Some(&current.task.id),
        );
        let list = loaded.list.unwrap();
        assert_eq!(list.tasks.len(), 1);
        assert_eq!(list.tasks[0].task.id, current.task.id);
        let full = loaded.selected.unwrap().unwrap();
        assert_eq!(full, current);
        assert_eq!(full.task.subtasks[1].dependencies, ["s1"]);
        assert_eq!(full.task.subtasks[2].dependencies, ["s2"]);
    }
    assert!(
        load(
            &root,
            &Scope::Repository(Some("unrelated".into())),
            false,
            PAGE_SIZE,
            None
        )
        .list
        .unwrap()
        .tasks
        .is_empty()
    );
    assert!(
        load(&root, &Scope::Repository(None), false, PAGE_SIZE, None)
            .list
            .is_err()
    );
    assert_eq!(
        load(&root, &Scope::Global, false, PAGE_SIZE, None)
            .list
            .unwrap()
            .tasks
            .len(),
        2
    );
    let explicit = store
        .update(
            &unplanned.task.id,
            &unplanned.revision,
            TaskPatch {
                repositories: Some(vec!["unrelated".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        load(
            &root,
            &Scope::Repository(Some("unrelated".into())),
            false,
            PAGE_SIZE,
            None
        )
        .list
        .unwrap()
        .tasks,
        vec![explicit]
    );
    assert_eq!(
        [Status::Todo, Status::Doing, Status::Blocked, Status::Done].map(status_label),
        ["todo", "doing", "blocked", "done"]
    );
}

#[test]
fn recent_summaries_are_bounded_live_and_exclude_archived_tasks() {
    let (_dir, root, store) = fixture();
    let first = idea(&store);
    for _ in 0..5 {
        idea(&store);
    }
    let mut latest = store
        .update(
            &first.task.id,
            &first.revision,
            TaskPatch {
                title: Some("Revised idea".into()),
                ..Default::default()
            },
        )
        .unwrap();
    // Several creations can share a millisecond; ties intentionally sort by
    // ID. Advance this record past the siblings without relying on sleeps.
    let newest = store
        .list(&ListOptions {
            limit: Some(PAGE_SIZE),
            ..Default::default()
        })
        .unwrap()
        .tasks
        .iter()
        .map(|snapshot| snapshot.task.updated_at)
        .max()
        .unwrap();
    while latest.task.updated_at <= newest {
        latest = store
            .update(&latest.task.id, &latest.revision, TaskPatch::default())
            .unwrap();
    }
    let recent = load(&root, &Scope::Global, false, 4, None).list.unwrap();
    assert_eq!(recent.tasks.len(), 4);
    assert!(recent.truncated);
    assert_eq!(recent.tasks[0], latest);
    let archived = store
        .set_archived(&latest.task.id, &latest.revision, true)
        .unwrap();
    let active = load(
        &root,
        &Scope::Global,
        false,
        PAGE_SIZE,
        Some(&latest.task.id),
    );
    assert!(
        active
            .list
            .unwrap()
            .tasks
            .iter()
            .all(|s| s.task.id != latest.task.id)
    );
    assert_eq!(active.selected.unwrap().unwrap(), archived);
    assert_eq!(progress_label(&archived.task), "Not planned");
    assert!(
        store
            .set_archived(&latest.task.id, &latest.revision, false)
            .is_err()
    );
    let restored = store
        .set_archived(&latest.task.id, &archived.revision, false)
        .unwrap();
    assert_eq!(restored.task.subtasks, latest.task.subtasks);
}

#[test]
fn warnings_broken_links_and_missing_records_do_not_hide_valid_siblings() {
    let (_dir, root, store) = fixture();
    let artifacts = ArtifactStore::new(&root);
    let artifact = artifacts
        .create(NewArtifact {
            title: "Plan".into(),
            kind: Kind::Plan,
            content: "# Approach".into(),
        })
        .unwrap();
    let task = store
        .create(NewTask {
            title: "Linked task".into(),
            repositories: vec!["backend".into()],
            artifacts: vec![artifact.artifact.id.clone()],
            ..Default::default()
        })
        .unwrap();
    let archived_artifact = artifacts
        .set_archived(&artifact.artifact.id, &artifact.revision, true)
        .unwrap();
    let loaded = load(&root, &Scope::Global, false, PAGE_SIZE, Some(&task.task.id));
    assert!(loaded.selected.unwrap().unwrap().warnings.is_empty());
    assert_eq!(
        artifacts.get(&artifact.artifact.id).unwrap(),
        archived_artifact
    );
    std::fs::remove_file(
        root.portable_dir()
            .join("artifacts")
            .join(&artifact.artifact.id)
            .join("artifact.json"),
    )
    .unwrap();
    std::fs::remove_file(
        root.portable_dir()
            .join("repositories/backend/repository.json"),
    )
    .unwrap();
    let loaded = load(
        &root,
        &Scope::Repository(Some("backend".into())),
        false,
        PAGE_SIZE,
        Some(&task.task.id),
    );
    let selected = loaded.selected.unwrap().unwrap();
    assert_eq!(selected.task.artifacts, task.task.artifacts);
    assert_eq!(selected.warnings.len(), 2);
    let valid = idea(&store);
    let path = root
        .portable_dir()
        .join("tasks")
        .join(&task.task.id)
        .join("task.json");
    std::fs::write(&path, "malformed").unwrap();
    let loaded = load(&root, &Scope::Global, false, PAGE_SIZE, Some(&task.task.id));
    let list = loaded.list.unwrap();
    assert_eq!(list.tasks, vec![valid]);
    assert_eq!(list.errors.len(), 1);
    assert!(loaded.selected.unwrap().is_err());
    std::fs::remove_file(path).unwrap();
    assert!(
        load(&root, &Scope::Global, true, PAGE_SIZE, Some(&task.task.id))
            .selected
            .unwrap()
            .unwrap_err()
            .contains("missing or unreadable")
    );
}

#[test]
fn progress_is_only_subtask_status_and_archive_does_not_finish_work() {
    let (_dir, _root, store) = fixture();
    let first = idea(&store);
    let planned = store
        .create_subtask(
            &first.task.id,
            &first.revision,
            NewSubtask {
                title: "Implement".into(),
                repository: "backend".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(progress_label(&planned.task), "0/1 done");
    let archived = store
        .set_archived(&planned.task.id, &planned.revision, true)
        .unwrap();
    assert_eq!(archived.task.subtasks, planned.task.subtasks);
    let done = store
        .update_subtask(
            &archived.task.id,
            &archived.revision,
            "s1",
            SubtaskPatch {
                status: Some(Status::Done),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(progress_label(&done.task), "Complete · 1/1 done");
}
