//! Exercise the shipped binary, not internal handlers, with isolated data roots.
//! The same shell contract is used by both OpenCode and Claude instructions.
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data root");
        let cwd = temp.path().join("unrelated terminal");
        fs::create_dir(&cwd).unwrap();
        Self {
            _temp: temp,
            root,
            cwd,
        }
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_devcroft"))
            .args(args)
            .env("DEVCROFT_DATA_DIR", &self.root)
            .env("DEVCROFT_SOCK", self.root.join("no-desktop.sock"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Failed parsing may close stdin without consuming the supplied bytes.
        if let Err(error) = child.stdin.take().unwrap().write_all(stdin) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str], stdin: &[u8]) -> Value {
        let mut args = args.to_vec();
        if !args.contains(&"--json") {
            args.push("--json");
        }
        let output = self.run(&args, stdin);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected diagnostic: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["formatVersion"], 1);
        value
    }

    fn error(&self, args: &[&str], stdin: &[u8], code: i32, message: &str) {
        let output = self.run(args, stdin);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "failed operation emitted stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "expected {message:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repositories(&self) {
        for key in ["public-api", "backend", "deployment"] {
            self.record(
                "repositories",
                key,
                "repository.json",
                json!({"key": key, "displayName": key}),
            );
        }
    }

    fn record(&self, family: &str, id: &str, file: &str, value: Value) -> PathBuf {
        let dir = self.root.join("portable").join(family).join(id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    fn idea(&self) -> Value {
        self.json(&["task", "create", "--title", "Expose the API"], b"")
    }

    fn mutate_task(
        &self,
        current: &Value,
        family: &str,
        verb: &str,
        extra: &[&str],
        stdin: &[u8],
    ) -> Value {
        let mut args = vec![
            family,
            verb,
            task_id(current),
            "--revision",
            revision(current),
        ];
        args.extend_from_slice(extra);
        self.json(&args, stdin)
    }

    fn mutate_artifact(&self, current: &Value, verb: &str, extra: &[&str], stdin: &[u8]) -> Value {
        let mut args = vec![
            "artifact",
            verb,
            artifact_id(current),
            "--revision",
            revision(current),
        ];
        args.extend_from_slice(extra);
        self.json(&args, stdin)
    }

    fn assert_headless(&self) {
        assert!(!self.root.join("device.json").exists());
        assert!(!self.root.join("portable/.git").exists());
        assert!(!self.root.join("cache/devcroft.sock").exists());
    }
}

fn task_id(value: &Value) -> &str {
    value["task"]["id"].as_str().unwrap()
}
fn artifact_id(value: &Value) -> &str {
    value["artifact"]["id"].as_str().unwrap()
}
fn revision(value: &Value) -> &str {
    value["revision"].as_str().unwrap()
}

#[test]
fn standalone_artifact_browser_smoke_contract() {
    let f = Fixture::new();
    let first = f.json(
        &[
            "artifact",
            "create",
            "--title",
            "Standalone RFC",
            "--kind",
            "rfc",
            "--content-file",
            "-",
        ],
        b"# Before\n",
    );
    assert!(!f.root.join("portable/tasks").exists());
    let revised = f.mutate_artifact(
        &first,
        "update",
        &["--content-file", "-"],
        b"# After\n\nLive revision\n",
    );
    assert_ne!(revision(&first), revision(&revised));
    let read = f.json(&["artifact", "get", artifact_id(&first)], b"");
    assert_eq!(read, revised);
    let archived = f.mutate_artifact(&revised, "archive", &[], b"");
    assert_eq!(f.json(&["artifact", "list"], b"")["artifacts"], json!([]));
    assert_eq!(
        f.json(&["artifact", "get", artifact_id(&first)], b""),
        archived
    );
    assert_eq!(
        f.json(&["artifact", "list", "--include-archived"], b"")["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let restored = f.mutate_artifact(&archived, "unarchive", &[], b"");
    assert_eq!(
        restored["artifact"]["content"],
        "# After\n\nLive revision\n"
    );
    f.assert_headless();
}

#[test]
fn shared_agent_workflow_is_headless_and_revision_checked() {
    let f = Fixture::new();
    assert_eq!(
        f.json(&["repository", "list"], b"")["repositories"],
        json!([])
    );
    f.repositories();
    let discovery = f.json(&["repository", "--json", "list"], b"");
    assert_eq!(
        discovery["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["backend", "deployment", "public-api"]
    );
    let mut task = f.idea();
    assert!(task_id(&task).starts_with("task-"));
    assert_eq!(task["progress"], json!({"completed": 0, "total": 0}));
    assert_eq!(task["involvedRepositories"], json!([]));
    let mut rfc = f.json(
        &[
            "artifact",
            "create",
            "--title",
            "API RFC",
            "--kind",
            "rfc",
            "--content-file",
            "-",
        ],
        "# API\n\nRequirements: λ\n".as_bytes(),
    );
    fs::write(
        f.cwd.join("plan with spaces.md"),
        "# Plan\n\nContract → handler → deployment.\n",
    )
    .unwrap();
    let plan = f.json(
        &[
            "artifact",
            "create",
            "--title",
            "Implementation plan",
            "--kind",
            "plan",
            "--content-file",
            "plan with spaces.md",
        ],
        b"",
    );
    assert!(artifact_id(&rfc).starts_with("art-"));
    task = f.mutate_task(
        &task,
        "task",
        "update",
        &[
            "--repository",
            "deployment",
            "--artifact",
            artifact_id(&rfc),
            "--artifact",
            artifact_id(&plan),
            "--description-file",
            "-",
        ],
        b"## Requirements\n\nExpose the agreed API.\n",
    );
    let shared = f.json(
        &[
            "task",
            "create",
            "--title",
            "Another consumer",
            "--artifact",
            artifact_id(&rfc),
        ],
        b"",
    );
    assert_eq!(shared["task"]["artifacts"], json!([artifact_id(&rfc)]));

    for (repo, dep, expected) in [
        ("public-api", None, "s1"),
        ("backend", Some("s1"), "s2"),
        ("deployment", Some("s2"), "s3"),
    ] {
        let mut args = vec![
            "--title",
            repo,
            "--repository",
            repo,
            "--artifact",
            artifact_id(&plan),
            "--description-file",
            "-",
        ];
        if let Some(dep) = dep {
            args.extend(["--depends-on", dep]);
        }
        task = f.mutate_task(
            &task,
            "subtask",
            "create",
            &args,
            b"Objective, scope, and completion criteria.\n",
        );
        assert_eq!(task["subtaskId"], expected);
    }
    assert_eq!(task["progress"], json!({"completed": 0, "total": 3}));
    assert_eq!(
        task["involvedRepositories"],
        json!(["backend", "deployment", "public-api"])
    );
    for repo in ["public-api", "backend", "deployment"] {
        let listed = f.json(&["task", "list", "--repository", repo], b"");
        assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(
            listed["tasks"][0]["task"]["subtasks"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    for status in ["doing", "blocked", "todo", "done"] {
        // Dependencies communicate order but do not prohibit explicit completion.
        task = f.mutate_task(&task, "subtask", "update", &["s3", "--status", status], b"");
        assert_eq!(task["task"]["subtasks"][2]["status"], status);
    }
    assert_eq!(task["progress"]["completed"], 1);
    let old_rfc = rfc.clone();
    rfc = f.mutate_artifact(
        &rfc,
        "update",
        &["--content-file", "-"],
        b"# Revised API\n\nCurrent content, no pinned revision.\n",
    );
    assert_eq!(rfc["artifact"]["title"], "API RFC");
    rfc = f.mutate_artifact(&rfc, "update", &["--title", "Revised RFC"], b"");
    assert_eq!(
        rfc["artifact"]["content"],
        "# Revised API\n\nCurrent content, no pinned revision.\n"
    );
    f.error(
        &[
            "artifact",
            "update",
            artifact_id(&rfc),
            "--revision",
            revision(&old_rfc),
            "--clear-content",
            "--json",
        ],
        b"",
        1,
        "artifact_changed",
    );
    assert_eq!(
        revision(&f.json(&["task", "get", task_id(&task)], b"")),
        revision(&task)
    );

    let before_order = task.clone();
    task = f.mutate_task(&task, "subtask", "reorder", &["s3", "s1", "s2"], b"");
    assert_eq!(task["task"]["subtasks"][0]["id"], "s3");
    assert_eq!(task["task"]["subtasks"][0]["status"], "done");
    f.error(
        &[
            "task",
            "update",
            task_id(&task),
            "--revision",
            revision(&before_order),
            "--title",
            "Stale",
            "--json",
        ],
        b"",
        1,
        "task_changed",
    );
    f.error(
        &[
            "subtask",
            "remove",
            task_id(&task),
            "s1",
            "--revision",
            revision(&task),
            "--json",
        ],
        b"",
        1,
        "still referenced",
    );
    task = f.mutate_task(
        &task,
        "subtask",
        "update",
        &[
            "s2",
            "--clear-dependencies",
            "--clear-artifacts",
            "--clear-description",
            "--repository",
            "public-api",
        ],
        b"",
    );
    assert_eq!(task["task"]["subtasks"][2]["dependencies"], json!([]));
    assert_eq!(task["task"]["subtasks"][2]["artifacts"], json!([]));
    assert_eq!(task["task"]["subtasks"][2]["description"], "");
    task = f.mutate_task(&task, "subtask", "remove", &["s1"], b"");
    task = f.mutate_task(
        &task,
        "subtask",
        "create",
        &["--title", "Follow-up", "--repository", "backend"],
        b"",
    );
    assert_eq!(task["subtaskId"], "s4");

    task = f.mutate_task(
        &task,
        "task",
        "update",
        &["--clear-repositories", "--clear-description"],
        b"",
    );
    assert_eq!(task["task"]["repositories"], json!([]));
    assert_eq!(task["task"]["description"], "");
    rfc = f.mutate_artifact(&rfc, "archive", &[], b"");
    assert!(
        f.json(&["task", "get", task_id(&task)], b"")["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.json(&["artifact", "get", artifact_id(&rfc)], b"")["artifact"]["archived"],
        true
    );
    assert_eq!(
        f.json(&["artifact", "list"], b"")["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.json(&["artifact", "list", "--include-archived"], b"")["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    task = f.mutate_task(&task, "task", "archive", &[], b"");
    assert_eq!(task["progress"]["completed"], 1);
    assert_eq!(
        f.json(&["task", "list"], b"")["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.json(&["task", "list", "--include-archived"], b"")["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.json(&["task", "get", task_id(&task)], b"")["task"]["archived"],
        true
    );
    assert_eq!(
        f.json(&["artifact", "get", artifact_id(&plan)], b"")["artifact"]["archived"],
        false
    );
    task = f.mutate_task(&task, "task", "unarchive", &[], b"");
    rfc = f.mutate_artifact(&rfc, "unarchive", &[], b"");
    assert_eq!(task["task"]["archived"], false);
    assert_eq!(rfc["artifact"]["archived"], false);
    rfc = f.mutate_artifact(&rfc, "update", &["--kind", "note", "--clear-content"], b"");
    assert_eq!(rfc["artifact"]["kind"], "note");
    assert_eq!(rfc["artifact"]["content"], "");
    let mut other_cwd = Fixture::new();
    other_cwd.root = f.root.clone();
    assert_eq!(other_cwd.json(&["task", "get", task_id(&task)], b""), task);
    f.assert_headless();
}

#[test]
fn help_usage_human_output_and_limits() {
    let f = Fixture::new();
    for args in [
        vec!["--help"],
        vec!["task", "--help"],
        vec!["task", "update", "--help"],
        vec!["artifact", "create", "--help"],
        vec!["subtask", "reorder", "--help"],
        vec!["repository", "list", "--help"],
    ] {
        let output = f.run(&args, b"");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
        assert!(output.stderr.is_empty());
        assert!(!f.root.exists());
    }
    for args in [
        vec!["task", "update", "task-22222222"],
        vec!["artifact", "create", "--title", "RFC", "--kind", "rfc"],
        vec![
            "subtask",
            "update",
            "task-22222222",
            "s1",
            "--revision",
            "token",
            "--status",
            "merged",
        ],
        vec![
            "artifact",
            "create",
            "--title",
            "RFC",
            "--kind",
            "approval",
            "--content-file",
            "-",
        ],
        vec!["task", "list", "--limit", "0"],
        vec!["artifact", "list", "--limit", "1001"],
    ] {
        f.error(&args, b"", 2, "error:");
    }
    let task = f.idea();
    let output = f.run(&["task", "get", task_id(&task)], b"");
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(human.contains("Not planned"));
    assert!(human.contains(task_id(&task)));
    assert!(human.contains(revision(&task)));
    let _ = f.idea();
    let list = f.json(&["task", "list", "--limit", "1"], b"");
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(list["truncated"], true);
    f.mutate_task(&task, "subtask", "reorder", &[], b"");
    f.assert_headless();
}

#[test]
fn patch_conflicts_and_domain_errors_never_partially_apply() {
    let f = Fixture::new();
    f.repositories();
    let task = f.idea();
    for flags in [
        vec!["--clear-description", "--description-file", "-"],
        vec!["--clear-repositories", "--repository", "backend"],
        vec!["--clear-artifacts", "--artifact", "art-22222222"],
    ] {
        let mut args = vec![
            "task",
            "update",
            task_id(&task),
            "--revision",
            revision(&task),
            "--title",
            "Must not persist",
            "--json",
        ];
        args.extend(flags);
        f.error(&args, b"", 2, "cannot be used");
        assert_eq!(f.json(&["task", "get", task_id(&task)], b""), task);
    }
    for flags in [
        vec!["--title", " "],
        vec!["--repository", "unknown"],
        vec!["--artifact", "art-22222222"],
        vec!["--repository", "backend,backend"],
    ] {
        let mut args = vec![
            "task",
            "update",
            task_id(&task),
            "--revision",
            revision(&task),
            "--json",
        ];
        args.extend(flags);
        f.error(&args, b"", 1, "devcroft task");
        assert_eq!(f.json(&["task", "get", task_id(&task)], b""), task);
    }
    let task = f.mutate_task(
        &task,
        "subtask",
        "create",
        &["--title", "Contract", "--repository", "backend"],
        b"",
    );
    for (flags, code) in [
        (vec!["s1", "--depends-on", "s1"], 1),
        (vec!["s1", "--depends-on", "s9"], 1),
        (vec!["s1", "--depends-on", "s9", "--clear-dependencies"], 2),
    ] {
        let mut args = vec![
            "subtask",
            "update",
            task_id(&task),
            "--revision",
            revision(&task),
            "--json",
        ];
        args.extend(flags);
        f.error(
            &args,
            b"",
            code,
            if code == 2 {
                "error:"
            } else {
                "devcroft subtask"
            },
        );
        assert_eq!(
            f.json(&["task", "get", task_id(&task)], b"")["revision"],
            task["revision"]
        );
    }
    f.error(
        &[
            "subtask",
            "reorder",
            task_id(&task),
            "s1",
            "s1",
            "--revision",
            revision(&task),
            "--json",
        ],
        b"",
        1,
        "duplicate",
    );
    for family in ["task", "artifact"] {
        f.error(
            &[family, "get", "../escape", "--json"],
            b"",
            1,
            "invalid or unsupported",
        );
    }
}

#[test]
fn markdown_input_is_bounded_utf8_and_checked_before_mutation() {
    let f = Fixture::new();
    let task = f.idea();
    fs::write(f.cwd.join("invalid.md"), [0xff, 0xfe]).unwrap();
    fs::write(f.cwd.join("too-big.md"), vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    for (path, message) in [
        ("invalid.md", "UTF-8"),
        ("too-big.md", "4194304"),
        (".", "regular file"),
        ("missing.md", "reading Markdown file"),
    ] {
        f.error(
            &[
                "task",
                "update",
                task_id(&task),
                "--revision",
                revision(&task),
                "--title",
                "Must not persist",
                "--description-file",
                path,
                "--json",
            ],
            b"",
            1,
            message,
        );
        assert_eq!(f.json(&["task", "get", task_id(&task)], b""), task);
    }
    let create = [
        "artifact",
        "create",
        "--title",
        "RFC",
        "--kind",
        "rfc",
        "--content-file",
        "-",
        "--json",
    ];
    f.error(&create, &[0xff], 1, "UTF-8");
    f.error(&create, &vec![b'x'; 4 * 1024 * 1024 + 1], 1, "4194304");
    // Input at the read limit is accepted by the reader but fails the record
    // limit because metadata/JSON escaping also consumes bytes.
    f.error(&create, &vec![b'x'; 4 * 1024 * 1024], 1, "artifact exceeds");
    assert_eq!(f.json(&["artifact", "list"], b"")["artifacts"], json!([]));
    let empty = f.json(&create, b"");
    assert_eq!(empty["artifact"]["content"], "");
    f.error(
        &[
            "artifact",
            "update",
            artifact_id(&empty),
            "--revision",
            revision(&empty),
            "--clear-content",
            "--content-file",
            "-",
            "--json",
        ],
        b"",
        2,
        "cannot be used",
    );
}

#[test]
fn malformed_siblings_missing_links_and_external_edits_are_visible() {
    let f = Fixture::new();
    f.repositories();
    let artifact = f.json(
        &[
            "artifact",
            "create",
            "--title",
            "RFC",
            "--kind",
            "rfc",
            "--content-file",
            "-",
        ],
        b"# Original",
    );
    let task = f.json(
        &[
            "task",
            "create",
            "--title",
            "Linked",
            "--artifact",
            artifact_id(&artifact),
        ],
        b"",
    );
    let mut external = artifact["artifact"].clone();
    external["content"] = json!("# External Markdown edit");
    f.record(
        "artifacts",
        artifact_id(&artifact),
        "artifact.json",
        external,
    );
    f.error(
        &[
            "artifact",
            "archive",
            artifact_id(&artifact),
            "--revision",
            revision(&artifact),
            "--json",
        ],
        b"",
        1,
        "artifact_changed",
    );
    let current = f.json(&["artifact", "get", artifact_id(&artifact)], b"");
    assert_ne!(revision(&current), revision(&artifact));
    assert_eq!(f.json(&["task", "get", task_id(&task)], b""), task);
    let bad_artifact = f.record(
        "artifacts",
        "art-22222222",
        "artifact.json",
        json!({"schemaVersion": 1}),
    );
    f.record(
        "tasks",
        "task-22222222",
        "task.json",
        json!({"schemaVersion": 1}),
    );
    f.record(
        "repositories",
        "broken",
        "repository.json",
        json!({"displayName": 42}),
    );
    for (family, field, expected) in [
        ("task", "tasks", 1),
        ("artifact", "artifacts", 1),
        ("repository", "repositories", 3),
    ] {
        let output = f.run(&[family, "list", "--json"], b"");
        assert_eq!(output.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value[field].as_array().unwrap().len(), expected);
        assert_eq!(value["errors"].as_array().unwrap().len(), 1);
        assert!(!output.stderr.is_empty());
    }
    let old_bytes = fs::read(&bad_artifact).unwrap();
    f.error(
        &[
            "artifact",
            "update",
            "art-22222222",
            "--revision",
            "token",
            "--title",
            "Overwrite",
            "--json",
        ],
        b"",
        1,
        "unsupported artifact schema",
    );
    assert_eq!(fs::read(&bad_artifact).unwrap(), old_bytes);
    fs::remove_file(
        f.root
            .join("portable/artifacts")
            .join(artifact_id(&artifact))
            .join("artifact.json"),
    )
    .unwrap();
    let output = f.run(&["task", "get", task_id(&task), "--json"], b"");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("warning:"));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["task"]["artifacts"], task["task"]["artifacts"]);
    assert_eq!(value["warnings"].as_array().unwrap().len(), 1);
    let cleared = f.mutate_task(&task, "task", "update", &["--clear-artifacts"], b"");
    assert_eq!(cleared["task"]["artifacts"], json!([]));
}

#[test]
fn repository_discovery_reports_mismatches_and_rejects_symlinks() {
    let f = Fixture::new();
    f.record(
        "repositories",
        "backend",
        "repository.json",
        json!({"key": "wrong-key", "displayName": "Backend"}),
    );
    let output = f.run(&["repository", "list", "--json"], b"");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["repositories"][0]["key"], "backend");
    assert_eq!(value["repositories"][0]["displayName"], "Backend");
    assert!(String::from_utf8_lossy(&output.stderr).contains("differs from directory key"));
    #[cfg(unix)]
    {
        let outside = f.cwd.join("outside.json");
        fs::write(&outside, "{}").unwrap();
        let dir = f.root.join("portable/repositories/linked");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(outside, dir.join("repository.json")).unwrap();
        let output = f.run(&["repository", "list", "--json"], b"");
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("symlinks"));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["repositories"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn storage_errors_are_runtime_failures_without_starting_gui() {
    let mut f = Fixture::new();
    let file = f.cwd.join("not a directory");
    fs::write(&file, "unchanged").unwrap();
    f.root = file.join("data");
    f.error(
        &["task", "list", "--json"],
        b"",
        1,
        "creating data directory",
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "unchanged");
    assert!(!Path::new(&f.root).exists());
}
