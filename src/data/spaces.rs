//! Portable space catalog: user-defined isolation profiles.
//!
//! A space partitions the user's portable records — repositories, Home items
//! (PRs, todos, reading), and artifacts through their repository — so one
//! profile's work never shows up while another profile is active. The catalog
//! lives in `portable/spaces.json` and syncs with the records it scopes;
//! which space is active is machine-local (`device.json`), so each device
//! resumes its own last choice.
//!
//! Legacy data carried per-record groups: a `Personal`/`Work` enum on Home
//! items and a free-form string on repositories. Migration is read-tolerant
//! (`group`/`pr_group` translate onto `space`), and
//! [`ensure_spaces_unlocked`] seeds the catalog with the Personal/Work pair
//! plus one space per distinct referenced group name, so nothing is
//! reassigned silently.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::DataRoot;
use super::store_lock::{portable_gate, repository_graph_lock};

/// File name (inside `portable/`) holding the catalog.
const FILE_NAME: &str = "spaces.json";
const SCHEMA_VERSION: u64 = 1;
/// Catalog size guard: names are user-facing and hand-editable, so a
/// malformed or runaway file must not explode startup memory.
const MAX_SPACES: usize = 100;
const MAX_NAME_CHARS: usize = 48;

/// Fallback space for records that predate spaces or never set one. Kept as
/// the seeded first space so an untouched install behaves exactly like the
/// old `Group::Personal` default.
pub(crate) const DEFAULT_SPACE: &str = "Personal";

/// Spaces seeded into a catalog that has none.
pub(crate) const DEFAULT_SEED: [&str; 2] = ["Personal", "Work"];

/// One named profile. Unknown fields round-trip so a future color/icon field
/// survives older builds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Space {
    pub(crate) name: String,
    #[serde(flatten, default)]
    pub(crate) extra: BTreeMap<String, Value>,
}

impl Space {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            extra: BTreeMap::new(),
        }
    }
}

/// The portable catalog, in user order. `formatVersion` is informational and
/// never gates loading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Spaces {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format_version: Option<u64>,
    #[serde(default)]
    pub(crate) spaces: Vec<Space>,
    #[serde(flatten, default)]
    extra: BTreeMap<String, Value>,
}

/// Validate and normalize a user-supplied space name: trimmed, internal
/// whitespace collapsed, control characters rejected. Pure so the Settings
/// form and CLI stay unit-testable without a store.
pub(crate) fn normalize_name(raw: &str) -> Result<String> {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(!collapsed.is_empty(), "A space name is required");
    ensure!(
        collapsed.chars().count() <= MAX_NAME_CHARS,
        "Space names are limited to {MAX_NAME_CHARS} characters"
    );
    ensure!(
        !collapsed.chars().any(char::is_control),
        "Space names cannot contain control characters"
    );
    Ok(collapsed)
}

/// Whitespace-collapsed form used for tolerant comparison. Unlike
/// [`normalize_name`] this never fails and enforces no limits: hand-edited or
/// legacy values ("Client  A") must still match the catalog's canonical
/// spelling ("Client A") instead of becoming unreachable.
fn comparison_key(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case- and whitespace-insensitive space comparison. Stored spellings are
/// canonicalized by [`Spaces::resolve`]; this is for the tolerant reads (CLI
/// filters, records written by another device) that must not split a space
/// on case or spacing alone.
pub(crate) fn space_eq(left: &str, right: &str) -> bool {
    comparison_key(left).eq_ignore_ascii_case(&comparison_key(right))
}

/// Whether a stored space value belongs to `active`. Absent/blank values
/// count as [`DEFAULT_SPACE`], matching how pre-space records behave.
pub(crate) fn space_matches(stored: Option<&str>, active: &str) -> bool {
    match stored.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => space_eq(value, active),
        None => space_eq(DEFAULT_SPACE, active),
    }
}

impl Spaces {
    pub(crate) fn path(root: &DataRoot) -> std::path::PathBuf {
        root.portable_dir().join(FILE_NAME)
    }

