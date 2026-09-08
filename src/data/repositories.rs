//! Portable repository records plus checkout inspection.
//!
//! Mirrors the Electron `RepositoryService` create path
//! (`docs/feature-parity.md` §§2–3): a checkout directory is inspected with
//! the git CLI (toplevel, remotes, base branch), an editable key suggestion
//! is derived, `portable/repositories/<key>/repository.json` is written
//! atomically, and the checkout binding is saved to machine-local
//! `device.json`. Portable metadata stays `camelCase` so a directory from
//! the original app keeps opening here; device bindings stay `snake_case`
//! like the rest of `device.json`.
//!
//! Creation is intentionally not transactional: the portable record is
//! written first, then the device binding. A crash between the two leaves a
//! portable record with no binding, which Home shows as "not linked" — the
//! same recovery shape as linking on another device.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::device::DeviceRepositoryBinding;
use super::store_lock::{portable_gate, reject_symlink};
use super::tasks::read_bounded;
use super::{DataRoot, DeviceStore, run_git_in, write_json_atomic};

/// Portable repository metadata. `camelCase` matches the Electron
/// `repository.json` shape; unknown fields round-trip for forward
/// compatibility. The enclosing directory is the primary identity — `key`
/// repeats it as a manual-inspection convenience and a mismatch is a
/// warning elsewhere, never a load failure here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RepositoryMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(
        default,
        rename = "displayName",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(default, rename = "cloneUrl", skip_serializing_if = "Option::is_none")]
    pub(crate) clone_url: Option<String>,
    #[serde(
        default,
        rename = "baseBranch",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) base_branch: Option<String>,
    #[serde(flatten, default)]
    pub(crate) extra: HashMap<String, Value>,
}

/// Portable discovery does not require or load machine-local checkout bindings.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositorySummary {
    pub(crate) key: String,
    pub(crate) display_name: Option<String>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct RepositoryList {
    pub(crate) repositories: Vec<RepositorySummary>,
    pub(crate) errors: Vec<String>,
    pub(crate) truncated: bool,
}

/// Unlike palette recents, report malformed siblings and include unbound keys.
/// The portable gate prevents built-in sync/checkout from changing the scan.
pub(crate) fn list_repositories(root: &DataRoot, limit: usize) -> Result<RepositoryList> {
    let _gate = portable_gate(root, false)?;
    reject_symlink(&root.portable_dir())?;
    let dir = root.portable_dir().join("repositories");
    reject_symlink(&dir)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RepositoryList::default()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut result = RepositoryList::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                result.errors.push(e.to_string());
                continue;
            }
        };
        let loaded = (|| -> Result<RepositorySummary> {
            let key = require_repository_key(&entry.file_name().to_string_lossy())?;
            reject_symlink(&entry.path())?;
            let path = entry.path().join("repository.json");
            let metadata: RepositoryMetadata = serde_json::from_slice(&read_bounded(&path)?)
                .with_context(|| format!("malformed repository at {}", path.display()))?;
            let mut warnings = Vec::new();
            if metadata
                .key
                .as_ref()
                .is_some_and(|recorded| recorded != &key)
            {
                warnings.push(format!(
                    "repository {key}: metadata key {:?} differs from directory key; use {key}",
                    metadata.key
                ));
            }
            Ok(RepositorySummary {
                key,
                display_name: metadata.display_name,
                warnings,
            })
        })();
        match loaded {
            Ok(record) => result.repositories.push(record),
            Err(e) => result
                .errors
                .push(format!("{}: {e:#}", entry.path().display())),
        }
    }
    result.repositories.sort_by(|a, b| a.key.cmp(&b.key));
    result.errors.sort();
    result.truncated = result.repositories.len() > limit;
    result.repositories.truncate(limit);
    Ok(result)
}

/// Editable inputs for [`create_repository`]. Every field is optional except
/// the key and checkout path, which travel as separate arguments; a blank
/// (`None` or whitespace-only) harvested field falls back to the inspected
/// checkout value, while an explicit value always wins.
#[derive(Clone, Debug, Default)]
pub(crate) struct NewRepositoryInput {
    pub(crate) display_name: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) group: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) clone_url: Option<String>,
    pub(crate) base_branch: Option<String>,
}

/// What checkout inspection harvested from a local directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckoutInspection {
    pub(crate) checkout_path: PathBuf,
    pub(crate) remote_name: String,
    pub(crate) clone_url: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
    pub(crate) base_branch: String,
    pub(crate) suggested_key: String,
}

