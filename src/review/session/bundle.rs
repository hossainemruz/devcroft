use super::Capture;
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Claim {
    pub id: String,
    pub text: String,
    pub evidence_ids: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Chapter {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub document: String,
    pub evidence_ids: Vec<String>,
    pub claims: Vec<Claim>,
    pub questions: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub runtime: u32,
    pub capture: String,
    pub title: String,
    pub summary: String,
    pub chapters: Vec<Chapter>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Bundle {
    pub manifest: Manifest,
    pub documents: BTreeMap<String, String>,
}
impl Bundle {
    /// Read a complete author commit into memory once. Matching every byte
    /// against the final marker prevents partial edits or racing writes from
    /// replacing the last usable guide.
    #[cfg(any(target_os = "macos", test))]
    pub fn load_committed(directory: &Path, capture: &Capture) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Commit {
            capture: String,
            files: BTreeMap<String, String>,
        }
        let marker = read_file(directory, "ready.json", 16 * 1024)?;
        let commit: Commit =
            serde_json::from_str(&marker).context("Invalid guide commit marker")?;
        ensure!(
            commit.capture == capture.id,
            "Guide update targets an earlier capture"
        );
        let manifest = read_file(directory, "manifest.json", 128 * 1024)?;
        ensure!(
            commit.files.get("manifest.json") == Some(&super::digest(&manifest)),
            "Manifest is still being edited; waiting for a complete commit"
        );
        let manifest: Manifest =
            serde_json::from_str(&manifest).context("Invalid guide manifest")?;
        ensure!(
            manifest.chapters.len() <= 16 && commit.files.len() == manifest.chapters.len() + 1,
            "Guide commit must name exactly the manifest and chapter files"
        );
        let mut documents = BTreeMap::new();
        for chapter in &manifest.chapters {
            ensure!(
                !documents.contains_key(&chapter.document)
                    && chapter.document != "manifest.json"
                    && chapter.document != "ready.json",
                "Duplicate or reserved chapter document"
            );
            let html = read_file(directory, &chapter.document, 512 * 1024)?;
            ensure!(
                commit.files.get(&chapter.document) == Some(&super::digest(&html)),
                "Chapter is still being edited; waiting for a complete commit"
            );
            documents.insert(chapter.document.clone(), html);
        }
        ensure!(
            read_file(directory, "ready.json", 16 * 1024)? == marker,
            "Guide commit changed during validation; retry"
        );
        let bundle = Self {
            manifest,
            documents,
        };
        bundle.validate(capture)?;
        Ok(bundle)
    }
    // Consumed by the macOS-only visual workspace.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn chapter(&self, id: &str) -> Option<(&Chapter, &str)> {
        let c = self.manifest.chapters.iter().find(|c| c.id == id)?;
        Some((c, self.documents.get(&c.document)?.as_str()))
    }
    // Consumed by the macOS-only visual workspace.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn load(directory: &Path, capture: &Capture) -> Result<Self> {
        let manifest = read_file(directory, "manifest.json", 128 * 1024)?;
        let manifest: Manifest =
            serde_json::from_str(&manifest).context("Invalid guide manifest")?;
        let mut documents = BTreeMap::new();
        for chapter in &manifest.chapters {
            ensure!(
                !documents.contains_key(&chapter.document),
                "Each chapter must have its own document"
            );
            documents.insert(
                chapter.document.clone(),
                read_file(directory, &chapter.document, 512 * 1024)?,
            );
        }
        let result = Self {
            manifest,
            documents,
        };
        result.validate(capture)?;
        Ok(result)
    }
    // Consumed by the macOS-only visual workspace.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn validate(&self, capture: &Capture) -> Result<()> {
        let m = &self.manifest;
        ensure!(
            m.runtime == 1 && m.capture == capture.id,
            "Guide targets another capture or unsupported runtime"
        );
        ensure!(
            !m.title.trim().is_empty()
                && m.title.len() <= 240
                && !m.summary.trim().is_empty()
                && m.summary.len() <= 4000,
            "Guide needs a bounded title and summary"
        );
        ensure!(
            !m.chapters.is_empty() && m.chapters.len() <= 16,
            "Guide must contain 1–16 behaviors"
        );
        ensure!(
            self.documents.len() == m.chapters.len(),
            "Unexpected guide documents"
        );
        let mut ids = HashSet::new();
        let mut total = 0;
        for c in &m.chapters {
            let document = Path::new(&c.document);
            ensure!(
                !document.is_absolute()
                    && document
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
                    && !c.document.is_empty()
                    && c.document != "manifest.json"
                    && c.document != "ready.json",
                "Invalid chapter document path"
            );
            ensure!(
                valid_id(&c.id) && ids.insert(c.id.clone()),
                "Invalid or duplicate chapter ID"
            );
            ensure!(
                !c.title.trim().is_empty()
                    && c.title.len() <= 240
                    && !c.summary.trim().is_empty()
                    && c.summary.len() <= 8000,
                "Chapter needs a bounded title and readable text equivalent"
            );
            ensure!(
                !c.evidence_ids.is_empty() && c.evidence_ids.len() <= 32,
                "Each behavior needs 1–32 evidence references"
            );
            for e in &c.evidence_ids {
                capture.evidence(e)?;
            }
            ensure!(
                !c.claims.is_empty() && c.claims.len() <= 24 && c.questions.len() <= 12,
                "Chapter needs explicit claims and bounded questions"
            );
            for claim in &c.claims {
                ensure!(
                    valid_id(&claim.id)
                        && ids.insert(claim.id.clone())
                        && !claim.text.trim().is_empty()
                        && claim.text.len() <= 2000,
                    "Invalid claim"
                );
                ensure!(
                    !claim.evidence_ids.is_empty()
                        && claim
                            .evidence_ids
                            .iter()
                            .all(|e| c.evidence_ids.contains(e)),
                    "Claim references evidence outside its chapter"
                );
            }
            ensure!(
                c.questions
                    .iter()
                    .all(|q| !q.trim().is_empty() && q.len() <= 2000),
                "Invalid review question"
            );
            let html = self
                .documents
                .get(&c.document)
                .context("Missing chapter document")?;
            ensure!(
                !html.trim().is_empty() && html.len() <= 512 * 1024,
                "Empty or oversized chapter document"
            );
            total += html.len();
        }
        ensure!(total <= 4 * 1024 * 1024, "Guide exceeds 4 MiB");
        Ok(())
    }
}
// Consumed by the macOS-only visual workspace (via `Bundle::validate`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
// Consumed by the macOS-only visual workspace (via `Bundle::load`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn read_file(root: &Path, name: &str, limit: u64) -> Result<String> {
    let root_meta = fs::symlink_metadata(root)?;
    ensure!(
        root_meta.is_dir() && !root_meta.file_type().is_symlink(),
        "Guide directory must be a real directory"
    );
    let path = Path::new(name);
    ensure!(
        !path.is_absolute() && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Guide path escapes candidate directory"
    );
    let mut resolved = root.to_path_buf();
    for component in path.components() {
        resolved.push(component);
        let meta = fs::symlink_metadata(&resolved)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Guide symlinks are forbidden"
        );
    }
    let meta = fs::symlink_metadata(&resolved)?;
    ensure!(
        meta.is_file() && meta.len() <= limit,
        "Guide file is not a bounded regular file"
    );
    use std::io::Read as _;
    let mut text = String::new();
    fs::File::open(resolved)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= limit,
        "Guide file grew beyond its limit"
    );
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_paths_symlinks_and_oversized_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("chapter.html"), "hello").unwrap();
        assert!(read_file(tmp.path(), "../chapter.html", 1024).is_err());
        assert!(read_file(tmp.path(), "chapter.html", 4).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("chapter.html", tmp.path().join("link.html")).unwrap();
            assert!(read_file(tmp.path(), "link.html", 1024).is_err());
        }
    }
}