    /// Tolerant load: a missing file is an empty catalog (migration decides
    /// what to seed); malformed JSON is an error the caller surfaces.
    pub(crate) fn load(root: &DataRoot) -> Result<Self> {
        let path = Self::path(root);
        let bytes = match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
            Ok(bytes) => bytes,
        };
        let mut catalog: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        catalog.spaces.truncate(MAX_SPACES);
        // Drop blank and equivalent duplicates (a hand-edited catalog can
        // hold "Client A" and "Client  A"): canonicalization treats them as
        // one space, so keeping both would make management ambiguous. The
        // equivalence rule is exactly `space_eq`'s (whitespace-collapsed,
        // ASCII case-insensitive), so load, lookup, and creation agree.
        let mut seen: Vec<String> = Vec::new();
        catalog.spaces.retain(|space| {
            if space.name.trim().is_empty() {
                return false;
            }
            let key = comparison_key(&space.name);
            if seen
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&key))
            {
                return false;
            }
            seen.push(key);
            true
        });
        Ok(catalog)
    }

    /// Stale-write guard, matching [`super::dashboard::Dashboard::save`]:
    /// refuse to replace a catalog that changed since it was read.
    pub(crate) fn save(&self, root: &DataRoot, expected: &Self) -> Result<()> {
        if Self::load(root)? != *expected {
            bail!("Spaces changed on disk. Reload spaces before saving again.");
        }
        self.write(root)
    }

    fn write(&self, root: &DataRoot) -> Result<()> {
        let bytes = self.encoded()?;
        let text = std::str::from_utf8(&bytes).context("encoding spaces catalog")?;
        super::write_text_atomic(&Self::path(root), text).context("writing spaces catalog")
    }

    /// Encoded catalog bytes (two-space JSON plus trailing newline), for
    /// callers that stage the catalog as part of a larger atomic rewrite.
    fn encoded(&self) -> Result<Vec<u8>> {
        let mut catalog = self.clone();
        catalog.format_version = Some(SCHEMA_VERSION);
        let mut text =
            serde_json::to_string_pretty(&catalog).context("serializing spaces catalog")?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spaces.is_empty()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.spaces.iter().map(|space| space.name.clone()).collect()
    }

    pub(crate) fn first(&self) -> Option<&str> {
        self.spaces.first().map(|space| space.name.as_str())
    }

    /// Canonical catalog spelling of `name`, matched case-insensitively and
    /// ignoring whitespace differences.
    pub(crate) fn canonical(&self, name: &str) -> Option<String> {
        if name.trim().is_empty() {
            return None;
        }
        self.spaces
            .iter()
            .find(|space| space_eq(&space.name, name))
            .map(|space| space.name.clone())
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.canonical(name).is_some()
    }

    /// The concrete space for a stored value: canonical spelling when the
    /// catalog knows it, the trimmed stored value when it does not (a record
    /// from a newer device), else the catalog's first space, else
    /// [`DEFAULT_SPACE`].
    pub(crate) fn resolve(&self, stored: Option<&str>) -> String {
        let stored = stored.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = stored {
            if let Some(canonical) = self.canonical(value) {
                return canonical;
            }
            return value.to_owned();
        }
        self.first()
            .map(str::to_owned)
            .unwrap_or_else(|| DEFAULT_SPACE.to_owned())
    }

    /// Add `name` when no case-insensitive match exists. Returns whether the
    /// catalog changed. Invalid names and a full catalog are no-ops.
    pub(crate) fn ensure_name(&mut self, name: &str) -> bool {
        if self.spaces.len() >= MAX_SPACES {
            return false;
        }
        let Ok(name) = normalize_name(name) else {
            return false;
        };
        if self.contains(&name) {
            return false;
        }
        self.spaces.push(Space::new(name));
        true
    }

    /// Rename in place. The caller rewrites records that reference the old
    /// name (see [`rename_space`]) before saving the catalog.
    pub(crate) fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let to = normalize_name(to)?;
        let index = self
            .spaces
            .iter()
            .position(|space| space_eq(&space.name, from))
            .with_context(|| format!("Space {from:?} was removed. Reload spaces"))?;
        ensure!(
            !self
                .spaces
                .iter()
                .enumerate()
                .any(|(i, space)| i != index && space_eq(&space.name, &to)),
            "The space {to:?} already exists"
        );
        self.spaces[index].name = to;
        Ok(())
    }

    /// Remove `name`. The caller moves records to a destination first (see
    /// [`delete_space`]).
    pub(crate) fn remove(&mut self, name: &str) -> Result<()> {
        let index = self
            .spaces
            .iter()
            .position(|space| space_eq(&space.name, name))
            .with_context(|| format!("Space {name:?} was removed. Reload spaces"))?;
        ensure!(
            self.spaces.len() > 1,
            "Keep at least one space; move items instead of deleting the last one"
        );
        self.spaces.remove(index);
        Ok(())
    }
}

/// What a catalog rewrite touched, for Settings confirmation text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpaceRewrite {
    pub(crate) repositories: usize,
    pub(crate) items: usize,
    /// Records that reference the catalog but could not be parsed: reported
    /// so a rename never looks complete while some record still holds the
    /// old name.
    pub(crate) skipped: usize,
}

/// Ensure the portable catalog exists and covers every space referenced by
/// portable records: seed Personal/Work into an empty catalog, then add any
/// legacy `group`/`pr_group` name (or a `space` written by a newer device)
/// that has no case-insensitive match. Existing entries are never removed or
/// reordered, so the call is safe to repeat at every startup.
pub(crate) fn ensure_spaces(root: &DataRoot) -> Result<Spaces> {
    let _gate = portable_gate(root, false)?;
    let _lock = repository_graph_lock(root, false)?;
    ensure_spaces_unlocked(root)
}

/// [`ensure_spaces`] for callers already holding the portable gate and the
/// repository graph lock. Never reacquires either.
pub(super) fn ensure_spaces_unlocked(root: &DataRoot) -> Result<Spaces> {
    let existed = Spaces::path(root).try_exists()?;
    let mut catalog = Spaces::load(root)?;
    let mut changed = false;
    if catalog.is_empty() {
        for name in DEFAULT_SEED {
            changed |= catalog.ensure_name(name);
        }
    }
    for name in referenced_names(root) {
        changed |= catalog.ensure_name(&name);
    }
    if changed || !existed {
        catalog
            .write(root)
            .context("seeding portable spaces catalog")?;
    }
    Ok(catalog)
}