/// What [`create_repository`] persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreatedRepository {
    pub(crate) key: String,
    pub(crate) repository_path: PathBuf,
    pub(crate) checkout_path: PathBuf,
}

/// One switchable repository for the palette: portable metadata joined
/// with its machine-local checkout binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecentRepository {
    pub(crate) key: String,
    pub(crate) display_name: Option<String>,
    pub(crate) checkout_path: PathBuf,
    pub(crate) last_opened_at: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) group: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) name: Option<String>,
}

impl RecentRepository {
    /// Palette label: display name when set, otherwise the key.
    pub(crate) fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.key)
    }
}

/// Recent repositories for the palette switcher, most-recent first,
/// capped at `limit`. Only records with a linked checkout that still
/// exists on disk are switchable, so unlinked, malformed, and missing
/// records are skipped here — Home (later) owns their error cards, and
/// the palette must never break on a half-written record.
pub(crate) fn recent_repositories(root: &DataRoot, limit: usize) -> Vec<RecentRepository> {
    let mut recents = Vec::new();
    let state = DeviceStore::new(root).load().unwrap_or_default();
    let bindings = state.repositories.unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(root.portable_dir().join("repositories")) else {
        return recents;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let key = entry.file_name().to_string_lossy().into_owned();
        if require_repository_key(&key).is_err() {
            continue;
        }
        let metadata: RepositoryMetadata = match std::fs::read(entry.path().join("repository.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(metadata) => metadata,
            None => continue,
        };
        let binding = bindings.get(&key);
        let checkout = binding
            .and_then(|binding| binding.checkout_path.as_deref())
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_dir());
        let Some(checkout_path) = checkout else {
            continue;
        };
        recents.push(RecentRepository {
            key,
            display_name: metadata.display_name.filter(|name| !name.trim().is_empty()),
            checkout_path,
            last_opened_at: binding
                .and_then(|binding| binding.last_opened_at.clone())
                .filter(|opened| !opened.is_empty()),
            description: metadata
                .description
                .filter(|value| !value.trim().is_empty()),
            group: metadata.group.filter(|value| !value.trim().is_empty()),
            owner: metadata.owner.filter(|value| !value.trim().is_empty()),
            name: metadata.name.filter(|value| !value.trim().is_empty()),
        });
    }
    recents.sort_by(
        |left, right| match (&left.last_opened_at, &right.last_opened_at) {
            // ISO-8601 `last_opened_at` values sort chronologically as strings.
            (Some(before), Some(after)) => after.cmp(before).then_with(|| left.key.cmp(&right.key)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.key.cmp(&right.key),
        },
    );
    recents.truncate(limit);
    recents
}

/// Linked checkout for one repository key, if it is still on disk. The
/// palette resolves through this (not its cached list) so a record added
/// or linked since the palette opened still switches.
pub(crate) fn checkout_for(root: &DataRoot, key: &str) -> Option<PathBuf> {
    let key = require_repository_key(key).ok()?;
    let state = DeviceStore::new(root).load().ok()?;
    let checkout = state
        .repositories?
        .get(&key)?
        .checkout_path
        .clone()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())?;
    Some(checkout)
}

/// Record opening a repository: stamp `last_opened_at` on its binding and
/// remember it as `last_repository`, mirroring Electron's open path whose
/// recency feeds the shell and switcher. Requires the portable record —
/// switching to a key with no record is a caller bug, surfaced here.
pub(crate) fn record_repository_open(root: &DataRoot, key: &str) -> Result<()> {
    let key = require_repository_key(key)?;
    if !repository_dir(root, &key).join("repository.json").is_file() {
        bail!("repository \"{key}\" has no portable record");
    }
    let opened_at = now_rfc3339_utc();
    DeviceStore::new(root)
        .update(|state| {
            let mut bindings = state.repositories.take().unwrap_or_default();
            let mut binding = bindings.remove(&key).unwrap_or_default();
            binding.last_opened_at = Some(opened_at.clone());
            bindings.insert(key.clone(), binding);
            state.repositories = Some(bindings);
            state.last_repository = Some(key.clone());
        })
        .context("recording repository recency in device.json")
}

