use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devcroft"))
        .env("DEVCROFT_DATA_DIR", root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(args)
        .output()
        .unwrap()
}
fn read(root: &Path, args: &[&str]) -> Value {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn complete_headless_graph_lifecycle_and_repository_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for key in ["api", "backend", "ui", "cli", "isolated"] {
        let repo = root.join(format!("portable/repositories/{key}"));
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("repository.json"), json!({"description": format!("Purpose of {key}"), "group": if key == "ui" { "Personal" } else { "Work" }, "future": 42}).to_string()).unwrap();
    }
    let text = root.join("relationship.txt");
    fs::write(&text, "Original direction\nMultiline description").unwrap();
    let mut graph = read(root, &["repository", "relationships", "--json"]);
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 5);
    let mut ids = Vec::new();
    for (from, to) in [
        ("api", "backend"),
        ("api", "ui"),
        ("api", "cli"),
        ("backend", "ui"),
        ("backend", "cli"),
        ("ui", "backend"),
    ] {
        let created = read(
            root,
            &[
                "repository",
                "relationship",
                "create",
                "--from",
                from,
                "--to",
                to,
                "--description-file",
                text.to_str().unwrap(),
                "--revision",
                graph["revision"].as_str().unwrap(),
                "--json",
            ],
        );
        ids.push(created["id"].as_str().unwrap().to_owned());
        graph = read(root, &["repository", "relationships", "--json"]);
    }
    let query = read(
        root,
        &[
            "repository",
            "relationships",
            "backend",
            "--depth",
            "2",
            "--json",
        ],
    );
    assert_eq!(query["dependencies"], json!(["api", "ui"]));
    assert_eq!(query["dependents"], json!(["cli", "ui"]));
    assert_eq!(query["relationships"].as_array().unwrap().len(), 6);
    let group = read(
        root,
        &["repository", "relationships", "--group", "work", "--json"],
    );
    assert_eq!(group["excludedRelationships"].as_array().unwrap().len(), 3);
    let old_revision = graph["revision"].as_str().unwrap();
    let changed = read(
        root,
        &[
            "repository",
            "relationship",
            "update",
            &ids[4],
            "--to",
            "isolated",
            "--description",
            "Rewired",
            "--revision",
            old_revision,
            "--json",
        ],
    );
    assert!(
        !run(
            root,
            &[
                "repository",
                "relationship",
                "delete",
                &ids[0],
                "--revision",
                old_revision
            ]
        )
        .status
        .success()
    );
    read(
        root,
        &[
            "repository",
            "relationship",
            "delete",
            &ids[4],
            "--revision",
            changed["revision"].as_str().unwrap(),
            "--json",
        ],
    );
    let metadata = read(root, &["repository", "get", "api", "--json"]);
    read(
        root,
        &[
            "repository",
            "update",
            "api",
            "--description",
            "Shared contracts",
            "--revision",
            metadata["revision"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !run(
            root,
            &[
                "repository",
                "update",
                "api",
                "--group",
                "Personal",
                "--revision",
                metadata["revision"].as_str().unwrap()
            ]
        )
        .status
        .success()
    );
    let metadata = read(root, &["repository", "get", "api", "--json"]);
    assert_eq!(metadata["metadata"]["description"], "Shared contracts");
    assert_eq!(metadata["metadata"]["future"], 42);
    let graph = read(root, &["repository", "relationships", "--json"]);
    assert_eq!(graph["relationships"].as_array().unwrap().len(), 5);
    assert!(
        graph["relationships"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["description"] == "Original direction\nMultiline description")
    );
    assert!(!root.join("device.json").exists());
}
