use std::{
    ffi::OsString,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use super::model::{Change, ChangeKind, OperationState, RepositorySnapshot};

const MAX_STATUS_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 2 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const REPOSITORY_ENVIRONMENT: [&str; 9] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

pub(super) struct CommandOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) stdout_truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoadError {
    GitUnavailable,
    NotRepository,
    Failed(String),
}

pub(crate) fn load_snapshot(root: &Path) -> Result<RepositorySnapshot, LoadError> {
    let output = run_git(
        root,
        [
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    if !output.status.success() {
        let message = stderr_message(&output);
        if message.contains("not a git repository") {
            return Err(LoadError::NotRepository);
        }
        return Err(LoadError::Failed(message));
    }
    if output.stdout_truncated {
        return Err(LoadError::Failed(
            "Repository status is too large to display safely".to_owned(),
        ));
    }

    let mut snapshot = parse_status(&output.stdout)?;
    snapshot.remotes = load_remotes(root)?;
    snapshot.branches = super::branches::load_branches(root)?;
    snapshot.operation = detect_operation(root);
    Ok(snapshot)
}

fn run_git<const N: usize>(root: &Path, args: [&str; N]) -> Result<CommandOutput, LoadError> {
    run_git_args(
        root,
        args.into_iter().map(OsString::from).collect(),
        None,
        COMMAND_TIMEOUT,
        MAX_STATUS_BYTES,
        MAX_ERROR_BYTES,
    )
}

/// Run one checkout-scoped Git process with bounded, concurrently drained
/// output. Mutations and file-diff reads share this boundary so inherited
/// repository-selection variables cannot redirect either class of command.
pub(super) fn run_git_args(
    root: &Path,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<CommandOutput, LoadError> {
    run_git_process(
        root,
        args,
        stdin,
        timeout,
        stdout_limit,
        stderr_limit,
        false,
    )
}

pub(super) fn run_git_noninteractive(
    root: &Path,
    args: Vec<OsString>,
    timeout: Duration,
    limit: usize,
) -> Result<CommandOutput, LoadError> {
    run_git_process(root, args, None, timeout, limit, limit, true)
}

#[allow(clippy::too_many_arguments)]
fn run_git_process(
    root: &Path,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    noninteractive: bool,
) -> Result<CommandOutput, LoadError> {
    let mut command = git_command(root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    if noninteractive {
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "false")
            .env("SSH_ASKPASS", "false")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true");
        // core.sshCommand above contains the preserved command with BatchMode.
        command.env_remove("GIT_SSH_COMMAND");
    }
    let mut child = command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                LoadError::GitUnavailable
            } else {
                LoadError::Failed(error.to_string())
            }
        })?;
    let writer = stdin.map(|input| {
        let mut stream = child.stdin.take().expect("piped Git stdin");
        std::thread::spawn(move || stream.write_all(&input))
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LoadError::Failed("Could not read Git output".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| LoadError::Failed("Could not read Git output".to_owned()))?;
    let stdout = read_stream(stdout, stdout_limit);
    let stderr = read_stream(stderr, stderr_limit);
    let deadline = Instant::now() + timeout;
    let mut exit_status = None;
    let status = loop {
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(status) => exit_status = status,
                Err(error) => {
                    terminate_git(&mut child);
                    return Err(LoadError::Failed(error.to_string()));
                }
            }
        }
        if let Some(status) = exit_status
            && stdout.is_finished()
            && stderr.is_finished()
            && writer.as_ref().is_none_or(|writer| writer.is_finished())
        {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_git(&mut child);
            return Err(LoadError::Failed("Git command timed out. Check hooks, authentication or signing in your terminal before retrying.".to_owned()));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if let Some(writer) = writer {
        let result = writer
            .join()
            .map_err(|_| LoadError::Failed("Could not write Git input".into()))?;
        if status.success() {
            result.map_err(|error| LoadError::Failed(error.to_string()))?;
        }
    }
    let (stdout, stdout_truncated) = join_stream(stdout)?;
    let (stderr, _) = join_stream(stderr)?;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
    })
}

fn terminate_git(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: each Git child is assigned its own process group at spawn. A
    // negative PID targets only that group, including hooks and helpers.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Build a checkout-scoped Git process. Repository selection must come from
/// `root`, even when Devcroft itself was launched by a Git hook or wrapper
/// that exported variables for another checkout.
pub(super) fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    configure_git_command(&mut command, root);
    command
}