/// Match the startup checkout against linked bindings, so the palette can
/// mark the current repository without waiting for the first switch.
pub(crate) fn resolve_current_key(root: &DataRoot, cwd: &Path) -> Option<String> {
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let state = DeviceStore::new(root).load().ok()?;
    state.repositories?.into_iter().find_map(|(key, binding)| {
        let checkout = binding.checkout_path?;
        (std::fs::canonicalize(&checkout).ok()? == cwd).then_some(key)
    })
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled from Unix
/// seconds (civil-from-days) so recency needs no date dependency;
/// lexicographic order stays chronological order.
fn now_rfc3339_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix_secs(secs)
}

fn rfc3339_from_unix_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    )
}

/// Directory holding one portable record: `portable/repositories/<key>`.
pub(crate) fn repository_dir(root: &DataRoot, key: &str) -> PathBuf {
    root.portable_dir().join("repositories").join(key)
}

/// Lowercase, filesystem-friendly normalization for suggested keys:
/// lowercase, runs of disallowed characters become one `-`, no leading or
/// trailing `.-_`. Mirrors Electron's `normalizeRepositoryKey`.
pub(crate) fn normalize_repository_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_dash = true; // Treat a leading run as trimmable.
    for ch in value.trim().to_lowercase().chars() {
        let allowed =
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-';
        if allowed {
            // Collapse only `-` runs (Electron collapses `-{2,}`); `.`/`_`
            // runs pass through, and the final trim below strips edges.
            if ch == '-' {
                if last_was_dash {
                    continue;
                }
                last_was_dash = true;
            } else {
                last_was_dash = false;
            }
            out.push(ch);
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.starts_with(['.', '_', '-']) {
        out.remove(0);
    }
    while out.ends_with(['.', '_', '-']) {
        out.pop();
    }
    out
}

/// Editable key suggestion in `<owner>-<name>` form. An empty owner falls
/// back to the bare name (normalization would trim the stray `-` anyway).
pub(crate) fn suggest_repository_key(owner: &str, name: &str) -> String {
    if owner.trim().is_empty() {
        normalize_repository_key(name)
    } else {
        normalize_repository_key(&format!("{}-{}", owner.trim(), name.trim()))
    }
}

/// Validate an explicit key without normalizing it: the stored key must
/// already be normalized, matching Electron's create-time rejection (which
/// prompts for a different key on collision rather than inventing one).
pub(crate) fn require_repository_key(key: &str) -> Result<String> {
    if key.is_empty() || key == "." || key == ".." {
        bail!("repository key must not be empty");
    }
    if key != normalize_repository_key(key) || !key_is_well_formed(key) {
        bail!(
            "repository key must be lowercase and contain only letters, numbers, periods, underscores, and hyphens"
        );
    }
    Ok(key.to_owned())
}

fn key_is_well_formed(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    let mut last = first;
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-')
        {
            return false;
        }
        last = ch;
    }
    last.is_ascii_lowercase() || last.is_ascii_digit()
}

/// Inspect a local directory as a repository checkout: resolve symlinks,
/// prove it is inside a git worktree, harvest remotes (preferring
/// `origin`), parse owner/name from the remote URL, and detect the base
/// branch. Never writes anything.
pub(crate) fn inspect_checkout(path: &Path) -> Result<CheckoutInspection> {
    let selected = std::fs::canonicalize(path)
        .with_context(|| format!("could not open checkout at {}", path.display()))?;
    let raw_root = run_git_in(&selected, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        anyhow::anyhow!(
            "directory at {} is not a git checkout: {error:#}",
            selected.display()
        )
    })?;
    let raw_root = raw_root.trim();
    if raw_root.is_empty() {
        bail!("directory at {} is not a git checkout", selected.display());
    }
    let checkout_path = std::fs::canonicalize(raw_root)
        .with_context(|| format!("resolving checkout root at {raw_root}"))?;
    let remotes = list_remotes(&checkout_path);
    let remote = remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .or(remotes.first());
    let remote_name = remote
        .map_or("origin", |remote| remote.name.as_str())
        .to_owned();
    let clone_url = remote.map(|remote| remote.url.clone());
    let identity = parse_remote_identity(clone_url.as_deref());
    let name = identity.name.clone().unwrap_or_else(|| {
        checkout_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository")
            .to_owned()
    });
    let base_branch = detect_base_branch(&checkout_path, &remote_name);
    let suggested_key = suggest_repository_key(identity.owner.as_deref().unwrap_or(""), &name);
    Ok(CheckoutInspection {
        checkout_path,
        remote_name,
        clone_url,
        owner: identity.owner,
        name,
        base_branch,
        suggested_key,
    })
}

