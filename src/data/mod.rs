//! App-owned data directory: root resolution, device store, and portable sync.
//!
//! One root (`DEVCROFT_DATA_DIR` or a per-OS default) holds machine-local
//! `device.json` next to a Git-backed `portable/` subtree. The root itself
//! is never a Git repository, so sync scoped to `portable/` structurally
//! cannot see `device.json` — no `.gitignore` trust required.
//!
//! The submodules split the responsibilities: [`device`] owns `device.json`
//! load/save, [`portable`] owns first-run init plus the `origin` remote,
//! [`repositories`] owns portable repository records plus checkout
//! inspection, and [`sync`] owns the portable-only Git pipeline over the git CLI.

pub(crate) mod artifacts;
pub(crate) mod dashboard;
mod device;
mod portable;
mod record;
pub(crate) mod relationships;
mod repositories;
pub(crate) mod spaces;
mod store_lock;
mod sync;

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

pub(crate) use device::{
    DeviceRepositoryBinding, DeviceState, DeviceStore, SYNC_INTERVAL_OPTIONS,
    is_supported_sync_interval,
};
pub(crate) use portable::{
    CheckoutOutcome, InitOptions, InitOutcome, WorkspaceDoc, checkout_branch, clear_origin,
    current_branch_name, current_upstream, ensure_portable_init, get_origin, list_local_branches,
    load_workspace, set_origin,
};
pub(crate) use repositories::{
    CheckoutInspection, CreatedRepository, LinkedRepository, NewRepositoryInput, RecentRepository,
    RepositoryEntry, RepositoryMetadata, all_repositories, checkout_for, create_repository,
    get_repository_metadata, inspect_checkout, link_repository, list_repositories,
    normalize_repository_key, patch_repository_purpose, recent_repositories,
    record_repository_open, remove_repository, repository_dir, require_repository_key,
    resolve_current_key, suggest_repository_key, unlink_repository, update_repository_metadata,
};
pub(crate) use spaces::{
    DEFAULT_SPACE, SpaceRewrite, SpaceSelection, Spaces, delete_space, ensure_spaces,
    normalize_name, rename_space, space_eq, space_matches,
};
pub(crate) use sync::{
    SyncOutcome, SyncStatus, SyncTracker, sync_portable, sync_portable_with_tracker,
};

/// Environment override for the data root (tests, smoke isolation, moves).
pub(crate) const ENV_OVERRIDE: &str = "DEVCROFT_DATA_DIR";

/// Commit message used when sync auto-commits dirty portable files.
pub(crate) const SYNC_COMMIT_MESSAGE: &str = "chore(devcroft): sync portable data";

/// The data root: `device.json` plus the `portable/` repo live directly here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataRoot {
    root: PathBuf,
}

impl DataRoot {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Git repo root: the only subtree sync ever stages, commits, fetches,
    /// rebases, or pushes.
    pub(crate) fn portable_dir(&self) -> PathBuf {
        self.root.join("portable")
    }

    /// Machine-local state. Lives next to `portable/`, never inside it.
    pub(crate) fn device_path(&self) -> PathBuf {
        self.root.join("device.json")
    }
}

/// Resolve the data root: `DEVCROFT_DATA_DIR` wins, else the per-OS default.
/// Creates the root and `portable/` (`mkdir -p`).
pub(crate) fn resolve_data_root() -> Result<DataRoot> {
    let override_dir = env::var_os(ENV_OVERRIDE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_data_root_from_override(override_dir)
}

/// Testable core of [`resolve_data_root`] without touching the environment.
pub(crate) fn resolve_data_root_from_override(override_dir: Option<PathBuf>) -> Result<DataRoot> {
    let root = match override_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => default_data_root()?,
    };
    ensure_dirs(&root)?;
    Ok(DataRoot::new(root))
}

/// `mkdir -p` the root and its `portable/` child. Later `cache/ logs/ tmp/`
/// siblings are created on demand by their owners, never synced.
pub(crate) fn ensure_dirs(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating data directory at {}", root.display()))?;
    let portable = root.join("portable");
    std::fs::create_dir_all(&portable)
        .with_context(|| format!("creating portable directory at {}", portable.display()))?;
    Ok(())
}

