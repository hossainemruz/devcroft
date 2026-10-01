//! Immutable evidence and durable, revision-checked reviewer state.
mod bundle;
pub(crate) mod pr;
pub(crate) use bundle::Bundle;
#[cfg(test)]
use bundle::{Chapter, Claim, Manifest};

use super::{
    comments::Side,
    git,
    model::{FileContent, LineTag, ReviewDiff},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SourceLine {
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub tag: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct File {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub old: Option<String>,
    pub new: Option<String>,
    pub lines: Vec<SourceLine>,
    pub unavailable: Option<String>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Evidence {
    pub id: String,
    pub path: String,
    pub side: Side,
    pub start: u32,
    pub end: u32,
    pub source_hash: String,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Capture {
    pub id: String,
    pub label: String,
    pub base: String,
    pub head: String,
    pub branch: Option<String>,
    pub files: Vec<File>,
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub pr: Option<pr::Metadata>,
}
impl Capture {
    pub fn local(cwd: &Path, diff: &ReviewDiff, label: &str) -> Result<Self> {
        // Rebuild hunks from the exact bytes retained here. A checkout edit
        // during capture cannot leave the inspector showing a different diff.
        let mut files = Vec::new();
        for f in &diff.files {
            let old = git::anchor_source(cwd, diff, &f.path, Side::Old)?;
            let new = git::anchor_source(cwd, diff, &f.path, Side::New)?;
            let (hunks, truncated) = match &f.content {
                FileContent::Text { .. } => super::model::diff_text(
                    old.as_deref().unwrap_or(""),
                    new.as_deref().unwrap_or(""),
                ),
                _ => (vec![], false),
            };
            let lines = hunks
                .into_iter()
                .flat_map(|h| h.lines)
                .map(|l| SourceLine {
                    old: l.old_no,
                    new: l.new_no,
                    text: l.text,
                    tag: match l.tag {
                        LineTag::Context => "context",
                        LineTag::Addition => "addition",
                        LineTag::Deletion => "deletion",
                    }
                    .into(),
                })
                .collect();
            let counts = if matches!(f.content, FileContent::Text { .. }) {
                super::model::count_changes(
                    old.as_deref().unwrap_or(""),
                    new.as_deref().unwrap_or(""),
                )
            } else {
                (0, 0)
            };
            files.push(File {
                path: f.path.clone(),
                old_path: f.old_path.clone(),
                status: f.status.label().into(),
                additions: counts.0,
                deletions: counts.1,
                old,
                new,
                lines,
                truncated,
                unavailable: match &f.content {
                    FileContent::Unavailable(reason) => Some(reason.label().into()),
                    _ => None,
                },
            });
        }
        Self::from_files(
            label,
            &diff.base_commit,
            &diff.head_commit,
            diff.head_branch.clone(),
            files,
        )
    }
    pub fn stable_local(cwd: &Path, scope: &git::ReviewScope, label: &str) -> Result<Self> {
        // A local worktree is mutable. Require a stable complete scan around
        // retained bytes rather than silently omitting a concurrently added file.
        for _ in 0..3 {
            let before = git::load_review(cwd, scope)?;
            let capture = Self::local(cwd, &before, label)?;
            let after = git::load_review(cwd, scope)?;
            if before == after {
                let retained = Self::local(cwd, &after, label)?;
                if capture.id == retained.id {
                    return Ok(capture);
                }
            }
        }
        bail!("Checkout changed while capturing. Retry after edits finish")
    }
    pub fn from_files(
        label: &str,
        base: &str,
        head: &str,
        branch: Option<String>,
        files: Vec<File>,
    ) -> Result<Self> {
        ensure!(
            serde_json::to_vec(&files)?.len() <= 64 * 1024 * 1024,
            "Captured source exceeds 64 MiB; narrow the comparison"
        );
        let id = digest(serde_json::to_vec(&(label, base, head, &files))?);
        let mut evidence = vec![];
        for file in &files {
            for (side, source) in [(Side::Old, &file.old), (Side::New, &file.new)] {
                if let Some(source) = source {
                    let source_hash = digest(source);
                    // Every source range is registered, including unchanged
                    // context. Authors never supply authoritative source text.
                    for (i, chunk) in source.lines().collect::<Vec<_>>().chunks(48).enumerate() {
                        let start = (i * 48 + 1) as u32;
                        let end = start + chunk.len() as u32 - 1;
                        let evidence_id = digest(serde_json::to_vec(&(
                            &id,
                            &file.path,
                            side,
                            start,
                            end,
                            &source_hash,
                        ))?);
                        evidence.push(Evidence {
                            id: format!("e-{}", &evidence_id[..20]),
                            path: file.path.clone(),
                            side,
                            start,
                            end,
                            source_hash: source_hash.clone(),
                            kind: "cited_source".into(),
                        });
                    }
                }
            }
        }
        Ok(Self {
            id,
            label: label.into(),
            base: base.into(),
            head: head.into(),
            branch,
            files,
            evidence,
            pr: None,
        })
    }
    pub fn with_pr(mut self, metadata: pr::Metadata) -> Result<Self> {
        // CI and timestamps are observations, not source identity. The target
        // tip is part of identity even when its merge base did not change.
        let bound = digest(serde_json::to_vec(&(
            &self.id,
            &metadata.repository,
            metadata.number,
            &metadata.target_tip,
            &metadata.base_branch,
            &metadata.head_branch,
            &metadata.title,
            &metadata.description,
            &metadata.author,
        ))?);
        for e in &mut self.evidence {
            e.id = format!(
                "e-{}",
                &digest(serde_json::to_vec(&(
                    &bound,
                    &e.path,
                    e.side,
                    e.start,
                    e.end,
                    &e.source_hash
                ))?)[..20]
            );
        }
        self.id = bound;
        self.pr = Some(metadata);
        Ok(self)
    }
    pub fn evidence(&self, id: &str) -> Result<&Evidence> {
        self.evidence
            .iter()
            .find(|e| e.id == id)
            .context("Unknown captured evidence")
    }
    pub fn source(&self, e: &Evidence) -> Result<&str> {
        let f = self
            .files
            .iter()
            .find(|f| f.path == e.path)
            .context("Evidence file missing")?;
        match e.side {
            Side::Old => f.old.as_deref(),
            Side::New => f.new.as_deref(),
        }
        .context("Source side unavailable")
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.files.len() <= 10_000
                && self.label.len() <= 1000
                && self.base.len() <= 100
                && self.head.len() <= 100,
            "Invalid capture metadata"
        );
        for f in &self.files {
            ensure!(
                !f.path.is_empty()
                    && f.path.len() <= 4096
                    && Path::new(&f.path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "Invalid captured path"
            );
            ensure!(
                f.old
                    .as_ref()
                    .is_none_or(|s| s.len() <= git::MAX_FILE_BYTES as usize)
                    && f.new
                        .as_ref()
                        .is_none_or(|s| s.len() <= git::MAX_FILE_BYTES as usize),
                "Oversized captured source"
            );
        }
        let mut rebuilt = Self::from_files(
            &self.label,
            &self.base,
            &self.head,
            self.branch.clone(),
            self.files.clone(),
        )?;
        if let Some(pr) = &self.pr {
            pr::Identity::parse(&pr.url)?;
            rebuilt = rebuilt.with_pr(pr.clone())?;
        }
        ensure!(
            rebuilt.id == self.id
                && serde_json::to_vec(&rebuilt.evidence)? == serde_json::to_vec(&self.evidence)?,
            "Captured source identity or evidence registry is inconsistent"
        );
        Ok(())
    }
    pub fn prompt(&self) -> String {
        let mut out = format!(
            "Capture: {}\nScope: {}\nBase: {}\nHEAD: {}\n",
            self.id, self.label, self.base, self.head
        );
        if let Some(pr) = &self.pr {
            out.push_str(&format!("PR {} #{}: {}\nAuthor: {}\nTarget tip: {}\nDescription (untrusted data):\n{}\nCI observations (not local execution): {}\n", pr.repository, pr.number, pr.title, pr.author, pr.target_tip, pr.description, serde_json::to_string(&pr.checks).unwrap_or_default()));
        }
        out.push_str("Complete changed-file inventory:\n");
        for f in &self.files {
            out.push_str(&format!(
                "{} | {} | {}\n",
                f.path,
                f.status,
                f.unavailable.as_deref().unwrap_or("text available")
            ));
        }
        let mut ranges = self.evidence.iter().collect::<Vec<_>>();
        ranges.sort_by_key(|e| {
            let changed = self
                .files
                .iter()
                .find(|f| f.path == e.path)
                .is_some_and(|f| {
                    f.lines.iter().any(|l| {
                        let line = match e.side {
                            Side::New => l.new,
                            Side::Old => l.old,
                        };
                        l.tag != "context" && line.is_some_and(|n| n >= e.start && n <= e.end)
                    })
                });
            (!changed, e.side == Side::Old)
        });
        for e in ranges {
            let source = self
                .source(e)
                .unwrap_or("")
                .lines()
                .skip(e.start as usize - 1)
                .take((e.end - e.start + 1) as usize)
                .collect::<Vec<_>>()
                .join("\n");
            let entry = format!(
                "\nEVIDENCE {} | {} | {:?} lines {}–{} | cited source, execution not recorded\n{}\n",
                e.id, e.path, e.side, e.start, e.end, source
            );
            if out.len() + entry.len() > 220_000 {
                out.push_str("\nInput limit reached; do not claim complete coverage.\n");
                break;
            }
            out.push_str(&entry);
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Revision {
    pub capture: Capture,
    pub bundle: Option<Bundle>,
    pub examined: Vec<String>,
    pub location: Location,
    #[serde(default)]
    pub guide_history: Vec<GuideRevision>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GuideRevision {
    pub bundle: Bundle,
    pub examined: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Location {
    pub page: String,
    pub chapter: Option<String>,
    pub evidence: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub id: String,
    pub capture: String,
    pub chapter: Option<String>,
    pub evidence: Option<String>,
    pub body: String,
    pub resolved: bool,
    #[serde(default)]
    pub range: Option<SourceRange>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SourceRange {
    pub start: u32,
    pub end: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Investigation {
    pub id: String,
    pub capture: String,
    pub chapter: Option<String>,
    pub evidence: Option<String>,
    pub question: String,
    pub answer: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Review {
    pub schema: u32,
    pub version: u64,
    pub current: usize,
    pub revisions: Vec<Revision>,
    pub findings: Vec<Finding>,
    pub investigations: Vec<Investigation>,
    pub drafts: BTreeMap<String, String>,
    #[serde(default)]
    pub submissions: Vec<pr::Submission>,
}
impl Review {
    pub fn active(&self) -> &Revision {
        &self.revisions[self.current]
    }
    pub fn active_mut(&mut self) -> &mut Revision {
        &mut self.revisions[self.current]
    }
    pub fn start(capture: Capture) -> Self {
        Self {
            schema: 1,
            version: 0,
            current: 0,
            revisions: vec![Revision {
                capture,
                bundle: None,
                guide_history: vec![],
                examined: vec![],
                location: Location {
                    page: "changes".into(),
                    ..Default::default()
                },
            }],
            findings: vec![],
            investigations: vec![],
            drafts: BTreeMap::new(),
            submissions: vec![],
        }
    }
    pub fn add_capture(&mut self, capture: Capture) {
        if let Some(i) = self
            .revisions
            .iter()
            .position(|r| r.capture.id == capture.id)
        {
            self.current = i;
            // Refresh observations without replacing immutable source or judgments.
            self.revisions[i].capture.pr = capture.pr;
            return;
        }
        // Preserve old decisions as history; never infer approval from lines
        // that happen to remain unchanged. Regeneration within one revision
        // is handled separately from a source update.
        self.revisions.push(Revision {
            capture,
            bundle: None,
            guide_history: vec![],
            examined: vec![],
            location: Location {
                page: "changes".into(),
                ..Default::default()
            },
        });
        self.current = self.revisions.len() - 1;
    }
    pub fn install(&mut self, bundle: Bundle) -> Result<()> {
        bundle.validate(&self.active().capture)?;
        let revision = self.active_mut();
        if revision
            .bundle
            .as_ref()
            .is_some_and(|old| serde_json::to_vec(old).ok() == serde_json::to_vec(&bundle).ok())
        {
            return Ok(());
        }
        ensure!(
            revision.guide_history.len() < 32,
            "Guide history limit reached; earlier explanations are retained"
        );
        let old_examined = revision.examined.clone();
        // A changed claim or document requires another look. A presentation
        // retry with identical content retains the reviewer's decision.
        revision.examined.retain(|id| {
            revision
                .bundle
                .as_ref()
                .and_then(|old| old.chapter(id))
                .zip(bundle.chapter(id))
                .is_some_and(|(a, b)| a == b)
        });
        if let Some(old) = revision.bundle.replace(bundle) {
            revision.guide_history.push(GuideRevision {
                bundle: old,
                examined: old_examined,
            });
        }
        Ok(())
    }
    pub fn select_guide(&mut self, hash: &str) -> Result<()> {
        let revision = self.active_mut();
        let index = revision
            .guide_history
            .iter()
            .position(|old| serde_json::to_vec(old).is_ok_and(|bytes| digest(bytes) == hash))
            .context("Unknown saved guide")?;
        let old = revision.guide_history.remove(index);
        if let Some(current) = revision.bundle.replace(old.bundle) {
            revision.guide_history.push(GuideRevision {
                bundle: current,
                examined: revision.examined.clone(),
            });
        }
        revision.examined = old.examined;
        revision.location.page = "overview".into();
        revision.location.chapter = None;
        revision.location.evidence = None;
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Store {
    directory: PathBuf,
}
pub(crate) struct ReviewLock {
    file: fs::File,
}
impl Drop for ReviewLock {
    fn drop(&mut self) {
        // Closing only this descriptor can leave the lock alive in a child
        // forked concurrently by another thread. End the operation explicitly.
        let _ = self.file.unlock();
    }
}
impl Store {
    pub fn open(cwd: &Path, label: &str) -> Result<Self> {
        let root = crate::data::resolve_data_root()?;
        Ok(Self::at(root.root().join("reviews").join(digest(
            serde_json::to_vec(&(cwd.canonicalize()?, label))?,
        ))))
    }
    pub fn at(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn authoring_directory(&self, capture: &Capture) -> PathBuf {
        self.directory.join("authoring").join(&capture.id)
    }
    pub fn authoring_lock(&self, capture: &Capture) -> Result<ReviewLock> {
        self.operation_lock(&format!("authoring-{}.lock", capture.id), "Another review window has an agent editing this guide. Close that agent before starting another")
    }
    pub fn publication_lock(&self) -> Result<ReviewLock> {
        self.operation_lock("publication.lock", "Another window is checking or changing GitHub review status. Wait for it to finish, then reload")
    }
    fn operation_lock(&self, name: &str, busy: &str) -> Result<ReviewLock> {
        fs::create_dir_all(&self.directory)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(name))?;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(ReviewLock { file: lock }),
                Err(fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                Err(fs::TryLockError::WouldBlock) => anyhow::bail!("{busy}"),
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).context("Locking review operation");
                }
            }
        }
    }
    pub fn load(&self) -> Result<Option<Review>> {
        let path = self.directory.join("review.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= 128 * 1024 * 1024,
            "Invalid or oversized review record"
        );
        let review: Review = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(
            review.schema == 1 && review.current < review.revisions.len(),
            "Unsupported review record"
        );
        for revision in &review.revisions {
            revision.capture.validate()?;
            if let Some(bundle) = &revision.bundle {
                bundle.validate(&revision.capture)?;
            }
            ensure!(
                revision.guide_history.len() <= 32,
                "Oversized guide history"
            );
            for old in &revision.guide_history {
                old.bundle.validate(&revision.capture)?;
            }
        }
        for finding in &review.findings {
            let capture = &review
                .revisions
                .iter()
                .find(|r| r.capture.id == finding.capture)
                .context("Finding revision missing")?
                .capture;
            ensure!(finding.body.len() <= 12000, "Oversized saved finding");
            if let Some(evidence) = &finding.evidence {
                let e = capture.evidence(evidence)?;
                if let Some(range) = &finding.range {
                    ensure!(
                        range.start >= e.start && range.end <= e.end && range.start <= range.end,
                        "Invalid saved finding range"
                    );
                }
            } else {
                ensure!(
                    finding.range.is_none(),
                    "Finding range lacks source evidence"
                );
            }
        }
        Ok(Some(review))
    }
    pub fn save(&self, review: &mut Review) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join("store.lock"))?;
        lock.lock()?;
        let current = self.load()?;
        if current.as_ref().map_or(0, |r| r.version) != review.version {
            bail!(
                "Review changed in another window. Reopen it to load the latest state; your draft remains in this window."
            );
        }
        let mut next = review.clone();
        next.version += 1;
        let bytes = serde_json::to_vec(&next)?;
        ensure!(
            bytes.len() <= 128 * 1024 * 1024,
            "Review history exceeds 128 MiB"
        );
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.directory)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.directory.join("review.json"))?;
        *review = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capture(source: &str) -> Capture {
        Capture::from_files(
            "Local snapshot",
            "base",
            "head",
            None,
            vec![File {
                path: "a.rs".into(),
                old_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                old: Some("before\n".into()),
                new: Some(source.into()),
                lines: vec![],
                unavailable: None,
                truncated: false,
            }],
        )
        .unwrap()
    }
    #[test]
    fn publication_lock_releases_even_with_an_inherited_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("review"));
        let guard = store.publication_lock().unwrap();
        let inherited = guard.file.try_clone().unwrap();
        assert!(store.publication_lock().is_err());
        drop(guard);
        let next = store.publication_lock().unwrap();
        drop(next);
        drop(inherited);
    }
    #[test]
    fn authoring_is_single_writer_per_capture_and_releases_for_replacement_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("review"));
        let first = capture("first\n");
        let guard = store.authoring_lock(&first).unwrap();
        assert!(store.authoring_lock(&first).is_err());
        assert!(
            store
                .authoring_lock(&capture("different capture\n"))
                .is_ok()
        );
        assert!(store.publication_lock().is_ok());
        let inherited = guard.file.try_clone().unwrap();
        drop(guard);
        assert!(store.authoring_lock(&first).is_ok());
        drop(inherited);
    }
    #[test]
    fn guide_revisions_preserve_old_decisions_and_restore_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("review"));
        let mut review = Review::start(capture("after\n"));
        let evidence = review.active().capture.evidence[0].id.clone();
        let chapters = (0..2)
            .map(|i| Chapter {
                id: format!("behavior-{i}"),
                title: format!("Behavior {i}"),
                summary: "Static readable explanation".into(),
                document: format!("chapters/{i}.html"),
                evidence_ids: vec![evidence.clone()],
                claims: vec![Claim {
                    id: format!("claim-{i}"),
                    text: "Inspect the captured source".into(),
                    evidence_ids: vec![evidence.clone()],
                }],
                questions: vec![],
            })
            .collect();
        let original = Bundle {
            manifest: Manifest {
                runtime: 1,
                capture: review.active().capture.id.clone(),
                title: "Guide".into(),
                summary: "Summary".into(),
                chapters,
            },
            documents: BTreeMap::from([
                ("chapters/0.html".into(), "<p>First</p>".into()),
                ("chapters/1.html".into(), "<p>Second</p>".into()),
            ]),
        };
        review.install(original.clone()).unwrap();
        review.active_mut().examined = vec!["behavior-0".into(), "behavior-1".into()];
        let mut repaired = original.clone();
        repaired
            .documents
            .insert("chapters/0.html".into(), "<p>Repaired visual</p>".into());
        review.install(repaired).unwrap();
        assert_eq!(review.active().examined, ["behavior-1"]);
        assert_eq!(review.active().guide_history[0].examined.len(), 2);
        let hash = digest(serde_json::to_vec(&review.active().guide_history[0]).unwrap());
        store.save(&mut review).unwrap();
        let mut restored = store.load().unwrap().unwrap();
        restored.select_guide(&hash).unwrap();
        assert_eq!(restored.active().examined.len(), 2);
        assert_eq!(
            restored.active().bundle.as_ref().unwrap().documents,
            original.documents
        );
        assert_eq!(restored.active().guide_history.len(), 1);
    }
    #[test]
    fn evidence_tracks_bytes_and_survives_checkout_independently() {
        let a = capture("after\ncontext\n");
        let b = capture("later\n");
        assert_ne!(a.id, b.id);
        let e = a.evidence.iter().find(|e| e.side == Side::New).unwrap();
        assert_eq!(a.source(e).unwrap(), "after\ncontext\n");
        assert!(b.evidence(&e.id).is_err());
    }
    #[test]
    fn restart_conflicts_and_new_revisions_preserve_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().into());
        let mut review = Review::start(capture("one\n"));
        review.drafts.insert("finding".into(), "keep me".into());
        store.save(&mut review).unwrap();
        let mut stale = review.clone();
        review.add_capture(capture("two\n"));
        store.save(&mut review).unwrap();
        assert!(store.save(&mut stale).is_err());
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.revisions.len(), 2);
        assert_eq!(loaded.drafts["finding"], "keep me");
        assert!(loaded.active().examined.is_empty());
    }
}