/// Create `portable/repositories/<key>/repository.json` and bind the
/// checkout in `device.json`. Explicit non-blank input wins; otherwise the
/// inspected checkout value is used. Stamps `last_opened_at` and
/// `last_repository` like [`record_repository_open`], so a just-added
/// repository sorts first in [`recent_repositories`] instead of sinking
/// below every opened record and falling off the palette cap. Errors when
/// the key is invalid, taken, or the directory is not a git checkout.
pub(crate) fn create_repository(
    root: &DataRoot,
    key: &str,
    checkout_path: &Path,
    input: &NewRepositoryInput,
) -> Result<CreatedRepository> {
    let key = require_repository_key(key)?;
    let dir = repository_dir(root, &key);
    if dir.exists() {
        bail!("repository key \"{key}\" already exists");
    }
    let inspection = inspect_checkout(checkout_path)?;
    let metadata = RepositoryMetadata {
        key: Some(key.clone()),
        owner: clean_optional(input.owner.as_deref())
            .map(str::to_owned)
            .or(inspection.owner.clone()),
        name: clean_optional(input.name.as_deref())
            .map(str::to_owned)
            .or(Some(inspection.name.clone())),
        display_name: clean_optional(input.display_name.as_deref()).map(str::to_owned),
        description: clean_optional(input.description.as_deref()).map(str::to_owned),
        group: clean_optional(input.group.as_deref()).map(str::to_owned),
        tags: clean_tags(&input.tags),
        clone_url: clean_optional(input.clone_url.as_deref())
            .map(str::to_owned)
            .or(inspection.clone_url.clone()),
        base_branch: clean_optional(input.base_branch.as_deref())
            .map(str::to_owned)
            .or(Some(inspection.base_branch.clone())),
        extra: HashMap::new(),
    };
    write_json_atomic(&dir.join("repository.json"), &metadata)
        .with_context(|| format!("saving repository record to {}", dir.display()))?;
    let checkout_display = inspection.checkout_path.to_string_lossy().into_owned();
    let remote_display = inspection.remote_name.clone();
    let opened_at = now_rfc3339_utc();
    DeviceStore::new(root)
        .update(|state| {
            let mut bindings = state.repositories.take().unwrap_or_default();
            let mut binding = bindings.remove(&key).unwrap_or_default();
            binding.checkout_path = Some(checkout_display.clone());
            binding.remote_name = Some(remote_display.clone());
            binding.last_opened_at = Some(opened_at.clone());
            bindings.insert(key.clone(), binding);
            state.repositories = Some(bindings);
            state.last_repository = Some(key.clone());
        })
        .context("saving checkout binding to device.json")?;
    Ok(CreatedRepository {
        key,
        repository_path: dir.join("repository.json"),
        checkout_path: inspection.checkout_path,
    })
}

fn clean_optional(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|cleaned| !cleaned.is_empty())
}

fn clean_tags(tags: &[String]) -> Option<Vec<String>> {
    let mut cleaned = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && !cleaned.iter().any(|kept| kept == tag) {
            cleaned.push(tag.to_owned());
        }
    }
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

struct GitRemote {
    name: String,
    url: String,
}

/// `git config --get-regexp ^remote\..*\.url$`, parsed and sorted by name.
/// A checkout with no remotes is normal (local-only work), not an error.
fn list_remotes(checkout: &Path) -> Vec<GitRemote> {
    let Ok(output) = run_git_in(checkout, &["config", "--get-regexp", r"^remote\..*\.url$"]) else {
        return Vec::new();
    };
    let mut remotes: Vec<GitRemote> = output
        .lines()
        .filter_map(|line| {
            let (key, url) = line.split_once(char::is_whitespace)?;
            let name = key
                .strip_prefix("remote.")?
                .strip_suffix(".url")?
                .trim()
                .to_owned();
            let url = url.trim().to_owned();
            if name.is_empty() || url.is_empty() {
                None
            } else {
                Some(GitRemote { name, url })
            }
        })
        .collect();
    remotes.sort_by(|left, right| left.name.cmp(&right.name));
    remotes
}

struct RemoteIdentity {
    owner: Option<String>,
    name: Option<String>,
}

