use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devcroft"))
        .args(args)
        .env("DEVCROFT_DATA_DIR", root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}
fn json(root: &Path, args: &[&str]) -> Value {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn resource_and_feedback_workflow_without_desktop() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    for key in ["repo", "other"] {
        let path = root.join("portable/repositories").join(key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("repository.json"), "{}").unwrap();
    }
    let markdown = temp.path().join("plan.md");
    fs::write(&markdown, "# Plan\n- [ ] Implement\n").unwrap();
    let sessions = temp.path().join("sessions.json");
    fs::write(&sessions, r#"[{"repository":"other","title":"Discussion","key":{"provider":"codex","store":"/tmp/provider","id":"session-id"}}]"#).unwrap();
    let created = json(
        &root,
        &[
            "artifact",
            "create",
            "--repository",
            "repo",
            "--title",
            "Plan",
            "--kind",
            "plan",
            "--content-file",
            markdown.to_str().unwrap(),
            "--sessions-file",
            sessions.to_str().unwrap(),
            "--json",
        ],
    );
    let id = created["artifact"]["id"].as_str().unwrap();
    let revision = created["revision"].as_str().unwrap();
    assert_eq!(created["artifact"]["sessions"][0]["repository"], "other");
    assert!(
        root.join("portable/artifacts")
            .join(id)
            .join("artifact.md")
            .is_file()
    );
    assert!(!root.join("portable/tasks").exists());
    let list = json(
        &root,
        &["artifact", "list", "--repository", "repo", "--json"],
    );
    assert_eq!(list["artifacts"].as_array().unwrap().len(), 1);
    let empty = json(
        &root,
        &["artifact", "list", "--repository", "other", "--json"],
    );
    assert!(empty["artifacts"].as_array().unwrap().is_empty());
    let added = json(
        &root,
        &[
            "artifact",
            "comment",
            "create",
            id,
            "--revision",
            revision,
            "--body",
            "Clarify scope",
            "--json",
        ],
    );
    let comment = added["artifact"]["comments"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let listed = json(&root, &["artifact", "comment", "list", id, "--json"]);
    assert_eq!(listed["comments"][0]["body"], "Clarify scope");
    let stale = run(
        &root,
        &[
            "artifact",
            "update",
            id,
            "--revision",
            revision,
            "--title",
            "Stale",
            "--json",
        ],
    );
    assert!(!stale.status.success());
    assert!(stale.stdout.is_empty());
    let mut current = added;
    for command in ["resolve", "reopen", "delete"] {
        current = json(
            &root,
            &[
                "artifact",
                "comment",
                command,
                id,
                &comment,
                "--revision",
                current["revision"].as_str().unwrap(),
                "--json",
            ],
        );
        if command != "delete" {
            assert_eq!(
                current["artifact"]["comments"][0]["resolved"],
                command == "resolve"
            );
        }
    }
    assert!(
        current["artifact"]["comments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        current["artifact"]["content"],
        created["artifact"]["content"]
    );
    let updated = json(
        &root,
        &[
            "artifact",
            "update",
            id,
            "--revision",
            current["revision"].as_str().unwrap(),
            "--repository",
            "other",
            "--json",
        ],
    );
    assert_eq!(updated["artifact"]["repository"], "other");
}
#[test]
fn task_commands_are_removed_and_repository_is_required() {
    let temp = tempfile::tempdir().unwrap();
    for family in ["task", "subtask"] {
        assert_eq!(run(temp.path(), &[family, "list"]).status.code(), Some(2));
    }
    assert_eq!(
        run(
            temp.path(),
            &[
                "artifact",
                "create",
                "--title",
                "Plan",
                "--kind",
                "plan",
                "--content-file",
                "-"
            ]
        )
        .status
        .code(),
        Some(2)
    );
    let help = run(temp.path(), &["--help"]);
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(!text.contains("subtask"));
    assert!(text.contains("artifact"));
    assert!(text.contains("session"));
}
