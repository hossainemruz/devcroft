//! Portable-only sync over the git CLI.
//!
//! The pipeline stages portable files, commits them with
//! `chore(devcroft): sync portable data` when dirty, fetches and rebases
//! onto the upstream (which, right after `fetch`, is the exact fetched
//! commit), then pushes. Every invocation runs with cwd scoped to
//! `portable/`, so `device.json` next door is structurally unreachable.
//! Mutating git goes through the CLI so ssh-agent, credential helpers, and
//! signing config come free; `gix` stays read-only for Review diffs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;

use super::{DataRoot, SYNC_COMMIT_MESSAGE, run_git_in, sanitize_git_message};

/// In-memory sync status. Never persisted, per the plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SyncStatus {
    #[default]
    Idle,
    Syncing,
    Error,
}

/// In-memory sync tracker for the home header status strip and settings
/// dialog to poll. Cheap to clone; all clones share one status.
#[derive(Clone, Debug, Default)]
pub(crate) struct SyncTracker {
    status: Arc<Mutex<SyncStatus>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl SyncTracker {
    pub(crate) fn status(&self) -> SyncStatus {
        *self.status.lock()
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    fn begin(&self) {
        *self.status.lock() = SyncStatus::Syncing;
    }

    fn succeed(&self) {
        *self.status.lock() = SyncStatus::Idle;
        *self.last_error.lock() = None;
    }

    fn fail(&self, error: &str) {
        *self.status.lock() = SyncStatus::Error;
        *self.last_error.lock() = Some(error.to_owned());
    }
}

/// What one sync did. `reload_required` marks outcomes where the rebase may
/// have changed portable files, so Home/Tasks/Review projections must
/// reload — the plan checklist item 6 hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncOutcome {
    pub(crate) committed: bool,
    pub(crate) pushed: bool,
    pub(crate) reload_required: bool,
    pub(crate) head_before: Option<String>,
    pub(crate) head_after: Option<String>,
}

/// Process-wide serializer: concurrent triggers block and run one at a time
/// instead of interleaving git invocations in `portable/`.
static SYNC_SERIALIZER: Mutex<()> = Mutex::new(());

/// Run one sync with the status left at its default tracker-less state.
///
/// Errors stop the pipeline and leave the working tree as git left it
/// (usable: resolve or abort in `portable/` with ordinary git tooling
/// before retrying). Callers must reload portable projections both when
/// `reload_required` is set and after an error — a failed rebase or push
/// still means the rebase may have changed working-tree files.
pub(crate) fn sync_portable(root: &DataRoot) -> Result<SyncOutcome> {
    sync_portable_inner(root, None)
}

/// Run one sync while reporting `idle`/`syncing`/`error` to `tracker`.
pub(crate) fn sync_portable_with_tracker(
    root: &DataRoot,
    tracker: &SyncTracker,
) -> Result<SyncOutcome> {
    sync_portable_inner(root, Some(tracker))
}

fn sync_portable_inner(root: &DataRoot, tracker: Option<&SyncTracker>) -> Result<SyncOutcome> {
    let _serial = SYNC_SERIALIZER.lock();
    if let Some(tracker) = tracker {
        tracker.begin();
    }
    let result = sync_once(root);
    if let Some(tracker) = tracker {
        match &result {
            Ok(_) => tracker.succeed(),
            Err(error) => tracker.fail(&format!("{error:#}")),
        }
    }
    result
}

fn sync_once(root: &DataRoot) -> Result<SyncOutcome> {
    let portable = root.portable_dir();
    if !portable.join(".git").exists() {
        bail!(
            "portable directory at {} is not a git repository (run first-run init)",
            portable.display()
        );
    }
    check_no_operation_in_progress(&portable)?;

    run_git_in(&portable, &["add", "-A"])?;
    let status = run_git_in(&portable, &["status", "--porcelain"])?;
    let committed = if status.trim().is_empty() {
        false
    } else {
        commit_portable(&portable)?;
        true
    };
    let head_before = current_head(&portable)?;

    // No remote: the local commit is the whole sync.
    if remote_url(&portable)?.is_none() {
        return Ok(SyncOutcome {
            committed,
            pushed: false,
            reload_required: false,
            head_before: head_before.clone(),
            head_after: head_before,
        });
    }

    run_git_in(&portable, &["fetch", "origin"])
        .context("fetching portable origin (check network and credentials)")?;
    ensure_upstream(&portable)?;
    // The upstream ref now points at the exact commit just fetched, so a
    // plain rebase integrates precisely that — the plan's documented
    // strategy, with no ref race between fetch and rebase.
    run_git_in(&portable, &["rebase"]).context(
        "rebasing portable onto upstream (resolve or abort the rebase in portable/ before retrying)",
    )?;
    let head_after = current_head(&portable)?;
    let reload_required = head_after != head_before;
    run_git_in(&portable, &["push"])
        .context("pushing portable (check credentials and remote permissions)")?;

    Ok(SyncOutcome {
        committed,
        pushed: true,
        reload_required,
        head_before,
        head_after,
    })
}

fn commit_portable(portable: &Path) -> Result<()> {
    match run_git_in(portable, &["commit", "-m", SYNC_COMMIT_MESSAGE]) {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = format!("{error:#}");
            if is_missing_identity(&message) {
                bail!(
                    "git commit failed: git identity is not configured. Set user.name/user.email for the portable repo (e.g. `git -C {} config user.email you@example.com`) and retry. Details: {}",
                    portable.display(),
                    sanitize_git_message(&message),
                );
            }
            Err(error)
        }
    }
}

