//! Agent review loop against the shipped binary, without a desktop.
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cli(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_devcroft"))
        .current_dir(cwd)
        .arg("review")
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

fn list(cwd: &Path, args: &[&str]) -> Value {
    let output = cli(cwd, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn agent_review_loop_persists_reanchors_and_isolates_branches() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    git(cwd, &["init", "-b", "main"]);
    git(cwd, &["config", "user.name", "Test"]);
    git(cwd, &["config", "user.email", "test@example.com"]);
    fs::write(cwd.join("code.rs"), "base\n").unwrap();
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-m", "base"]);
    git(cwd, &["checkout", "-b", "topic"]);
    let source = "base\nreview me\nlast\n";
    fs::write(cwd.join("code.rs"), source).unwrap();
    // Seed the desktop-owned record; the CLI deliberately cannot author bodies.
    let store_dir = cwd.join(".git/devcroft-review");
    fs::create_dir_all(&store_dir).unwrap();
    fs::write(store_dir.join("comments.json"), serde_json::to_vec(&json!({
        "next_id": 1,
        "comments": [{"id": "c1", "pair": "[\"origin\",\"main\",\"topic\"]", "scope": "full",
            "anchor": {"path": "code.rs", "side": "new", "start": 2, "end": 2, "outdated": false, "source": source},
            "body": "Please fix this", "resolved": false, "revision": 1}]
    })).unwrap()).unwrap();
    let first = list(cwd, &["list", "--open"]);
    assert_eq!(first[0]["excerpt"], "review me");
    assert!(first[0]["comment"]["anchor"].get("source").is_none());
    fs::write(cwd.join("code.rs"), "prefix\nbase\nreview me\nlast\n").unwrap();
    let shifted = list(cwd, &["list"]);
    assert_eq!(shifted[0]["comment"]["anchor"]["start"], 3);
    assert_eq!(shifted[0]["comment"]["anchor"]["outdated"], false);
    assert!(
        !cli(cwd, &["resolve", "c1", "--revision", "1"])
            .status
            .success()
    );
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-m", "agent code"]);
    assert_eq!(list(cwd, &["list"])[0]["comment"]["id"], "c1");
    assert!(
        list(cwd, &["list", "--uncommitted"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    git(cwd, &["checkout", "-b", "other"]);
    assert!(list(cwd, &["list"]).as_array().unwrap().is_empty());
    assert!(!cli(cwd, &["delete", "c1"]).status.success());
    git(cwd, &["checkout", "topic"]);
    fs::write(cwd.join("code.rs"), "prefix\nbase\nfixed\nlast\n").unwrap();
    assert_eq!(
        list(cwd, &["list"])[0]["comment"]["anchor"]["outdated"],
        true
    );
    assert!(cli(cwd, &["resolve", "c1"]).status.success());
    assert!(
        list(cwd, &["list", "--open"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        list(cwd, &["list", "--resolved"])[0]["comment"]["body"],
        "Please fix this"
    );
    assert!(cli(cwd, &["reopen", "c1"]).status.success());
    assert_eq!(
        list(cwd, &["list", "--open"])[0]["comment"]["resolved"],
        false
    );
    assert!(cli(cwd, &["delete", "c1"]).status.success());
    assert!(list(cwd, &["list"]).as_array().unwrap().is_empty());
    assert!(!cli(cwd, &["resolve", "missing"]).status.success());
}
