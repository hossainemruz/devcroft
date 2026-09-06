//! UI-ready diff model for the Review tab.
//!
//! [`ReviewDiff`] is plain data with no git dependency: [`git`](super::git)
//! builds it from a repository, and the stream/tree views render it. Keeping
//! the line-diffing here (over [`similar`]) means it is unit-testable with
//! plain strings and fixture-free.

use std::cmp::Ordering;
use std::ops::Range;

use similar::{Algorithm, ChangeTag, TextDiff};

/// Context lines kept around each change when grouping hunks.
pub(crate) const CONTEXT_LINES: usize = 3;
/// Maximum hunk lines (context + changes) emitted per file before truncation.
pub(crate) const MAX_HUNK_LINES_PER_FILE: usize = 4_000;

/// A complete reviewable changeset: one base tree compared against one
/// worktree state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewDiff {
    pub(crate) files: Vec<ChangedFile>,
    pub(crate) base_commit: String,
    pub(crate) head_commit: String,
    /// Resolved base ref (e.g. `origin/main`), if the base came from a ref.
    pub(crate) base_ref: Option<String>,
    /// Current branch short name, if HEAD is attached.
    pub(crate) head_branch: Option<String>,
}

/// One changed path in a [`ReviewDiff`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedFile {
    /// Repository-relative path with forward slashes.
    pub(crate) path: String,
    /// Previous path for renames.
    pub(crate) old_path: Option<String>,
    pub(crate) status: FileStatus,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    pub(crate) content: FileContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

impl FileStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::TypeChanged => "type changed",
        }
    }

    /// Single-letter sidebar cue: A/M/D/R/T.
    pub(crate) fn abbrev(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::TypeChanged => "T",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileContent {
    Text { hunks: Vec<Hunk>, truncated: bool },
    Unavailable(UnavailableReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    Binary,
    TooLarge,
    InvalidUtf8,
    Missing,
    Submodule,
    /// Mode changes between incompatible kinds (file ↔ symlink) where
    /// neither side is usefully diffable as text.
    Unsupported,
}

impl UnavailableReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Binary => "Binary file",
            Self::TooLarge => "File too large to diff",
            Self::InvalidUtf8 => "Not valid UTF-8 text",
            Self::Missing => "Content unavailable",
            Self::Submodule => "Submodule",
            Self::Unsupported => "Cannot diff across file types",
        }
    }
}

/// One hunk of a unified diff with full-file 1-based line numbers.
///
/// Both `old_start` and `new_start` are 1-based; either span may be empty
/// (zero lines) for pure insertions or deletions. Comment anchoring in later
/// milestones depends on these numbers, so they must always describe the
/// position in the complete file, never a hunk-relative offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub(crate) old_start: u32,
    pub(crate) old_lines: u32,
    pub(crate) new_start: u32,
    pub(crate) new_lines: u32,
    /// Equal lines skipped between the previous hunk (or file start) and
    /// this hunk, rendered as a collapsed `…` separator.
    pub(crate) collapsed_before: u32,
    pub(crate) lines: Vec<HunkLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HunkLine {
    pub(crate) tag: LineTag,
    pub(crate) old_no: Option<u32>,
    pub(crate) new_no: Option<u32>,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineTag {
    Context,
    Deletion,
    Addition,
}

/// Diff two text contents into hunks, keeping `CONTEXT_LINES` of context and
/// capping total emitted lines at `MAX_HUNK_LINES_PER_FILE`.
///
/// Returns the hunks plus whether emission was truncated.
pub(crate) fn diff_text(old: &str, new: &str) -> (Vec<Hunk>, bool) {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(old, new);
    let mut hunks = Vec::new();
    let mut truncated = false;
    let mut emitted = 0_usize;
    let mut consumed_new = 0_usize;

    for group in diff.grouped_ops(CONTEXT_LINES) {
        let (old_range, new_range) = group_ranges(&group);
        let collapsed_before = new_range.start.saturating_sub(consumed_new) as u32;
        // The diff algorithm may emit insertions before deletions at one
        // position; unified diffs conventionally show all removals first,
        // so stable-partition each contiguous change run accordingly.
        // Per-line numbers travel with their lines, keeping anchors exact.
        let mut lines = Vec::new();
        let mut pending: Vec<HunkLine> = Vec::new();
        let flush = |pending: &mut Vec<HunkLine>, lines: &mut Vec<HunkLine>| {
            let mut insertions = Vec::new();
            for line in pending.drain(..) {
                if line.tag == LineTag::Addition {
                    insertions.push(line);
                } else {
                    lines.push(line);
                }
            }
            lines.extend(insertions);
        };
        for op in &group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => LineTag::Context,
                    ChangeTag::Delete => LineTag::Deletion,
                    ChangeTag::Insert => LineTag::Addition,
                };
                let line = HunkLine {
                    tag,
                    old_no: change.old_index().map(|index| index as u32 + 1),
                    new_no: change.new_index().map(|index| index as u32 + 1),
                    text: change.value().trim_end_matches(['\r', '\n']).to_owned(),
                };
                if tag == LineTag::Context {
                    flush(&mut pending, &mut lines);
                    lines.push(line);
                } else {
                    pending.push(line);
                }
            }
        }
        flush(&mut pending, &mut lines);
        if emitted + lines.len() > MAX_HUNK_LINES_PER_FILE {
            truncated = true;
            break;
        }
        emitted += lines.len();
        consumed_new = new_range.end;
        hunks.push(Hunk {
            old_start: old_range.start as u32 + 1,
            old_lines: old_range.len() as u32,
            new_start: new_range.start as u32 + 1,
            new_lines: new_range.len() as u32,
            collapsed_before,
            lines,
        });
    }
    (hunks, truncated)
}