/// Owner/name parsed from a remote URL (`https://host/owner/name(.git)`
/// or scp-like `git@host:owner/name(.git)`). `file:` URLs and unparsable
/// values yield no identity — the directory basename fills in instead.
fn parse_remote_identity(remote_url: Option<&str>) -> RemoteIdentity {
    let Some(remote_url) = remote_url else {
        return RemoteIdentity {
            owner: None,
            name: None,
        };
    };
    let remote_path = if let Some(scheme_end) = remote_url.find("://") {
        let scheme = &remote_url[..scheme_end];
        if scheme.eq_ignore_ascii_case("file") {
            return RemoteIdentity {
                owner: None,
                name: None,
            };
        }
        let after_scheme = &remote_url[scheme_end + 3..];
        match after_scheme.find('/') {
            Some(slash) => &after_scheme[slash..],
            None => {
                return RemoteIdentity {
                    owner: None,
                    name: None,
                };
            }
        }
    } else if let Some(colon) = remote_url.find(':') {
        // scp-like `host:path`, but not Windows `C:\...` drive prefixes.
        let before = &remote_url[..colon];
        if before.len() == 1 || before.contains('/') {
            return RemoteIdentity {
                owner: None,
                name: None,
            };
        }
        &remote_url[colon + 1..]
    } else {
        return RemoteIdentity {
            owner: None,
            name: None,
        };
    };
    let parts: Vec<&str> = remote_path
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return RemoteIdentity {
            owner: None,
            name: None,
        };
    }
    let raw_name = parts[parts.len() - 1];
    let name = raw_name
        .strip_suffix(".git")
        .or_else(|| raw_name.strip_suffix(".GIT"))
        .unwrap_or(raw_name);
    if name.is_empty() {
        return RemoteIdentity {
            owner: None,
            name: None,
        };
    }
    RemoteIdentity {
        owner: Some(parts[parts.len() - 2].to_owned()),
        name: Some(name.to_owned()),
    }
}

