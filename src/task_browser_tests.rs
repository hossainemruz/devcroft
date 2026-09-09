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

#[test]
fn cards_derive_status_from_subtasks_without_a_task_level_field() {
    let (_dir, _root, store) = fixture();
    let snapshot = idea(&store);
    assert_eq!(task_card_status(&snapshot.task), "Not planned");
    assert_eq!(blocked_subtask_count(&snapshot.task), 0);
    let mut current = snapshot;
    for repository in ["public-api", "backend"] {
        current = store
            .create_subtask(
                &current.task.id,
                &current.revision,
                NewSubtask {
                    title: repository.into(),
                    repository: repository.into(),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    assert_eq!(task_card_status(&current.task), "Todo");
    let mut doing = current.task.clone();
    doing.subtasks[0].status = Status::Doing;
    assert_eq!(task_card_status(&doing), "In progress");
    assert_eq!(blocked_subtask_count(&doing), 0);
    let mut stuck = current.task.clone();
    stuck.subtasks[0].status = Status::Blocked;
    stuck.subtasks[1].status = Status::Done;
    assert_eq!(task_card_status(&stuck), "In progress");
    assert_eq!(blocked_subtask_count(&stuck), 1);
    // The blocked pill lives on the stats row, so a blocked task must
    // always have stats to host it.
    assert!(task_card_stats(&stuck).is_some());
    let mut done = current.task.clone();
    for subtask in &mut done.subtasks {
        subtask.status = Status::Done;
    }
    assert_eq!(task_card_status(&done), "Complete");
    let mut archived = current.task.clone();
    archived.archived = true;
    assert_eq!(task_card_status(&archived), "Archived");
}

#[test]
fn recent_grid_is_a_single_row_up_to_three_across() {
    assert_eq!(recent_task_columns(600.), 1);
    assert_eq!(recent_task_columns(800.), 2);
    assert_eq!(recent_task_columns(900.), 2);
    assert_eq!(recent_task_columns(1024.), 2);
    // Typical Mac widths fit 3 across; four would squeeze to ~336px and
    // crop titles, and the Home body caps at 1440px so wider windows gain
    // no extra room.
    assert_eq!(recent_task_columns(1100.), 3);
    assert_eq!(recent_task_columns(1200.), 3);
    assert_eq!(recent_task_columns(1440.), 3);
    assert_eq!(recent_task_columns(2560.), 3);
    for (viewport, columns) in [(600., 1.), (800., 2.), (1200., 3.), (1440., 3.)] {
        let occupied = recent_task_card_width(viewport) * columns + (columns - 1.) * 16.;
        let available = viewport.min(1440.) - 48.;
        // Floored to whole pixels: must fit, leaving less than one pixel of
        // slack per card for rounding (notably Retina 2x).
        assert!(
            occupied <= available + 0.01,
            "{viewport}: {occupied} > {available}"
        );
        assert!(
            available - occupied < columns,
            "{viewport}: {occupied} leaves too much slack in {available}"
        );
    }
}

#[test]
fn cards_show_status_and_recency_in_the_meta_line() {
    let (_dir, _root, store) = fixture();
    let snapshot = idea(&store);
    let mut task = snapshot.task;
    task.updated_at = 1_717_200_000;
    assert_eq!(
        task_card_meta(&task, 1_717_200_000),
        "Not planned · Updated just now"
    );
    assert_eq!(
        task_card_meta(&task, 1_717_200_000 + 7_200),
        "Not planned · Updated 2h ago"
    );
    assert_eq!(task_card_progress(&task), None);
    let planned = store
        .create_subtask(
            &task.id,
            &snapshot.revision,
            NewSubtask {
                title: "Only".into(),
                repository: "backend".into(),
                ..Default::default()
            },
        )
        .unwrap();
    task.subtasks = planned.task.subtasks;
    assert_eq!(task_card_progress(&task).as_deref(), Some("0/1"));
    task.subtasks[0].status = Status::Done;
    assert_eq!(task_card_progress(&task).as_deref(), Some("1/1"));
    assert_eq!(
        task_card_meta(&task, 1_717_200_000),
        "Complete · Updated just now"
    );
}

#[test]
fn cards_count_subtasks_repositories_and_artifacts() {
    let (_dir, _root, store) = fixture();
    let snapshot = idea(&store);
    assert_eq!(task_card_stats(&snapshot.task), None);
    let planned = store
        .create_subtask(
            &snapshot.task.id,
            &snapshot.revision,
            NewSubtask {
                title: "First".into(),
                repository: "backend".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let mut task = planned.task;
    task.repositories = vec!["backend".into(), "public-api".into()];
    task.artifacts = vec!["art-1".into()];
    assert_eq!(
        task_card_stats(&task).as_deref(),
        Some("1 Subtask · 2 Repositories · 1 Artifact")
    );
    let second = store
        .create_subtask(
            &task.id,
            &planned.revision,
            NewSubtask {
                title: "Second".into(),
                repository: "backend".into(),
                ..Default::default()
            },
        )
        .unwrap();
    task.subtasks = second.task.subtasks;
    assert_eq!(
        task_card_stats(&task).as_deref(),
        Some("2 Subtasks · 2 Repositories · 1 Artifact")
    );
}

#[test]
fn cards_point_at_the_next_actionable_subtask() {
    let (_dir, _root, store) = fixture();
    let snapshot = idea(&store);
    assert_eq!(task_next_up(&snapshot.task), None);
    // Beta waits on Alpha; Gamma is free.
    let mut current = snapshot;
    for (title, dependencies) in [
        ("Alpha", vec![]),
        ("Beta", vec!["s1".into()]),
        ("Gamma", vec![]),
    ] {
        current = store
            .create_subtask(
                &current.task.id,
                &current.revision,
                NewSubtask {
                    title: title.into(),
                    repository: "backend".into(),
                    dependencies,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    assert_eq!(task_next_up(&current.task).as_deref(), Some("Next: Alpha"));
    let mut doing = current.task.clone();
    doing.subtasks[2].status = Status::Doing;
    assert_eq!(task_next_up(&doing).as_deref(), Some("Active: Gamma"));
    // Finishing Alpha unblocks Beta; Gamma still in flight.
    let mut unblocked = doing.clone();
    unblocked.subtasks[0].status = Status::Done;
    assert_eq!(task_next_up(&unblocked).as_deref(), Some("Active: Gamma"));
    let mut idle = unblocked.clone();
    idle.subtasks[2].status = Status::Todo;
    assert_eq!(task_next_up(&idle).as_deref(), Some("Next: Beta"));
    let mut stuck = idle.clone();
    stuck.subtasks[1].status = Status::Blocked;
    stuck.subtasks[2].status = Status::Blocked;
    assert_eq!(task_next_up(&stuck).as_deref(), Some("Blocked on: Beta"));
    let mut done = stuck.clone();
    for subtask in &mut done.subtasks {
        subtask.status = Status::Done;
    }
    assert_eq!(task_next_up(&done), None);
}

#[test]
fn status_pills_map_each_state_to_its_hue() {
    assert_eq!(task_status_color("Complete"), ColorName::Green);
    assert_eq!(task_status_color("In progress"), ColorName::Blue);
    assert_eq!(task_status_color("Todo"), ColorName::Yellow);
    assert_eq!(task_status_color("Blocked"), ColorName::Red);
    for dormant in ["Not planned", "Archived", "anything-else"] {
        assert_eq!(task_status_color(dormant), ColorName::Gray);
    }
}
