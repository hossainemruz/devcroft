//! Local branch-pair review feedback, shared by the desktop and headless CLI.
use super::{
    git::{self, ReviewScope},
    model::ReviewDiff,
};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Old,
    New,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Anchor {
    pub path: String,
    pub side: Side,
    pub start: u32,
    pub end: u32,
    pub outdated: bool,
    // Full source permits diff-based relocation even outside visible hunks.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Comment {
    pub id: String,
    pub pair: String,
    pub scope: String,
    pub anchor: Anchor,
    pub body: String,
    pub resolved: bool,
    pub revision: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    next_id: u64,
    comments: Vec<Comment>,
}

impl Anchor {
    fn validate(&self) -> Result<()> {
        if self.start == 0
            || self.end < self.start
            || self.end as usize > self.source.lines().count()
        {
            bail!("Invalid comment line range");
        }
        if self.path.is_empty()
            || Path::new(&self.path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            bail!("Invalid comment file path");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Store {
    dir: PathBuf,
    pub pair: String,
    pub scope: String,
}

impl Store {
    pub fn ensure_checkout(&self, cwd: &Path) -> Result<()> {
        let repo = gix::discover(cwd)?;
        let (_, _, expected): (String, String, String) = serde_json::from_str(&self.pair)?;
        let current = repo
            .head_name()?
            .map(|n| n.shorten().to_string())
            .unwrap_or(repo.head_id()?.to_string());
        if current != expected {
            bail!("The checkout branch changed. Refresh before changing comments.");
        }
        Ok(())
    }

    pub fn open(
        cwd: &Path,
        base: &str,
        remote: &str,
        scope: &ReviewScope,
        diff: &ReviewDiff,
    ) -> Result<Self> {
        let repo = gix::discover(cwd)?;
        Ok(Self {
            dir: repo.common_dir().join("devcroft-review"),
            pair: serde_json::to_string(&(
                remote,
                base,
                diff.head_branch.as_deref().unwrap_or(&diff.head_commit),
            ))?,
            scope: match scope {
                ReviewScope::FullDiff { .. } => "full",
                ReviewScope::UncommittedChanges => "uncommitted",
            }
            .into(),
        })
    }

    fn transaction<T>(&self, write: bool, f: impl FnOnce(&mut Record) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join("store.lock"))?;
        lock.lock()?;
        let path = self.dir.join("comments.json");
        let mut record: Record = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading review comments")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(e.into()),
        };
        for comment in &record.comments {
            comment
                .anchor
                .validate()
                .context("Invalid stored review comment")?;
        }
        let result = f(&mut record)?;
        if write {
            let tmp = self.dir.join("comments.json.tmp");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            serde_json::to_writer_pretty(&mut file, &record)?;
            file.sync_all()?;
            fs::rename(tmp, path)?;
            fs::File::open(&self.dir)?.sync_all()?;
        }
        Ok(result)
    }

    pub fn list(&self) -> Result<Vec<Comment>> {
        self.transaction(false, |r| {
            Ok(r.comments
                .iter()
                .filter(|c| c.pair == self.pair && c.scope == self.scope)
                .cloned()
                .collect())
        })
    }

    pub fn create(&self, anchor: Anchor, body: String) -> Result<String> {
        anchor.validate()?;
        if body.trim().is_empty() {
            bail!("Comment cannot be empty");
        }
        self.transaction(true, |r| {
            r.next_id += 1;
            let id = format!("c{}", r.next_id);
            r.comments.push(Comment {
                id: id.clone(),
                pair: self.pair.clone(),
                scope: self.scope.clone(),
                anchor,
                body,
                resolved: false,
                revision: 1,
            });
            Ok(id)
        })
    }

    pub fn change(
        &self,
        id: &str,
        expected: Option<u64>,
        body: Option<String>,
        resolved: Option<bool>,
        delete: bool,
    ) -> Result<()> {
        if body.as_ref().is_some_and(|b| b.trim().is_empty()) {
            bail!("Comment cannot be empty");
        }
        self.transaction(true, |r| {
            let i = r
                .comments
                .iter()
                .position(|c| c.id == id && c.pair == self.pair && c.scope == self.scope)
                .context("Comment no longer exists in this review")?;
            let c = &mut r.comments[i];
            if expected.is_some_and(|v| v != c.revision) {
                bail!("Comment changed externally. Refresh before retrying.");
            }
            if delete {
                r.comments.remove(i);
            } else {
                if let Some(body) = body {
                    c.body = body;
                }
                if let Some(resolved) = resolved {
                    c.resolved = resolved;
                }
                c.revision += 1;
            }
            Ok(())
        })
    }

    pub fn refresh(&self, cwd: &Path, diff: &ReviewDiff) -> Result<Vec<Comment>> {
        self.transaction(true, |r| {
            let mut sources = std::collections::HashMap::new();
            for c in r
                .comments
                .iter_mut()
                .filter(|c| c.pair == self.pair && c.scope == self.scope)
            {
                let path = diff
                    .files
                    .iter()
                    .find(|f| f.old_path.as_deref() == Some(&c.anchor.path))
                    .map(|f| f.path.as_str())
                    .unwrap_or(&c.anchor.path)
                    .to_owned();
                let key = (path.clone(), c.anchor.side);
                if let std::collections::hash_map::Entry::Vacant(entry) = sources.entry(key.clone())
                {
                    entry.insert(git::anchor_source(cwd, diff, &path, c.anchor.side)?);
                }
                let text = &sources[&key];
                let before = (
                    c.anchor.start,
                    c.anchor.end,
                    c.anchor.outdated,
                    c.anchor.path.clone(),
                );
                relocate(&mut c.anchor, text.as_deref());
                c.anchor.path = path;
                if before
                    != (
                        c.anchor.start,
                        c.anchor.end,
                        c.anchor.outdated,
                        c.anchor.path.clone(),
                    )
                {
                    c.revision += 1;
                }
            }
            Ok(r.comments
                .iter()
                .filter(|c| c.pair == self.pair && c.scope == self.scope)
                .cloned()
                .collect())
        })
    }
}

pub(crate) fn relocate(anchor: &mut Anchor, current: Option<&str>) {
    if anchor.validate().is_err() {
        anchor.outdated = true;
        return;
    }
    let Some(current) = current else {
        anchor.outdated = true;
        return;
    };
    if anchor.source == current {
        anchor.outdated = false;
        return;
    }
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_millis(250))
        .diff_lines(&anchor.source, current);
    let start = anchor.start as usize - 1;
    let end = anchor.end as usize;
    let mapped = diff.ops().iter().find_map(|op| {
        let (tag, old, new) = op.as_tag_tuple();
        (tag == similar::DiffTag::Equal && old.start <= start && old.end >= end)
            .then(|| (new.start + start - old.start, new.start + end - old.start))
    });
    if let Some((start, end)) = mapped.filter(|(start, _)| unambiguous(anchor, current, *start)) {
        anchor.start = start as u32 + 1;
        anchor.end = end as u32;
        anchor.source = current.into();
        anchor.outdated = false;
    } else {
        anchor.outdated = true;
    }
}