/// Artifact JSON metadata from either representation: the current
/// `artifact.md` front matter first, legacy `artifact.json` otherwise.
fn read_artifact_metadata(dir: &std::path::Path) -> Option<Value> {
    let markdown = dir.join("artifact.md");
    if let Ok(bytes) = std::fs::read(&markdown)
        && let Ok(text) = std::str::from_utf8(&bytes)
        && let Some(rest) = text.strip_prefix("---\n")
        && let Some((metadata, _)) = rest.split_once("\n---\n")
        && let Ok(value) = serde_json::from_str(metadata)
    {
        return Some(value);
    }
    std::fs::read(dir.join("artifact.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Space names referenced by portable records, in stable order: repository
/// records (sorted by key), Home items (in order), then artifacts (sorted by
/// id). Reads raw JSON so it stays tolerant of records written by older and
/// newer builds alike.
fn referenced_names(root: &DataRoot) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |name: Option<String>| {
        if let Some(name) = name {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|seen: &String| space_eq(seen, name)) {
                names.push(name.to_owned());
            }
        }
    };
    let mut records = Vec::new();
    let repositories = root.portable_dir().join("repositories");
    if let Ok(entries) = std::fs::read_dir(&repositories) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path().join("repository.json");
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
            {
                records.push(record);
            }
        }
    }
    for record in &records {
        push(record.as_object().and_then(effective_space_value));
    }

    let mut items = Vec::new();
    if let Ok(bytes) = std::fs::read(root.portable_dir().join("dashboard.json"))
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(rows) = value.get("items").and_then(Value::as_array)
    {
        items = rows.clone();
    }
    for item in &items {
        push(item.as_object().and_then(effective_space_value));
    }

    let artifact_dir = root.portable_dir().join("artifacts");
    if let Ok(entries) = std::fs::read_dir(&artifact_dir) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            push(
                read_artifact_metadata(&entry.path())
                    .and_then(|record| record.as_object().and_then(effective_space_value)),
            );
        }
    }
    names
}

/// Add `name` to the portable catalog when missing, seeding the defaults
/// first on a fresh catalog. Callers already hold the portable gate.
/// Failures are ignored: the record's own value is the source of truth, and
/// the next [`ensure_spaces`] reconciles the catalog from the records.
pub(super) fn ensure_name_written(root: &DataRoot, name: &str) {
    let Ok(mut catalog) = Spaces::load(root) else {
        return;
    };
    let mut changed = false;
    if catalog.is_empty() {
        for seed in DEFAULT_SEED {
            changed |= catalog.ensure_name(seed);
        }
    }
    changed |= catalog.ensure_name(name);
    if changed {
        let _ = catalog.write(root);
    }
}

/// Legacy space keys, in the order they superseded one another.
const LEGACY_SPACE_KEYS: [&str; 2] = ["group", "pr_group"];

/// Effective space value of a raw record object, applying one precedence
/// rule everywhere (repository parsing, Home migration, catalog discovery,
/// and catalog rewrites): a non-blank `space` wins, else the first non-blank
/// legacy key. Null, blank, and non-string values all count as unset.
pub(super) fn effective_space_value(object: &serde_json::Map<String, Value>) -> Option<String> {
    let mut value = object
        .get("space")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    for key in LEGACY_SPACE_KEYS {
        if value.is_none() {
            value = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
        }
    }
    value
}

/// Migrate a raw record object onto `space`: a non-blank `space` wins, else
/// the first non-blank legacy key is promoted, else the key is dropped as
/// unset. Legacy keys are always removed. Shared by repository parsing and
/// Home loading so both agree with the catalog rewrite.
pub(super) fn normalize_space_keys(object: &mut serde_json::Map<String, Value>) {
    match effective_space_value(object) {
        Some(space) => {
            object.insert("space".to_owned(), Value::String(space));
        }
        None => {
            object.remove("space");
        }
    }
    for key in LEGACY_SPACE_KEYS {
        object.remove(key);
    }
}

/// Rewrite one record object's space: a matching `space` (or legacy key)
/// value is repointed at `to`, a non-matching legacy value is promoted to
/// `space` unchanged, and the legacy keys are always dropped so the rewrite
/// also completes the group→space migration. With `materialize_unset`, a
/// record with no space at all gains an explicit `to`: callers use this when
/// removing the catalog's first space, which is the implicit home of every
/// space-less record. Returns whether bytes changed.
fn rewrite_object_space(
    object: &mut serde_json::Map<String, Value>,
    from: &str,
    to: &str,
    materialize_unset: bool,
) -> bool {
    let mut changed = false;
    match effective_space_value(object) {
        Some(value) => {
            let replacement = if space_eq(&value, from) { to } else { &value };
            if object.get("space").and_then(Value::as_str) != Some(replacement) {
                object.insert("space".to_owned(), Value::String(replacement.to_owned()));
                changed = true;
            }
        }
        None if materialize_unset => {
            object.insert("space".to_owned(), Value::String(to.to_owned()));
            changed = true;
        }
        None => {}
    }
    for key in LEGACY_SPACE_KEYS {
        if object.remove(key).is_some() {
            changed = true;
        }
    }
    changed
}

