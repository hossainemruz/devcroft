//! Branch and transport operations. Called while holding the checkout mutation lock.
use super::{
    model::RepositorySnapshot,
    repository::{LoadError, load_snapshot, run_git_args, stderr_message},
};
use std::{ffi::OsString, path::Path, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Source {
    branch: Option<String>,
    oid: Option<String>,
    upstream: Option<String>,
}
impl Source {
    pub fn new(snapshot: &RepositorySnapshot) -> Self {
        Self {
            branch: snapshot.branch.clone(),
            oid: snapshot.oid.clone(),
            upstream: snapshot.upstream.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Fetch,
    Switch {
        reference: String,
    },
    Create {
        name: String,
        start: String,
        start_oid: String,
        track: bool,
    },
    Pull {
        rebase: bool,
    },
    Push {
        remote: String,
        branch: String,
        set_upstream: bool,
    },
    Rebase {
        target: String,
        target_oid: String,
    },
}
impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fetch => "Fetch",
            Self::Switch { .. } => "Switch branch",
            Self::Create { track: true, .. } => "Track remote branch",
            Self::Create { .. } => "Create branch",
            Self::Pull { rebase: false } => "Pull (fast-forward only)",
            Self::Pull { rebase: true } => "Pull with Rebase",
            Self::Push { .. } => "Push",
            Self::Rebase { .. } => "Rebase",
        }
    }
    pub fn changes_worktree(&self) -> bool {
        !matches!(self, Self::Fetch | Self::Push { .. })
    }
}

fn error(error: LoadError) -> String {
    match error {
        LoadError::Failed(message) => message,
        LoadError::GitUnavailable => "Git is unavailable".into(),
        LoadError::NotRepository => "Not a Git repository".into(),
    }
}

