//! Standalone Markdown artifacts. One JSON record is the atomic commit unit;
//! see docs/task-storage.md for representation and interrupted-write behavior.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::DataRoot;
use super::store_lock::{artifact_lock, ensure_directory, portable_gate, reject_symlink};
use super::tasks::{MAX_BYTES, atomic_replace, nonblank, random_id, read_bounded, timestamp};

const SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Rfc,
    Plan,
    Note,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artifact {
    schema_version: u32,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: Kind,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) archived: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Artifact {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported artifact schema {}",
            self.schema_version
        );
        validate_id(&self.id)?;
        nonblank(&self.title, "artifact title")?;
        ensure!(
            self.updated_at >= self.created_at,
            "updatedAt precedes createdAt"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NewArtifact {
    pub(crate) title: String,
    pub(crate) kind: Kind,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ArtifactPatch {
    pub(crate) title: Option<String>,
    pub(crate) kind: Option<Kind>,
    pub(crate) content: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) artifact: Artifact,
    pub(crate) revision: String,
}

#[derive(Debug, Default)]
pub(crate) struct ListOptions {
    pub(crate) include_archived: bool,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct ArtifactList {
    pub(crate) artifacts: Vec<Snapshot>,
    pub(crate) errors: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    root: DataRoot,
}

impl ArtifactStore {
    pub(crate) fn new(root: &DataRoot) -> Self {
        Self { root: root.clone() }
    }

    pub(crate) fn create(&self, input: NewArtifact) -> Result<Snapshot> {
        self.create_with_ids(input, || random_id().replacen("task-", "art-", 1))
    }

    fn create_with_ids(
        &self,
        input: NewArtifact,
        mut generate: impl FnMut() -> String,
    ) -> Result<Snapshot> {
        let _gate = portable_gate(&self.root, false)?;
        ensure_directory(&self.root.portable_dir())?;
        ensure_directory(&self.checked_path(None)?)?;
        for _ in 0..64 {
            let id = generate();
            validate_id(&id)?;
            let _record = artifact_lock(&self.root, &id, true)?;
            let dir = self.checked_path(Some(&id))?;
            if fs::symlink_metadata(&dir).is_ok() {
                continue;
            }
            let now = timestamp()?;
            let artifact = Artifact {
                schema_version: SCHEMA_VERSION,
                id,
                title: input.title.trim().to_owned(),
                kind: input.kind,
                content: input.content.clone(),
                archived: false,
                created_at: now,
                updated_at: now,
                extra: BTreeMap::new(),
            };
            artifact.validate()?;
            match fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("creating artifact directory"),
            }
            let result = self.write(&artifact, || Ok(()));
            if result.is_err() {
                let _ = fs::remove_dir(&dir);
            }
            return result;
        }
        bail!("could not allocate a unique artifact ID after 64 attempts")
    }

    pub(crate) fn get(&self, id: &str) -> Result<Snapshot> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        self.get_under_gate(id)
    }

    /// Caller holds the portable gate. Task -> artifact is the only nested
    /// record-lock order; artifact operations never acquire task locks.
    pub(super) fn get_under_gate(&self, id: &str) -> Result<Snapshot> {
        validate_id(id)?;
        let _record = artifact_lock(&self.root, id, false)?;
        self.read(id)
    }

    pub(crate) fn list(&self, options: &ListOptions) -> Result<ArtifactList> {
        let _gate = portable_gate(&self.root, false)?;
        let entries = match fs::read_dir(self.checked_path(None)?) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ArtifactList::default());
            }
            Err(e) => return Err(e).context("listing artifacts"),
        };
        let mut result = ArtifactList::default();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    result.errors.push(e.to_string());
                    continue;
                }
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            match self.get_under_gate(&id) {
                Ok(snapshot) if options.include_archived || !snapshot.artifact.archived => {
                    result.artifacts.push(snapshot)
                }
                Ok(_) => {}
                Err(e) => result
                    .errors
                    .push(format!("{}: {e:#}", entry.path().display())),
            }
        }
        result.artifacts.sort_by(|a, b| {
            b.artifact
                .updated_at
                .cmp(&a.artifact.updated_at)
                .then_with(|| a.artifact.id.cmp(&b.artifact.id))
        });
        result.errors.sort();
        let limit = options.limit.unwrap_or(50);
        result.truncated = result.artifacts.len() > limit;
        result.artifacts.truncate(limit);
        Ok(result)
    }

    pub(crate) fn update(
        &self,
        id: &str,
        revision: &str,
        patch: ArtifactPatch,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |artifact| {
            if let Some(v) = patch.title {
                artifact.title = v.trim().to_owned();
            }
            if let Some(v) = patch.kind {
                artifact.kind = v;
            }
            if let Some(v) = patch.content {
                artifact.content = v;
            }
        })
    }

    pub(crate) fn set_archived(
        &self,
        id: &str,
        revision: &str,
        archived: bool,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |artifact| artifact.archived = archived)
    }

    fn mutate(
        &self,
        id: &str,
        revision: &str,
        change: impl FnOnce(&mut Artifact),
    ) -> Result<Snapshot> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        let _record = artifact_lock(&self.root, id, true)?;
        let old = self.read(id)?;
        ensure!(
            old.revision == revision,
            "artifact_changed: {id} changed on disk; read the latest version and retry"
        );
        let mut artifact = old.artifact;
        change(&mut artifact);
        artifact.updated_at = timestamp()?.max(
            artifact
                .updated_at
                .checked_add(1)
                .context("artifact timestamp exhausted")?,
        );
        artifact.validate()?;
        self.write(&artifact, || Ok(()))
    }

    fn checked_path(&self, id: Option<&str>) -> Result<PathBuf> {
        let mut path = self.root.portable_dir();
        reject_symlink(&path)?;
        path.push("artifacts");
        reject_symlink(&path)?;
        if let Some(id) = id {
            validate_id(id)?;
            path.push(id);
            reject_symlink(&path)?;
        }
        Ok(path)
    }

    fn read(&self, id: &str) -> Result<Snapshot> {
        let path = self.checked_path(Some(id))?.join("artifact.json");
        // Never silently adopt an old two-file representation.
        let split_content = self.checked_path(Some(id))?.join("content.md");
        reject_symlink(&split_content)?;
        ensure!(
            !split_content.try_exists()?,
            "unsupported split artifact representation for {id}; record was not modified"
        );
        let bytes =
            read_bounded(&path).with_context(|| format!("missing or unreadable artifact {id}"))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        ensure!(
            value.get("schemaVersion").and_then(Value::as_u64) == Some(SCHEMA_VERSION.into()),
            "unsupported artifact schema in {}; record was not modified",
            path.display()
        );
        let artifact: Artifact = serde_json::from_value(value)
            .with_context(|| format!("decoding {}", path.display()))?;
        artifact.validate()?;
        ensure!(
            artifact.id == id,
            "artifact ID does not match directory {id}"
        );
        snapshot(artifact, &bytes)
    }

    fn write(
        &self,
        artifact: &Artifact,
        before_rename: impl FnOnce() -> Result<()>,
    ) -> Result<Snapshot> {
        let mut bytes = serde_json::to_vec_pretty(artifact)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "artifact exceeds {MAX_BYTES} byte limit"
        );
        let snapshot = snapshot(artifact.clone(), &bytes)?;
        atomic_replace(
            &self.checked_path(Some(&artifact.id))?.join("artifact.json"),
            &bytes,
            before_rename,
        )?;
        Ok(snapshot)
    }
}

fn snapshot(artifact: Artifact, bytes: &[u8]) -> Result<Snapshot> {
    Ok(Snapshot {
        artifact,
        revision: format!(
            "git-blob-sha1:{}",
            gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)?
        ),
    })
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.strip_prefix("art-")
            .is_some_and(|suffix| suffix.len() == 8
                && suffix
                    .bytes()
                    .all(|b| b"23456789abcdefghjkmnpqrstuvwxyz".contains(&b))),
        "invalid or unsupported artifact ID {id:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