/// One record rewrite staged before any file is touched: a mid-flight
/// failure can then put every already-replaced file back.
struct PendingRewrite {
    path: std::path::PathBuf,
    /// `None` when the file did not exist before the rewrite (rollback
    /// removes it instead of writing an empty file).
    original: Option<Vec<u8>>,
    replacement: Vec<u8>,
}

/// Pretty JSON plus trailing newline, matching [`super::write_json_atomic`].
fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(value).context("serializing rewritten record")?;
    text.push('\n');
    Ok(text.into_bytes())
}

/// Stage a JSON-object rewrite for `path`. `Ok(None)` covers missing files
/// and records that need no change; malformed JSON is an error the caller
/// counts as skipped.
fn stage_json_rewrite(
    path: &std::path::Path,
    rewrite: impl FnOnce(&mut serde_json::Map<String, Value>) -> bool,
) -> Result<Option<PendingRewrite>> {
    let bytes = match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        Ok(bytes) => bytes,
    };
    let mut record: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let Some(object) = record.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };
    if !rewrite(object) {
        return Ok(None);
    }
    Ok(Some(PendingRewrite {
        path: path.to_owned(),
        original: Some(bytes),
        replacement: json_bytes(&record)?,
    }))
}

/// Stage a space rewrite for one artifact record: current `artifact.md`
/// front matter first, legacy `artifact.json` otherwise. The Markdown body is
/// preserved verbatim; only the metadata block is re-serialized.
fn stage_artifact_rewrite(
    dir: &std::path::Path,
    from: &str,
    to: &str,
    materialize_unset: bool,
) -> Result<Option<PendingRewrite>> {
    let markdown = dir.join("artifact.md");
    if let Ok(bytes) = std::fs::read(&markdown) {
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("reading {}", markdown.display()))?;
        let Some(rest) = text.strip_prefix("---\n") else {
            bail!("{} has no artifact metadata", markdown.display());
        };
        let Some((metadata, content)) = rest.split_once("\n---\n") else {
            bail!("{} has unterminated artifact metadata", markdown.display());
        };
        let mut record: Value = serde_json::from_str(metadata)
            .with_context(|| format!("parsing {}", markdown.display()))?;
        let Some(object) = record.as_object_mut() else {
            bail!("{} metadata is not a JSON object", markdown.display());
        };
        if !rewrite_object_space(object, from, to, materialize_unset) {
            return Ok(None);
        }
        let replacement = format!(
            "---\n{}\n---\n{content}",
            serde_json::to_string_pretty(&record)?
        )
        .into_bytes();
        return Ok(Some(PendingRewrite {
            path: markdown,
            original: Some(bytes),
            replacement,
        }));
    }
    stage_json_rewrite(&dir.join("artifact.json"), |object| {
        rewrite_object_space(object, from, to, materialize_unset)
    })
}

/// Apply staged rewrites atomically per file, rolling back the ones already
/// written when a later file fails: a rename never half-applies because one
/// record was unwritable. The rollback itself is best effort, but failures
/// are reported so a partially restored tree is never mistaken for a clean
/// one.
fn commit_rewrites(pending: &[PendingRewrite]) -> Result<()> {
    let mut written: Vec<&PendingRewrite> = Vec::new();
    for item in pending {
        if let Err(error) = super::record::atomic_replace(&item.path, &item.replacement, || Ok(()))
        {
            let mut unrestored = 0;
            for done in written.iter().rev() {
                let restored = match &done.original {
                    Some(bytes) => super::record::atomic_replace(&done.path, bytes, || Ok(())),
                    None => std::fs::remove_file(&done.path).map_err(Into::into),
                };
                if restored.is_err() {
                    unrestored += 1;
                }
            }
            if unrestored > 0 {
                return Err(error.context(format!(
                    "rollback also failed for {unrestored} file(s); review the affected records before retrying"
                )));
            }
            return Err(error);
        }
        written.push(item);
    }
    Ok(())
}