pub(super) fn execute(root: &Path, source: &Source, action: &Action) -> Result<String, String> {
    let snapshot = load_snapshot(root).map_err(error)?;
    if !matches!(action, Action::Fetch) && &Source::new(&snapshot) != source {
        return Err("The current branch, HEAD, or upstream changed. Refresh and review the operation again.".into());
    }
    if action.changes_worktree() && snapshot.operation.is_some() {
        return Err(
            "Finish or abort the current Git operation before changing branches or pulling.".into(),
        );
    }
    if matches!(
        action,
        Action::Rebase { .. } | Action::Pull { rebase: true }
    ) && snapshot.is_dirty()
    {
        return Err("Save and commit or stash working-tree changes before rebasing. Devcroft does not auto-stash.".into());
    }
    let mut args: Vec<OsString> = Vec::new();
    match action {
        Action::Fetch => {
            if snapshot.remotes.is_empty() {
                return Err("Add a remote in Git before fetching.".into());
            }
            args.extend(["fetch", "--all"].map(Into::into));
        }
        Action::Switch { reference } => {
            let branch = snapshot
                .branches
                .iter()
                .find(|b| &b.reference == reference && !b.remote)
                .ok_or("The selected local branch no longer exists. Refresh the branch list.")?;
            args.extend(
                [
                    "switch",
                    "--no-guess",
                    "--no-overwrite-ignore",
                    "--",
                    &branch.name,
                ]
                .map(Into::into),
            );
        }
        Action::Create {
            name,
            start,
            start_oid,
            track,
        } => {
            validate_branch(root, name)?;
            if *track
                && !snapshot
                    .branches
                    .iter()
                    .any(|b| b.remote && &b.reference == start)
            {
                return Err("Select a remote branch to track.".into());
            }
            if start != "HEAD" && !snapshot.branches.iter().any(|b| &b.reference == start) {
                return Err("The selected start branch no longer exists.".into());
            }
            let current_oid = if start == "HEAD" {
                snapshot.oid.as_deref()
            } else {
                snapshot
                    .branches
                    .iter()
                    .find(|b| &b.reference == start)
                    .map(|b| b.oid.as_str())
            };
            if current_oid != Some(start_oid.as_str()) {
                return Err(
                    "The selected start point changed. Refresh and review branch creation again."
                        .into(),
                );
            }
            args.extend(
                [
                    "switch",
                    "--no-overwrite-ignore",
                    if *track {
                        "--track=direct"
                    } else {
                        "--no-track"
                    },
                    "-c",
                    name,
                    "--",
                    if *track { start } else { start_oid },
                ]
                .map(Into::into),
            );
        }
        Action::Pull { rebase } => {
            require_branch(&snapshot)?;
            if snapshot.upstream.is_none() {
                return Err("Configure an upstream with Push before pulling.".into());
            }
            let local = snapshot
                .branches
                .iter()
                .find(|branch| branch.current)
                .ok_or("Current branch is unavailable")?;
            if local.push_remote.is_empty()
                || local.push_ref.is_empty()
                || local.push_remote.starts_with('-')
            {
                return Err(
                    "The upstream cannot be fetched. Check the branch configuration.".into(),
                );
            }
            // Fetch first so collisions can be checked before any worktree write.
            // An explicit ref keeps remote fetch refspecs from selecting extra heads.
            run_command(
                root,
                ["fetch", "--", &local.push_remote, &local.push_ref]
                    .map(Into::into)
                    .to_vec(),
            )?;
            let refreshed = load_snapshot(root).map_err(error)?;
            if &Source::new(&refreshed) != source {
                return Err("HEAD or upstream changed during fetch. Refresh and try again.".into());
            }
            if refreshed.operation.is_some() || (*rebase && refreshed.is_dirty()) {
                return Err("The checkout changed during fetch. Finish the current operation or commit/stash changes before continuing.".into());
            }
            let target = read(
                root,
                &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
                None,
            )?;
            let target = String::from_utf8(target)
                .map_err(|_| "Invalid fetched object id")?
                .trim()
                .to_owned();
            protect_ignored(root, &target, *rebase)?;
            args.extend(if *rebase {
                ["rebase", "--no-autostash", "--", &target]
                    .map(Into::into)
                    .to_vec()
            } else {
                [
                    "merge",
                    "--ff-only",
                    "--no-autostash",
                    "--no-overwrite-ignore",
                    "--",
                    &target,
                ]
                .map(Into::into)
                .to_vec()
            });
        }
        Action::Push {
            remote,
            branch,
            set_upstream,
        } => {
            let current = require_branch(&snapshot)?;
            validate_branch(root, branch)?;
            if !snapshot.remotes.contains(remote) || remote.starts_with('-') {
                return Err("Select a configured remote.".into());
            }
            if !set_upstream {
                let local = snapshot
                    .branches
                    .iter()
                    .find(|b| b.current)
                    .ok_or("Commit before pushing.")?;
                if local.push_remote != *remote || local.push_ref != format!("refs/heads/{branch}")
                {
                    return Err("The upstream destination changed. Refresh and try again.".into());
                }
            }
            // Explicit source/destination avoids push.default, configured push refspecs,
            // and accidental pushes of additional branches or tags.
            args.extend([
                "-c".into(),
                format!("remote.{remote}.mirror=false").into(),
                "push".into(),
                "--no-force".into(),
                "--no-follow-tags".into(),
            ]);
            if *set_upstream {
                args.push("--set-upstream".into());
            }
            args.extend([
                OsString::from("--"),
                remote.into(),
                format!("refs/heads/{current}:refs/heads/{branch}").into(),
            ]);
        }
        Action::Rebase { target, target_oid } => {
            require_branch(&snapshot)?;
            if !snapshot
                .branches
                .iter()
                .any(|b| &b.reference == target && &b.oid == target_oid)
            {
                return Err(
                    "The selected target branch changed. Refresh and confirm the rebase again."
                        .into(),
                );
            }
            protect_ignored(root, target_oid, true)?;
            args.extend(["rebase", "--no-autostash", "--", target_oid].map(Into::into));
        }
    }
    run_command(root, args)?;
    Ok(format!("{} completed", action.label()))
}

fn run_command(root: &Path, mut args: Vec<OsString>) -> Result<(), String> {
    // Respect custom SSH commands while making OpenSSH fail instead of waiting
    // for an invisible terminal prompt. Other SSH variants retain their setup.
    let ssh = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .or_else(|| config(root, "core.sshCommand"));
    let variant = std::env::var("GIT_SSH_VARIANT")
        .ok()
        .or_else(|| config(root, "ssh.variant"));
    if variant.as_deref().is_none_or(|v| v == "ssh" || v == "auto")
        && std::env::var_os("GIT_SSH").is_none()
    {
        let ssh = format!("{} -oBatchMode=yes", ssh.as_deref().unwrap_or("ssh"));
        args.splice(
            0..0,
            [
                OsString::from("-c"),
                format!("core.sshCommand={ssh}").into(),
            ],
        );
    } else if let Some(ssh) = ssh {
        args.splice(
            0..0,
            [
                OsString::from("-c"),
                format!("core.sshCommand={ssh}").into(),
            ],
        );
    }
    let output = super::repository::run_git_noninteractive(
        root,
        args,
        Duration::from_secs(120),
        1024 * 1024,
    )
    .map_err(error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|part| part.chars().take(2048).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        if message.is_empty() {
            message = stderr_message(&output);
        }
        let lower = message.to_lowercase();
        if [
            "authentication",
            "permission denied",
            "could not read",
            "signing failed",
            "pinentry",
            "terminal prompts disabled",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            message.push_str("\nComplete authentication/signing in your terminal or configure a noninteractive credential helper/SSH agent, then retry.");
        }
        return Err(message);
    }
    Ok(())
}

