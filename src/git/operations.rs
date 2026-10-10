//! Serialized Git mutations used by the native dialog.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

use super::{
    model::{Change, RepositorySnapshot},
    repository::{LoadError, load_snapshot, run_git_args, stderr_message},
};

const MUTATION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;

type CheckoutLock = Arc<Mutex<()>>;
static CHECKOUT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetKind {
    Missing,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TargetFingerprint {
    kind: TargetKind,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mutation {
    Stage {
        paths: Vec<PathBuf>,
    },
    Unstage {
        paths: Vec<PathBuf>,
        unborn: bool,
    },
    StageAll,
    UnstageAll {
        unborn: bool,
    },
    Commit {
        message: String,
    },
    Restore {
        change: Change,
        fingerprint: TargetFingerprint,
    },
    Trash {
        path: PathBuf,
        fingerprint: TargetFingerprint,
    },
}

impl Mutation {
    pub(super) fn progress(&self) -> String {
        match self {
            Self::Stage { paths } => path_progress("Staging", paths),
            Self::Unstage { paths, .. } => path_progress("Unstaging", paths),
            Self::StageAll => "Staging all changes…".into(),
            Self::UnstageAll { .. } => "Unstaging all changes…".into(),
            Self::Commit { .. } => "Creating commit…".into(),
            Self::Restore { change, .. } => {
                format!("Discarding changes to {}…", change.path.display())
            }
            Self::Trash { path, .. } => format!("Moving {} to Trash…", path.display()),
        }
    }

    fn success(&self) -> String {
        match self {
            Self::Stage { paths } => path_progress("Staged", paths),
            Self::Unstage { paths, .. } => path_progress("Unstaged", paths),
            Self::StageAll => "Staged all changes".into(),
            Self::UnstageAll { .. } => "Unstaged all changes".into(),
            Self::Commit { .. } => "Commit created".into(),
            Self::Restore { change, .. } => {
                format!("Discarded changes to {}", change.path.display())
            }
            Self::Trash { path, .. } => format!("Moved {} to Trash", path.display()),
        }
    }
}

pub(super) fn execute(root: &Path, mutation: &Mutation) -> Result<String, String> {
    let lock = checkout_lock(root);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    execute_locked(root, mutation)
}

fn execute_locked(root: &Path, mutation: &Mutation) -> Result<String, String> {
    match mutation {
        Mutation::Trash { path, fingerprint } => {
            ensure_target_unchanged(root, path, fingerprint)?;
            let snapshot = fresh_snapshot(root)?;
            if !snapshot.untracked.iter().any(|change| &change.path == path) {
                return Err(format!(
                    "{} is no longer an untracked file. Refresh and review it again.",
                    path.display()
                ));
            }
            move_to_trash(root, path, fingerprint)
        }
        Mutation::Commit { message } => {
            if message.trim().is_empty() {
                return Err("Enter a commit message before committing.".into());
            }
            if message.len() > MAX_COMMIT_MESSAGE_BYTES {
                return Err("Commit message is too large (maximum 64 KiB).".into());
            }
            let snapshot = fresh_snapshot(root)?;
            ensure_no_conflicts(&snapshot, "committing")?;
            if snapshot.staged.is_empty() {
                return Err("Stage at least one file before committing.".into());
            }
            run_git_mutation(
                root,
                vec!["commit".into(), "--file=-".into()],
                Some(message.as_bytes().to_vec()),
                mutation,
            )
        }
        Mutation::Stage { paths } => {
            validate_paths(paths)?;
            ensure_paths_are_not_conflicted(root, paths, "staging")?;
            let mut args = literal_args(["add", "--"]);
            args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
            run_git_mutation(root, args, None, mutation)
        }
        Mutation::Unstage { paths, unborn } => {
            validate_paths(paths)?;
            ensure_paths_are_not_conflicted(root, paths, "unstaging")?;
            let mut args = if *unborn {
                literal_args(["rm", "-q", "--cached", "--ignore-unmatch", "--"])
            } else {
                literal_args(["reset", "-q", "HEAD", "--"])
            };
            args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
            run_git_mutation(root, args, None, mutation)
        }
        Mutation::StageAll => {
            ensure_no_conflicts(&fresh_snapshot(root)?, "staging all files")?;
            run_git_mutation(root, literal_args(["add", "-A", "--", "."]), None, mutation)
        }
        Mutation::UnstageAll { unborn } => {
            ensure_no_conflicts(&fresh_snapshot(root)?, "unstaging all files")?;
            let args = if *unborn {
                literal_args(["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--", "."])
            } else {
                literal_args(["reset", "-q", "HEAD", "--", "."])
            };
            run_git_mutation(root, args, None, mutation)
        }
        Mutation::Restore {
            change,
            fingerprint,
        } => {
            let path = &change.path;
            validate_paths(std::slice::from_ref(path))?;
            ensure_target_unchanged(root, path, fingerprint)?;
            let snapshot = fresh_snapshot(root)?;
            if !snapshot.unstaged.contains(change) {
                return Err(format!(
                    "{} changed after the confirmation opened. Refresh and review it again.",
                    path.display()
                ));
            }
            if let Some(original) = &change.original_path {
                if !trash_available() {
                    return Err(
                        "Discarding a working-tree rename requires recoverable Trash support."
                            .into(),
                    );
                }
                let mut args = literal_args(["restore", "--worktree", "--"]);
                args.push(original.as_os_str().to_owned());
                run_git_mutation(root, args, None, mutation)?;
                move_to_trash(root, path, fingerprint)?;
                return Ok(mutation.success());
            }
            let mut args = literal_args(["restore", "--worktree", "--"]);
            args.push(path.as_os_str().to_owned());
            run_git_mutation(root, args, None, mutation)
        }
    }
}

fn run_git_mutation(
    root: &Path,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    mutation: &Mutation,
) -> Result<String, String> {
    let output = run_git_args(
        root,
        args,
        stdin,
        MUTATION_TIMEOUT,
        MAX_OUTPUT_BYTES,
        MAX_OUTPUT_BYTES,
    )
    .map_err(load_error)?;
    if !output.status.success() {
        return Err(stderr_message(&output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .filter(|line| !line.is_empty());
    Ok(detail
        .map(|detail| format!("{} · {detail}", mutation.success()))
        .unwrap_or_else(|| mutation.success()))
}

fn load_error(error: LoadError) -> String {
    match error {
        LoadError::GitUnavailable => "Git is unavailable".into(),
        LoadError::NotRepository => "This folder is not a Git repository".into(),
        LoadError::Failed(message) => message,
    }
}

fn literal_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    std::iter::once(OsString::from("--literal-pathspecs"))
        .chain(args.into_iter().map(OsString::from))
        .collect()
}

fn validate_paths(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("No files were selected.".into());
    }
    for path in paths {
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!(
                "Git returned an unsafe repository path: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn checkout_lock(root: &Path) -> CheckoutLock {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let locks = CHECKOUT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn fresh_snapshot(root: &Path) -> Result<RepositorySnapshot, String> {
    load_snapshot(root).map_err(load_error)
}

fn ensure_no_conflicts(snapshot: &RepositorySnapshot, action: &str) -> Result<(), String> {
    if snapshot.conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Resolve merge changes before {action}. Devcroft will not stage conflict markers."
        ))
    }
}

fn ensure_paths_are_not_conflicted(
    root: &Path,
    paths: &[PathBuf],
    action: &str,
) -> Result<(), String> {
    let snapshot = fresh_snapshot(root)?;
    if let Some(conflict) = snapshot
        .conflicts
        .iter()
        .find(|change| paths.iter().any(|path| path == &change.path))
    {
        Err(format!(
            "Resolve {} before {action} it. Devcroft will not stage conflict markers.",
            conflict.path.display()
        ))
    } else {
        Ok(())
    }
}

pub(super) fn capture_restore_target(
    root: &Path,
    path: &Path,
) -> Result<TargetFingerprint, String> {
    let fingerprint = capture_target(root, path)?;
    if fingerprint.kind == TargetKind::Other {
        Err(format!(
            "{} is no longer a file that Devcroft can safely replace.",
            path.display()
        ))
    } else {
        Ok(fingerprint)
    }
}

pub(super) fn capture_trash_target(root: &Path, path: &Path) -> Result<TargetFingerprint, String> {
    let fingerprint = capture_target(root, path)?;
    if matches!(fingerprint.kind, TargetKind::File | TargetKind::Symlink) {
        Ok(fingerprint)
    } else {
        Err(format!(
            "{} is no longer an untracked file that Devcroft can move to Trash.",
            path.display()
        ))
    }
}

fn ensure_target_unchanged(
    root: &Path,
    path: &Path,
    expected: &TargetFingerprint,
) -> Result<(), String> {
    let current = capture_target(root, path)?;
    if &current == expected {
        Ok(())
    } else {
        Err(format!(
            "{} changed after the confirmation opened. Refresh and review it again.",
            path.display()
        ))
    }
}

fn capture_target(root: &Path, path: &Path) -> Result<TargetFingerprint, String> {
    validate_paths(&[path.to_owned()])?;
    let mut candidate = root
        .canonicalize()
        .map_err(|error| format!("Could not verify the checkout path: {error}"))?;
    let components = path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        candidate.push(component.as_os_str());
        if index + 1 < components.len() {
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "{} crosses a symbolic link and cannot be changed safely.",
                        path.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(missing_fingerprint());
                }
                Err(error) => {
                    return Err(format!("Could not inspect {}: {error}", path.display()));
                }
            }
        }
    }
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(missing_fingerprint());
        }
        Err(error) => return Err(format!("Could not inspect {}: {error}", path.display())),
    };
    let kind = if metadata.file_type().is_symlink() {
        TargetKind::Symlink
    } else if metadata.is_file() {
        TargetKind::File
    } else {
        TargetKind::Other
    };
    Ok(metadata_fingerprint(kind, &metadata))
}

