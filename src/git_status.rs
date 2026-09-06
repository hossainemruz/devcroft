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
    !collect_status_items(repo, 1).is_empty()
}

/// One worktree change exactly as gix sees it, for diagnosing loader-vs-CLI
/// disagreements (e.g. the header dot disagreeing with `git status`):
/// repo-relative path plus a short kind tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusItem {
    pub(crate) path: String,
    pub(crate) kind: String,
}

/// Itemized worktree status for `workdir`: what [`load_git_status`] sees,
/// capped at `limit` items. The same walk backs the header dot (with
/// `limit` 1), so the inspector and the dot can never disagree about the
/// underlying data — only about how it is displayed.
pub(crate) fn status_items(workdir: &Path, limit: usize) -> Vec<StatusItem> {
    let Ok(repo) = gix::discover(workdir) else {
        return Vec::new();
    };
    collect_status_items(&repo, limit)
}

/// Whether gix sees tracked modifications, independent of untracked files.
/// Reported separately by the inspector so a dirty dot can be attributed to
/// the tracked scan versus the untracked walk.
pub(crate) fn has_tracked_changes(workdir: &Path) -> bool {
    let Ok(repo) = gix::discover(workdir) else {
        return false;
    };
    repo.is_dirty().unwrap_or(false)
}

fn collect_status_items(repo: &gix::Repository, limit: usize) -> Vec<StatusItem> {
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return Vec::new();
    };
    let Ok(iter) = platform
        // Collapsed directories can be reported as untracked even when they
        // contain only ignored files and empty directories. Inspect files so
        // those containers do not produce a false dirty dot. This leaves a
        // walk disabled by status.showUntrackedFiles=no disabled.
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_rewrites(None)
        .index_worktree_submodules(gix::status::Submodule::AsConfigured { check_dirty: true })
        .into_index_worktree_iter(Vec::new())
    else {
        return Vec::new();
    };
    iter.filter_map(Result::ok)
        .take(limit)
        .map(|item| match item {
            gix::status::index_worktree::Item::Modification {
                rela_path, status, ..
            } => StatusItem {
                path: rela_path.to_string(),
                kind: format!("tracked:{status:?}"),
            },
            gix::status::index_worktree::Item::DirectoryContents { entry, .. } => StatusItem {
                path: entry.rela_path.to_string(),
                kind: format!("walk:{:?}", entry.status),
            },
            gix::status::index_worktree::Item::Rewrite { dirwalk_entry, .. } => StatusItem {
                path: dirwalk_entry.rela_path.to_string(),
                kind: "rewrite".to_owned(),
            },
        })
        .collect()
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
        assert!(
            !status.dirty,
            "committed changes must not mark the tree dirty"
        );
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

    /// `git status --porcelain=v1` emptiness as ground truth: the header dot
    /// must agree with the CLI on every fixture below. No `--untracked-files`
    /// override, so repo-local config (e.g. `showUntrackedFiles`) applies to
    /// the oracle exactly as it should to the loader. The oracle runs with
    /// the same isolated config as the fixtures, so machine-global git
    /// config cannot skew either side.
    fn cli_reports_dirty(dir: &Path) -> bool {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain=v1"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git CLI must be available for the oracle");
        assert!(output.status.success());
        !output.stdout.iter().all(|byte| byte.is_ascii_whitespace())
    }

    #[test]
    fn untracked_directory_with_only_ignored_children_is_clean() {
        let dir = init_repo();
        write(dir.path(), ".gitignore", b"xcuserdata/\n");
        commit_all(dir.path(), "initial");
        write(
            dir.path(),
            "Project.xcodeproj/project.xcworkspace/xcuserdata/state",
            b"ignored\n",
        );
        fs::create_dir_all(
            dir.path()
                .join("Project.xcodeproj/project.xcworkspace/xcshareddata"),
        )
        .unwrap();
        assert!(!cli_reports_dirty(dir.path()));
        assert!(!load_git_status(dir.path()).dirty);
        assert!(status_items(dir.path(), 10).is_empty());
        // A real untracked file inside the same directory must still count.
        write(
            dir.path(),
            "Project.xcodeproj/project.xcworkspace/xcshareddata/settings",
            b"new\n",
        );
        assert!(cli_reports_dirty(dir.path()));
        assert!(load_git_status(dir.path()).dirty);
        assert_eq!(status_items(dir.path(), 1).len(), 1);
    }

    #[test]
    fn metadata_only_change_is_clean() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        // Deterministically invalidate the cached stat without changing content.
        let file = fs::File::options()
            .write(true)
            .open(dir.path().join("a.txt"))
            .unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!cli_reports_dirty(dir.path()));
        assert!(!load_git_status(dir.path()).dirty);
        assert!(!has_tracked_changes(dir.path()));
        assert!(status_items(dir.path(), 10).is_empty());
    }

    #[test]
    fn loader_agrees_with_cli_on_show_untracked_files_no() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        git(dir.path(), &["config", "status.showUntrackedFiles", "no"]);
        write(dir.path(), "hidden.txt", b"untracked\n");
        // Oracle with the same local config also hides the file.
        assert!(!cli_reports_dirty(dir.path()));
        assert!(!load_git_status(dir.path()).dirty);
    }

    #[test]
    fn loader_agrees_with_cli_on_submodule_untracked_content() {
        let sub = init_repo();
        write(sub.path(), "lib.txt", b"lib\n");
        commit_all(sub.path(), "lib initial");
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        git(
            dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.path().to_str().expect("tempdir is utf-8"),
                "sub",
            ],
        );
        commit_all(dir.path(), "add submodule");
        write(dir.path(), "sub/untracked.txt", b"untracked\n");
        assert!(cli_reports_dirty(dir.path()));
        assert!(load_git_status(dir.path()).dirty);
    }

    #[test]
    fn loader_agrees_with_cli_on_ignored_files_only() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        write(dir.path(), ".gitignore", b"ignored.txt\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "ignored.txt", b"ignored\n");
        assert!(!cli_reports_dirty(dir.path()));
        assert!(!load_git_status(dir.path()).dirty);
    }

    #[test]
    fn status_items_list_untracked_paths_with_kinds() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "new.txt", b"fresh\n");
        assert!(!has_tracked_changes(dir.path()));
        let items = status_items(dir.path(), 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "new.txt");
        assert!(
            items[0].kind.contains("Untracked"),
            "unexpected kind: {}",
            items[0].kind
        );
    }

    #[test]
    fn status_items_tag_tracked_modifications() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "a.txt", b"two\n");
        assert!(has_tracked_changes(dir.path()));
        let items = status_items(dir.path(), 10);
        // The tracked scan already caught this, so the walk may or may not
        // repeat it — but if it does, the path and tracked tag must match.
        for item in &items {
            assert_eq!(item.path, "a.txt");
            assert!(
                item.kind.starts_with("tracked:"),
                "unexpected kind: {}",
                item.kind
            );
        }
    }

    #[test]
    fn status_items_cap_at_limit() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\n");
        commit_all(dir.path(), "initial");
        for index in 0..5 {
            write(dir.path(), &format!("new-{index}.txt"), b"fresh\n");
        }
        assert_eq!(status_items(dir.path(), 2).len(), 2);
        assert_eq!(status_items(dir.path(), 10).len(), 5);
    }

    #[test]
    fn status_items_empty_for_non_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(status_items(dir.path(), 10).is_empty());
        assert!(!has_tracked_changes(dir.path()));
    }
}