fn read(root: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
    let output = run_git_args(
        root,
        args.iter().map(OsString::from).collect(),
        stdin,
        Duration::from_secs(30),
        8 * 1024 * 1024,
        4096,
    )
    .map_err(error)?;
    if !output.status.success() {
        return Err(stderr_message(&output));
    }
    if output.stdout_truncated {
        return Err(
            "Git safety check is too large; perform this operation in the terminal.".into(),
        );
    }
    Ok(output.stdout)
}

/// Git's integration commands may overwrite ignored files without asking. Check
/// incoming paths (including replayed commits) before handing them the worktree.
fn protect_ignored(root: &Path, target: &str, rebase: bool) -> Result<(), String> {
    let ignored = read(
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        None,
    )?;
    if ignored.is_empty() {
        return Ok(());
    }
    let mut incoming = read(root, &["ls-tree", "-r", "--name-only", "-z", target], None)?;
    if rebase {
        let commits = read(root, &["rev-list", &format!("{target}..HEAD")], None)?;
        incoming.extend(read(
            root,
            &[
                "diff-tree",
                "--stdin",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-z",
            ],
            Some(commits),
        )?);
    }
    let case_insensitive = config(root, "core.ignorecase").as_deref() == Some("true");
    let normalize = |path: &[u8]| {
        if case_insensitive {
            path.to_ascii_lowercase()
        } else {
            path.to_vec()
        }
    };
    let mut incoming: Vec<_> = incoming
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(normalize)
        .collect();
    incoming.sort();
    incoming.dedup();
    for path in ignored
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let normalized = normalize(path);
        let collision = incoming.binary_search(&normalized).is_ok()
            || normalized.iter().enumerate().any(|(index, byte)| {
                *byte == b'/'
                    && incoming
                        .binary_search(&normalized[..index].to_vec())
                        .is_ok()
            })
            || {
                let mut prefix = normalized.clone();
                prefix.push(b'/');
                let index = incoming.partition_point(|entry| entry < &prefix);
                incoming
                    .get(index)
                    .is_some_and(|entry| entry.starts_with(&prefix))
            };
        if collision {
            return Err(format!(
                "Ignored file {} would be overwritten. Move or back it up before continuing.",
                String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(())
}

fn require_branch(snapshot: &RepositorySnapshot) -> Result<&str, String> {
    if snapshot.unborn {
        return Err("Create a commit before using this operation.".into());
    }
    snapshot
        .branch
        .as_deref()
        .ok_or_else(|| "Switch to a local branch before using this operation.".into())
}

pub(super) fn validate_branch(root: &Path, name: &str) -> Result<(), String> {
    // --branch expands @{-N}; UI input must always be a literal branch name.
    if name.is_empty() || name.starts_with('-') || name == "HEAD" || name.contains("@{") {
        return Err("Enter a valid literal branch name.".into());
    }
    let output = run_git_args(
        root,
        ["check-ref-format", "--branch", name]
            .into_iter()
            .map(Into::into)
            .collect(),
        None,
        Duration::from_secs(10),
        4096,
        4096,
    )
    .map_err(error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(stderr_message(&output))
    }
}

fn config(root: &Path, key: &str) -> Option<String> {
    let output = run_git_args(
        root,
        ["config", "--get", key]
            .into_iter()
            .map(Into::into)
            .collect(),
        None,
        Duration::from_secs(10),
        8192,
        1024,
    )
    .ok()?;
    (output.status.success() && !output.stdout_truncated)
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};
    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn init(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }
    fn commit(root: &Path, path: &str, text: &str) {
        fs::write(root.join(path), text).unwrap();
        git(root, &["add", "--", path]);
        git(root, &["commit", "-m", text]);
    }
    fn run(root: &Path, action: Action) -> Result<String, String> {
        let source = Source::new(&load_snapshot(root).unwrap());
        super::super::operations::execute(
            root,
            &super::super::operations::Mutation::Remote { source, action },
        )
    }
    fn push() -> Action {
        Action::Push {
            remote: "origin".into(),
            branch: "main".into(),
            set_upstream: true,
        }
    }
    #[test]
    fn create_rejects_a_moved_start_point() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root);
        commit(root, "file", "base");
        let start_oid = git(root, &["rev-parse", "HEAD"]);
        git(root, &["branch", "start"]);
        commit(root, "file", "next");
        git(root, &["branch", "-f", "start", "HEAD"]);
        let result = run(
            root,
            Action::Create {
                name: "new".into(),
                start: "refs/heads/start".into(),
                start_oid,
                track: false,
            },
        );
        assert!(result.unwrap_err().contains("start point changed"));
        assert!(git(root, &["branch", "--list", "new"]).is_empty());
    }
    #[test]
    fn create_switch_and_stale_source_preserve_user_work() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root);
        commit(root, "file", "base");
        run(
            root,
            Action::Create {
                name: "feature/nice".into(),
                start: "HEAD".into(),
                start_oid: git(root, &["rev-parse", "HEAD"]),
                track: false,
            },
        )
        .unwrap();
        commit(root, "file", "feature");
        run(
            root,
            Action::Switch {
                reference: "refs/heads/main".into(),
            },
        )
        .unwrap();
        fs::write(root.join("file"), "unsaved disk change").unwrap();
        assert!(
            run(
                root,
                Action::Switch {
                    reference: "refs/heads/feature/nice".into()
                }
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.join("file")).unwrap(),
            "unsaved disk change"
        );
        for name in ["-bad", "a..b", "@{-1}", "HEAD", "with space"] {
            assert!(validate_branch(root, name).is_err());
        }
        let source = Source::new(&load_snapshot(root).unwrap());
        commit(root, "file", "later");
        assert!(
            execute(
                root,
                &source,
                &Action::Switch {
                    reference: "refs/heads/feature/nice".into()
                }
            )
            .unwrap_err()
            .contains("changed")
        );
    }
    #[test]
    fn local_remote_fetch_tracking_push_and_fast_forward_pull() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        init(&root);
        let remote = temp.path().join("remote.git");
        git(
            temp.path(),
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        commit(&root, "base", "base");
        run(&root, push()).unwrap();
        assert_eq!(
            load_snapshot(&root).unwrap().upstream.as_deref(),
            Some("origin/main")
        );
        let peer = temp.path().join("peer");
        init(&peer);
        git(
            &peer,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&peer, Action::Fetch).unwrap();
        run(
            &peer,
            Action::Create {
                name: "tracked".into(),
                start: "refs/remotes/origin/main".into(),
                start_oid: git(&peer, &["rev-parse", "refs/remotes/origin/main"]),
                track: true,
            },
        )
        .unwrap();
        commit(&peer, "peer", "peer");
        run(
            &peer,
            Action::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
            },
        )
        .unwrap();
        run(&root, Action::Pull { rebase: false }).unwrap();
        assert!(root.join("peer").exists());
        // Explicit single-ref pushes ignore configured wildcard refspecs and mirror mode.
        git(&root, &["branch", "private"]);
        git(
            &root,
            &["config", "remote.origin.push", "refs/heads/*:refs/heads/*"],
        );
        git(&root, &["config", "remote.origin.mirror", "true"]);
        commit(&root, "local", "local");
        run(
            &root,
            Action::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
            },
        )
        .unwrap();
        assert!(!git(&remote, &["for-each-ref", "--format=%(refname)"]).contains("private"));
    }
    #[test]
    fn divergent_pull_refuses_merge_but_explicit_rebase_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        init(&root);
        let remote = temp.path().join("remote.git");
        git(
            temp.path(),
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        commit(&root, "base", "base");
        run(&root, push()).unwrap();
        let peer = temp.path().join("peer");
        init(&peer);
        git(
            &peer,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&peer, Action::Fetch).unwrap();
        run(
            &peer,
            Action::Create {
                name: "tracked".into(),
                start: "refs/remotes/origin/main".into(),
                start_oid: git(&peer, &["rev-parse", "refs/remotes/origin/main"]),
                track: true,
            },
        )
        .unwrap();
        commit(&peer, "peer", "peer");
        run(
            &peer,
            Action::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
            },
        )
        .unwrap();
        commit(&root, "local", "local");
        let head = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["config", "pull.rebase", "true"]);
        git(&root, &["config", "pull.ff", "only"]);
        assert!(run(&root, Action::Pull { rebase: false }).is_err());
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
        assert!(
            run(
                &root,
                Action::Push {
                    remote: "origin".into(),
                    branch: "main".into(),
                    set_upstream: false
                }
            )
            .is_err()
        );
        run(&root, Action::Pull { rebase: true }).unwrap();
        assert!(root.join("local").exists() && root.join("peer").exists());
    }
    #[test]
    fn worktree_operations_preserve_ignored_files() {
        for operation in 0..4 {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root);
            commit(root, ".gitignore", "secret\n");
            git(root, &["branch", "base"]);
            fs::write(root.join("secret"), "incoming").unwrap();
            git(root, &["add", "-f", "secret"]);
            git(root, &["commit", "-m", "incoming secret"]);
            let target_oid = git(root, &["rev-parse", "HEAD"]);
            git(root, &["switch", "base"]);
            fs::write(root.join("secret"), "local ignored work").unwrap();
            // A local upstream exercises fetch-before-integration without a network.
            git(root, &["branch", "--set-upstream-to=main", "base"]);
            let action = match operation {
                0 => Action::Switch {
                    reference: "refs/heads/main".into(),
                },
                1 => Action::Create {
                    name: "new".into(),
                    start: "refs/heads/main".into(),
                    start_oid: target_oid.clone(),
                    track: false,
                },
                2 => Action::Pull { rebase: false },
                _ => Action::Rebase {
                    target: "refs/heads/main".into(),
                    target_oid,
                },
            };
            assert!(
                run(root, action).is_err(),
                "operation {operation} overwrote ignored work"
            );
            assert_eq!(
                fs::read_to_string(root.join("secret")).unwrap(),
                "local ignored work"
            );
            assert_eq!(git(root, &["branch", "--show-current"]), "base");
        }
    }

    #[cfg(unix)]
    #[test]
    fn push_hook_stdout_is_visible_and_prompts_are_disabled() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        init(&root);
        let remote = temp.path().join("remote.git");
        git(
            temp.path(),
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        commit(&root, "base", "base");
        let hook = root.join(".git/hooks/pre-push");
        fs::write(&hook, "#!/bin/sh\ntest \"$GIT_TERMINAL_PROMPT\" = 0 || exit 2\ntest \"$GCM_INTERACTIVE\" = never || exit 2\necho hook-says-run-checks\nexit 1\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            run(&root, push())
                .unwrap_err()
                .contains("hook-says-run-checks")
        );
    }

    #[test]
    fn rebase_rejects_a_target_moved_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root);
        commit(root, "file", "base");
        git(root, &["branch", "target"]);
        let target_oid = git(root, &["rev-parse", "target"]);
        commit(root, "other", "newer");
        let source = Source::new(&load_snapshot(root).unwrap());
        git(root, &["branch", "-f", "target", "HEAD"]);
        let error = execute(
            root,
            &source,
            &Action::Rebase {
                target: "refs/heads/target".into(),
                target_oid,
            },
        )
        .unwrap_err();
        assert!(error.contains("target branch changed"), "{error}");
        assert!(load_snapshot(root).unwrap().operation.is_none());
    }

    #[test]
    fn rebase_blocks_dirty_state_and_preserves_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root);
        commit(root, "file", "base\n");
        run(
            root,
            Action::Create {
                name: "feature".into(),
                start: "HEAD".into(),
                start_oid: git(root, &["rev-parse", "HEAD"]),
                track: false,
            },
        )
        .unwrap();
        commit(root, "file", "feature\n");
        run(
            root,
            Action::Switch {
                reference: "refs/heads/main".into(),
            },
        )
        .unwrap();
        commit(root, "file", "main\n");
        run(
            root,
            Action::Switch {
                reference: "refs/heads/feature".into(),
            },
        )
        .unwrap();
        fs::write(root.join("extra"), "untracked").unwrap();
        git(root, &["config", "rebase.autoStash", "true"]);
        assert!(
            run(
                root,
                Action::Rebase {
                    target: "refs/heads/main".into(),
                    target_oid: git(root, &["rev-parse", "main"]),
                }
            )
            .unwrap_err()
            .contains("auto-stash")
        );
        fs::remove_file(root.join("extra")).unwrap();
        assert!(
            run(
                root,
                Action::Rebase {
                    target: "refs/heads/main".into(),
                    target_oid: git(root, &["rev-parse", "main"]),
                }
            )
            .is_err()
        );
        let state = load_snapshot(root).unwrap();
        assert_eq!(
            state.operation,
            Some(super::super::model::OperationState::Rebase)
        );
        assert_eq!(state.conflicts.len(), 1);
        assert!(
            fs::read_to_string(root.join("file"))
                .unwrap()
                .contains("<<<<<<<")
        );
    }
}
