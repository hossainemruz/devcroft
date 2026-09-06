//! Live git state for the workspace header: branch, dirtiness, and
//! ahead/behind tracking.
//!
//! This is the only consumer of `gix` outside the Review tab. Everything is
//! read-only and failure-tolerant: any lookup can fail (non-git directory,
//! unborn HEAD, missing upstream, bare repo) and the result degrades to
//! `branch: None` or zero counts instead of erroring, so the header never
//! breaks on unusual checkouts.
//!
//! Refresh cost: one `is_dirty` scan (tracked changes) plus, when that is
//! clean, one early-exiting status walk for untracked files, plus one
//! merge-base lookup for ahead/behind booleans and two capped revision walks
//! for counts. All of it runs off the main thread (see `Workspace`'s poll
//! loop), so per-frame rendering only reads the cached [`GitStatus`].

use std::path::Path;

/// Upper bound for ahead/behind counting. Revision walks stop here, so a
/// pathological divergence (e.g. a shallow clone compared against a full
/// upstream) cannot stall the background poll. Counts that hit the cap
/// display with a `+` suffix (see [`GitStatus::ahead_label`]).
const COUNT_CAP: usize = 999;

/// Header-ready git state for one working tree. `branch` is `None` when the
/// directory is not inside a git repository; otherwise it holds the short
/// branch name, or (when [`GitStatus::detached`] is set) the short commit id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GitStatus {
    pub branch: Option<String>,
    pub detached: bool,
    pub dirty: bool,
    pub ahead: usize,
    pub behind: usize,
    pub ahead_truncated: bool,
    pub behind_truncated: bool,
    pub has_upstream: bool,
}

impl GitStatus {
    /// `↑n` when ahead of the upstream, else empty. Counts at the cap gain a
    /// `+` suffix so `999+` never reads as exact.
    pub(crate) fn ahead_label(&self) -> Option<String> {
        if !self.has_upstream || self.ahead == 0 {
            return None;
        }
        Some(if self.ahead_truncated {
            format!("↑{}+", self.ahead)
        } else {
            format!("↑{}", self.ahead)
        })
    }

    /// `↓n` when behind the upstream, else empty.
    pub(crate) fn behind_label(&self) -> Option<String> {
        if !self.has_upstream || self.behind == 0 {
            return None;
        }
        Some(if self.behind_truncated {
            format!("↓{}+", self.behind)
        } else {
            format!("↓{}", self.behind)
        })
    }
}

/// Load the header state for `workdir`. Never fails: unresolvable state
/// yields a default (non-repo or zero-count) status.
pub(crate) fn load_git_status(workdir: &Path) -> GitStatus {
    let Ok(repo) = gix::discover(workdir) else {
        return GitStatus::default();
    };
    // Bare repositories have no working tree to be dirty or ahead of; the
    // branch name is still useful header context.
    let has_workdir = repo.workdir().is_some();

    let head_name = repo.head_name().ok().flatten();
    let (branch, detached) = match &head_name {
        Some(name) => (Some(name.shorten().to_string()), false),
        None => match repo.head_id().ok().map(|id| id.detach()) {
            Some(id) => (Some(short_id(&id)), true),
            None => (None, false),
        },
    };
    // `discover` succeeded but neither a branch nor a commit resolved (e.g.
    // an unborn HEAD with no symref): treat as a non-repo for display. The
    // branch pill stays hidden rather than showing a placeholder.
    if branch.is_none() {
        return GitStatus::default();
    }

    let dirty = has_workdir && is_worktree_dirty(&repo);
    let (ahead, ahead_truncated, behind, behind_truncated, has_upstream) =
        ahead_behind(&repo, head_name.as_ref());

    GitStatus {
        branch,
        detached,
        dirty,
        ahead,
        behind,
        ahead_truncated,
        behind_truncated,
        has_upstream,
    }
}

