use super::*;
use std::process::Command;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, TaskStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    super::super::ensure_dirs(root.root()).unwrap();
    for key in ["public-api", "backend", "deployment"] {
        let path = root.portable_dir().join("repositories").join(key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("repository.json"), "{}").unwrap();
    }
    (dir, TaskStore::new(&root))
}

fn idea(store: &TaskStore) -> Snapshot {
    store
        .create(NewTask {
            title: "  Expose API  ".into(),
            description: "## Requirements\n\nKeep this idea.".into(),
            ..Default::default()
        })
        .unwrap()
}

fn add(store: &TaskStore, current: &Snapshot, repo: &str, dependencies: &[&str]) -> Snapshot {
    store
        .create_subtask(
            &current.task.id,
            &current.revision,
            NewSubtask {
                title: format!("Implement {repo}"),
                repository: repo.into(),
                dependencies: dependencies.iter().map(|s| (*s).into()).collect(),
                ..Default::default()
            },
        )
        .unwrap()
}

fn status(store: &TaskStore, current: &Snapshot, id: &str, status: Status) -> Snapshot {
    store
        .update_subtask(
            &current.task.id,
            &current.revision,
            id,
            SubtaskPatch {
                status: Some(status),
                ..Default::default()
            },
        )
        .unwrap()
}

fn record_path(store: &TaskStore, id: &str) -> PathBuf {
    store
        .root
        .portable_dir()
        .join("tasks")
        .join(id)
        .join("task.json")
}