/// Stage every repository, Home item, and artifact rewrite that references
/// `from` so it references `to`, dropping the legacy `group`/`pr_group` keys
/// on the way. `materialize_unset` also assigns `to` to records with no
/// stored space (see [`rewrite_object_space`]). Nothing is written here:
/// callers add any further files (the catalog) to the batch and commit it
/// with [`commit_rewrites`]. Malformed records are counted in
/// [`SpaceRewrite::skipped`] and reported instead of failing or being
/// half-rewritten.
fn stage_reference_rewrites(
    root: &DataRoot,
    from: &str,
    to: &str,
    materialize_unset: bool,
) -> (SpaceRewrite, Vec<PendingRewrite>) {
    let mut touched = SpaceRewrite::default();
    let mut pending = Vec::new();
    let mut stage = |result: Result<Option<PendingRewrite>>, counter: &mut usize| match result {
        Ok(Some(rewrite)) => {
            pending.push(rewrite);
            *counter += 1;
        }
        Ok(None) => {}
        Err(_) => touched.skipped += 1,
    };

    let repositories = root.portable_dir().join("repositories");
    if let Ok(entries) = std::fs::read_dir(&repositories) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path().join("repository.json");
            let result = stage_json_rewrite(&path, |object| {
                rewrite_object_space(object, from, to, materialize_unset)
            });
            let mut count = 0;
            stage(result, &mut count);
            touched.repositories += count;
        }
    }

    let dashboard = root.portable_dir().join("dashboard.json");
    let mut items = 0;
    stage(
        stage_json_rewrite(&dashboard, |object| {
            let mut changed = false;
            if let Some(rows) = object.get_mut("items").and_then(Value::as_array_mut) {
                for row in rows.iter_mut() {
                    if let Some(record) = row.as_object_mut()
                        && rewrite_object_space(record, from, to, materialize_unset)
                    {
                        items += 1;
                        changed = true;
                    }
                }
            }
            changed
        }),
        &mut 0,
    );
    touched.items = items;

    let artifacts = root.portable_dir().join("artifacts");
    if let Ok(entries) = std::fs::read_dir(&artifacts) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            stage(
                stage_artifact_rewrite(&entry.path(), from, to, materialize_unset),
                &mut 0,
            );
        }
    }

    (touched, pending)
}

/// Stage the catalog itself as the final file in a rewrite batch, so a
/// failure to replace it rolls the records back instead of leaving them
/// pointing at a name the catalog never adopted.
fn stage_catalog_rewrite(root: &DataRoot, catalog: &Spaces) -> Result<PendingRewrite> {
    let path = Spaces::path(root);
    Ok(PendingRewrite {
        path: path.clone(),
        original: std::fs::read(&path).ok(),
        replacement: catalog.encoded()?,
    })
}

/// Rename a space and every reference to it. Takes the exclusive portable
/// gate plus the graph lock so repository edits cannot interleave.
pub(crate) fn rename_space(root: &DataRoot, from: &str, to: &str) -> Result<SpaceRewrite> {
    let _gate = portable_gate(root, true)?;
    let _lock = repository_graph_lock(root, true)?;
    let mut catalog = Spaces::load(root)?;
    let canonical_from = catalog
        .canonical(from)
        .with_context(|| format!("Space {from:?} was removed. Reload spaces"))?;
    catalog.rename(&canonical_from, to)?;
    let canonical_to = catalog
        .canonical(to)
        .expect("rename installed the normalized target name");
    // Space-less records follow the catalog's first entry dynamically, so a
    // rename needs no materialization: the new name becomes first in place.
    let (touched, mut pending) =
        stage_reference_rewrites(root, &canonical_from, &canonical_to, false);
    pending.push(stage_catalog_rewrite(root, &catalog)?);
    commit_rewrites(&pending)?;
    retarget_active_space(root, &canonical_from, &canonical_to);
    Ok(touched)
}

/// Delete a space after moving every reference to `destination`. Refuses the
/// last remaining space. Removing the catalog's first space also assigns its
/// implicit members (records with no stored space) to the destination.
pub(crate) fn delete_space(root: &DataRoot, name: &str, destination: &str) -> Result<SpaceRewrite> {
    let _gate = portable_gate(root, true)?;
    let _lock = repository_graph_lock(root, true)?;
    let mut catalog = Spaces::load(root)?;
    let canonical = catalog
        .canonical(name)
        .with_context(|| format!("Space {name:?} was removed. Reload spaces"))?;
    let destination = catalog
        .canonical(destination)
        .with_context(|| format!("Space {destination:?} was removed. Reload spaces"))?;
    ensure!(
        !space_eq(&canonical, &destination),
        "Choose a different destination space"
    );
    let removing_default = catalog
        .first()
        .is_some_and(|first| space_eq(first, &canonical));
    let (touched, mut pending) =
        stage_reference_rewrites(root, &canonical, &destination, removing_default);
    catalog.remove(&canonical)?;
    pending.push(stage_catalog_rewrite(root, &catalog)?);
    commit_rewrites(&pending)?;
    retarget_active_space(root, &canonical, &destination);
    Ok(touched)
}

