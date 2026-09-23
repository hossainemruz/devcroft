//! Virtualized diff stream rows for the Review tab.
//!
//! The stream flattens a loaded diff into [`StreamRow`]s once per load
//! ([`flatten`]); the view renders only the visible window through
//! a variable-height list. Diff rows are [`ROW_H`] tall; the view can append
//! an inline comment editor to the row at the end of a selected range.
//! Per-load flattening plus per-frame visible-only rendering is what keeps
//! large diffs smooth: fully expanded, a big review would otherwise be tens
//! of thousands of flex elements laid out on every frame.
//!
//! The message rows ("no line changes", truncation) are crate-visible so the
//! text diff tool says the same things the same way; the diff rows themselves
//! are the review's, since the tool renders its own side-by-side cells.

use std::collections::HashSet;

use gpui_kit::component::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Styled, StyledText, div, px, rgb,
};

use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::metrics::review_font_size;

use super::ReviewView;
use super::model::{
    ChangedFile, FileStatus, Hunk, HunkLine, LineTag, ReviewDiff, UnavailableReason,
};

/// Height of every diff content row. File headers use [`FILE_HEADER_H`] so
/// they stand out from code lines (see module docs).
pub(crate) const ROW_H: f32 = 24.0;
/// Tint of a removed line. Shared so the diff tool's cells read as the same
/// kind of change the review rows do.
pub(crate) const DELETION_BG: u32 = 0x33191a;
/// Tint of an added line, shared the same way.
pub(crate) const ADDITION_BG: u32 = 0x0e2a1a;
/// Height of file section headers. Taller than [`ROW_H`] so each file
/// boundary is noticeable while scrolling.
pub(crate) const FILE_HEADER_H: f32 = 36.0;

/// One virtualized row of the diff stream, addressing file/hunk/line by
/// index into a [`LoadedReview`](super::LoadedReview).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamRow {
    FileHeader {
        file: usize,
    },
    HunkHeader {
        file: usize,
        hunk: usize,
    },
    Line {
        file: usize,
        hunk: usize,
        line: usize,
    },
    Collapsed {
        file: usize,
        hunk: usize,
    },
    Truncated {
        file: usize,
    },
    Unavailable {
        file: usize,
    },
    NoChanges {
        file: usize,
    },
}

impl StreamRow {
    pub(crate) fn file(self) -> usize {
        match self {
            Self::FileHeader { file }
            | Self::HunkHeader { file, .. }
            | Self::Line { file, .. }
            | Self::Collapsed { file, .. }
            | Self::Truncated { file }
            | Self::Unavailable { file }
            | Self::NoChanges { file } => file,
        }
    }
}

/// Flatten a diff into rows plus each file's starting row.
///
/// Row order mirrors the input file order, so callers must pass files in
/// [`compare_review_paths`](super::model::compare_review_paths) order to
/// match the sidebar. Files in `collapsed` (by repo-relative path) emit only
/// their [`StreamRow::FileHeader`]; expanding re-flattens to restore content.
pub(crate) fn flatten(
    diff: &ReviewDiff,
    collapsed: &HashSet<String>,
) -> (Vec<StreamRow>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_row_start = Vec::with_capacity(diff.files.len());
    for (file_ix, file) in diff.files.iter().enumerate() {
        file_row_start.push(rows.len());
        rows.push(StreamRow::FileHeader { file: file_ix });
        if collapsed.contains(&file.path) {
            continue;
        }
        match &file.content {
            super::model::FileContent::Text { hunks, truncated } => {
                for (hunk_ix, hunk) in hunks.iter().enumerate() {
                    if hunk.collapsed_before > 0 {
                        rows.push(StreamRow::Collapsed {
                            file: file_ix,
                            hunk: hunk_ix,
                        });
                    }
                    rows.push(StreamRow::HunkHeader {
                        file: file_ix,
                        hunk: hunk_ix,
                    });
                    for (line_ix, _) in hunk.lines.iter().enumerate() {
                        rows.push(StreamRow::Line {
                            file: file_ix,
                            hunk: hunk_ix,
                            line: line_ix,
                        });
                    }
                }
                if *truncated {
                    rows.push(StreamRow::Truncated { file: file_ix });
                }
                if hunks.is_empty() && !truncated {
                    rows.push(StreamRow::NoChanges { file: file_ix });
                }
            }
            super::model::FileContent::Unavailable(_) => {
                rows.push(StreamRow::Unavailable { file: file_ix });
            }
        }
    }
    (rows, file_row_start)
}

pub(crate) fn status_color(status: FileStatus) -> u32 {
    match status {
        FileStatus::Added => 0x4ade80,
        FileStatus::Modified => 0xfbbf24,
        FileStatus::Deleted => 0xf87171,
        FileStatus::Renamed => 0x7dd3fc,
        FileStatus::TypeChanged => 0xc4b5fd,
    }
}