/// Seven-character short id, matching `git rev-parse --short HEAD`.
fn short_id(id: &gix::ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}

/// Tracked modifications (staged or unstaged) plus untracked files.
/// `is_dirty` ignores untracked files by contract, so a clean result falls
/// through to one early-exiting status walk: since tracked state is already
/// known-clean, any item that walk yields implies untracked content (or a
/// concurrent worktree race, which equally deserves the dirty dot).
fn is_worktree_dirty(repo: &gix::Repository) -> bool {
    if repo.is_dirty().unwrap_or(false) {
        return true;
    }
    has_untracked(repo)
}

fn has_untracked(repo: &gix::Repository) -> bool {
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return false;
    };
    let Ok(iter) = platform
        .index_worktree_rewrites(None)
        .index_worktree_submodules(gix::status::Submodule::AsConfigured { check_dirty: true })
        .into_index_worktree_iter(Vec::new())
    else {
        return false;
    };
    iter.filter_map(Result::ok).next().is_some()
}

/// Ahead/behind against the fetch upstream of the current branch.
///
/// Resolution is `branch.<name>.remote` + fetch refspecs via
/// `branch_remote_tracking_ref_name`, so only configured upstreams count;
/// detached HEADs and branches without (or without a fetched) upstream
/// report `has_upstream: false` and zero counts. Direction comes from the
/// merge-base: equal ids are in sync, a base equal to one side means the
/// other side moved, otherwise both did. Counts are capped revision walks
/// (see [`COUNT_CAP`]).
fn ahead_behind(
    repo: &gix::Repository,
    head_name: Option<&gix::refs::FullName>,
) -> (usize, bool, usize, bool, bool) {
    const CLEAN: (usize, bool, usize, bool, bool) = (0, false, 0, false, false);
    let Some(name) = head_name else {
        return CLEAN;
    };
    let Ok(head) = repo.head_id().map(|id| id.detach()) else {
        return CLEAN;
    };
    let tracking = repo
        .branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)
        .and_then(Result::ok);
    let Some(tracking) = tracking else {
        return CLEAN;
    };
    let Some(upstream) = repo
        .find_reference(&tracking)
        .ok()
        .and_then(|mut reference| reference.peel_to_id().ok())
        .map(|id| id.detach())
    else {
        // Configured but never fetched: no local ref to compare against.
        return CLEAN;
    };
    if head == upstream {
        return (0, false, 0, false, true);
    }
    let Ok(base) = repo.merge_base(head, upstream).map(|id| id.detach()) else {
        // Unrelated histories: direction is undefined, but the upstream
        // exists, so report it without arrows rather than hiding it.
        return CLEAN_WITH_UPSTREAM;
    };
    if base == head {
        let (behind, truncated) = count_range(repo, upstream, base);
        (0, false, behind, truncated, true)
    } else if base == upstream {
        let (ahead, truncated) = count_range(repo, head, base);
        (ahead, truncated, 0, false, true)
    } else {
        let (ahead, ahead_truncated) = count_range(repo, head, base);
        let (behind, behind_truncated) = count_range(repo, upstream, base);
        (ahead, ahead_truncated, behind, behind_truncated, true)
    }
}

const CLEAN_WITH_UPSTREAM: (usize, bool, usize, bool, bool) = (0, false, 0, false, true);