// Repeated lines must retain unique surrounding context. A diff algorithm may
// otherwise align a deleted occurrence with an identical surviving occurrence.
fn unambiguous(anchor: &Anchor, current: &str, mapped: usize) -> bool {
    let old: Vec<_> = anchor.source.lines().collect();
    let new: Vec<_> = current.lines().collect();
    let start = anchor.start as usize - 1;
    let end = anchor.end as usize;
    let selected = &old[start..end];
    if new.len() < selected.len() {
        return false;
    }
    let old_count = old
        .windows(selected.len())
        .filter(|w| *w == selected)
        .count();
    let new_count = new
        .windows(selected.len())
        .filter(|w| *w == selected)
        .count();
    if old_count == 1 && new_count == 1 {
        return true;
    }
    let context_start = start.saturating_sub(3);
    let context_end = (end + 3).min(old.len());
    let context = &old[context_start..context_end];
    if new.len() < context.len() {
        return false;
    }
    let mut hits = new
        .windows(context.len())
        .enumerate()
        .filter(|(_, w)| *w == context);
    matches!((hits.next(), hits.next()), (Some((i, _)), None) if i + start - context_start == mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn anchor(source: &str, start: u32, end: u32) -> Anchor {
        Anchor {
            path: "file.rs".into(),
            side: Side::New,
            start,
            end,
            source: source.into(),
            outdated: false,
        }
    }

    #[test]
    fn edited_ranges_are_outdated_and_restore_when_code_returns() {
        let mut a = anchor("a\nb\nc\nd\n", 2, 3);
        relocate(&mut a, Some("a\nb\ninsert\nc\nd\n"));
        assert!(a.outdated);
        relocate(&mut a, Some("prefix\na\nb\nc\nd\n"));
        assert_eq!((a.start, a.end, a.outdated), (3, 4, false));
        relocate(&mut a, Some(""));
        assert!(a.outdated);
    }

    #[test]
    fn deleted_duplicate_does_not_attach_to_survivor() {
        let mut a = anchor("x\nx\n", 1, 1);
        relocate(&mut a, Some("x\n"));
        assert!(a.outdated);
        let mut unique_context = anchor("first\nx\none\ntwo\nthree\nsecond\nx\n", 2, 2);
        relocate(
            &mut unique_context,
            Some("prefix\nfirst\nx\none\ntwo\nthree\nsecond\nx\n"),
        );
        assert_eq!((unique_context.start, unique_context.outdated), (3, false));
    }

    #[test]
    fn concurrent_creates_preserve_every_comment_and_unique_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            dir: dir.path().into(),
            pair: "main/topic".into(),
            scope: "full".into(),
        };
        std::thread::scope(|s| {
            for i in 0..12 {
                let store = store.clone();
                s.spawn(move || {
                    store
                        .create(anchor("a\n", 1, 1), format!("comment {i}"))
                        .unwrap()
                });
            }
        });
        let comments = store.list().unwrap();
        assert_eq!(comments.len(), 12);
        assert_eq!(
            comments
                .iter()
                .map(|c| &c.id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            12
        );
    }

    #[test]
    fn malformed_store_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            dir: dir.path().into(),
            pair: "main/topic".into(),
            scope: "full".into(),
        };
        let path = dir.path().join("comments.json");
        fs::write(&path, "broken").unwrap();
        assert!(
            store
                .create(anchor("a\n", 1, 1), "feedback".into())
                .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }
    #[test]
    fn ranges_shift_and_deleted_lines_stay_outdated() {
        let mut a = Anchor {
            path: "a".into(),
            side: Side::New,
            start: 2,
            end: 3,
            source: "a\nb\nc\nd\n".into(),
            outdated: false,
        };
        relocate(&mut a, Some("prefix\na\nb\nc\nd\n"));
        assert_eq!((a.start, a.end, a.outdated), (3, 4, false));
        relocate(&mut a, Some("prefix\na\nc\nd\n"));
        assert!(a.outdated);
        relocate(&mut a, None);
        assert!(a.outdated);
    }
    #[test]
    fn store_survives_sessions_and_rejects_stale_edits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store {
            dir: dir.path().into(),
            pair: "main/topic".into(),
            scope: "full".into(),
        };
        store
            .create(
                Anchor {
                    path: "a".into(),
                    side: Side::New,
                    start: 1,
                    end: 1,
                    source: "a".into(),
                    outdated: false,
                },
                "fix".into(),
            )
            .unwrap();
        let c = store.list().unwrap().remove(0);
        store.change(&c.id, None, None, Some(true), false).unwrap();
        assert!(
            store
                .change(&c.id, Some(c.revision), Some("stale".into()), None, false)
                .is_err()
        );
        assert!(store.clone().list().unwrap()[0].resolved);
        let other = Store {
            pair: "main/other".into(),
            ..store.clone()
        };
        assert!(other.list().unwrap().is_empty());
        store.change(&c.id, None, None, None, true).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