fn configure_git_command(command: &mut Command, root: &Path) {
    command
        .current_dir(root)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_SUPER_PREFIX");
    for variable in REPOSITORY_ENVIRONMENT {
        command.env_remove(variable);
    }
}

fn read_stream(
    mut stream: impl std::io::Read + Send + 'static,
    limit: usize,
) -> std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stream
            .by_ref()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        std::io::copy(&mut stream, &mut std::io::sink())?;
        Ok((bytes, truncated))
    })
}

fn join_stream(
    reader: std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), LoadError> {
    reader
        .join()
        .map_err(|_| LoadError::Failed("Could not read Git output".to_owned()))?
        .map_err(|error| LoadError::Failed(error.to_string()))
}

fn load_remotes(root: &Path) -> Result<Vec<String>, LoadError> {
    let output = run_git(root, ["--no-optional-locks", "remote"])?;
    if !output.status.success() {
        return Err(LoadError::Failed(stderr_message(&output)));
    }
    if output.stdout_truncated {
        return Err(LoadError::Failed(
            "Git remote list is too large to display safely".to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn detect_operation(root: &Path) -> Option<OperationState> {
    let repo = gix::discover(root).ok()?;
    let git_dir = repo.git_dir();
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        Some(OperationState::Rebase)
    } else if git_dir.join("MERGE_HEAD").exists() {
        Some(OperationState::Merge)
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        Some(OperationState::CherryPick)
    } else if git_dir.join("REVERT_HEAD").exists() {
        Some(OperationState::Revert)
    } else {
        None
    }
}

pub(super) fn stderr_message(output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        message.chars().take(2_048).collect()
    }
}

fn parse_status(bytes: &[u8]) -> Result<RepositorySnapshot, LoadError> {
    let mut snapshot = RepositorySnapshot::default();
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            if value == b"(initial)" {
                snapshot.unborn = true;
            } else {
                snapshot.oid = Some(String::from_utf8_lossy(value).into_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            if value == b"(detached)" {
                snapshot.detached = true;
            } else {
                snapshot.branch = Some(String::from_utf8_lossy(value).into_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            snapshot.upstream = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let value = String::from_utf8_lossy(value);
            for part in value.split_ascii_whitespace() {
                if let Some(ahead) = part.strip_prefix('+') {
                    snapshot.ahead = ahead.parse().unwrap_or_default();
                } else if let Some(behind) = part.strip_prefix('-') {
                    snapshot.behind = behind.parse().unwrap_or_default();
                }
            }
        } else if record.starts_with(b"1 ") {
            let fields = split_fields(record, 9)?;
            push_tracked(
                &mut snapshot,
                fields[1],
                raw_path(fields[8]),
                None,
                fields[4],
                fields[7],
            );
        } else if record.starts_with(b"2 ") {
            let fields = split_fields(record, 10)?;
            let original = records
                .next()
                .ok_or_else(|| malformed("rename record is missing its original path"))?;
            push_tracked(
                &mut snapshot,
                fields[1],
                raw_path(fields[9]),
                Some(raw_path(original)),
                fields[4],
                fields[7],
            );
        } else if record.starts_with(b"u ") {
            let fields = split_fields(record, 11)?;
            snapshot.conflicts.push(Change {
                path: raw_path(fields[10]),
                original_path: None,
                kind: ChangeKind::Unmerged,
                index_mode: None,
                index_oid: None,
            });
        } else if let Some(path) = record.strip_prefix(b"? ") {
            snapshot.untracked.push(Change {
                path: raw_path(path),
                original_path: None,
                kind: ChangeKind::Untracked,
                index_mode: None,
                index_oid: None,
            });
        } else if record.starts_with(b"! ") {
            // Ignored entries only appear when explicitly requested; this
            // loader does not request them, but accepting them is harmless.
        } else {
            return Err(malformed("unknown porcelain-v2 record"));
        }
    }
    Ok(snapshot)
}

fn split_fields(record: &[u8], count: usize) -> Result<Vec<&[u8]>, LoadError> {
    let fields = record
        .splitn(count, |byte| *byte == b' ')
        .collect::<Vec<_>>();
    if fields.len() != count {
        Err(malformed("incomplete porcelain-v2 record"))
    } else {
        Ok(fields)
    }
}

fn push_tracked(
    snapshot: &mut RepositorySnapshot,
    xy: &[u8],
    path: PathBuf,
    original_path: Option<PathBuf>,
    index_mode: &[u8],
    index_oid: &[u8],
) {
    let index_mode = Some(String::from_utf8_lossy(index_mode).into_owned());
    let index_oid = Some(String::from_utf8_lossy(index_oid).into_owned());
    let index = xy.first().copied().and_then(ChangeKind::from_status);
    let worktree = xy.get(1).copied().and_then(ChangeKind::from_status);
    if let Some(kind) = index {
        snapshot.staged.push(Change {
            path: path.clone(),
            original_path: rename_origin(kind, &original_path),
            kind,
            index_mode: index_mode.clone(),
            index_oid: index_oid.clone(),
        });
    }
    if let Some(kind) = worktree {
        snapshot.unstaged.push(Change {
            path,
            original_path: rename_origin(kind, &original_path),
            kind,
            index_mode,
            index_oid,
        });
    }
}

fn rename_origin(kind: ChangeKind, original_path: &Option<PathBuf>) -> Option<PathBuf> {
    matches!(kind, ChangeKind::Renamed | ChangeKind::Copied)
        .then(|| original_path.clone())
        .flatten()
}

fn malformed(detail: &str) -> LoadError {
    LoadError::Failed(format!("Could not read repository status: {detail}"))
}

#[cfg(unix)]
fn raw_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn raw_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_covers_descendants_holding_output_pipes() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        run(root, &["init", "-b", "main"]);
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        let hook = root.join(".git/hooks/post-commit");
        fs::write(&hook, "#!/bin/sh\nsleep 5 &\nexit 0\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
        let start = Instant::now();
        let result = run_git_noninteractive(
            root,
            ["commit", "--allow-empty", "-m", "test"]
                .map(Into::into)
                .to_vec(),
            Duration::from_millis(300),
            4096,
        );
        assert!(matches!(result, Err(LoadError::Failed(message)) if message.contains("timed out")));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn parses_branch_divergence_and_every_change_bucket() {
        let status = b"# branch.oid 1234567890abcdef\x00# branch.head main\x00# branch.upstream origin/main\x00# branch.ab +2 -3\x001 M. N... 100644 100644 100644 a b staged.txt\x001 .M N... 100644 100644 100644 a b unstaged.txt\x002 R. N... 100644 100644 100644 a b R100 renamed.txt\x00old name.txt\x00u UU N... 100644 100644 100644 100644 a b c conflict.txt\x00? new file.txt\x00";
        let snapshot = parse_status(status).expect("valid porcelain");
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.upstream.as_deref(), Some("origin/main"));
        assert_eq!((snapshot.ahead, snapshot.behind), (2, 3));
        assert_eq!(snapshot.conflicts.len(), 1);
        assert_eq!(snapshot.staged.len(), 2);
        assert_eq!(snapshot.unstaged.len(), 1);
        assert_eq!(snapshot.untracked.len(), 1);
        assert_eq!(snapshot.staged[0].index_mode.as_deref(), Some("100644"));
        assert_eq!(snapshot.staged[0].index_oid.as_deref(), Some("b"));
        assert_eq!(snapshot.unstaged[0].index_mode.as_deref(), Some("100644"));
        assert_eq!(snapshot.unstaged[0].index_oid.as_deref(), Some("b"));
        assert_eq!(snapshot.staged[1].path, PathBuf::from("renamed.txt"));
        assert_eq!(
            snapshot.staged[1].original_path.as_deref(),
            Some(Path::new("old name.txt"))
        );
    }

    #[test]
    fn a_worktree_edit_after_a_staged_rename_has_no_worktree_rename_origin() {
        let status = b"2 RM N... 100644 100644 100644 a b R100 new.txt\x00old.txt\x00";
        let snapshot = parse_status(status).expect("valid porcelain");

        assert_eq!(snapshot.staged.len(), 1);
        assert_eq!(snapshot.staged[0].kind, ChangeKind::Renamed);
        assert_eq!(
            snapshot.staged[0].original_path.as_deref(),
            Some(Path::new("old.txt"))
        );
        assert_eq!(snapshot.unstaged.len(), 1);
        assert_eq!(snapshot.unstaged[0].kind, ChangeKind::Modified);
        assert_eq!(snapshot.unstaged[0].path, PathBuf::from("new.txt"));
        assert!(snapshot.unstaged[0].original_path.is_none());
        assert_eq!(
            snapshot.staged[0].index_mode,
            snapshot.unstaged[0].index_mode
        );
        assert_eq!(snapshot.staged[0].index_oid, snapshot.unstaged[0].index_oid);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt as _;

        let snapshot = parse_status(b"? invalid-\xff.txt\0").expect("valid porcelain");
        assert_eq!(
            snapshot.untracked[0].path.as_os_str().as_bytes(),
            b"invalid-\xff.txt"
        );
    }

    #[test]
    fn checkout_scope_clears_inherited_repository_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).expect("first checkout");
        fs::create_dir_all(&second).expect("second checkout");
        run(&first, &["init", "-b", "first"]);
        run(&second, &["init", "-b", "second"]);

        let mut command = Command::new("git");
        command.env("GIT_DIR", second.join(".git"));
        configure_git_command(&mut command, &first);
        let output = command
            .args(["status", "--porcelain=v2", "--branch", "-z"])
            .output()
            .expect("status runs");
        assert!(output.status.success());
        let snapshot = parse_status(&output.stdout).expect("valid status");
        assert_eq!(snapshot.branch.as_deref(), Some("first"));
    }

    #[test]
    fn loads_a_real_checkout_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        run(root, &["init", "-b", "main"]);
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("tracked.txt"), "one\n").expect("write tracked");
        run(root, &["add", "tracked.txt"]);
        run(root, &["commit", "-m", "initial"]);
        run(root, &["mv", "tracked.txt", "renamed.txt"]);
        fs::write(root.join("renamed.txt"), "two\n").expect("modify renamed");
        fs::write(root.join("staged.txt"), "staged\n").expect("write staged");
        run(root, &["add", "staged.txt"]);
        fs::write(root.join("untracked.txt"), "new\n").expect("write untracked");

        let snapshot = load_snapshot(root).expect("snapshot loads");
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.staged.len(), 2);
        assert_eq!(snapshot.unstaged.len(), 1);
        assert_eq!(snapshot.untracked.len(), 1);
        let rename = snapshot
            .staged
            .iter()
            .find(|change| change.kind == ChangeKind::Renamed)
            .expect("real rename is preserved");
        assert_eq!(rename.path, PathBuf::from("renamed.txt"));
        assert_eq!(
            rename.original_path.as_deref(),
            Some(Path::new("tracked.txt"))
        );
        assert_eq!(snapshot.unstaged[0].path, PathBuf::from("renamed.txt"));
        assert!(snapshot.unstaged[0].original_path.is_none());
        assert_eq!(rename.index_mode, snapshot.unstaged[0].index_mode);
        assert_eq!(rename.index_oid, snapshot.unstaged[0].index_oid);
    }

    #[test]
    fn loads_unborn_and_detached_head_states() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        run(root, &["init", "-b", "main"]);

        let unborn = load_snapshot(root).expect("unborn snapshot loads");
        assert!(unborn.unborn);
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert!(unborn.oid.is_none());

        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("tracked.txt"), "one\n").expect("write tracked");
        run(root, &["add", "tracked.txt"]);
        run(root, &["commit", "-m", "initial"]);
        run(root, &["checkout", "--detach"]);

        let detached = load_snapshot(root).expect("detached snapshot loads");
        assert!(detached.detached);
        assert!(detached.branch.is_none());
        assert!(detached.oid.is_some());
        assert!(detached.header_status().branch.is_some());
    }

    #[test]
    fn loads_a_linked_worktree_from_its_own_git_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let linked = temp.path().join("linked");
        fs::create_dir_all(&root).expect("root checkout");
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        run(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("tracked.txt"), "one\n").expect("write tracked");
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "initial"]);
        let linked_arg = linked.to_string_lossy().into_owned();
        run(&root, &["worktree", "add", "-b", "linked", &linked_arg]);
        fs::write(linked.join("tracked.txt"), "two\n").expect("modify linked checkout");

        let snapshot = load_snapshot(&linked).expect("linked snapshot loads");
        assert_eq!(snapshot.branch.as_deref(), Some("linked"));
        assert_eq!(snapshot.unstaged.len(), 1);
        assert_eq!(snapshot.unstaged[0].path, PathBuf::from("tracked.txt"));
    }
}