/// Render one flattened row. Pure over the loaded diff except the file
/// header's viewed/collapsed toggles, which update the view through its entity.
pub(crate) fn render_row(
    loaded: &super::LoadedReview,
    row: StreamRow,
    viewed: &HashSet<String>,
    collapsed: &HashSet<String>,
    view: &Entity<ReviewView>,
    selected: bool,
    dark: bool,
) -> AnyElement {
    let file = &loaded.diff.files[row.file()];
    match row {
        StreamRow::FileHeader { .. } => file_header_row(
            file,
            viewed.contains(&file.path),
            collapsed.contains(&file.path),
            view,
        ),
        StreamRow::HunkHeader { hunk, .. } => match &file.content {
            super::model::FileContent::Text { hunks, .. } => hunk_header_row(&hunks[hunk]),
            super::model::FileContent::Unavailable(_) => unavailable_row_for(file),
        },
        StreamRow::Line { hunk, line, .. } => match &file.content {
            super::model::FileContent::Text { hunks, .. } => line_row(
                &hunks[hunk].lines[line],
                loaded.syntax.line(dark, row.file(), hunk, line),
                selected,
            ),
            super::model::FileContent::Unavailable(_) => unavailable_row_for(file),
        },
        StreamRow::Collapsed { hunk, .. } => match &file.content {
            super::model::FileContent::Text { hunks, .. } => {
                collapsed_row(hunks[hunk].collapsed_before)
            }
            super::model::FileContent::Unavailable(_) => unavailable_row_for(file),
        },
        StreamRow::Truncated { .. } => truncated_row(),
        StreamRow::Unavailable { .. } => unavailable_row_for(file),
        StreamRow::NoChanges { .. } => no_changes_row(),
    }
}

pub(crate) fn file_header_row(
    file: &ChangedFile,
    is_viewed: bool,
    is_collapsed: bool,
    view: &Entity<ReviewView>,
) -> AnyElement {
    let path = file.path.clone();
    let collapse_path = file.path.clone();
    let toggle_view = view.clone();
    let toggle_collapse = view.clone();
    let stats = if file.additions > 0 || file.deletions > 0 {
        format!("+{} −{}", file.additions, file.deletions)
    } else {
        String::new()
    };
    let title = match (&file.status, &file.old_path) {
        (FileStatus::Renamed, Some(old)) => format!("{old} → {}", file.path),
        _ => file.path.clone(),
    };
    div()
        .h(px(FILE_HEADER_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .gap_2()
        .bg(rgb(0x151a1a))
        .border_b_1()
        .border_color(rgb(0x2a2e2e))
        .child(
            div()
                .flex_none()
                .w(px(20.))
                .text_sm()
                .text_color(rgb(0x858989))
                .cursor_pointer()
                .child(if is_collapsed { "▸" } else { "▾" })
                .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                    cx.update_entity(&toggle_collapse, |view, cx| {
                        view.toggle_collapsed(&collapse_path, cx);
                    });
                }),
        )
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(rgb(status_color(file.status)))
                .child(file.status.label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .font_semibold()
                .child(title),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(rgb(0x858989))
                .child(stats),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .cursor_pointer()
                .text_color(if is_viewed {
                    rgb(0x4ade80)
                } else {
                    rgb(0x555a5a)
                })
                .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                    cx.update_entity(&toggle_view, |view, cx| {
                        view.toggle_viewed(&path, cx);
                    });
                })
                .child(if is_viewed { "✓" } else { "○" }),
        )
        .into_any_element()
}

fn line_row(line: &HunkLine, spans: &[super::syntax::SyntaxSpan], selected: bool) -> AnyElement {
    let (background, sign, sign_color) = match line.tag {
        LineTag::Context => (None, " ", 0x555a5a),
        LineTag::Deletion => (Some(DELETION_BG), "-", 0xf87171),
        LineTag::Addition => (Some(ADDITION_BG), "+", 0x4ade80),
    };
    let old_no = match line.old_no {
        Some(number) => format!("{number:>5}"),
        None => "     ".to_owned(),
    };
    let new_no = match line.new_no {
        Some(number) => format!("{number:>5}"),
        None => "     ".to_owned(),
    };
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(TERMINAL_FONT_FAMILY)
        .text_size(px(review_font_size()))
        .line_height(px(ROW_H))
        .when_some(background, |this, color| this.bg(rgb(color)))
        .when(selected, |this| this.bg(rgb(0x203442)))
        .child(
            div()
                .w(px(44.))
                .flex_none()
                .text_color(rgb(0x555a5a))
                .child(old_no),
        )
        .child(
            div()
                .w(px(44.))
                .flex_none()
                .text_color(rgb(0x555a5a))
                .child(new_no),
        )
        .child(
            div()
                .w(px(20.))
                .flex_none()
                .text_color(rgb(sign_color))
                .child(sign),
        )
        .child(
            div().flex_1().min_w_0().child(
                StyledText::new(line.text.clone())
                    .with_highlights(super::syntax::styled_highlights(spans)),
            ),
        )
        .into_any_element()
}

fn hunk_header_row(hunk: &Hunk) -> AnyElement {
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .gap_2()
        .bg(rgb(0x101314))
        .text_color(rgb(0x7d8585))
        .font_family(TERMINAL_FONT_FAMILY)
        .text_size(px(review_font_size()))
        .child(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ))
        .into_any_element()
}

