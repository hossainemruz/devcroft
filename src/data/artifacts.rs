//! Repository Markdown artifacts. One Markdown file is the atomic commit unit;
//! see docs/resources.md for representation and interrupted-write behavior.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::DataRoot;
use super::record::{MAX_BYTES, atomic_replace, nonblank, random_id, read_bounded, timestamp};
use super::store_lock::{artifact_lock, ensure_directory, portable_gate, reject_symlink};

pub(crate) mod anchors;
pub(crate) use anchors::CommentAnchor;

const SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Rfc,
    Plan,
    Note,
    Review,
}

impl Kind {
    pub(crate) const ALL: [Self; 4] = [Self::Rfc, Self::Plan, Self::Note, Self::Review];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Rfc => "RFC",
            Self::Plan => "Plan",
            Self::Note => "Note",
            Self::Review => "Review",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginSession {
    pub repository: String,
    pub key: crate::agent_sessions::SessionKey,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Comment {
    pub id: String,
    pub body: String,
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<CommentAnchor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artifact {
    schema_version: u32,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: Kind,
    #[serde(default)]
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) repository: Option<String>,
    #[serde(default)]
    pub(crate) sessions: Vec<OriginSession>,
    #[serde(default)]
    pub(crate) comments: Vec<Comment>,
    #[serde(default)]
    pub(crate) archived: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Artifact {
    fn relocate_comments(&mut self) {
        if !self.comments.iter().any(|c| c.anchor.is_some()) {
            return;
        }
        let blocks = anchors::blocks(&self.content);
        for comment in &mut self.comments {
            if let Some(anchor) = &mut comment.anchor {
                anchor.relocate(&self.content, &blocks);
            }
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported artifact schema {}",
            self.schema_version
        );
        validate_id(&self.id)?;
        nonblank(&self.title, "artifact title")?;
        if let Some(key) = &self.repository {
            super::require_repository_key(key)?;
        }
        for origin in &self.sessions {
            super::require_repository_key(&origin.repository)?;
            nonblank(&origin.key.provider, "session provider")?;
            nonblank(&origin.key.id, "session ID")?;
        }
        let mut ids = std::collections::BTreeSet::new();
        for comment in &self.comments {
            nonblank(&comment.id, "comment ID")?;
            nonblank(&comment.body, "comment body")?;
            ensure!(ids.insert(&comment.id), "duplicate comment ID");
            if let Some(anchor) = &comment.anchor {
                anchor.validate()?;
            }
        }
        ensure!(
            self.updated_at >= self.created_at,
            "updatedAt precedes createdAt"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NewArtifact {
    pub(crate) repository: Option<String>,
    pub(crate) sessions: Vec<OriginSession>,
    pub(crate) title: String,
    pub(crate) kind: Kind,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ArtifactPatch {
    pub(crate) repository: Option<String>,
    pub(crate) sessions: Option<Vec<OriginSession>>,
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
    pub(crate) repository: Option<String>,
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
        self.create_with_ids(input, random_id)
    }

    fn create_with_ids(
        &self,
        input: NewArtifact,
        mut generate: impl FnMut() -> String,
    ) -> Result<Snapshot> {
        let _gate = portable_gate(&self.root, false)?;
        self.require_repository(
            input
                .repository
                .as_deref()
                .context("artifact repository is required")?,
        )?;
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
                repository: input.repository.clone(),
                sessions: input.sessions.clone(),
                comments: Vec::new(),
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

    /// Caller holds the portable gate before acquiring the artifact lock.
    pub(super) fn get_under_gate(&self, id: &str) -> Result<Snapshot> {
        validate_id(id)?;
        let _record = artifact_lock(&self.root, id, false)?;
        self.read(id)
    }

    pub(crate) fn list(&self, options: &ListOptions) -> Result<ArtifactList> {
        if let Some(key) = &options.repository {
            super::require_repository_key(key)?;
        }
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
                Ok(snapshot)
                    if (options.include_archived || !snapshot.artifact.archived)
                        && options.repository.as_ref().is_none_or(|key| {
                            snapshot.artifact.repository.as_ref() == Some(key)
                        }) =>
                {
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
            if let Some(v) = patch.repository {
                artifact.repository = Some(v);
            }
            if let Some(v) = patch.sessions {
                artifact.sessions = v;
            }
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

    pub(crate) fn delete(&self, id: &str, revision: &str) -> Result<()> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        let _record = artifact_lock(&self.root, id, true)?;
        let old = self.read(id)?;
        ensure!(
            old.revision == revision,
            "artifact_changed: {id} changed on disk; read the latest version and retry"
        );
        let dir = self.checked_path(Some(id))?;
        if dir.try_exists()? {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("deleting artifact directory {id}"))?;
        }
        Ok(())
    }

    pub(crate) fn comment(
        &self,
        id: &str,
        revision: &str,
        change: CommentChange,
    ) -> Result<Snapshot> {
        // Validate before entering the mutation so errors never commit partial changes.
        if let CommentChange::Create(body)
        | CommentChange::CreateBlock { body, .. }
        | CommentChange::CreateSelection { body, .. }
        | CommentChange::Edit(_, body) = &change
        {
            nonblank(body, "comment body")?;
        }
        self.mutate_result(id, revision, |artifact| {
            match change {
                CommentChange::Create(body) => artifact.comments.push(Comment {
                    id: random_id().replacen("art-", "comment-", 1),
                    body,
                    resolved: false,
                    anchor: None,
                }),
                CommentChange::CreateBlock { body, block, quote } => {
                    let blocks = anchors::blocks(&artifact.content);
                    let block = blocks.get(block).context("comment block not found")?;
                    ensure!(!block.definition, "cannot comment on a link definition");
                    artifact.comments.push(Comment {
                        id: random_id().replacen("art-", "comment-", 1),
                        body,
                        resolved: false,
                        anchor: Some(CommentAnchor::new(&artifact.content, block, quote)),
                    });
                }
                CommentChange::CreateSelection { body, range, quote } => {
                    let anchor = CommentAnchor::selection(&artifact.content, range, quote)?;
                    artifact.comments.push(Comment {
                        id: random_id().replacen("art-", "comment-", 1),
                        body,
                        resolved: false,
                        anchor: Some(anchor),
                    });
                }
                CommentChange::Edit(id, body) => {
                    artifact
                        .comments
                        .iter_mut()
                        .find(|c| c.id == id)
                        .context("comment not found")?
                        .body = body
                }
                CommentChange::Resolve(id, resolved) => {
                    artifact
                        .comments
                        .iter_mut()
                        .find(|c| c.id == id)
                        .context("comment not found")?
                        .resolved = resolved
                }
                CommentChange::Delete(id) => {
                    let index = artifact
                        .comments
                        .iter()
                        .position(|c| c.id == id)
                        .context("comment not found")?;
                    artifact.comments.remove(index);
                }
            }
            Ok(())
        })
    }

    fn mutate(
        &self,
        id: &str,
        revision: &str,
        change: impl FnOnce(&mut Artifact),
    ) -> Result<Snapshot> {
        self.mutate_result(id, revision, |artifact| {
            change(artifact);
            Ok(())
        })
    }

    fn mutate_result(
        &self,
        id: &str,
        revision: &str,
        change: impl FnOnce(&mut Artifact) -> Result<()>,
    ) -> Result<Snapshot> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        let _record = artifact_lock(&self.root, id, true)?;
        let old = self.read(id)?;
        ensure!(
            old.revision == revision,
            "artifact_changed: {id} changed on disk; read the latest version and retry"
        );
        let old_repository = old.artifact.repository.clone();
        let mut artifact = old.artifact;
        change(&mut artifact)?;
        artifact.relocate_comments();
        if artifact.repository != old_repository
            && let Some(key) = &artifact.repository
        {
            self.require_repository(key)?;
        }
        artifact.updated_at = timestamp()?.max(
            artifact
                .updated_at
                .checked_add(1)
                .context("artifact timestamp exhausted")?,
        );
        artifact.validate()?;
        self.write(&artifact, || Ok(()))
    }

    fn require_repository(&self, key: &str) -> Result<()> {
        super::require_repository_key(key)?;
        let base = self.root.portable_dir().join("repositories");
        reject_symlink(&self.root.portable_dir())?;
        reject_symlink(&base)?;
        let dir = base.join(key);
        reject_symlink(&dir)?;
        let bytes = read_bounded(&dir.join("repository.json"))
            .with_context(|| format!("unknown or unreadable repository {key}"))?;
        serde_json::from_slice::<super::RepositoryMetadata>(&bytes)?;
        Ok(())
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
        let dir = self.checked_path(Some(id))?;
        let path = dir.join("artifact.md");
        reject_symlink(&path)?;
        let (bytes, mut artifact) = if path.try_exists()? {
            let bytes = read_bounded(&path)?;
            let text = std::str::from_utf8(&bytes)?;
            let rest = text
                .strip_prefix("---\n")
                .context("missing artifact metadata")?;
            let (metadata, content) = rest
                .split_once("\n---\n")
                .context("unterminated artifact metadata")?;
            let mut artifact: Artifact =
                serde_json::from_str(metadata).context("parsing artifact metadata")?;
            artifact.content = content.to_owned();
            (bytes, artifact)
        } else {
            // Read legacy artifacts without modifying them. The first explicit edit
            // writes Markdown; the original JSON remains available for recovery.
            let split = dir.join("content.md");
            reject_symlink(&split)?;
            ensure!(
                !split.try_exists()?,
                "unsupported split artifact representation; record was not modified"
            );
            let bytes = read_bounded(&dir.join("artifact.json"))?;
            let mut artifact: Artifact = serde_json::from_slice(&bytes)?;
            ensure!(
                artifact.schema_version == 3,
                "unsupported legacy artifact schema"
            );
            artifact.schema_version = SCHEMA_VERSION;
            (bytes, artifact)
        };
        artifact.validate()?;
        ensure!(
            artifact.id == id,
            "artifact ID does not match directory {id}"
        );
        artifact.relocate_comments();
        snapshot(artifact, &bytes)
    }

    fn write(
        &self,
        artifact: &Artifact,
        before_rename: impl FnOnce() -> Result<()>,
    ) -> Result<Snapshot> {
        let mut metadata = serde_json::to_value(artifact)?;
        metadata.as_object_mut().unwrap().remove("content");
        let bytes = format!(
            "---\n{}\n---\n{}",
            serde_json::to_string_pretty(&metadata)?,
            artifact.content
        )
        .into_bytes();
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "artifact exceeds {MAX_BYTES} byte limit"
        );
        let snapshot = snapshot(artifact.clone(), &bytes)?;
        atomic_replace(
            &self.checked_path(Some(&artifact.id))?.join("artifact.md"),
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

pub(crate) fn validate_id(id: &str) -> Result<()> {
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

pub(crate) enum CommentChange {
    Create(String),
    CreateBlock {
        body: String,
        block: usize,
        quote: Option<String>,
    },
    CreateSelection {
        body: String,
        range: std::ops::Range<usize>,
        quote: Option<String>,
    },
    Edit(String, String),
    Resolve(String, bool),
    Delete(String),
}

#[cfg(test)]
mod tests;