/// Resolve, `mkdir -p`, seed a fresh installation's editor choice, and init
/// `portable/` (clone when `clone_url` is given, else `git init` plus
/// `workspace.json` seeding).
/// Callers build [`DeviceStore`] and sync from the returned root.
pub(crate) fn ensure_ready(clone_url: Option<&str>) -> Result<DataRoot> {
    let override_dir = env::var_os(ENV_OVERRIDE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    ensure_ready_with_override(override_dir, clone_url)
}

/// Testable core of [`ensure_ready`] without touching the environment.
pub(crate) fn ensure_ready_with_override(
    override_dir: Option<PathBuf>,
    clone_url: Option<&str>,
) -> Result<DataRoot> {
    let root = resolve_data_root_from_override(override_dir)?;
    DeviceStore::new(&root).seed_fresh_install(&root)?;
    ensure_portable_init(
        &root,
        &InitOptions {
            clone_url: clone_url.filter(|url| !url.is_empty()).map(str::to_owned),
        },
    )?;
    Ok(root)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OsKind {
    Linux,
    Macos,
    Windows,
}

/// Per-OS default for `DEVCROFT_DATA_DIR` (no directories created).
pub(crate) fn default_data_root() -> Result<PathBuf> {
    let os = if cfg!(windows) {
        OsKind::Windows
    } else if cfg!(target_os = "macos") {
        OsKind::Macos
    } else {
        OsKind::Linux
    };
    default_root_for(
        os,
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
        env::var_os("LOCALAPPDATA").as_deref(),
        env::var_os("USERPROFILE").as_deref(),
    )
    .context("resolving default data directory (home environment must be set)")
}

/// Pure OS-default mapping, kept free of `cfg` so every row is unit-testable
/// on every host. Empty values count as unset.
///
/// - Linux: `$XDG_DATA_HOME/devcroft`, fallback `~/.local/share/devcroft`.
/// - macOS: `~/Library/Application Support/devcroft`.
/// - Windows: `%LOCALAPPDATA%\devcroft`, fallback
///   `%USERPROFILE%\AppData\Local\devcroft` (`Local`, not `Roaming`: Roaming
///   replicates to domain controllers and is wrong for a git-synced tree).
pub(crate) fn default_root_for(
    os: OsKind,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
    localappdata: Option<&OsStr>,
    userprofile: Option<&OsStr>,
) -> Option<PathBuf> {
    let present =
        |value: Option<&OsStr>| value.filter(|value| !value.is_empty()).map(PathBuf::from);
    match os {
        OsKind::Linux => {
            if let Some(dir) = present(xdg_data_home) {
                return Some(dir.join("devcroft"));
            }
            present(home).map(|home| home.join(".local/share/devcroft"))
        }
        OsKind::Macos => {
            present(home).map(|home| home.join("Library/Application Support/devcroft"))
        }
        OsKind::Windows => {
            if let Some(dir) = present(localappdata) {
                return Some(dir.join("devcroft"));
            }
            present(userprofile).map(|profile| profile.join("AppData/Local/devcroft"))
        }
    }
}

/// Two-space JSON plus trailing newline, written atomically.
pub(crate) fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).context("serializing JSON")?;
    text.push('\n');
    write_text_atomic(path, &text)
}

/// Raw text written to a hidden temp sibling in the same directory and then
/// renamed over the target. Same-filesystem rename keeps replacement atomic;
/// readers never see a torn file.
pub(crate) fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&tmp, text.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

/// Run the git CLI with the process working directory scoped to `dir` — the
/// plan's "cwd always `portable/`" holds literally, never via inherited cwd.
/// `stdin` is nulled and terminal prompts disabled so credential requests
/// fail fast with a surfaced error instead of hanging the app.
pub(crate) fn run_git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| {
            format!(
                "spawning git for `{}` in {}",
                render_git_args(args),
                dir.display()
            )
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut detail = stderr.trim().to_owned();
    if detail.is_empty() {
        detail = stdout.trim().to_owned();
    }
    if detail.is_empty() {
        detail = format!("exit {}", output.status);
    }
    bail!(
        "git {} in {} failed: {}",
        render_git_args(args),
        dir.display(),
        sanitize_git_message(&detail)
    );
}