fn collapsed_row(count: u32) -> AnyElement {
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .text_color(rgb(0x555a5a))
        .font_family(TERMINAL_FONT_FAMILY)
        .text_size(px(review_font_size()))
        .child(format!("··· {count} unchanged lines ···"))
        .into_any_element()
}

fn unavailable_row_for(file: &ChangedFile) -> AnyElement {
    let reason = match &file.content {
        super::model::FileContent::Unavailable(reason) => *reason,
        super::model::FileContent::Text { .. } => UnavailableReason::Missing,
    };
    unavailable_row(reason)
}

fn unavailable_row(reason: UnavailableReason) -> AnyElement {
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .text_color(rgb(0x737878))
        .child(format!("○ {}", reason.label()))
        .into_any_element()
}

pub(crate) fn no_changes_row() -> AnyElement {
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .text_color(rgb(0x555a5a))
        .text_sm()
        .child("No line changes")
        .into_any_element()
}

pub(crate) fn truncated_row() -> AnyElement {
    div()
        .h(px(ROW_H))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px_3()
        .text_color(rgb(0xd9a648))
        .child("··· diff truncated for performance ···")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        ChangedFile, FileContent, FileStatus, Hunk, HunkLine, LineTag, ReviewDiff,
    };
    use super::*;

    fn line(tag: LineTag) -> HunkLine {
        HunkLine {
            tag,
            old_no: Some(1),
            new_no: Some(1),
            text: "x".to_owned(),
        }
    }

    #[test]
    fn flatten_assigns_every_row_and_file_start() {
        let diff = ReviewDiff {
            files: vec![
                ChangedFile {
                    path: "a.rs".to_owned(),
                    old_path: None,
                    status: FileStatus::Modified,
                    additions: 1,
                    deletions: 1,
                    content: FileContent::Text {
                        hunks: vec![Hunk {
                            old_start: 1,
                            old_lines: 3,
                            new_start: 1,
                            new_lines: 3,
                            collapsed_before: 10,
                            lines: vec![
                                line(LineTag::Context),
                                line(LineTag::Deletion),
                                line(LineTag::Addition),
                            ],
                        }],
                        truncated: false,
                    },
                },
                ChangedFile {
                    path: "b.bin".to_owned(),
                    old_path: None,
                    status: FileStatus::Added,
                    additions: 0,
                    deletions: 0,
                    content: FileContent::Unavailable(UnavailableReason::Binary),
                },
            ],
            base_commit: String::new(),
            head_commit: String::new(),
            base_ref: None,
            head_branch: None,
        };
        let (rows, starts) = flatten(&diff, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                StreamRow::FileHeader { file: 0 },
                StreamRow::Collapsed { file: 0, hunk: 0 },
                StreamRow::HunkHeader { file: 0, hunk: 0 },
                StreamRow::Line {
                    file: 0,
                    hunk: 0,
                    line: 0
                },
                StreamRow::Line {
                    file: 0,
                    hunk: 0,
                    line: 1
                },
                StreamRow::Line {
                    file: 0,
                    hunk: 0,
                    line: 2
                },
                StreamRow::FileHeader { file: 1 },
                StreamRow::Unavailable { file: 1 },
            ]
        );
        assert_eq!(starts, vec![0, 6]);
        assert!(rows.iter().all(|row| row.file() < diff.files.len()));
    }

    #[test]
    fn flatten_collapsed_file_emits_only_its_header() {
        let diff = ReviewDiff {
            files: vec![
                ChangedFile {
                    path: "a.rs".to_owned(),
                    old_path: None,
                    status: FileStatus::Modified,
                    additions: 1,
                    deletions: 1,
                    content: FileContent::Text {
                        hunks: vec![Hunk {
                            old_start: 1,
                            old_lines: 3,
                            new_start: 1,
                            new_lines: 3,
                            collapsed_before: 0,
                            lines: vec![line(LineTag::Context)],
                        }],
                        truncated: false,
                    },
                },
                ChangedFile {
                    path: "b.rs".to_owned(),
                    old_path: None,
                    status: FileStatus::Modified,
                    additions: 1,
                    deletions: 0,
                    content: FileContent::Text {
                        hunks: vec![Hunk {
                            old_start: 1,
                            old_lines: 1,
                            new_start: 1,
                            new_lines: 2,
                            collapsed_before: 0,
                            lines: vec![line(LineTag::Addition)],
                        }],
                        truncated: false,
                    },
                },
            ],
            base_commit: String::new(),
            head_commit: String::new(),
            base_ref: None,
            head_branch: None,
        };
        let collapsed = HashSet::from(["a.rs".to_owned()]);
        let (rows, starts) = flatten(&diff, &collapsed);
        assert_eq!(
            rows,
            vec![
                StreamRow::FileHeader { file: 0 },
                StreamRow::FileHeader { file: 1 },
                StreamRow::HunkHeader { file: 1, hunk: 0 },
                StreamRow::Line {
                    file: 1,
                    hunk: 0,
                    line: 0
                },
            ]
        );
        assert_eq!(starts, vec![0, 1]);
    }
}
