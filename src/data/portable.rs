//! First-run `portable/` init, the `origin` remote, and `workspace.json`.
//!
//! `portable/` is the git repo root. Initializing into a root that already
//! holds `device.json` is expected (clone lands next to it); `device.json`
//! itself is never touched here and can never be committed by sync.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DataRoot, ensure_dirs, run_git_in, write_json_atomic};

/// Portable workspace descriptor. `formatVersion` is informational only and
/// must never gate loading (see `feature-parity.md` §2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkspaceDoc {
    #[serde(default, rename = "formatVersion")]
    pub(crate) format_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(flatten)]
    pub(crate) extra: HashMap<String, Value>,
}

/// Seed for a fresh `workspace.json`.
fn seed_doc() -> WorkspaceDoc {
    WorkspaceDoc {
        format_version: Some(1),
        name: None,
        extra: HashMap::new(),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InitOptions {
    pub(crate) clone_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitOutcome {
    AlreadyInitialized,
    InitializedEmpty,
    Cloned,
}

/// Idempotent first-run init:
///
/// - `portable/.git` present: seed `workspace.json` if absent, no git writes.
/// - `clone_url` given: `git clone <url> portable` with cwd at the root
///   (which may already hold `device.json`), then seed if absent.
/// - otherwise: `git init -b main` in place (adopting any existing files)
///   plus seed.
///
/// `workspace.json` is seeded even when git is unavailable so local files
/// stay usable; sync surfaces the actionable git error later.
pub(crate) fn ensure_portable_init(root: &DataRoot, options: &InitOptions) -> Result<InitOutcome> {
    ensure_dirs(root.root())?;
    let portable = root.portable_dir();
    if portable.join(".git").exists() {
        seed_workspace_json(&portable)?;
        return Ok(InitOutcome::AlreadyInitialized);
    }
    if let Some(url) = options.clone_url.as_deref().filter(|url| !url.is_empty()) {
        clone_portable(root, url)?;
        seed_workspace_json(&portable)?;
        return Ok(InitOutcome::Cloned);
    }
    let git_result = init_with_main_branch(&portable);
    seed_workspace_json(&portable)?;
    git_result?;
    Ok(InitOutcome::InitializedEmpty)
}

/// `git init` defaulting the unborn branch to `main` regardless of the
/// machine's `init.defaultBranch` (or the old `master` default). `init -b`
/// needs git 2.28+; older toolchains fall back to plain init plus an
/// explicit unborn-HEAD rename. Only called when `portable/.git` is absent,
/// so existing repos (even ones already on `master`) are never renamed
/// behind the user's back.
fn init_with_main_branch(portable: &Path) -> Result<()> {
    match run_git_in(portable, &["init", "-b", "main"]) {
        Ok(_) => Ok(()),
        Err(first) => {
            if portable.join(".git").exists() {
                // `-b` was rejected but something was created: surface the
                // failure rather than layering a fallback on top of a
                // half-initialized repo.
                return Err(first).context("initializing portable git repository");
            }
            run_git_in(portable, &["init"]).context("initializing portable git repository")?;
            run_git_in(portable, &["symbolic-ref", "HEAD", "refs/heads/main"])
                .context("setting portable initial branch to main")?;
            Ok(())
        }
    }
}

/// Tolerant `workspace.json` load: a missing file is default state, and the
/// version never gates.
pub(crate) fn load_workspace(root: &DataRoot) -> Result<WorkspaceDoc> {
    let path = root.portable_dir().join("workspace.json");
    let bytes = match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceDoc::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
        Ok(bytes) => bytes,
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Show the `origin` remote URL, if one is configured.
pub(crate) fn get_origin(root: &DataRoot) -> Result<Option<String>> {
    let portable = root.portable_dir();
    require_repo(&portable)?;
    match run_git_in(&portable, &["remote", "get-url", "origin"]) {
        Ok(url) => Ok(Some(url.trim().to_owned())),
        // No `origin` configured is normal (fresh `git init`); anything else
        // (git missing, corrupt repo) surfaces on the next git invocation.
        Err(_) => Ok(None),
    }
}

/// Set the `origin` remote URL: `remote add` when absent, `set-url` when
/// present. The URL itself never appears in error output (it may embed a
/// token); git's own stderr is sanitized by the runner.
pub(crate) fn set_origin(root: &DataRoot, url: &str) -> Result<()> {
    let portable = root.portable_dir();
    require_repo(&portable)?;
    if get_origin(root)?.is_some() {
        run_git_in(&portable, &["remote", "set-url", "origin", url])
            .context("setting portable origin remote")?;
    } else {
        run_git_in(&portable, &["remote", "add", "origin", url])
            .context("adding portable origin remote")?;
    }
    Ok(())
}

/// Remove the `origin` remote. Idempotent: a missing `origin` is success and
/// reports `false`, so a Remove action stays honest about what changed.
pub(crate) fn clear_origin(root: &DataRoot) -> Result<bool> {
    let portable = root.portable_dir();
    require_repo(&portable)?;
    if get_origin(root)?.is_none() {
        return Ok(false);
    }
    run_git_in(&portable, &["remote", "remove", "origin"])
        .context("removing portable origin remote")?;
    Ok(true)
}

/// Local branch names (`refs/heads`), sorted, without any prefix.
pub(crate) fn list_local_branches(root: &DataRoot) -> Result<Vec<String>> {
    let portable = root.portable_dir();
    require_repo(&portable)?;
    let output = run_git_in(
        &portable,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Current branch name, or `None` on a detached HEAD. Falls back from
/// `branch --show-current` to `symbolic-ref` for very old git.
pub(crate) fn current_branch_name(portable: &Path) -> Result<Option<String>> {
    if let Ok(name) = run_git_in(portable, &["branch", "--show-current"]) {
        let name = name.trim().to_owned();
        if !name.is_empty() {
            return Ok(Some(name));
        }
    }
    match run_git_in(portable, &["symbolic-ref", "--short", "HEAD"]) {
        Ok(name) => {
            let name = name.trim().to_owned();
            Ok((!name.is_empty()).then_some(name))
        }
        Err(_) => Ok(None),
    }
}

/// Upstream of the current branch (`origin/main` style), if one is recorded.
pub(crate) fn current_upstream(root: &DataRoot) -> Result<Option<String>> {
    let portable = root.portable_dir();
    require_repo(&portable)?;
    match run_git_in(
        &portable,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        Ok(name) => {
            let name = name.trim().to_owned();
            Ok((!name.is_empty()).then_some(name))
        }
        Err(_) => Ok(None),
    }
}

/// How [`checkout_branch`] resolved `name`, so the UI can report exactly
/// what happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutOutcome {
    /// Switched to an existing local branch.
    Switched,
    /// No local branch, but `origin/<name>` existed: git's DWIM created a
    /// local branch tracking it.
    TrackedRemote,
    /// Neither existed: created from the previous HEAD.
    Created,
}

/// Switch the portable repo to branch `name`.
///
/// Resolution order: existing local branch (plain switch), else git's DWIM
/// tracking `origin/<name>` when the last fetch brought it, else create
/// from HEAD. A dirty tree that conflicts with the switch fails with git's
/// own (sanitized) error and changes nothing — commit via sync first or
/// resolve it with ordinary git tooling.
pub(crate) fn checkout_branch(root: &DataRoot, name: &str) -> Result<CheckoutOutcome> {
    let _gate = super::store_lock::portable_gate(root, true)?;
    let portable = root.portable_dir();
    require_repo(&portable)?;
    let name = name.trim();
    if name.is_empty() || name.starts_with('-') || name == "HEAD" {
        bail!("invalid portable branch name {name:?}");
    }
    run_git_in(&portable, &["check-ref-format", "--branch", name])
        .with_context(|| format!("invalid portable branch name {name:?}"))?;
    if list_local_branches(root)?
        .iter()
        .any(|branch| branch == name)
    {
        run_git_in(&portable, &["checkout", name])
            .with_context(|| format!("switching portable to branch {name:?}"))?;
        return Ok(CheckoutOutcome::Switched);
    }
    // No local branch: only let `checkout` DWIM-adopt `origin/<name>` when
    // that ref really exists — otherwise a name matching a file path would
    // restore that path instead of switching branches.
    if run_git_in(
        &portable,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/origin/{name}"),
        ],
    )
    .is_ok()
    {
        run_git_in(&portable, &["checkout", name])
            .with_context(|| format!("tracking portable branch origin/{name}"))?;
        return Ok(CheckoutOutcome::TrackedRemote);
    }
    run_git_in(&portable, &["checkout", "-b", name])
        .with_context(|| format!("creating portable branch {name:?}"))?;
    Ok(CheckoutOutcome::Created)
}

fn require_repo(portable: &Path) -> Result<()> {
    if portable.join(".git").exists() {
        Ok(())
    } else {
        bail!(
            "portable directory at {} is not a git repository (run first-run init)",
            portable.display()
        )
    }
}

fn clone_portable(root: &DataRoot, url: &str) -> Result<()> {
    let portable = root.portable_dir();
    if portable.exists() {
        let empty = std::fs::read_dir(&portable)
            .with_context(|| format!("listing {}", portable.display()))?
            .next()
            .is_none();
        if !empty {
            bail!(
                "portable directory at {} is not empty; move it aside before cloning",
                portable.display()
            );
        }
        std::fs::remove_dir(&portable)
            .with_context(|| format!("removing empty {}", portable.display()))?;
    }
    // `git clone <url> portable` with cwd at the root, which may already
    // hold `device.json` — the clone only creates `portable/`.
    run_git_in(root.root(), &["clone", url, "portable"]).context("cloning portable data")?;
    Ok(())
}

fn seed_workspace_json(portable: &Path) -> Result<()> {
    let path = portable.join("workspace.json");
    if path.exists() {
        return Ok(());
    }
    write_json_atomic(&path, &seed_doc()).with_context(|| format!("seeding {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataRoot;
    use std::process::Command;

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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git CLI must be available for assertions");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}",
            dir.display()
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn init_defaults_to_main_branch() {
        let (_dir, root) = fresh_root();
        let outcome = ensure_portable_init(&root, &InitOptions::default()).unwrap();
        assert_eq!(outcome, InitOutcome::InitializedEmpty);
        // Unborn HEAD: `--show-current` prints the branch name on modern
        // git. The hermetic env above pins no `init.defaultBranch`, so this
        // fails if init ever inherits the old `master` default again.
        let branch = git_output(&root.portable_dir(), &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "main");
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", message]);
    }

    #[test]
    fn checkout_switches_between_local_branches() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
        std::fs::write(root.portable_dir().join("base.txt"), "base\n").unwrap();
        commit_all(&root.portable_dir(), "base");
        git(&root.portable_dir(), &["checkout", "-b", "experiment"]);
        std::fs::write(root.portable_dir().join("exp.txt"), "exp\n").unwrap();
        commit_all(&root.portable_dir(), "experiment work");

        let mut branches = list_local_branches(&root).unwrap();
        branches.sort();
        assert_eq!(branches, vec!["experiment".to_owned(), "main".to_owned()]);

        assert_eq!(
            checkout_branch(&root, "main").unwrap(),
            CheckoutOutcome::Switched
        );
        assert_eq!(
            current_branch_name(&root.portable_dir())
                .unwrap()
                .as_deref(),
            Some("main")
        );
        assert!(!root.portable_dir().join("exp.txt").exists());
        assert!(root.portable_dir().join("base.txt").exists());
    }

    #[test]
    fn checkout_creates_a_new_branch_from_head() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
        commit_all(&root.portable_dir(), "base");

        assert_eq!(
            checkout_branch(&root, "fresh").unwrap(),
            CheckoutOutcome::Created
        );
        assert_eq!(
            current_branch_name(&root.portable_dir())
                .unwrap()
                .as_deref(),
            Some("fresh")
        );
        assert!(
            list_local_branches(&root)
                .unwrap()
                .contains(&"fresh".to_owned())
        );
    }

    #[test]
    fn checkout_adopts_remote_branch_when_only_remote_exists() {
        // Remote `main` seeded elsewhere.
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        let seed = tempfile::tempdir().unwrap();
        git(seed.path(), &["init", "-b", "main"]);
        configure_identity(seed.path());
        std::fs::write(seed.path().join("remote.txt"), "remote\n").unwrap();
        commit_all(seed.path(), "seed");
        let url = remote.path().to_str().unwrap().to_owned();
        git(seed.path(), &["remote", "add", "origin", &url]);
        git(seed.path(), &["push", "-u", "origin", "main"]);

        // Local sits on an unrelated branch with no `main` of its own.
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
        commit_all(&root.portable_dir(), "base");
        git(&root.portable_dir(), &["branch", "-m", "main", "scratch"]);
        set_origin(&root, &url).unwrap();
        git(&root.portable_dir(), &["fetch", "origin"]);

        assert_eq!(
            checkout_branch(&root, "main").unwrap(),
            CheckoutOutcome::TrackedRemote
        );
        assert_eq!(
            current_branch_name(&root.portable_dir())
                .unwrap()
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            current_upstream(&root).unwrap().as_deref(),
            Some("origin/main")
        );
        assert!(root.portable_dir().join("remote.txt").exists());
    }

    #[test]
    fn checkout_rejects_bad_names_and_missing_repo() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
        commit_all(&root.portable_dir(), "base");

        for bad in ["", "   ", "with space", "-leading-dash", "HEAD", "a?b"] {
            assert!(
                checkout_branch(&root, bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // Still on main afterwards: rejections change nothing.
        assert_eq!(
            current_branch_name(&root.portable_dir())
                .unwrap()
                .as_deref(),
            Some("main")
        );

        let (_empty_dir, empty_root) = fresh_root();
        std::fs::create_dir_all(empty_root.portable_dir()).unwrap();
        assert!(list_local_branches(&empty_root).is_err());
        assert!(checkout_branch(&empty_root, "main").is_err());
    }

    #[test]
    fn current_branch_name_reports_none_when_detached() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        configure_identity(&root.portable_dir());
        commit_all(&root.portable_dir(), "base");
        git(&root.portable_dir(), &["checkout", "--detach", "HEAD"]);
        assert_eq!(current_branch_name(&root.portable_dir()).unwrap(), None);
        assert_eq!(current_upstream(&root).unwrap(), None);
    }

    #[test]
    fn init_empty_seeds_and_is_idempotent() {
        let (_dir, root) = fresh_root();
        let outcome = ensure_portable_init(&root, &InitOptions::default()).unwrap();
        assert_eq!(outcome, InitOutcome::InitializedEmpty);
        assert!(root.portable_dir().join(".git").exists());

        let workspace = load_workspace(&root).unwrap();
        assert_eq!(workspace.format_version, Some(1));

        let again = ensure_portable_init(&root, &InitOptions::default()).unwrap();
        assert_eq!(again, InitOutcome::AlreadyInitialized);
    }

    #[test]
    fn seed_never_overwrites_existing_workspace() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        std::fs::write(
            root.portable_dir().join("workspace.json"),
            "{\"formatVersion\": 7, \"name\": \"mine\"}\n",
        )
        .unwrap();
        assert_eq!(
            ensure_portable_init(&root, &InitOptions::default()).unwrap(),
            InitOutcome::AlreadyInitialized
        );
        let workspace = load_workspace(&root).unwrap();
        assert_eq!(workspace.format_version, Some(7));
        assert_eq!(workspace.name.as_deref(), Some("mine"));
    }

    #[test]
    fn version_never_gates_loading() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        std::fs::write(
            root.portable_dir().join("workspace.json"),
            "{\"formatVersion\": 999}\n",
        )
        .unwrap();
        assert_eq!(load_workspace(&root).unwrap().format_version, Some(999));
    }

    #[test]
    fn missing_workspace_loads_default() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        std::fs::remove_file(root.portable_dir().join("workspace.json")).unwrap();
        assert_eq!(load_workspace(&root).unwrap(), WorkspaceDoc::default());
    }

    #[test]
    fn clone_lands_next_to_existing_device_json() {
        // Source repo with one commit.
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-b", "main"]);
        configure_identity(source.path());
        std::fs::write(source.path().join("seed.txt"), "seed\n").unwrap();
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-m", "seed"]);

        let (_dir, root) = fresh_root();
        // The root may already hold machine-local state before cloning.
        std::fs::create_dir_all(root.root()).unwrap();
        std::fs::write(root.device_path(), "{\"theme\":\"dark\"}\n").unwrap();

        let url = source.path().to_str().unwrap().to_owned();
        let outcome = ensure_portable_init(
            &root,
            &InitOptions {
                clone_url: Some(url.clone()),
            },
        )
        .unwrap();
        assert_eq!(outcome, InitOutcome::Cloned);
        assert!(root.portable_dir().join("seed.txt").exists());
        // Machine-local state survives untouched beside the clone.
        assert_eq!(
            std::fs::read_to_string(root.device_path()).unwrap(),
            "{\"theme\":\"dark\"}\n"
        );
        assert_eq!(get_origin(&root).unwrap().as_deref(), Some(url.as_str()));
    }

    #[test]
    fn clone_refuses_non_empty_portable() {
        let (_dir, root) = fresh_root();
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        std::fs::write(root.portable_dir().join("user.txt"), "keep me\n").unwrap();
        let error = ensure_portable_init(
            &root,
            &InitOptions {
                clone_url: Some("https://example.com/o/r.git".to_owned()),
            },
        )
        .expect_err("must refuse to clobber");
        assert!(format!("{error:#}").contains("not empty"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(root.portable_dir().join("user.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn origin_set_and_show_round_trip() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        assert_eq!(get_origin(&root).unwrap(), None);

        set_origin(&root, "https://example.com/o/r.git").unwrap();
        assert_eq!(
            get_origin(&root).unwrap().as_deref(),
            Some("https://example.com/o/r.git")
        );
        // Second set rewrites via set-url.
        set_origin(&root, "https://example.com/o/other.git").unwrap();
        assert_eq!(
            get_origin(&root).unwrap().as_deref(),
            Some("https://example.com/o/other.git")
        );
    }

    #[test]
    fn origin_requires_a_repo() {
        let (_dir, root) = fresh_root();
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        let error = get_origin(&root).expect_err("must fail without .git");
        assert!(
            format!("{error:#}").contains("not a git repository"),
            "{error:#}"
        );
        assert!(set_origin(&root, "https://example.com/o/r.git").is_err());
        assert!(clear_origin(&root).is_err());
    }

    #[test]
    fn origin_clear_is_idempotent() {
        let (_dir, root) = fresh_root();
        ensure_portable_init(&root, &InitOptions::default()).unwrap();
        // Nothing configured: success reporting no change.
        assert!(!clear_origin(&root).unwrap());

        set_origin(&root, "https://example.com/o/r.git").unwrap();
        assert!(clear_origin(&root).unwrap());
        assert_eq!(get_origin(&root).unwrap(), None);
        // Second clear is a no-op success.
        assert!(!clear_origin(&root).unwrap());
    }
}