/// Pure classifier for the missing-identity failure, unit-tested directly:
/// reproducing it through the real CLI would depend on the machine's global
/// git config, which tests must never touch.
fn is_missing_identity(message: &str) -> bool {
    message.contains("Author identity unknown")
        || message.contains("Committer identity unknown")
        || message.contains("unable to auto-detect email address")
}

fn current_head(portable: &Path) -> Result<Option<String>> {
    // `None` covers the unborn-HEAD case (a repo with no commits yet); every
    // earlier git invocation already proved the CLI itself works.
    match run_git_in(portable, &["rev-parse", "HEAD"]) {
        Ok(head) => Ok(Some(head.trim().to_owned())),
        Err(_) => Ok(None),
    }
}

fn remote_url(portable: &Path) -> Result<Option<String>> {
    match run_git_in(portable, &["remote", "get-url", "origin"]) {
        Ok(url) => Ok(Some(url.trim().to_owned())),
        Err(_) => Ok(None),
    }
}

fn ensure_upstream(portable: &Path) -> Result<()> {
    match run_git_in(
        portable,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        Ok(_) => Ok(()),
        Err(_) => bail!(
            "no upstream is configured for the current branch in {} — set one (e.g. `git push -u origin HEAD`) or resolve it with ordinary git tooling before syncing",
            portable.display()
        ),
    }
}