#[test]
fn idea_roundtrip_and_patch_clearing() {
    let (_dir, store) = fixture();
    let first = idea(&store);
    assert_eq!(first.task.title, "Expose API");
    assert!(first.task.repositories.is_empty());
    assert!(first.task.subtasks.is_empty());
    assert!(!first.task.progress().is_planned());
    assert!(!first.task.progress().is_complete());
    assert_eq!(store.get(&first.task.id).unwrap(), first);
    let next = store
        .update(
            &first.task.id,
            &first.revision,
            TaskPatch {
                description: Some(String::new()),
                repositories: Some(vec!["backend".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(next.task.description.is_empty());
    assert_eq!(next.task.created_at, first.task.created_at);
    assert!(next.task.updated_at > first.task.updated_at);
    assert_ne!(next.revision, first.revision);
    assert!(
        store
            .update(
                &next.task.id,
                &next.revision,
                TaskPatch {
                    title: Some("  ".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(store.get(&next.task.id).unwrap(), next);
    let clear = store
        .update(
            &next.task.id,
            &next.revision,
            TaskPatch {
                repositories: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(clear.task.repositories.is_empty());
    assert!(!store.root.device_path().exists());
}

#[test]
fn repositories_progress_statuses_and_full_breakdown() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let a = store
        .update(
            &a.task.id,
            &a.revision,
            TaskPatch {
                repositories: Some(vec!["deployment".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    let a = add(&store, &a, "public-api", &[]);
    let a = add(&store, &a, "backend", &["s1"]);
    assert_eq!(
        a.task.involved_repositories(),
        ["backend", "deployment", "public-api"]
            .map(String::from)
            .into()
    );
    for repo in ["backend", "public-api", "deployment"] {
        let list = store
            .list(&ListOptions {
                repository: Some(repo.into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(list.tasks.len(), 1);
        assert_eq!(list.tasks[0].task.subtasks.len(), 2);
    }
    let mut a = a;
    for state in [
        Status::Doing,
        Status::Blocked,
        Status::Done,
        Status::Todo,
        Status::Done,
    ] {
        // Dependency remains unfinished; status is still explicitly editable.
        a = status(&store, &a, "s2", state);
        assert_eq!(
            a.task.progress().completed,
            usize::from(state == Status::Done)
        );
        assert_eq!(a.task.progress().total, 2);
    }
    assert!(!a.task.progress().is_complete());
    let a = status(&store, &a, "s1", Status::Done);
    assert!(a.task.progress().is_complete());
    assert_eq!(store.list(&ListOptions::default()).unwrap().tasks.len(), 1); // done stays visible
}

#[test]
fn dependency_validation_reorder_and_removal_preserve_identity() {
    let (_dir, store) = fixture();
    let a = add(&store, &idea(&store), "public-api", &[]);
    let a = add(&store, &a, "backend", &["s1"]);
    for deps in [
        vec!["s2".into()],
        vec!["s1".into()],
        vec!["s9".into()],
        vec!["s2".into(), "s2".into()],
    ] {
        assert!(
            store
                .update_subtask(
                    &a.task.id,
                    &a.revision,
                    "s1",
                    SubtaskPatch {
                        dependencies: Some(deps),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert_eq!(store.get(&a.task.id).unwrap(), a);
    }
    assert!(store.remove_subtask(&a.task.id, &a.revision, "s1").is_err());
    assert!(
        store
            .remove_subtask(&a.task.id, &a.revision, "missing")
            .is_err()
    );
    for order in [
        vec!["s1".into()],
        vec!["s1".into(), "s1".into()],
        vec!["s1".into(), "s9".into()],
    ] {
        assert!(
            store
                .reorder_subtasks(&a.task.id, &a.revision, &order)
                .is_err()
        );
    }
    let a = store
        .reorder_subtasks(&a.task.id, &a.revision, &["s2".into(), "s1".into()])
        .unwrap();
    assert_eq!(a.task.subtasks[0].id, "s2");
    assert_eq!(a.task.subtasks[0].dependencies, ["s1"]);
    let a = store
        .update_subtask(
            &a.task.id,
            &a.revision,
            "s2",
            SubtaskPatch {
                dependencies: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
    let a = store.remove_subtask(&a.task.id, &a.revision, "s1").unwrap();
    let a = store.remove_subtask(&a.task.id, &a.revision, "s2").unwrap();
    assert!(!a.task.progress().is_complete());
    let a = add(&store, &a, "deployment", &[]);
    assert_eq!(a.task.subtasks[0].id, "s3");
}

#[test]
fn invalid_repository_associations_and_missing_reference_warnings() {
    let (_dir, store) = fixture();
    for repos in [
        vec!["missing".into()],
        vec!["../backend".into()],
        vec!["backend".into(), "backend".into()],
    ] {
        assert!(
            store
                .create(NewTask {
                    title: "idea".into(),
                    repositories: repos,
                    ..Default::default()
                })
                .is_err()
        );
    }
    let a = add(&store, &idea(&store), "backend", &[]);
    for repo in ["", "missing", "../backend"] {
        assert!(
            store
                .create_subtask(
                    &a.task.id,
                    &a.revision,
                    NewSubtask {
                        title: "x".into(),
                        repository: repo.into(),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert!(
            store
                .update_subtask(
                    &a.task.id,
                    &a.revision,
                    "s1",
                    SubtaskPatch {
                        repository: Some(repo.into()),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
    fs::remove_file(
        store
            .root
            .portable_dir()
            .join("repositories/backend/repository.json"),
    )
    .unwrap();
    let missing = store.get(&a.task.id).unwrap();
    assert_eq!(missing.warnings.len(), 1);
    assert_eq!(missing.task.subtasks[0].repository, "backend");
    let updated = status(&store, &missing, "s1", Status::Doing);
    assert_eq!(updated.warnings.len(), 1);
    // Existing missing references survive, but new associations are rejected.
    assert!(
        store
            .create_subtask(
                &updated.task.id,
                &updated.revision,
                NewSubtask {
                    title: "Another".into(),
                    repository: "backend".into(),
                    ..Default::default()
                }
            )
            .is_err()
    );
    fs::write(
        store
            .root
            .portable_dir()
            .join("repositories/backend/repository.json"),
        "broken",
    )
    .unwrap();
    assert!(store.get(&a.task.id).unwrap().warnings[0].contains("malformed repository"));
}

#[test]
fn archive_list_limits_and_stale_archive_checks() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let b = idea(&store);
    let archived = store.set_archived(&a.task.id, &a.revision, true).unwrap();
    assert!(store.set_archived(&a.task.id, &a.revision, false).is_err());
    assert_eq!(store.get(&a.task.id).unwrap(), archived);
    assert!(!archived.task.progress().is_complete());
    let list = store.list(&ListOptions::default()).unwrap();
    assert_eq!(list.tasks.len(), 1);
    assert_eq!(list.tasks[0].task.id, b.task.id);
    assert_eq!(
        store
            .list(&ListOptions {
                include_archived: true,
                ..Default::default()
            })
            .unwrap()
            .tasks
            .len(),
        2
    );
    let unarchived = store
        .set_archived(&a.task.id, &archived.revision, false)
        .unwrap();
    let list = store
        .list(&ListOptions {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(list.tasks[0].task.id, unarchived.task.id);
    assert!(list.truncated);
    assert!(
        store
            .list(&ListOptions {
                limit: Some(0),
                ..Default::default()
            })
            .unwrap()
            .tasks
            .is_empty()
    );
}

#[test]
fn external_edits_invalidate_revisions_and_preserve_unknown_fields() {
    let (_dir, store) = fixture();
    let a = add(&store, &idea(&store), "backend", &[]);
    let path = record_path(&store, &a.task.id);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["futureTask"] = serde_json::json!({"keep": true});
    value["subtasks"][0]["futureSubtask"] = serde_json::json!([1, 2]);
    // No timestamp change: revision comes from bytes, not metadata.
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        store
            .update(&a.task.id, &a.revision, TaskPatch::default())
            .unwrap_err()
            .to_string()
            .contains("task_changed")
    );
    let current = store.get(&a.task.id).unwrap();
    let current = status(&store, &current, "s1", Status::Done);
    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["futureTask"]["keep"], true);
    assert_eq!(
        value["subtasks"][0]["futureSubtask"],
        serde_json::json!([1, 2])
    );
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, [bytes, b"\n".to_vec()].concat()).unwrap();
    assert_ne!(store.get(&a.task.id).unwrap().revision, current.revision);
}

#[test]
fn malformed_legacy_and_unsupported_records_do_not_hide_or_overwrite_siblings() {
    let (_dir, store) = fixture();
    let good = idea(&store);
    let bad = idea(&store);
    let path = record_path(&store, &bad.task.id);
    for content in [
        "{broken",
        "{\"schemaVersion\":1}",
        "{\"schemaVersion\":2}",
        "{\"schemaVersion\":99}",
    ] {
        fs::write(&path, content).unwrap();
        assert!(store.get(&bad.task.id).is_err());
        assert!(
            store
                .update(&bad.task.id, &bad.revision, TaskPatch::default())
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        let list = store.list(&ListOptions::default()).unwrap();
        assert_eq!(list.tasks.len(), 1);
        assert_eq!(list.tasks[0].task.id, good.task.id);
        assert_eq!(list.errors.len(), 1);
    }
    let legacy = store.root.portable_dir().join("tasks/task-0001");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("task.json"), "{\"schemaVersion\":2}").unwrap();
    assert_eq!(store.list(&ListOptions::default()).unwrap().errors.len(), 2);
    assert!(store.get("task-0001").is_err());
}

#[test]
fn malformed_domain_records_are_rejected_without_writing() {
    let (_dir, store) = fixture();
    let a = add(&store, &idea(&store), "backend", &[]);
    let path = record_path(&store, &a.task.id);
    let original = serde_json::to_value(&a.task).unwrap();
    for (pointer, value) in [
        ("/id", serde_json::json!("task-abcdefgh")),
        ("/title", serde_json::json!(" ")),
        ("/nextSubtaskId", serde_json::json!(1)),
        ("/subtasks/0/id", serde_json::json!("s01")),
        ("/subtasks/0/status", serde_json::json!("review")),
        ("/subtasks/0/dependencies", serde_json::json!(["s99"])),
    ] {
        let mut altered = original.clone();
        *altered.pointer_mut(pointer).unwrap() = value;
        let bytes = serde_json::to_vec(&altered).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(store.get(&a.task.id).is_err(), "{pointer}");
        assert!(store.set_archived(&a.task.id, &a.revision, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn short_ids_collision_retry_and_incomplete_creation_are_safe() {
    let (_dir, store) = fixture();
    let mut generated = BTreeSet::new();
    for _ in 0..1000 {
        let id = random_id();
        validate_id(&id).unwrap();
        assert!(generated.insert(id));
    }
    let a = store
        .create_with_ids(
            NewTask {
                title: "original".into(),
                ..Default::default()
            },
            || "task-abcdefgh".into(),
        )
        .unwrap();
    let incomplete = store.root.portable_dir().join("tasks/task-bcdefghj");
    fs::create_dir(&incomplete).unwrap();
    let mut ids = [a.task.id.as_str(), "task-bcdefghj", "task-cdefghjk"].into_iter();
    let b = store
        .create_with_ids(
            NewTask {
                title: "new".into(),
                ..Default::default()
            },
            || ids.next().unwrap().into(),
        )
        .unwrap();
    assert_eq!(b.task.id, "task-cdefghjk");
    assert_eq!(store.get(&a.task.id).unwrap(), a);
    assert!(fs::read_dir(incomplete).unwrap().next().is_none());
    assert!(
        store
            .create_with_ids(
                NewTask {
                    title: "collision".into(),
                    ..Default::default()
                },
                || a.task.id.clone()
            )
            .is_err()
    );
}

#[test]
fn atomic_failure_and_size_limits_preserve_original_bytes() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let path = record_path(&store, &a.task.id);
    let original = fs::read(&path).unwrap();
    assert!(
        atomic_replace(&path, b"replacement", || bail!(
            "injected before rename failure"
        ))
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    assert!(
        store
            .update(
                &a.task.id,
                &a.revision,
                TaskPatch {
                    description: Some("x".repeat(MAX_BYTES as usize)),
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::write(&path, vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
    assert!(
        store
            .get(&a.task.id)
            .unwrap_err()
            .to_string()
            .contains("exceeds")
    );
    // Initial oversized creation leaves no incomplete directory on normal failure.
    assert!(
        store
            .create_with_ids(
                NewTask {
                    title: "large".into(),
                    description: "x".repeat(MAX_BYTES as usize),
                    ..Default::default()
                },
                || "task-abcdefgh".into()
            )
            .is_err()
    );
    assert!(
        !store
            .root
            .portable_dir()
            .join("tasks/task-abcdefgh")
            .exists()
    );
}

#[test]
fn traversal_is_rejected_before_opening_record_locks() {
    let (_dir, store) = fixture();
    for id in [
        "../outside",
        "task-../../x",
        "/tmp/anything",
        "task-0001",
        "TASK-abcdefgh",
        "task-abcdefgi",
    ] {
        assert!(store.get(id).is_err());
        assert!(store.update(id, "", TaskPatch::default()).is_err());
    }
    assert!(!store.root.root().join("cache").exists());
}

#[cfg(unix)]
#[test]
fn symlink_records_and_directories_are_not_followed() {
    use std::os::unix::fs::symlink;
    let (_dir, store) = fixture();
    let a = idea(&store);
    let outside = tempfile::tempdir().unwrap();
    fs::write(
        outside.path().join("task.json"),
        serde_json::to_vec(&a.task).unwrap(),
    )
    .unwrap();
    let path = record_path(&store, &a.task.id);
    fs::remove_file(&path).unwrap();
    symlink(outside.path().join("task.json"), &path).unwrap();
    assert!(store.get(&a.task.id).is_err());
    assert!(
        store
            .update(&a.task.id, &a.revision, TaskPatch::default())
            .is_err()
    );
    fs::remove_file(&path).unwrap();
    fs::remove_dir(path.parent().unwrap()).unwrap();
    symlink(outside.path(), path.parent().unwrap()).unwrap();
    assert!(store.get(&a.task.id).is_err());
    assert_eq!(store.list(&ListOptions::default()).unwrap().errors.len(), 1);
    assert_eq!(
        fs::read(outside.path().join("task.json")).unwrap(),
        serde_json::to_vec(&a.task).unwrap()
    );
}

#[test]
fn concurrent_stale_writers_have_exactly_one_winner() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let store = store.clone();
            let a = a.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.update(
                    &a.task.id,
                    &a.revision,
                    TaskPatch {
                        description: Some(format!("writer {i}")),
                        ..Default::default()
                    },
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .find_map(|r| r.as_ref().err())
            .unwrap()
            .to_string()
            .contains("task_changed")
    );
}

#[test]
fn different_tasks_can_be_updated_while_another_record_is_locked() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let b = idea(&store);
    let _gate = portable_gate(&store.root, false).unwrap();
    let _record = task_lock(&store.root, &a.task.id, true).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(store.update(&b.task.id, &b.revision, TaskPatch::default()))
            .unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
    worker.join().unwrap();
}

fn child_probe(root: &DataRoot, path: &Path, shared: bool, blocked: bool) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data::tasks::tests::lock_probe_child",
            "--ignored",
        ])
        .env("DEVCROFT_TASK_TEST_ROOT", root.root())
        .env("DEVCROFT_TASK_TEST_LOCK", path)
        .env("DEVCROFT_TASK_TEST_SHARED", shared.to_string())
        .env("DEVCROFT_TASK_TEST_BLOCKED", blocked.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "subprocess helper; invoked only with isolated fixture paths"]
fn lock_probe_child() {
    let Some(root) = std::env::var_os("DEVCROFT_TASK_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let path = PathBuf::from(std::env::var_os("DEVCROFT_TASK_TEST_LOCK").unwrap());
    assert!(path.starts_with(root.join("cache")));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let shared = std::env::var("DEVCROFT_TASK_TEST_SHARED").unwrap() == "true";
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    let blocked = matches!(result, Err(std::fs::TryLockError::WouldBlock));
    assert_eq!(
        blocked,
        std::env::var("DEVCROFT_TASK_TEST_BLOCKED").unwrap() == "true"
    );
    if !blocked {
        result.unwrap();
    }
}

#[test]
fn locks_coordinate_processes_and_release_without_deleting_lock_files() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let gate_path = store.root.root().join("cache/portable-store.lock");
    let record_path = store
        .root
        .root()
        .join(format!("cache/task-locks/{}.lock", a.task.id));
    let gate = portable_gate(&store.root, false).unwrap();
    child_probe(&store.root, &gate_path, true, false);
    child_probe(&store.root, &gate_path, false, true);
    let record = task_lock(&store.root, &a.task.id, true).unwrap();
    child_probe(&store.root, &record_path, false, true);
    child_probe(&store.root, &record_path, true, true);
    drop(record);
    drop(gate);
    child_probe(&store.root, &record_path, false, false);
    let gate = portable_gate(&store.root, true).unwrap();
    child_probe(&store.root, &gate_path, true, true);
    drop(gate);
    assert!(gate_path.exists());
    assert!(record_path.exists());
}

#[test]
fn sync_and_branch_switch_wait_for_task_gate_and_never_commit_locks() {
    let (_dir, store) = fixture();
    let a = idea(&store);
    let portable = store.root.portable_dir();
    let git = |args: &[&str]| super::super::run_git_in(&portable, args).unwrap();
    git(&["init", "-b", "main"]);
    // Local throwaway fixture only; never change global config or a real repo.
    git(&["config", "user.name", "Devcroft Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    for checkout in [false, true] {
        let gate = portable_gate(&store.root, false).unwrap();
        let root = store.root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = if checkout {
                super::super::checkout_branch(&root, "planning").map(|_| ())
            } else {
                super::super::sync_portable(&root).map(|_| ())
            };
            tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(gate);
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        worker.join().unwrap();
    }
    let tracked = git(&["ls-files"]);
    assert!(tracked.contains(&format!("tasks/{}/task.json", a.task.id)));
    assert!(!tracked.contains(".lock"));
    assert!(!tracked.contains(".tmp"));
    assert_eq!(store.get(&a.task.id).unwrap(), a);
}
