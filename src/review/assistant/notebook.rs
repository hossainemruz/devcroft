//! Human-authored conclusions. The agent cannot set review outcomes.
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::tutorial::Excerpt;
use crate::review::{git::ReviewScope, model::ReviewDiff};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    #[default]
    FollowUp,
    Concern,
    Satisfied,
}

impl Outcome {
    pub const ALL: [Self; 3] = [Self::FollowUp, Self::Concern, Self::Satisfied];
    pub fn label(self) -> &'static str {
        match self {
            Self::FollowUp => "Needs follow-up",
            Self::Concern => "Concern",
            Self::Satisfied => "Satisfied",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Evidence {
    pub label: String,
    pub text: String,
}

impl From<&Excerpt> for Evidence {
    fn from(excerpt: &Excerpt) -> Self {
        Self {
            label: excerpt.label(),
            text: excerpt.text(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Conclusion {
    pub id: u64,
    pub revision: u64,
    pub capture: String,
    pub behavior: String,
    pub subject: String,
    pub body: String,
    pub outcome: Outcome,
    pub evidence: Vec<Evidence>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    scope: String,
    conclusion: Conclusion,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    next_id: u64,
    entries: Vec<Entry>,
}

pub(crate) struct Store {
    dir: PathBuf,
    scope: String,
}

impl Store {
    pub fn open(cwd: &Path, scope: &ReviewScope, diff: &ReviewDiff) -> Result<Self> {
        let repo = gix::discover(cwd)?;
        let scope = match scope {
            ReviewScope::FullDiff {
                base_branch,
                remote,
            } => ("full", base_branch.as_str(), remote.as_str()),
            ReviewScope::UncommittedChanges => ("uncommitted", "", ""),
        };
        Ok(Self {
            dir: repo.common_dir().join("devcroft-review"),
            scope: serde_json::to_string(&(
                cwd.canonicalize()?,
                diff.head_branch.as_deref().unwrap_or(&diff.head_commit),
                scope,
            ))?,
        })
    }

    fn transaction<T>(&self, write: bool, f: impl FnOnce(&mut Record) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join("guided-notes.lock"))?;
        lock.lock()?;
        let path = self.dir.join("guided-notes.json");
        let mut record: Record = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("reading saved review conclusions")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(error) => return Err(error.into()),
        };
        let result = f(&mut record)?;
        if write {
            crate::data::write_json_atomic(&path, &record)?;
        }
        Ok(result)
    }

    pub fn list(&self) -> Result<Vec<Conclusion>> {
        self.transaction(false, |record| {
            Ok(record
                .entries
                .iter()
                .filter(|entry| entry.scope == self.scope)
                .map(|entry| entry.conclusion.clone())
                .collect())
        })
    }

    pub fn append(&self, mut conclusion: Conclusion) -> Result<Conclusion> {
        if conclusion.body.trim().is_empty() || conclusion.capture.is_empty() {
            bail!("a conclusion needs your notes and a captured comparison");
        }
        self.transaction(true, |record| {
            record.next_id += 1;
            conclusion.id = record.next_id;
            conclusion.revision = 1;
            record.entries.push(Entry {
                scope: self.scope.clone(),
                conclusion: conclusion.clone(),
            });
            Ok(conclusion)
        })
    }

    pub fn set_outcome(&self, id: u64, revision: u64, outcome: Outcome) -> Result<()> {
        self.transaction(true, |record| {
            let entry = record
                .entries
                .iter_mut()
                .find(|entry| entry.scope == self.scope && entry.conclusion.id == id)
                .context("conclusion no longer exists")?;
            if entry.conclusion.revision != revision {
                bail!(
                    "this conclusion changed in another window; reopen the guide before editing it"
                );
            }
            entry.conclusion.outcome = outcome;
            entry.conclusion.revision += 1;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conclusion() -> Conclusion {
        Conclusion {
            id: 0,
            revision: 0,
            capture: "captured-source".into(),
            behavior: "Recover from failure".into(),
            subject: "Can cleanup fail?".into(),
            body: "Check retry handling".into(),
            outcome: Outcome::FollowUp,
            evidence: vec![Evidence {
                label: "source.rs · old 1 / new 2".into(),
                text: "+cleanup()".into(),
            }],
        }
    }

    #[test]
    fn conclusions_survive_reopening_without_crossing_scopes_or_losing_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store {
            dir: temp.path().into(),
            scope: "branch-a/full".into(),
        };
        let saved = store.append(conclusion()).unwrap();
        let reopened = Store {
            dir: temp.path().into(),
            scope: "branch-a/full".into(),
        };
        let notes = reopened.list().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].capture, "captured-source");
        assert_eq!(notes[0].evidence[0].text, "+cleanup()");
        let other = Store {
            dir: temp.path().into(),
            scope: "branch-b/full".into(),
        };
        assert!(other.list().unwrap().is_empty());
        assert!(
            other
                .set_outcome(saved.id, saved.revision, Outcome::Satisfied)
                .is_err()
        );
        reopened
            .set_outcome(saved.id, saved.revision, Outcome::Satisfied)
            .unwrap();
        assert!(
            store
                .set_outcome(saved.id, saved.revision, Outcome::Concern)
                .is_err()
        );
        assert_eq!(store.list().unwrap()[0].outcome, Outcome::Satisfied);
    }

    #[test]
    fn corrupt_notes_are_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store {
            dir: temp.path().into(),
            scope: "scope".into(),
        };
        let path = temp.path().join("guided-notes.json");
        fs::write(&path, "broken").unwrap();
        assert!(store.list().is_err());
        assert!(store.append(conclusion()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }
}
