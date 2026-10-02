//! Installer contract through the shipped binary, without a desktop or real HOME.
use std::{
    fs,
    process::{Command, Output},
};
fn run(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devcroft"))
        .arg("skill")
        .args(args)
        .env("HOME", home)
        .env("DEVCROFT_DATA_DIR", home.join("data"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}
#[test]
fn install_both_idempotently_and_remove_without_touching_other_skills() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let status = run(home, &["status"]);
    assert!(status.status.success());
    assert!(!home.join(".agents").exists());
    assert!(run(home, &["install"]).status.success());
    assert!(run(home, &["install"]).status.success());
    let shared = home.join(".agents/skills/devcroft");
    let claude = home.join(".claude/skills/devcroft");
    assert_eq!(
        fs::read(shared.join("SKILL.md")).unwrap(),
        fs::read(claude.join("SKILL.md")).unwrap()
    );
    assert!(shared.join("references/review.md").exists());
    assert!(shared.join("references/tutorials.md").exists());
    assert!(shared.join("templates/tutorial.html").exists());
    fs::create_dir_all(home.join(".agents/skills/other")).unwrap();
    assert!(run(home, &["uninstall"]).status.success());
    assert!(!shared.exists());
    assert!(!claude.exists());
    assert!(home.join(".agents/skills/other").exists());
}
#[test]
fn partial_failure_preserves_custom_skill_and_installs_other_target() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let custom = home.join(".claude/skills/devcroft");
    fs::create_dir_all(&custom).unwrap();
    fs::write(custom.join("SKILL.md"), "mine").unwrap();
    let result = run(home, &["install"]);
    assert!(!result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("Needs attention"));
    assert!(stdout.contains("Codex / shared agents: Installed"));
    assert_eq!(fs::read_to_string(custom.join("SKILL.md")).unwrap(), "mine");
    assert!(
        run(home, &["status", "--target", "agents"])
            .status
            .success()
    );
    assert!(
        run(home, &["--target", "agents", "uninstall"])
            .status
            .success()
    );
    assert!(custom.exists());
}