/// Point the machine-local active space at the replacement when the catalog
/// edit removed the name it currently holds. Best effort: a device-state
/// failure must not undo an otherwise complete catalog rewrite.
fn retarget_active_space(root: &DataRoot, from: &str, to: &str) {
    let store = super::DeviceStore::new(root);
    let Ok(state) = store.load() else {
        return;
    };
    if state
        .active_space
        .as_deref()
        .is_some_and(|active| space_eq(active, from))
    {
        let _ = store.update(|state| state.active_space = Some(to.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{DeviceStore, write_json_atomic};
    use serde_json::json;

    fn root() -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        crate::data::ensure_dirs(root.root()).unwrap();
        (dir, root)
    }

    #[test]
    fn names_normalize_whitespace_and_reject_blanks() {
        assert_eq!(normalize_name("  Work  ").unwrap(), "Work");
        assert_eq!(normalize_name("client  a").unwrap(), "client a");
        assert_eq!(normalize_name("bad\nname").unwrap(), "bad name");
        assert!(normalize_name("   ").is_err());
        assert!(normalize_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(normalize_name("bad\u{7}name").is_err());
    }

    #[test]
    fn missing_catalog_loads_empty_and_saves_with_format_version() {
        let (_dir, root) = root();
        let catalog = Spaces::load(&root).unwrap();
        assert!(catalog.is_empty());

        let mut seeded = catalog.clone();
        assert!(seeded.ensure_name("Personal"));
        assert!(seeded.ensure_name("Work"));
        seeded.save(&root, &catalog).unwrap();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(Spaces::path(&root)).unwrap()).unwrap();
        assert_eq!(raw["formatVersion"], 1);
        assert_eq!(raw["spaces"][0]["name"], "Personal");
    }

    #[test]
    fn save_refuses_a_stale_catalog() {
        let (_dir, root) = root();
        let mut first = Spaces::default();
        first.ensure_name("Personal");
        let stale = first.clone();
        first.save(&root, &Spaces::default()).unwrap();
        let mut other = Spaces::default();
        other.ensure_name("Work");
        assert!(other.save(&root, &stale).is_err());
    }

    #[test]
    fn canonical_resolution_is_case_insensitive_and_first_space_is_the_default() {
        let mut catalog = Spaces::default();
        catalog.ensure_name("Personal");
        catalog.ensure_name("Client A");
        assert_eq!(catalog.canonical("client a").as_deref(), Some("Client A"));
        assert_eq!(catalog.resolve(Some("CLIENT A")), "Client A");
        assert_eq!(catalog.resolve(Some("unknown")), "unknown");
        assert_eq!(catalog.resolve(None), "Personal");
        assert_eq!(Spaces::default().resolve(None), DEFAULT_SPACE);
    }

    #[test]
    fn space_matches_defaults_blank_values() {
        assert!(space_matches(Some("work"), "Work"));
        assert!(space_matches(None, "personal"));
        assert!(space_matches(Some("  "), "Personal"));
        assert!(!space_matches(Some("Work"), "Personal"));
    }

    #[test]
    fn rename_and_remove_keep_the_catalog_consistent() {
        let mut catalog = Spaces::default();
        catalog.ensure_name("Personal");
        catalog.ensure_name("Work");
        catalog.rename("personal", "Home").unwrap();
        assert_eq!(catalog.names(), vec!["Home", "Work"]);
        assert!(catalog.rename("Home", "Work").is_err());
        catalog.remove("Work").unwrap();
        assert_eq!(catalog.names(), vec!["Home"]);
        assert!(catalog.remove("Home").is_err(), "last space must stay");
    }

    #[test]
    fn ensure_seeds_defaults_plus_legacy_group_names_from_records() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "group": "Client A"}),
        )
        .unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/site/repository.json"),
            &json!({"key": "site", "space": "Personal"}),
        )
        .unwrap();
        write_json_atomic(
            &root.portable_dir().join("dashboard.json"),
            &json!({"items": [
                {"id": "1", "kind": "Todo", "title": "t", "pr_group": "Work"},
                {"id": "2", "kind": "Todo", "title": "u", "group": "Client A"},
            ]}),
        )
        .unwrap();

        let catalog = ensure_spaces(&root).unwrap();
        assert_eq!(catalog.names(), vec!["Personal", "Work", "Client A"]);

        // Idempotent: a second run neither duplicates nor rewrites.
        let before = std::fs::read_to_string(Spaces::path(&root)).unwrap();
        let again = ensure_spaces(&root).unwrap();
        assert_eq!(again.names(), catalog.names());
        assert_eq!(
            std::fs::read_to_string(Spaces::path(&root)).unwrap(),
            before
        );
    }

    #[test]
    fn ensure_creates_the_catalog_even_without_records() {
        let (_dir, root) = root();
        let catalog = ensure_spaces(&root).unwrap();
        assert_eq!(catalog.names(), vec!["Personal", "Work"]);
    }

    #[test]
    fn rename_rewrites_repository_and_dashboard_references() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "group": "Work", "description": "kept"}),
        )
        .unwrap();
        write_json_atomic(
            &root.portable_dir().join("dashboard.json"),
            &json!({"items": [
                {"id": "1", "kind": "PullRequest", "title": "p", "group": "Work"},
                {"id": "2", "kind": "Todo", "title": "t", "space": "Personal"},
            ]}),
        )
        .unwrap();
        // The catalog is seeded from those records first.
        ensure_spaces(&root).unwrap();

        let touched = rename_space(&root, "work", "Office").unwrap();
        assert_eq!(touched.repositories, 1);
        assert_eq!(touched.items, 1);
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/acme/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(repo["space"], "Office");
        assert!(repo.get("group").is_none(), "legacy key dropped");
        assert_eq!(repo["description"], "kept");
        let dashboard: Value = serde_json::from_str(
            &std::fs::read_to_string(root.portable_dir().join("dashboard.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(dashboard["items"][0]["space"], "Office");
        assert_eq!(dashboard["items"][1]["space"], "Personal");
    }

    #[test]
    fn rename_updates_the_machine_local_active_space() {
        let (_dir, root) = root();
        ensure_spaces(&root).unwrap();
        let store = DeviceStore::new(&root);
        store
            .update(|state| state.active_space = Some("Work".to_owned()))
            .unwrap();
        rename_space(&root, "Work", "Office").unwrap();
        assert_eq!(
            store.load().unwrap().active_space.as_deref(),
            Some("Office")
        );
    }

    #[test]
    fn rename_keeps_the_active_space_untouched_when_it_does_not_match() {
        let (_dir, root) = root();
        ensure_spaces(&root).unwrap();
        let store = DeviceStore::new(&root);
        store
            .update(|state| state.active_space = Some("Personal".to_owned()))
            .unwrap();
        rename_space(&root, "Work", "Office").unwrap();
        assert_eq!(
            store.load().unwrap().active_space.as_deref(),
            Some("Personal")
        );
    }

    #[test]
    fn delete_moves_references_to_the_destination() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "space": "Work"}),
        )
        .unwrap();
        ensure_spaces(&root).unwrap();

        let touched = delete_space(&root, "Work", "personal").unwrap();
        assert_eq!(touched.repositories, 1);
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/acme/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(repo["space"], "Personal");
        assert_eq!(Spaces::load(&root).unwrap().names(), vec!["Personal"]);
        assert!(delete_space(&root, "Personal", "Personal").is_err());
    }

    #[test]
    fn deleting_the_first_space_materializes_its_implicit_members() {
        let (_dir, root) = root();
        // A record with no space at all resolves to the catalog's first
        // space, so deleting that space must move it explicitly.
        std::fs::create_dir_all(root.portable_dir().join("repositories/plain")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/plain/repository.json"),
            &json!({"key": "plain"}),
        )
        .unwrap();
        write_json_atomic(
            &root.portable_dir().join("dashboard.json"),
            &json!({"items": [{"id": "1", "kind": "Todo", "title": "t"}]}),
        )
        .unwrap();
        ensure_spaces(&root).unwrap();

        let touched = delete_space(&root, "Personal", "Work").unwrap();
        assert_eq!(touched.repositories, 1);
        assert_eq!(touched.items, 1);
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/plain/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(repo["space"], "Work");
        let dashboard: Value = serde_json::from_str(
            &std::fs::read_to_string(root.portable_dir().join("dashboard.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(dashboard["items"][0]["space"], "Work");
    }

    #[test]
    fn catalog_discovery_uses_effective_space_values() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        // `space: null` counts as unset, so the legacy group names the space
        // the record actually loads into; discovery must offer it.
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "space": null, "group": "Client A"}),
        )
        .unwrap();
        let catalog = ensure_spaces(&root).unwrap();
        assert!(catalog.contains("Client A"));
    }

    #[test]
    fn equivalent_catalog_names_dedupe_on_load() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        std::fs::write(
            Spaces::path(&root),
            serde_json::to_string_pretty(&json!({
                "formatVersion": 1,
                "spaces": [{"name": "Client A"}, {"name": "Client  A"}, {"name": "  "}],
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = Spaces::load(&root).unwrap();
        assert_eq!(catalog.names(), vec!["Client A"]);

        // The dedupe rule is exactly `space_eq`'s: non-ASCII case variants
        // stay distinct, so they are never silently merged or dropped.
        std::fs::write(
            Spaces::path(&root),
            serde_json::to_string_pretty(&json!({
                "formatVersion": 1,
                "spaces": [{"name": "Équipe"}, {"name": "équipe"}],
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = Spaces::load(&root).unwrap();
        assert_eq!(catalog.names(), vec!["Équipe", "équipe"]);
        assert!(catalog.contains("Équipe"));
        assert!(catalog.contains("équipe"));
        assert_eq!(catalog.resolve(Some("équipe")), "équipe");
    }

    #[test]
    fn non_ascii_spaces_round_trip_through_ensure() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "space": "Équipe"}),
        )
        .unwrap();
        let first = ensure_spaces(&root).unwrap();
        assert!(first.contains("Équipe"));
        let before = std::fs::read_to_string(Spaces::path(&root)).unwrap();
        let again = ensure_spaces(&root).unwrap();
        assert_eq!(again.names(), first.names());
        assert_eq!(
            std::fs::read_to_string(Spaces::path(&root)).unwrap(),
            before,
            "ensure must be idempotent for non-ASCII names"
        );
    }

    #[test]
    fn blank_space_with_a_legacy_group_loads_and_renames_consistently() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        write_json_atomic(
            &root.portable_dir().join("dashboard.json"),
            &json!({"items": [{"id": "1", "kind": "Todo", "title": "t", "space": "", "group": "Work"}]}),
        )
        .unwrap();
        ensure_spaces(&root).unwrap();
        // Loading already applies the shared precedence.
        let loaded = crate::data::dashboard::Dashboard::load(&root).unwrap();
        assert_eq!(loaded.items[0].space, "Work");

        // Renaming the effective space moves the item; it does not silently
        // land in the first space.
        rename_space(&root, "Work", "Office").unwrap();
        let renamed = crate::data::dashboard::Dashboard::load(&root).unwrap();
        assert_eq!(renamed.items[0].space, "Office");
    }

    #[test]
    fn deleting_the_first_space_materializes_blank_members() {
        let (_dir, root) = root();
        // Blank values count as unset exactly like a missing key, so the
        // implicit members must follow the destination.
        std::fs::create_dir_all(root.portable_dir().join("repositories/blank")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/blank/repository.json"),
            &json!({"key": "blank", "space": "  "}),
        )
        .unwrap();
        write_json_atomic(
            &root.portable_dir().join("dashboard.json"),
            &json!({"items": [{"id": "1", "kind": "Todo", "title": "t", "space": ""}]}),
        )
        .unwrap();
        ensure_spaces(&root).unwrap();

        delete_space(&root, "Personal", "Work").unwrap();
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/blank/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(repo["space"], "Work");
        let dashboard: Value = serde_json::from_str(
            &std::fs::read_to_string(root.portable_dir().join("dashboard.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(dashboard["items"][0]["space"], "Work");
    }

    #[test]
    fn deleting_into_a_later_space_still_moves_the_implicit_members() {
        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/plain")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/plain/repository.json"),
            &json!({"key": "plain"}),
        )
        .unwrap();
        // Catalog [Personal, Work, Client]: the implicit members of Personal
        // must go to Client, not to the space that becomes first.
        let mut catalog = Spaces::load(&root).unwrap();
        catalog.ensure_name("Personal");
        catalog.ensure_name("Work");
        catalog.ensure_name("Client");
        catalog.save(&root, &Spaces::load(&root).unwrap()).unwrap();

        delete_space(&root, "Personal", "Client").unwrap();
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/plain/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(repo["space"], "Client");
        assert_eq!(Spaces::load(&root).unwrap().names(), vec!["Work", "Client"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_catalog_write_rolls_back_the_records() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, root) = root();
        std::fs::create_dir_all(root.portable_dir().join("repositories/acme")).unwrap();
        write_json_atomic(
            &root
                .portable_dir()
                .join("repositories/acme/repository.json"),
            &json!({"key": "acme", "space": "Work"}),
        )
        .unwrap();
        ensure_spaces(&root).unwrap();
        let catalog_before = std::fs::read_to_string(Spaces::path(&root)).unwrap();
        // Repository records stay writable; only the catalog replacement
        // (a sibling of `portable/`) fails.
        std::fs::set_permissions(root.portable_dir(), std::fs::Permissions::from_mode(0o555))
            .unwrap();

        let result = rename_space(&root, "Work", "Office");
        std::fs::set_permissions(root.portable_dir(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(result.is_err(), "the catalog write must surface");
        let repo: Value = serde_json::from_str(
            &std::fs::read_to_string(
                root.portable_dir()
                    .join("repositories/acme/repository.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            repo["space"], "Work",
            "records must be rolled back when the catalog cannot be written"
        );
        assert_eq!(
            std::fs::read_to_string(Spaces::path(&root)).unwrap(),
            catalog_before
        );
    }

    #[test]
    fn rename_rewrites_artifact_front_matter_and_preserves_the_body() {
        let (_dir, root) = root();
        let dir = root.portable_dir().join("artifacts/art-abc23456");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("artifact.md"),
            "---\n{\n  \"id\": \"art-abc23456\",\n  \"space\": \"Work\",\n  \"repository\": \"acme\"\n}\n---\n# Body\n\ntext\n",
        )
        .unwrap();
        ensure_spaces(&root).unwrap();
        assert!(Spaces::load(&root).unwrap().contains("Work"));

        rename_space(&root, "Work", "Office").unwrap();
        let text = std::fs::read_to_string(dir.join("artifact.md")).unwrap();
        assert!(text.contains("\"space\": \"Office\""), "{text}");
        assert!(text.ends_with("# Body\n\ntext\n"), "{text}");
    }

    #[test]
    fn malformed_records_are_reported_not_rewritten() {
        let (_dir, root) = root();
        let dir = root.portable_dir().join("repositories/broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("repository.json"), "{not json").unwrap();
        ensure_spaces(&root).unwrap();

        let touched = rename_space(&root, "Work", "Office").unwrap();
        assert_eq!(touched.skipped, 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("repository.json")).unwrap(),
            "{not json",
            "a malformed record is never rewritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_rolls_back_the_whole_rewrite() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, root) = root();
        for key in ["a", "b"] {
            let dir = root.portable_dir().join("repositories").join(key);
            std::fs::create_dir_all(&dir).unwrap();
            write_json_atomic(
                &dir.join("repository.json"),
                &json!({"key": key, "space": "Work"}),
            )
            .unwrap();
        }
        ensure_spaces(&root).unwrap();
        // Make one record unwritable: its temp-sibling write fails.
        let locked = root.portable_dir().join("repositories/b");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = rename_space(&root, "Work", "Office");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "the rewrite must surface the failed write");
        for key in ["a", "b"] {
            let record: Value = serde_json::from_str(
                &std::fs::read_to_string(
                    root.portable_dir()
                        .join("repositories")
                        .join(key)
                        .join("repository.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                record["space"], "Work",
                "{key} must be rolled back, not half-rewritten"
            );
        }
        assert_eq!(
            Spaces::load(&root).unwrap().names(),
            vec!["Personal", "Work"]
        );
    }
}