/// Base branch detection, mirroring Electron: the remote `HEAD` symbolic
/// ref first, then local `main`/`master`, then the current branch, finally
/// `main`. Never fails — a wrong guess is editable metadata, not an error.
fn detect_base_branch(checkout: &Path, remote_name: &str) -> String {
    let head_ref = format!("refs/remotes/{remote_name}/HEAD");
    if let Ok(target) = run_git_in(checkout, &["symbolic-ref", "--quiet", &head_ref]) {
        let prefix = format!("refs/remotes/{remote_name}/");
        let branch = target.trim().strip_prefix(&prefix).unwrap_or("").trim();
        if !branch.is_empty() {
            return branch.to_owned();
        }
    }
    for branch in ["main", "master"] {
        if run_git_in(
            checkout,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok()
        {
            return branch.to_owned();
        }
    }
    if let Ok(current) = run_git_in(checkout, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        let current = current.trim();
        if !current.is_empty() {
            return current.to_owned();
        }
    }
    "main".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn init_checkout(dir: &Path) -> PathBuf {
        git(dir, &["init", "-b", "main"]);
        configure_identity(dir);
        std::fs::write(dir.join("file.txt"), "hello\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", "seed"]);
        dir.to_owned()
    }

    #[test]
    fn key_normalization_matches_electron() {
        assert_eq!(
            normalize_repository_key("HossainEmruz-Devcroft"),
            "hossainemruz-devcroft"
        );
        assert_eq!(normalize_repository_key("  owner / name  "), "owner-name");
        assert_eq!(normalize_repository_key("a__b..c"), "a__b..c");
        assert_eq!(normalize_repository_key("--Trim Me--"), "trim-me");
        assert_eq!(normalize_repository_key("a---b"), "a-b");
        assert_eq!(normalize_repository_key(""), "");
    }

    #[test]
    fn key_suggestion_prefers_owner_name() {
        assert_eq!(
            suggest_repository_key("hossainemruz", "Devcroft"),
            "hossainemruz-devcroft"
        );
        assert_eq!(suggest_repository_key("", "Devcroft"), "devcroft");
        assert_eq!(suggest_repository_key("  ", "My Repo!"), "my-repo");
    }

    #[test]
    fn key_validation_rejects_without_normalizing() {
        assert!(require_repository_key("hossainemruz-devcroft").is_ok());
        assert!(require_repository_key("a").is_ok());
        assert!(require_repository_key("a.b_c-d9").is_ok());
        assert!(require_repository_key("").is_err());
        assert!(require_repository_key(".").is_err());
        assert!(require_repository_key("..").is_err());
        assert!(require_repository_key("Upper").is_err());
        assert!(require_repository_key("-leading").is_err());
        assert!(require_repository_key("trailing-").is_err());
        assert!(require_repository_key("has space").is_err());
        assert!(require_repository_key("under_score_").is_err());
    }

    #[test]
    fn remote_identity_parses_url_and_scp_forms() {
        let https = parse_remote_identity(Some("https://github.com/hossainemruz/devcroft.git"));
        assert_eq!(https.owner.as_deref(), Some("hossainemruz"));
        assert_eq!(https.name.as_deref(), Some("devcroft"));

        let scp = parse_remote_identity(Some("git@github.com:hossainemruz/devcroft.git"));
        assert_eq!(scp.owner.as_deref(), Some("hossainemruz"));
        assert_eq!(scp.name.as_deref(), Some("devcroft"));

        let nested = parse_remote_identity(Some("https://host.com/a/b/c"));
        assert_eq!(nested.owner.as_deref(), Some("b"));
        assert_eq!(nested.name.as_deref(), Some("c"));

        let file = parse_remote_identity(Some("file:///tmp/repo"));
        assert!(file.owner.is_none() && file.name.is_none());

        let bare = parse_remote_identity(Some("just-a-name"));
        assert!(bare.owner.is_none() && bare.name.is_none());

        let missing = parse_remote_identity(None);
        assert!(missing.owner.is_none() && missing.name.is_none());
    }

    #[test]
    fn inspect_harvests_remote_and_branch() {
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        git(
            checkout.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:hossainemruz/devcroft.git",
            ],
        );

        let inspection = inspect_checkout(checkout.path()).unwrap();
        assert_eq!(inspection.remote_name, "origin");
        assert_eq!(
            inspection.clone_url.as_deref(),
            Some("git@github.com:hossainemruz/devcroft.git")
        );
        assert_eq!(inspection.owner.as_deref(), Some("hossainemruz"));
        assert_eq!(inspection.name, "devcroft");
        assert_eq!(inspection.base_branch, "main");
        assert_eq!(inspection.suggested_key, "hossainemruz-devcroft");
    }

    #[test]
    fn inspect_rejects_non_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let error = format!("{:#}", inspect_checkout(dir.path()).expect_err("must fail"));
        assert!(error.contains("not a git checkout"), "{error}");
    }

    #[test]
    fn create_writes_record_and_binding() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());

        let created = create_repository(
            &root,
            "hossainemruz-devcroft",
            checkout.path(),
            &NewRepositoryInput {
                display_name: Some("Devcroft".to_owned()),
                ..NewRepositoryInput::default()
            },
        )
        .unwrap();
        assert_eq!(created.key, "hossainemruz-devcroft");

        // Portable record is camelCase for Electron parity.
        let text = std::fs::read_to_string(&created.repository_path).unwrap();
        assert!(text.contains("\"displayName\""), "{text}");
        assert!(text.contains("\"baseBranch\""), "{text}");
        let metadata: RepositoryMetadata = serde_json::from_str(&text).unwrap();
        assert_eq!(metadata.key.as_deref(), Some("hossainemruz-devcroft"));
        assert_eq!(metadata.display_name.as_deref(), Some("Devcroft"));
        assert_eq!(metadata.base_branch.as_deref(), Some("main"));

        // Device binding points at the real checkout root.
        let state = DeviceStore::new(&root).load().unwrap();
        let binding = state.repositories.as_ref().unwrap()["hossainemruz-devcroft"].clone();
        assert!(binding.checkout_path.as_deref().is_some());
        assert_eq!(binding.remote_name.as_deref(), Some("origin"));
        // Creating stamps recency so the new repo survives the palette cap.
        assert_eq!(
            state.last_repository.as_deref(),
            Some("hossainemruz-devcroft")
        );
        let opened = binding
            .last_opened_at
            .clone()
            .expect("create must stamp recency");
        assert!(
            opened.len() == 20 && opened.ends_with('Z'),
            "expected RFC-3339 UTC, got {opened}"
        );
    }

    #[test]
    fn create_rejects_collisions_and_bad_keys() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());

        create_repository(
            &root,
            "taken",
            checkout.path(),
            &NewRepositoryInput::default(),
        )
        .unwrap();
        let error = format!(
            "{:#}",
            create_repository(
                &root,
                "taken",
                checkout.path(),
                &NewRepositoryInput::default(),
            )
            .expect_err("collision must fail")
        );
        assert!(error.contains("already exists"), "{error}");
        assert!(
            create_repository(
                &root,
                "Not Valid",
                checkout.path(),
                &NewRepositoryInput::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn explicit_input_wins_over_harvest() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        git(
            checkout.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/o/harvested.git",
            ],
        );

        create_repository(
            &root,
            "custom",
            checkout.path(),
            &NewRepositoryInput {
                owner: Some("  explicit  ".to_owned()),
                clone_url: Some("https://example.com/o/explicit.git".to_owned()),
                base_branch: Some("trunk".to_owned()),
                tags: vec!["a".to_owned(), " a ".to_owned(), "".to_owned()],
                ..NewRepositoryInput::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(repository_dir(&root, "custom").join("repository.json"))
            .unwrap();
        let metadata: RepositoryMetadata = serde_json::from_str(&text).unwrap();
        assert_eq!(metadata.owner.as_deref(), Some("explicit"));
        assert_eq!(
            metadata.clone_url.as_deref(),
            Some("https://example.com/o/explicit.git")
        );
        assert_eq!(metadata.base_branch.as_deref(), Some("trunk"));
        assert_eq!(metadata.tags, Some(vec!["a".to_owned()]));
    }

    fn seed_linked(root: &DataRoot, key: &str, opened: Option<&str>) -> PathBuf {
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        // Leak the tempdir so the checkout outlives the call for `is_dir`.
        // Canonicalize first: `/var` is a symlink to `/private/var` on
        // macOS, while inspection always stores the canonical root.
        let checkout = std::fs::canonicalize(checkout.keep()).unwrap();
        create_repository(root, key, &checkout, &NewRepositoryInput::default()).unwrap();
        // `create_repository` stamps recency; the `None` fixture means
        // never-opened, so clear it back to preserve the ordering case.
        let opened = opened.map(str::to_owned);
        let key = key.to_owned();
        DeviceStore::new(root)
            .update(|state| {
                let mut bindings = state.repositories.take().unwrap_or_default();
                let mut binding = bindings.remove(&key).unwrap_or_default();
                binding.last_opened_at = opened.clone();
                bindings.insert(key.clone(), binding);
                state.repositories = Some(bindings);
            })
            .unwrap();
        checkout
    }

    #[test]
    fn recent_orders_by_recency_and_caps_at_limit() {
        let (_dir, root) = fresh_root();
        seed_linked(&root, "aaa-never", None);
        seed_linked(&root, "bbb-old", Some("2024-01-01T00:00:00Z"));
        seed_linked(&root, "ccc-new", Some("2024-06-01T00:00:00Z"));
        seed_linked(&root, "ddd-mid", Some("2024-03-01T00:00:00Z"));

        let keys: Vec<_> = recent_repositories(&root, 3)
            .into_iter()
            .map(|repo| repo.key)
            .collect();
        assert_eq!(keys, vec!["ccc-new", "ddd-mid", "bbb-old"]);

        // Ties break by key; never-opened sinks below every opened record.
        let (_dir, root) = fresh_root();
        seed_linked(&root, "b-tied", Some("2024-01-01T00:00:00Z"));
        seed_linked(&root, "a-tied", Some("2024-01-01T00:00:00Z"));
        seed_linked(&root, "c-fresh", None);
        let keys: Vec<_> = recent_repositories(&root, 10)
            .into_iter()
            .map(|repo| repo.key)
            .collect();
        assert_eq!(keys, vec!["a-tied", "b-tied", "c-fresh"]);
    }

    #[test]
    fn create_sorts_first_and_survives_cap() {
        let (_dir, root) = fresh_root();
        seed_linked(&root, "bbb-old", Some("2024-01-01T00:00:00Z"));
        seed_linked(&root, "ccc-mid", Some("2024-03-01T00:00:00Z"));
        seed_linked(&root, "ddd-prev", Some("2024-06-01T00:00:00Z"));

        // A just-added repository stamps `now` (2026 in tests), so it sorts
        // ahead of every 2024-dated record and survives the explicit limit
        // of 3 used here.
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        create_repository(
            &root,
            "aaa-new",
            checkout.path(),
            &NewRepositoryInput::default(),
        )
        .unwrap();

        let keys: Vec<_> = recent_repositories(&root, 3)
            .into_iter()
            .map(|repo| repo.key)
            .collect();
        assert_eq!(keys, vec!["aaa-new", "ddd-prev", "ccc-mid"]);
    }

    #[test]
    fn recent_prefers_display_name_and_skips_unswitchable() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        let checkout = std::fs::canonicalize(checkout.keep()).unwrap();
        create_repository(
            &root,
            "shown",
            &checkout,
            &NewRepositoryInput {
                display_name: Some("Shown Repo".to_owned()),
                ..NewRepositoryInput::default()
            },
        )
        .unwrap();

        // Record without a binding: visible to Home later, not switchable.
        std::fs::create_dir_all(repository_dir(&root, "unlinked")).unwrap();
        std::fs::write(
            repository_dir(&root, "unlinked").join("repository.json"),
            "{\"formatVersion\": 1}\n",
        )
        .unwrap();
        // Malformed record: skipped quietly here; Home owns the error card.
        std::fs::create_dir_all(repository_dir(&root, "broken")).unwrap();
        std::fs::write(
            repository_dir(&root, "broken").join("repository.json"),
            "{not json",
        )
        .unwrap();
        // Binding whose checkout is gone: not switchable.
        create_repository(&root, "gone", &checkout, &NewRepositoryInput::default()).unwrap();
        DeviceStore::new(&root)
            .update(|state| {
                let mut bindings = state.repositories.take().unwrap_or_default();
                if let Some(binding) = bindings.get_mut("gone") {
                    binding.checkout_path = Some("/nonexistent-checkout-dir".to_owned());
                }
                state.repositories = Some(bindings);
            })
            .unwrap();

        let recents = recent_repositories(&root, 10);
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].key, "shown");
        assert_eq!(recents[0].label(), "Shown Repo");
        assert_eq!(recents[0].checkout_path, checkout);
    }

    #[test]
    fn recent_passes_through_card_metadata() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        init_checkout(checkout.path());
        create_repository(
            &root,
            "grouped",
            checkout.path(),
            &NewRepositoryInput {
                display_name: Some("Grouped Repo".to_owned()),
                description: Some("What this checkout is for".to_owned()),
                group: Some("work".to_owned()),
                owner: Some("acme".to_owned()),
                name: Some("grouped".to_owned()),
                ..NewRepositoryInput::default()
            },
        )
        .unwrap();

        let recents = recent_repositories(&root, 10);
        assert_eq!(recents.len(), 1);
        assert_eq!(
            recents[0].description.as_deref(),
            Some("What this checkout is for")
        );
        assert_eq!(recents[0].group.as_deref(), Some("work"));
        assert_eq!(recents[0].owner.as_deref(), Some("acme"));
        assert_eq!(recents[0].name.as_deref(), Some("grouped"));
    }

    #[test]
    fn record_open_stamps_recency_and_last_repository() {
        let (_dir, root) = fresh_root();
        seed_linked(&root, "opened", None);

        record_repository_open(&root, "opened").unwrap();
        let state = DeviceStore::new(&root).load().unwrap();
        assert_eq!(state.last_repository.as_deref(), Some("opened"));
        let opened = state.repositories.as_ref().unwrap()["opened"]
            .last_opened_at
            .clone()
            .expect("open must stamp recency");
        assert!(
            opened.len() == 20 && opened.ends_with('Z'),
            "expected RFC-3339 UTC, got {opened}"
        );

        let error = format!(
            "{:#}",
            record_repository_open(&root, "missing").expect_err("unknown key must fail")
        );
        assert!(error.contains("no portable record"), "{error}");
    }

    #[test]
    fn checkout_for_resolves_only_existing_checkouts() {
        let (_dir, root) = fresh_root();
        let checkout = seed_linked(&root, "here", None);
        assert_eq!(checkout_for(&root, "here"), Some(checkout));
        assert_eq!(checkout_for(&root, "missing"), None);
        assert_eq!(checkout_for(&root, "Not Valid"), None);
    }

    #[test]
    fn resolve_current_key_matches_linked_checkout() {
        let (_dir, root) = fresh_root();
        let checkout = seed_linked(&root, "mine", None);
        assert_eq!(
            resolve_current_key(&root, &checkout).as_deref(),
            Some("mine")
        );
        assert_eq!(resolve_current_key(&root, Path::new("/elsewhere")), None);
    }

    #[test]
    fn rfc3339_formats_known_epochs() {
        assert_eq!(rfc3339_from_unix_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_from_unix_secs(1_700_000_000),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(rfc3339_from_unix_secs(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix_secs(-1), "1969-12-31T23:59:59Z");
    }
}
