use std::path::PathBuf;

use crate::git_status::GitStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Untracked,
    Unknown,
}

impl ChangeKind {
    pub(crate) fn from_status(value: u8) -> Option<Self> {
        match value {
            b'.' | b' ' => None,
            b'A' => Some(Self::Added),
            b'M' => Some(Self::Modified),
            b'D' => Some(Self::Deleted),
            b'R' => Some(Self::Renamed),
            b'C' => Some(Self::Copied),
            b'T' => Some(Self::TypeChanged),
            b'U' => Some(Self::Unmerged),
            b'?' => Some(Self::Untracked),
            _ => Some(Self::Unknown),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Unmerged => "U",
            Self::Untracked => "?",
            Self::Unknown => "•",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) path: PathBuf,
    pub(crate) original_path: Option<PathBuf>,
    pub(crate) kind: ChangeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationState {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl OperationState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge in progress",
            Self::Rebase => "Rebase in progress",
            Self::CherryPick => "Cherry-pick in progress",
            Self::Revert => "Revert in progress",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepositorySnapshot {
    pub(crate) branch: Option<String>,
    pub(crate) detached: bool,
    pub(crate) oid: Option<String>,
    pub(crate) unborn: bool,
    pub(crate) upstream: Option<String>,
    pub(crate) ahead: usize,
    pub(crate) behind: usize,
    pub(crate) operation: Option<OperationState>,
    pub(crate) remotes: Vec<String>,
    pub(crate) conflicts: Vec<Change>,
    pub(crate) staged: Vec<Change>,
    pub(crate) unstaged: Vec<Change>,
    pub(crate) untracked: Vec<Change>,
}

impl RepositorySnapshot {
    pub(crate) fn branch_label(&self) -> String {
        if let Some(branch) = &self.branch {
            return branch.clone();
        }
        if self.detached {
            return self
                .oid
                .as_deref()
                .map(|oid| format!("Detached at {}", &oid[..oid.len().min(7)]))
                .unwrap_or_else(|| "Detached HEAD".to_owned());
        }
        "No commits yet".to_owned()
    }

    pub(crate) fn change_count(&self) -> usize {
        self.conflicts.len() + self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.change_count() > 0
    }

    pub(crate) fn header_status(&self) -> GitStatus {
        GitStatus {
            branch: self.branch.clone().or_else(|| {
                self.detached.then(|| {
                    self.oid
                        .as_deref()
                        .map(|oid| oid[..oid.len().min(7)].to_owned())
                        .unwrap_or_else(|| "HEAD".to_owned())
                })
            }),
            detached: self.detached,
            dirty: self.is_dirty(),
            ahead: self.ahead,
            behind: self.behind,
            ahead_truncated: false,
            behind_truncated: false,
            has_upstream: self.upstream.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_snapshot_keeps_a_header_identity() {
        let snapshot = RepositorySnapshot {
            detached: true,
            oid: Some("1234567890abcdef".to_owned()),
            ..RepositorySnapshot::default()
        };

        let status = snapshot.header_status();
        assert!(status.detached);
        assert_eq!(status.branch.as_deref(), Some("1234567"));
    }
}