fn render_git_args(args: &[&str]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("\"{arg}\"")
            } else {
                (*arg).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Redact `://credentials@` userinfo that git echoes in URLs (tokens in
/// remote URLs must never reach the UI or logs). Operates on the message
/// text only; matching stops at whitespace or `/` so bare `user@host`
/// fragments and emails are left alone.
pub(crate) fn sanitize_git_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(pos) = rest.find("://") {
        let after = &rest[pos + 3..];
        let redacted = match after.find('@') {
            Some(at) => {
                let userinfo = &after[..at];
                if !userinfo.is_empty()
                    && !userinfo.contains('/')
                    && !userinfo.chars().any(char::is_whitespace)
                {
                    out.push_str(&rest[..pos]);
                    out.push_str("://***@");
                    rest = &after[at + 1..];
                    true
                } else {
                    false
                }
            }
            None => false,
        };
        if !redacted {
            out.push_str(&rest[..pos + 3]);
            rest = &rest[pos + 3..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_derives_portable_and_device_paths() {
        let root = DataRoot::new(PathBuf::from("/tmp/data"));
        assert_eq!(root.portable_dir(), PathBuf::from("/tmp/data/portable"));
        assert_eq!(root.device_path(), PathBuf::from("/tmp/data/device.json"));
    }

    #[test]
    fn override_wins_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom-root");
        let root = resolve_data_root_from_override(Some(custom.clone())).unwrap();
        assert_eq!(root.root(), custom);
        assert!(custom.is_dir());
        assert!(custom.join("portable").is_dir());
        assert_eq!(root.device_path(), custom.join("device.json"));
    }

    #[test]
    fn ensure_ready_composes_resolve_init_and_seed() {
        let dir = tempfile::tempdir().unwrap();
        let root = ensure_ready_with_override(Some(dir.path().join("data")), None).unwrap();
        assert!(root.portable_dir().join(".git").exists());
        let text = std::fs::read_to_string(root.portable_dir().join("workspace.json")).unwrap();
        assert!(text.contains("\"formatVersion\""));
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .editor_choice_or_default(),
            crate::editor::EditorChoice::BuiltIn
        );
        // Second startup is a no-op (already initialized).
        ensure_ready_with_override(Some(dir.path().join("data")), None).unwrap();
        assert_eq!(
            DeviceStore::new(&root)
                .load()
                .unwrap()
                .editor_choice_or_default(),
            crate::editor::EditorChoice::BuiltIn
        );
    }

    #[test]
    fn os_defaults_follow_the_plan() {
        // Linux: XDG wins, else ~/.local/share, empty counts as unset.
        assert_eq!(
            default_root_for(
                OsKind::Linux,
                Some(OsStr::new("/xdg")),
                Some(OsStr::new("/home/u")),
                None,
                None
            ),
            Some(PathBuf::from("/xdg/devcroft"))
        );
        assert_eq!(
            default_root_for(OsKind::Linux, None, Some(OsStr::new("/home/u")), None, None),
            Some(PathBuf::from("/home/u/.local/share/devcroft"))
        );
        assert_eq!(
            default_root_for(
                OsKind::Linux,
                Some(OsStr::new("")),
                Some(OsStr::new("/home/u")),
                None,
                None
            ),
            Some(PathBuf::from("/home/u/.local/share/devcroft"))
        );
        assert_eq!(
            default_root_for(OsKind::Linux, None, None, None, None),
            None
        );
        // macOS: ~/Library/Application Support/devcroft.
        assert_eq!(
            default_root_for(
                OsKind::Macos,
                None,
                Some(OsStr::new("/Users/u")),
                None,
                None
            ),
            Some(PathBuf::from(
                "/Users/u/Library/Application Support/devcroft"
            ))
        );
        assert_eq!(
            default_root_for(OsKind::Macos, None, None, None, None),
            None
        );
        // Windows: Local, never Roaming; USERPROFILE fallback appends AppData\Local.
        assert_eq!(
            default_root_for(
                OsKind::Windows,
                None,
                None,
                Some(OsStr::new("C:/Users/u/AppData/Local")),
                Some(OsStr::new("C:/Users/u"))
            ),
            Some(PathBuf::from("C:/Users/u/AppData/Local/devcroft"))
        );
        assert_eq!(
            default_root_for(
                OsKind::Windows,
                None,
                None,
                None,
                Some(OsStr::new("C:/Users/u"))
            ),
            Some(PathBuf::from("C:/Users/u/AppData/Local/devcroft"))
        );
        assert_eq!(
            default_root_for(OsKind::Windows, None, None, None, None),
            None
        );
    }

    #[test]
    fn atomic_write_is_two_space_json_with_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/device.json");
        write_json_atomic(&path, &json!({"b": 1, "a": {"nested": true}})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        assert!(
            text.contains("\n  \""),
            "expected two-space indent, got: {text}"
        );
        // No temp litter left beside the target.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("nested"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        // Round-trips.
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!({"a": {"nested": true}, "b": 1}));
    }

    #[test]
    fn sanitize_redacts_url_credentials_only() {
        assert_eq!(
            sanitize_git_message("fetch https://token123@github.com/o/r failed"),
            "fetch https://***@github.com/o/r failed"
        );
        assert_eq!(
            sanitize_git_message("fetch https://user:pass@host/x failed"),
            "fetch https://***@host/x failed"
        );
        assert_eq!(
            sanitize_git_message("nothing secret here"),
            "nothing secret here"
        );
        // No scheme: left alone.
        assert_eq!(
            sanitize_git_message("contact user@host for access"),
            "contact user@host for access"
        );
    }
}