/// Count added and deleted lines between two texts without keeping hunks.
pub(crate) fn count_changes(old: &str, new: &str) -> (u32, u32) {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(old, new);
    let mut additions = 0_u32;
    let mut deletions = 0_u32;
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Equal => {}
                ChangeTag::Delete => deletions += 1,
                ChangeTag::Insert => additions += 1,
            }
        }
    }
    (additions, deletions)
}

/// Hierarchical review order shared by the stream and the file tree.
///
/// Folders sort before sibling files at each level, names alphabetically
/// within, so a pre-order walk of the tree reads in exactly this order.
/// In particular `src/main.rs` sorts before the sibling file `src-old.rs`
/// (the directory wins at the first differing segment), matching the
/// folders-first tree layout.
pub(crate) fn compare_review_paths(a: &str, b: &str) -> Ordering {
    let mut a_segments = a.split('/');
    let mut b_segments = b.split('/');
    loop {
        match (a_segments.next(), b_segments.next()) {
            (Some(a_seg), Some(b_seg)) => {
                if a_seg != b_seg {
                    // One side descends into a subdirectory here; it wins.
                    // (File paths can never be a strict prefix of another,
                    // so reaching different final segments is impossible.)
                    let a_continues = a_segments.clone().next().is_some();
                    let b_continues = b_segments.clone().next().is_some();
                    if a_continues != b_continues {
                        return if a_continues {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        };
                    }
                    return a_seg.cmp(b_seg);
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

fn group_ranges(group: &[similar::DiffOp]) -> (Range<usize>, Range<usize>) {
    let mut old_start = usize::MAX;
    let mut old_end = 0_usize;
    let mut new_start = usize::MAX;
    let mut new_end = 0_usize;
    for op in group {
        let (old_range, new_range) = op_ranges(op);
        old_start = old_start.min(old_range.start);
        old_end = old_end.max(old_range.end);
        new_start = new_start.min(new_range.start);
        new_end = new_end.max(new_range.end);
    }
    if old_start == usize::MAX {
        old_start = old_end;
    }
    if new_start == usize::MAX {
        new_start = new_end;
    }
    (old_start..old_end, new_start..new_end)
}

fn op_ranges(op: &similar::DiffOp) -> (Range<usize>, Range<usize>) {
    match *op {
        similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } => (old_index..old_index + len, new_index..new_index + len),
        similar::DiffOp::Delete {
            old_index,
            old_len,
            new_index,
        } => (old_index..old_index + old_len, new_index..new_index),
        similar::DiffOp::Insert {
            old_index,
            new_index,
            new_len,
        } => (old_index..old_index, new_index..new_index + new_len),
        similar::DiffOp::Replace {
            old_index,
            old_len,
            new_index,
            new_len,
        } => (
            old_index..old_index + old_len,
            new_index..new_index + new_len,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_carry_full_file_line_numbers() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        let new = "one\nTWO\nthree\nfour\nfive\nSIX\nseven\n";
        let (hunks, truncated) = diff_text(old, new);
        assert!(!truncated);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.collapsed_before, 0);
        let changed: Vec<&HunkLine> = hunk
            .lines
            .iter()
            .filter(|line| line.tag != LineTag::Context)
            .collect();
        assert_eq!(changed.len(), 4);
        // Unified order: removals before additions at one position.
        assert_eq!(changed[0].old_no, Some(2));
        assert_eq!(changed[0].new_no, None);
        assert_eq!(changed[0].text, "two");
        assert_eq!(changed[1].old_no, None);
        assert_eq!(changed[1].new_no, Some(2));
        assert_eq!(changed[1].text, "TWO");
    }

    #[test]
    fn distant_changes_split_into_hunks_with_collapsed_counts() {
        let old = (1..=30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = old
            .replacen("line 2", "line TWO", 1)
            .replacen("line 28", "line TWENTYEIGHT", 1);
        let (hunks, truncated) = diff_text(&old, &new);
        assert!(!truncated);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].collapsed_before, 0);
        assert!(hunks[1].collapsed_before > 10);
        assert_eq!(hunks[1].new_start, 25);
    }

    #[test]
    fn change_counts_match_hunk_totals() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nd\n";
        let (additions, deletions) = count_changes(old, new);
        assert_eq!((additions, deletions), (2, 1));
    }

    #[test]
    fn status_labels_and_abbrevs_cover_all_variants() {
        assert_eq!(
            [
                FileStatus::Added,
                FileStatus::Modified,
                FileStatus::Deleted,
                FileStatus::Renamed,
                FileStatus::TypeChanged,
            ]
            .map(FileStatus::abbrev),
            ["A", "M", "D", "R", "T"]
        );
    }

    #[test]
    fn review_order_is_folders_first_then_alphabetical() {
        use std::cmp::Ordering::*;
        // Sibling file vs directory: the directory wins.
        assert_eq!(compare_review_paths("src/main.rs", "src-old.rs"), Less);
        assert_eq!(compare_review_paths("src-old.rs", "src/main.rs"), Greater);
        // Plain alphabetical within one level.
        assert_eq!(compare_review_paths("b.rs", "a.rs"), Greater);
        assert_eq!(compare_review_paths("a.rs", "a.rs"), Equal);
        // Nested directory beats a root file at the first difference.
        assert_eq!(compare_review_paths("a/z.rs", "a-b.rs"), Less);

        let mut paths = vec![
            "README.md",
            "src/ui/tree.rs",
            "src-old.rs",
            "a/b.rs",
            "src/main.rs",
        ];
        paths.sort_by(|a, b| compare_review_paths(a, b));
        assert_eq!(
            paths,
            vec![
                "a/b.rs",
                "src/ui/tree.rs",
                "src/main.rs",
                "README.md",
                "src-old.rs",
            ]
        );
    }
}