/// Unusual git states are left for ordinary tooling: stop before touching
/// anything when a merge, rebase, cherry-pick, revert, or bisect owns the
/// working tree.
fn check_no_operation_in_progress(portable: &Path) -> Result<()> {
    let git_dir = PathBuf::from(
        run_git_in(portable, &["rev-parse", "--absolute-git-dir"])?
            .trim()
            .to_owned(),
    );
    for (marker, what) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ] {
        if git_dir.join(marker).exists() {
            bail!(
                "a git {what} is in progress in {} — resolve or abort it there before syncing",
                portable.display()
            );
        }
    }
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        bail!(
            "a git rebase is in progress in {} — resolve or abort it there before syncing",
            portable.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{DataRoot, InitOptions, ensure_portable_init, set_origin};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        git_status(dir, args).expect("git fixture command failed");
    }

    fn git_status(dir: &Path, args: &[&str]) -> Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git CLI must be available for fixture setup");
        if status.success() {
            Ok(())
        } else {
            bail!("git {args:?} failed in {}", dir.display())
        }
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git CLI must be available for fixture setup");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn configure_identity(dir: &Path) {
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn fresh_root() -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        (dir, root)
    }

    fn init_with_identity(root: &DataRoot) {
        ensure_portable_init(root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
    }

    /// Pin the fixture branch to `main`: our `git init` honors the real
    /// user config while fixtures run hermetic, so the name is not stable
    /// across machines. `-B` renames unborn HEAD either way.
    fn normalize_branch_to_main(dir: &Path) {
        git(dir, &["checkout", "-B", "main"]);
    }

    #[test]
    fn missing_identity_classifier_matches_git_stderr() {
        assert!(is_missing_identity(
            "Author identity unknown\n\n*** Please tell me who you are."
        ));
        assert!(is_missing_identity(
            "Committer identity unknown\n\n*** Please tell me who you are."
        ));
        assert!(is_missing_identity(
            "fatal: unable to auto-detect email address (got 'u@x.(none)')"
        ));
        assert!(!is_missing_identity("error: failed to push some refs"));
        assert!(!is_missing_identity("CONFLICT (content): Merge conflict"));
    }

    #[test]
    fn sync_without_remote_commits_with_plan_message() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        std::fs::write(root.portable_dir().join("notes.txt"), "hello\n").unwrap();
        std::fs::write(root.device_path(), "{\"theme\":\"dark\"}\n").unwrap();

        let outcome = sync_portable(&root).unwrap();
        assert!(outcome.committed);
        assert!(!outcome.pushed);
        assert!(!outcome.reload_required);

        let log = git_output(&root.portable_dir(), &["log", "--format=%s", "-1"]);
        assert_eq!(log.trim(), SYNC_COMMIT_MESSAGE);
        // `device.json` never enters the portable history.
        let names = git_output(&root.portable_dir(), &["log", "--name-only", "--format="]);
        assert!(!names.contains("device.json"), "{names}");
    }

    #[test]
    fn clean_sync_commits_nothing() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);

        let outcome = sync_portable(&root).unwrap();
        assert!(!outcome.committed);
        assert!(!outcome.pushed);
        let count = git_output(&root.portable_dir(), &["rev-list", "--count", "HEAD"]);
        assert_eq!(count.trim(), "1");
    }

    #[test]
    fn device_json_is_unreachable_from_portable_git() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);

        // Dirty on both sides of the boundary.
        std::fs::write(root.portable_dir().join("notes.txt"), "portable\n").unwrap();
        std::fs::write(root.device_path(), "{\"theme\":\"light\"}\n").unwrap();

        // The plan's structural test: scoped status never lists device.json.
        let status = git_output(&root.portable_dir(), &["status", "--porcelain"]);
        assert!(!status.contains("device.json"), "{status}");
        assert!(status.contains("notes.txt"), "{status}");

        // The root itself is not a repository.
        assert!(git_status(root.root(), &["rev-parse"]).is_err());
        assert!(git_status(root.root(), &["status", "--porcelain"]).is_err());
    }

    #[test]
    fn clean_inside_portable_cannot_delete_device_json() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);
        std::fs::write(root.device_path(), "{\"theme\":\"dark\"}\n").unwrap();

        git(&root.portable_dir(), &["clean", "-fdx"]);
        assert!(root.device_path().exists());
        // ...while portable-side untracked files are really cleaned.
        std::fs::write(root.portable_dir().join("scratch.txt"), "x\n").unwrap();
        git(&root.portable_dir(), &["clean", "-fdx"]);
        assert!(!root.portable_dir().join("scratch.txt").exists());
        assert!(root.device_path().exists());
    }

    #[test]
    fn sync_pushes_and_reports_upstream_changes_for_reload() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        normalize_branch_to_main(&root.portable_dir());
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);

        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        let url = remote.path().to_str().unwrap().to_owned();
        set_origin(&root, &url).unwrap();
        git(&root.portable_dir(), &["push", "-u", "origin", "HEAD:main"]);

        // Local-only change round-trips to the remote.
        std::fs::write(root.portable_dir().join("local.txt"), "local\n").unwrap();
        let outcome = sync_portable(&root).unwrap();
        assert!(outcome.committed);
        assert!(outcome.pushed);
        assert!(!outcome.reload_required);

        // An upstream change made elsewhere arrives via rebase and flags reload.
        let elsewhere = tempfile::tempdir().unwrap();
        let clone_status = Command::new("git")
            .arg("clone")
            .arg(remote.path())
            .arg(elsewhere.path().join("clone"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(clone_status.success());
        let clone = elsewhere.path().join("clone");
        configure_identity(&clone);
        std::fs::write(clone.join("remote.txt"), "remote\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "remote work"]);
        git(&clone, &["push"]);

        let outcome = sync_portable(&root).unwrap();
        assert!(!outcome.committed);
        assert!(outcome.pushed);
        assert!(outcome.reload_required);
        assert!(root.portable_dir().join("remote.txt").exists());
        assert_ne!(outcome.head_before, outcome.head_after);
    }

    #[test]
    fn sync_stops_on_conflict_and_leaves_tree_usable() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        std::fs::write(root.portable_dir().join("shared.txt"), "base\n").unwrap();
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);

        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        set_origin(&root, remote.path().to_str().unwrap()).unwrap();
        git(&root.portable_dir(), &["push", "-u", "origin", "HEAD:main"]);

        // Diverging change upstream.
        let elsewhere = tempfile::tempdir().unwrap();
        let clone_status = Command::new("git")
            .arg("clone")
            .arg(remote.path())
            .arg(elsewhere.path().join("clone"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(clone_status.success());
        let clone = elsewhere.path().join("clone");
        configure_identity(&clone);
        std::fs::write(clone.join("shared.txt"), "upstream\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "upstream"]);
        git(&clone, &["push"]);

        // Conflicting local change stays uncommitted: sync commits it, then
        // the rebase conflicts and stops.
        std::fs::write(root.portable_dir().join("shared.txt"), "local\n").unwrap();
        let tracker = SyncTracker::default();
        let error = sync_portable_with_tracker(&root, &tracker).expect_err("must conflict");
        let message = format!("{error:#}");
        assert!(message.contains("rebase"), "{message}");
        assert_eq!(tracker.status(), SyncStatus::Error);
        assert!(
            tracker
                .last_error()
                .is_some_and(|error| error.contains("rebase"))
        );

        // The working tree is still there and usable for ordinary git.
        assert!(root.portable_dir().join("shared.txt").exists());
        assert!(root.portable_dir().join(".git").exists());
    }

    #[test]
    fn sync_refuses_when_operation_in_progress() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        git(&root.portable_dir(), &["add", "-A"]);
        git(&root.portable_dir(), &["commit", "-m", "base"]);
        std::fs::write(root.portable_dir().join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();

        let error = sync_portable(&root).expect_err("must refuse");
        assert!(format!("{error:#}").contains("in progress"), "{error:#}");
    }

    #[test]
    fn sync_requires_a_repo() {
        let (_dir, root) = fresh_root();
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        let error = sync_portable(&root).expect_err("must fail");
        assert!(
            format!("{error:#}").contains("not a git repository"),
            "{error:#}"
        );
    }

    #[test]
    fn tracker_reports_idle_through_success() {
        let (_dir, root) = fresh_root();
        init_with_identity(&root);
        let tracker = SyncTracker::default();
        assert_eq!(tracker.status(), SyncStatus::Idle);
        sync_portable_with_tracker(&root, &tracker).unwrap();
        assert_eq!(tracker.status(), SyncStatus::Idle);
        assert_eq!(tracker.last_error(), None);
    }
}
