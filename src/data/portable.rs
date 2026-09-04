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
/// - otherwise: `git init` in place (adopting any existing files) plus seed.
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
    let git_result = run_git_in(&portable, &["init"]);
    seed_workspace_json(&portable)?;
    git_result.map(|_| ())?;
    Ok(InitOutcome::InitializedEmpty)
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
    }
}