fn missing_fingerprint() -> TargetFingerprint {
    TargetFingerprint {
        kind: TargetKind::Missing,
        len: 0,
        modified: None,
        #[cfg(unix)]
        device: 0,
        #[cfg(unix)]
        inode: 0,
    }
}

fn metadata_fingerprint(kind: TargetKind, metadata: &fs::Metadata) -> TargetFingerprint {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;

    TargetFingerprint {
        kind,
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

fn path_progress(verb: &str, paths: &[PathBuf]) -> String {
    match paths {
        [path] => format!("{verb} {}", path.display()),
        _ => format!("{verb} {} files", paths.len()),
    }
}

#[cfg(target_os = "macos")]
fn move_to_trash(root: &Path, path: &Path, expected: &TargetFingerprint) -> Result<String, String> {
    ensure_target_unchanged(root, path, expected)?;
    capture_trash_target(root, path)?;
    let absolute = root.join(path);
    let mut command = trash_command(&absolute);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not open Trash: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("Could not read Trash result: {error}"))?;
                if status.success() {
                    return Ok(format!("Moved {} to Trash", path.display()));
                }
                let message = String::from_utf8_lossy(&output.stderr);
                let message = message.trim();
                return Err(if message.is_empty() {
                    "Finder could not move this file to Trash.".into()
                } else {
                    message.chars().take(2_048).collect()
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Moving the file to Trash timed out.".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Could not move the file to Trash: {error}"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn trash_command(path: &Path) -> Command {
    let mut command = Command::new("/usr/bin/osascript");
    command
        .arg("-e")
        .arg("on run argv\ntell application \"Finder\" to delete POSIX file (item 1 of argv)\nend run")
        .arg("--")
        .arg(path);
    command
}

#[cfg(not(target_os = "macos"))]
fn move_to_trash(_: &Path, _: &Path, _: &TargetFingerprint) -> Result<String, String> {
    Err("Recoverable Trash is not available on this platform.".into())
}

pub(super) const fn trash_available() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "commit.gpgsign", "false"]);
        temp
    }

    #[test]
    fn stage_unstage_commit_and_restore_touch_only_literal_targets() {
        let temp = setup();
        let odd = PathBuf::from(":(glob)*.txt");
        let other = PathBuf::from("other.txt");
        fs::write(temp.path().join(&odd), "one\n").unwrap();
        fs::write(temp.path().join(&other), "other\n").unwrap();

        execute(
            temp.path(),
            &Mutation::Stage {
                paths: vec![odd.clone()],
            },
        )
        .unwrap();
        let status = Command::new("git")
            .args(["status", "--short"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(status.contains(":(glob)*.txt"));
        assert!(status.contains("?? other.txt"));

        execute(
            temp.path(),
            &Mutation::Unstage {
                paths: vec![odd.clone()],
                unborn: true,
            },
        )
        .unwrap();
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(staged.stdout.is_empty());

        execute(
            temp.path(),
            &Mutation::Stage {
                paths: vec![odd.clone()],
            },
        )
        .unwrap();
        execute(
            temp.path(),
            &Mutation::Commit {
                message: "first commit".into(),
            },
        )
        .unwrap();
        fs::write(temp.path().join(&odd), "two\n").unwrap();
        let change = load_snapshot(temp.path()).unwrap().unstaged.remove(0);
        let fingerprint = capture_restore_target(temp.path(), &odd).unwrap();
        execute(
            temp.path(),
            &Mutation::Restore {
                change,
                fingerprint,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(temp.path().join(odd)).unwrap(), "one\n");
    }

    #[test]
    fn rejects_paths_outside_the_checkout() {
        assert!(validate_paths(&[PathBuf::from("../outside")]).is_err());
        assert!(validate_paths(&[PathBuf::from("/outside")]).is_err());
    }

    #[test]
    fn bulk_staging_and_unstaging_work_on_an_unborn_branch() {
        let temp = setup();
        fs::write(temp.path().join("one.txt"), "one\n").unwrap();
        fs::write(temp.path().join("two.txt"), "two\n").unwrap();
        execute(temp.path(), &Mutation::StageAll).unwrap();
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout).lines().count(), 2);

        execute(temp.path(), &Mutation::UnstageAll { unborn: true }).unwrap();
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(staged.stdout.is_empty());
    }

    #[test]
    fn stage_all_refuses_to_resolve_merge_conflicts() {
        let temp = setup();
        let root = temp.path();
        fs::write(root.join("conflict.txt"), "base\n").unwrap();
        git(root, &["add", "conflict.txt"]);
        git(root, &["commit", "-m", "base"]);
        git(root, &["checkout", "-b", "other"]);
        fs::write(root.join("conflict.txt"), "other\n").unwrap();
        git(root, &["commit", "-am", "other"]);
        git(root, &["checkout", "main"]);
        fs::write(root.join("conflict.txt"), "main\n").unwrap();
        git(root, &["commit", "-am", "main"]);
        let merge = Command::new("git")
            .args(["merge", "other"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(!merge.status.success());
        fs::write(root.join("ordinary.txt"), "ordinary\n").unwrap();

        let error = execute(root, &Mutation::StageAll).unwrap_err();
        assert!(error.contains("Resolve merge changes"), "{error}");
        let unmerged = Command::new("git")
            .args(["ls-files", "-u"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(!unmerged.stdout.is_empty());
        assert!(
            fs::read_to_string(root.join("conflict.txt"))
                .unwrap()
                .contains("<<<<<<<")
        );
    }

    #[test]
    fn destructive_actions_reject_targets_changed_after_confirmation() {
        let temp = setup();
        let root = temp.path();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "base"]);
        fs::write(root.join("tracked.txt"), "first edit\n").unwrap();
        let change = load_snapshot(root).unwrap().unstaged.remove(0);
        let fingerprint = capture_restore_target(root, &change.path).unwrap();
        fs::write(root.join("tracked.txt"), "newer external edit\n").unwrap();

        let error = execute(
            root,
            &Mutation::Restore {
                change,
                fingerprint,
            },
        )
        .unwrap_err();
        assert!(error.contains("changed after the confirmation"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "newer external edit\n"
        );

        fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        let fingerprint = capture_trash_target(root, Path::new("untracked.txt")).unwrap();
        git(root, &["add", "untracked.txt"]);
        let error = execute(
            root,
            &Mutation::Trash {
                path: "untracked.txt".into(),
                fingerprint,
            },
        )
        .unwrap_err();
        assert!(error.contains("no longer an untracked file"), "{error}");
        assert!(root.join("untracked.txt").is_file());
    }

    #[test]
    fn restore_rejects_an_index_entry_changed_after_confirmation() {
        let temp = setup();
        let root = temp.path();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "base"]);
        fs::write(root.join("tracked.txt"), "staged-one\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "worktree-edit\n").unwrap();

        let change = load_snapshot(root).unwrap().unstaged.remove(0);
        let fingerprint = capture_restore_target(root, &change.path).unwrap();
        let blob = run_git_args(
            root,
            vec!["hash-object".into(), "-w".into(), "--stdin".into()],
            Some(b"staged-two\n".to_vec()),
            MUTATION_TIMEOUT,
            MAX_OUTPUT_BYTES,
            MAX_OUTPUT_BYTES,
        )
        .unwrap();
        assert!(blob.status.success());
        let oid = String::from_utf8(blob.stdout).unwrap();
        git(
            root,
            &[
                "update-index",
                "--cacheinfo",
                "100644",
                oid.trim(),
                "tracked.txt",
            ],
        );

        let refreshed = load_snapshot(root).unwrap().unstaged.remove(0);
        assert_eq!(change.path, refreshed.path);
        assert_eq!(change.kind, refreshed.kind);
        assert_ne!(change.index_oid, refreshed.index_oid);
        let error = execute(
            root,
            &Mutation::Restore {
                change,
                fingerprint,
            },
        )
        .unwrap_err();
        assert!(error.contains("changed after the confirmation"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "worktree-edit\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_commit_hook_returns_its_reason_without_changing_the_index() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = setup();
        fs::write(temp.path().join("tracked.txt"), "content\n").unwrap();
        execute(temp.path(), &Mutation::StageAll).unwrap();
        let hook = temp.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho blocked-by-test-hook >&2\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        let error = execute(
            temp.path(),
            &Mutation::Commit {
                message: "retained message".into(),
            },
        )
        .unwrap_err();
        assert!(error.contains("blocked-by-test-hook"), "{error}");
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&staged.stdout).trim(),
            "tracked.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkout_lock_survives_dialog_lifetimes_and_serializes_a_blocking_hook() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = setup();
        let root = temp.path();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "base"]);
        fs::write(root.join("tracked.txt"), "commit me\n").unwrap();
        execute(
            root,
            &Mutation::Stage {
                paths: vec!["tracked.txt".into()],
            },
        )
        .unwrap();
        fs::write(root.join("later.txt"), "later\n").unwrap();

        let hook = root.join(".git/hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\ntouch hook-started\nwhile [ ! -f hook-release ]; do sleep 0.02; done\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        let commit_root = root.to_owned();
        let commit = std::thread::spawn(move || {
            execute(
                &commit_root,
                &Mutation::Commit {
                    message: "blocked commit".into(),
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("hook-started").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(root.join("hook-started").exists(), "hook did not start");

        let stage_root = root.to_owned();
        let stage = std::thread::spawn(move || {
            execute(
                &stage_root,
                &Mutation::Stage {
                    paths: vec!["later.txt".into()],
                },
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !stage.is_finished(),
            "second dialog mutation overlapped hook"
        );

        fs::write(root.join("hook-release"), "release\n").unwrap();
        commit.join().unwrap().unwrap();
        stage.join().unwrap().unwrap();
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "later.txt");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trash_passes_the_path_as_one_osascript_argument() {
        let command = trash_command(Path::new("/tmp/a file;$(touch nope)"));
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(
            *args.last().unwrap(),
            std::ffi::OsStr::new("/tmp/a file;$(touch nope)")
        );
    }
}
