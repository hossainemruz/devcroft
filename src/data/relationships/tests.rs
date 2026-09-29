use super::super::{
    get_repository_metadata, patch_repository_purpose, remove_repository, write_json_atomic,
};
use super::*;

fn fixture() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    for (key, group) in [
        ("api", "Work"),
        ("backend", "work"),
        ("ui", "Personal"),
        ("cli", "Work"),
        ("isolated", ""),
    ] {
        write_json_atomic(&root.portable_dir().join(format!("repositories/{key}/repository.json")), &serde_json::json!({
            "key": key, "description": format!("Purpose of {key}"), "group": group, "future": 42,
        })).unwrap();
    }
    (dir, root)
}

fn add(root: &DataRoot, from: &str, to: &str) -> MutationResult {
    mutate(
        root,
        &load(root, Query::default()).unwrap().revision,
        Mutation::Create {
            from: from.into(),
            to: to.into(),
            description: format!("{to} uses {from}\nOriginal text"),
        },
    )
    .unwrap()
}

#[test]
fn supplied_topology_reverse_queries_and_cycles() {
    let (_dir, root) = fixture();
    for (from, to) in [
        ("api", "backend"),
        ("api", "ui"),
        ("api", "cli"),
        ("backend", "ui"),
        ("backend", "cli"),
    ] {
        add(&root, from, to);
    }
    let query = Query {
        repository: Some("backend".into()),
        ..Default::default()
    };
    let graph = load(&root, query.clone()).unwrap();
    assert_eq!(graph.dependencies, ["api"]);
    assert_eq!(graph.dependents, ["cli", "ui"]);
    assert_eq!(graph.nodes.len(), 4);
    assert!(
        graph
            .nodes
            .iter()
            .all(|n| !n.repository.is_linked() && !n.repository.revision.is_empty())
    );
    assert_eq!(load(&root, Query::default()).unwrap().nodes.len(), 5);
    add(&root, "ui", "backend");
    let graph = load(
        &root,
        Query {
            depth: Some(8),
            ..query
        },
    )
    .unwrap();
    assert_eq!(graph.dependencies, ["api", "ui"]);
    assert_eq!(graph.dependents, ["cli", "ui"]);
    assert_eq!(graph.nodes.len(), 4);
    assert_eq!(graph.relationships.len(), 6);
    assert!(
        graph
            .relationships
            .iter()
            .all(|e| e.description == format!("{} uses {}\nOriginal text", e.to, e.from))
    );
    let filtered = load(
        &root,
        Query {
            space: Some("Work".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(filtered.nodes.len(), 3);
    assert_eq!(filtered.excluded_relationships.len(), 3);
    // A space-less legacy record resolves to the default space, so it shows
    // under Personal rather than a special "Ungrouped" bucket.
    let personal = load(
        &root,
        Query {
            space: Some("Personal".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        personal
            .nodes
            .iter()
            .map(|node| node.key().to_owned())
            .collect::<Vec<_>>(),
        vec!["isolated".to_owned(), "ui".to_owned()]
    );
}

#[test]
fn stale_invalid_and_duplicate_writes_preserve_bytes() {
    let (_dir, root) = fixture();
    let old = load(&root, Query::default()).unwrap().revision;
    let edge = add(&root, "api", "backend");
    let path = root.portable_dir().join("repository-relationships.json");
    let original = std::fs::read(&path).unwrap();
    assert!(
        mutate(
            &root,
            &old,
            Mutation::Delete {
                id: edge.id.clone()
            }
        )
        .is_err()
    );
    for (from, to) in [("api", "api"), ("api", "backend"), ("api", "unknown")] {
        assert!(
            mutate(
                &root,
                &edge.revision,
                Mutation::Create {
                    from: from.into(),
                    to: to.into(),
                    description: "text".into()
                }
            )
            .is_err()
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), original);
    for data in ["{", "{\"schemaVersion\":2,\"relationships\":[]}"] {
        std::fs::write(&path, data).unwrap();
        assert!(
            mutate(
                &root,
                &edge.revision,
                Mutation::Delete {
                    id: edge.id.clone()
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), data);
    }
}

#[test]
fn rewire_delete_unknown_fields_and_unresolved_endpoints() {
    let (_dir, root) = fixture();
    let edge = add(&root, "api", "backend");
    let path = root.portable_dir().join("repository-relationships.json");
    let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["future"] = 42.into();
    raw["relationships"][0]["future"] = true.into();
    write_json_atomic(&path, &raw).unwrap();
    assert!(remove_repository(&root, "api").is_err());
    let current = load(&root, Query::default()).unwrap();
    let changed = mutate(
        &root,
        &current.revision,
        Mutation::Update {
            id: edge.id.clone(),
            from: None,
            to: Some("ui".into()),
            description: None,
        },
    )
    .unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["future"], 42);
    assert_eq!(raw["relationships"][0]["future"], true);
    assert_eq!(raw["relationships"][0]["id"], edge.id);
    std::fs::remove_dir_all(root.portable_dir().join("repositories/api")).unwrap();
    let current = load(&root, Query::default()).unwrap();
    assert!(
        current
            .nodes
            .iter()
            .any(|n| n.key() == "api" && n.unresolved)
    );
    assert!(!current.diagnostics.is_empty());
    mutate(&root, &changed.revision, Mutation::Delete { id: edge.id }).unwrap();
    assert!(
        load(&root, Query::default())
            .unwrap()
            .relationships
            .is_empty()
    );
}

#[test]
fn metadata_revisions_preserve_unrelated_fields_and_drafts() {
    let (_dir, root) = fixture();
    let old = get_repository_metadata(&root, "api").unwrap();
    let current = patch_repository_purpose(
        &root,
        "api",
        "New purpose".into(),
        "Personal".into(),
        &old.revision,
    )
    .unwrap();
    assert!(
        patch_repository_purpose(&root, "api", "stale".into(), "Work".into(), &old.revision)
            .is_err()
    );
    assert_eq!(current.extra["future"], 42);
    let graph = load(&root, Query::default()).unwrap();
    let node = graph.nodes.iter().find(|n| n.key() == "api").unwrap();
    assert_eq!(node.repository.revision, current.revision);
    assert_eq!(node.repository.description.as_deref(), Some("New purpose"));
}

#[test]
fn concurrent_writers_have_exactly_one_winner() {
    let (_dir, root) = fixture();
    let revision = load(&root, Query::default()).unwrap().revision;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["backend", "ui"]
        .into_iter()
        .map(|to| {
            let (root, revision, barrier) = (root.clone(), revision.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                mutate(
                    &root,
                    &revision,
                    Mutation::Create {
                        from: "api".into(),
                        to: to.into(),
                        description: "text".into(),
                    },
                )
                .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter(|h| h.thread().id() != std::thread::current().id())
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(
        load(&root, Query::default()).unwrap().relationships.len(),
        1
    );
}

#[test]
fn portable_copy_works_without_device_bindings_and_reports_bad_catalog() {
    let (_dir, root) = fixture();
    add(&root, "api", "backend");
    let (_dir_b, device_b) = fixture();
    std::fs::copy(
        root.portable_dir().join("repository-relationships.json"),
        device_b
            .portable_dir()
            .join("repository-relationships.json"),
    )
    .unwrap();
    let a = load(&root, Query::default()).unwrap();
    let b = load(&device_b, Query::default()).unwrap();
    assert_eq!(a.revision, b.revision);
    assert_eq!(a.relationships, b.relationships);
    std::fs::write(
        device_b
            .portable_dir()
            .join("repositories/isolated/repository.json"),
        "{",
    )
    .unwrap();
    assert_eq!(
        load(&device_b, Query::default()).unwrap().diagnostics.len(),
        1
    );
}

#[test]
fn portable_git_sync_round_trip_includes_edits_and_deletions_only() {
    use crate::data::{InitOptions, ensure_portable_init, set_origin, sync_portable};
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn identity(root: &DataRoot) {
        git(&root.portable_dir(), &["config", "user.name", "Graph test"]);
        git(
            &root.portable_dir(),
            &["config", "user.email", "graph@example.invalid"],
        );
        git(&root.portable_dir(), &["config", "commit.gpgsign", "false"]);
    }
    let (_a, root) = fixture();
    ensure_portable_init(&root, &InitOptions::default()).unwrap();
    identity(&root);
    git(&root.portable_dir(), &["checkout", "-B", "main"]);
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "--bare", "-b", "main"]);
    set_origin(&root, remote.path().to_str().unwrap()).unwrap();
    let edge = add(&root, "api", "backend");
    std::fs::write(
        root.root().join("repository-graph-layout.json"),
        "local layout",
    )
    .unwrap();
    sync_portable(&root).unwrap();
    let b = tempfile::tempdir().unwrap();
    let device_b = DataRoot::new(b.path().join("device-b"));
    ensure_portable_init(
        &device_b,
        &InitOptions {
            clone_url: Some(remote.path().to_string_lossy().into()),
        },
    )
    .unwrap();
    identity(&device_b);
    let graph = load(
        &device_b,
        Query {
            repository: Some("backend".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(graph.dependencies, ["api"]);
    assert!(graph.nodes.iter().all(|n| !n.repository.is_linked()));
    assert!(
        !device_b
            .root()
            .join("repository-graph-layout.json")
            .exists()
    );
    let node = get_repository_metadata(&device_b, "backend").unwrap();
    patch_repository_purpose(
        &device_b,
        "backend",
        "Synced purpose".into(),
        "Personal".into(),
        &node.revision,
    )
    .unwrap();
    let edited = mutate(
        &device_b,
        &graph.revision,
        Mutation::Update {
            id: edge.id.clone(),
            from: None,
            to: Some("ui".into()),
            description: Some("Updated on B".into()),
        },
    )
    .unwrap();
    sync_portable(&device_b).unwrap();
    sync_portable(&root).unwrap();
    let graph = load(&root, Query::default()).unwrap();
    assert_eq!(graph.relationships[0].to, "ui");
    assert_eq!(graph.relationships[0].description, "Updated on B");
    assert_eq!(
        get_repository_metadata(&root, "backend")
            .unwrap()
            .description
            .as_deref(),
        Some("Synced purpose")
    );
    mutate(
        &device_b,
        &edited.revision,
        Mutation::Delete { id: edge.id },
    )
    .unwrap();
    sync_portable(&device_b).unwrap();
    sync_portable(&root).unwrap();
    assert!(
        load(&root, Query::default())
            .unwrap()
            .relationships
            .is_empty()
    );
}