/// Commits reachable from `tip` excluding `base` and its ancestors,
/// stopping after [`COUNT_CAP`] + 1 so the walk cost stays bounded. The flag
/// reports whether the cap cut the walk short.
fn count_range(repo: &gix::Repository, tip: gix::ObjectId, base: gix::ObjectId) -> (usize, bool) {
    if tip == base {
        return (0, false);
    }
    let Ok(walk) = repo.rev_walk([tip]).with_hidden([base]).all() else {
        return (0, false);
    };
    let mut count = 0;
    for item in walk {
        if item.is_err() {
            continue;
        }
        count += 1;
        if count > COUNT_CAP {
            return (COUNT_CAP, true);
        }
    }
    (count, false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git CLI must be available for fixture setup");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn write(dir: &Path, path: &str, content: &[u8]) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", message]);
    }

    /// Point `refs/remotes/origin/<branch>` at `rev` and configure it as the
    /// fetch upstream, without any network or actual remote. The fake remote
    /// needs a fetch refspec (via `remote add`) so gix can map the upstream
    /// branch to its tracking ref, exactly like a cloned checkout.
    fn set_upstream(dir: &Path, branch: &str, rev: &str) {
        git(
            dir,
            &["remote", "add", "origin", "https://example.com/repo.git"],
        );
        git(
            dir,
            &["update-ref", &format!("refs/remotes/origin/{branch}"), rev],
        );
        git(
            dir,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        git(
            dir,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        );
    }

    #[test]
    fn non_repo_yields_no_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = load_git_status(dir.path());
        assert_eq!(status, GitStatus::default());
        assert!(status.branch.is_none());
    }

    #[test]
    fn clean_tree_reports_branch_without_dirty_or_counts() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"hello\n");
        commit_all(dir.path(), "initial");
        let status = load_git_status(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(!status.detached);
        assert!(!status.dirty);
        assert!(!status.has_upstream);
        assert!(status.ahead_label().is_none());
        assert!(status.behind_label().is_none());
    }

    #[test]
    fn modified_file_is_dirty() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "a.txt", b"two\n");
        let status = load_git_status(dir.path());
        assert!(status.dirty);
    }

    #[test]
    fn untracked_file_is_dirty() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "new.txt", b"fresh\n");
        let status = load_git_status(dir.path());
        assert!(status.dirty);
    }

    #[test]
    fn staged_change_is_dirty() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "a.txt", b"two\n");
        git(dir.path(), &["add", "-A"]);
        let status = load_git_status(dir.path());
        assert!(status.dirty);
    }

    #[test]
    fn ahead_of_upstream_counts_commits() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        set_upstream(dir.path(), "main", "HEAD");
        write(dir.path(), "b.txt", b"two\n");
        commit_all(dir.path(), "second");
        let status = load_git_status(dir.path());
        assert!(status.has_upstream);
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 0);
        assert_eq!(status.ahead_label().as_deref(), Some("↑1"));
        assert!(status.behind_label().is_none());
    }

    #[test]
    fn behind_upstream_counts_commits() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "b.txt", b"two\n");
        commit_all(dir.path(), "second");
        // Upstream stays at the second commit while HEAD moves back: behind
        // by one.
        set_upstream(dir.path(), "main", "HEAD");
        git(dir.path(), &["reset", "--hard", "HEAD~1"]);
        let status = load_git_status(dir.path());
        assert!(status.has_upstream);
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 1);
        assert_eq!(status.behind_label().as_deref(), Some("↓1"));
    }

    #[test]
    fn diverged_reports_both_directions() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        // Fork the upstream side off the initial commit first, so neither
        // side contains the other and the merge-base stays at `initial`.
        git(dir.path(), &["checkout", "-b", "side"]);
        write(dir.path(), "side.txt", b"side\n");
        commit_all(dir.path(), "side work");
        git(dir.path(), &["checkout", "main"]);
        set_upstream(dir.path(), "main", "side");
        write(dir.path(), "local.txt", b"local\n");
        commit_all(dir.path(), "local work");
        let status = load_git_status(dir.path());
        assert!(status.has_upstream);
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn detached_head_shows_short_id() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        git(dir.path(), &["checkout", "--detach", "HEAD"]);
        let status = load_git_status(dir.path());
        assert!(status.detached);
        assert!(status.branch.is_some());
        let branch = status.branch.expect("detached HEAD still identifies");
        assert_eq!(branch.len(), 7);
        assert!(!status.has_upstream);
    }
}
