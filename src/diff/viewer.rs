//! A small reusable, virtualized unified-diff viewer.
//!
//! Review keeps its richer multi-file stream and comments, while Git uses
//! this one-file projection. Both consume the same bounded model and cached
//! syntax highlights from this module's siblings.

use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, IntoElement, ListAlignment, ListState, ParentElement, Render, Styled,
    StyledText, Window, div, list, px, rgb,
};

use crate::{fonts::TERMINAL_FONT_FAMILY, metrics::review_font_size};

use super::{
    model::{ChangedFile, FileContent, Hunk, HunkLine, LineTag, ReviewDiff, UnavailableReason},
    syntax::{self, SyntaxHighlights},
};

const ROW_HEIGHT: f32 = 24.;
const DELETION_BG: u32 = 0x33191a;
const ADDITION_BG: u32 = 0x0e2a1a;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffRow {
    HunkHeader(usize),
    Collapsed(usize),
    Line { hunk: usize, line: usize },
    Truncated,
    Unavailable,
    NoChanges,
}

pub(crate) struct PreparedFileDiff {
    file: ChangedFile,
    rows: Vec<DiffRow>,
    syntax: SyntaxHighlights,
}

impl PreparedFileDiff {
    pub(crate) fn new(file: ChangedFile) -> Self {
        let rows = rows_for(&file);
        let diff = ReviewDiff {
            files: vec![file.clone()],
            file_versions: Default::default(),
            base_commit: String::new(),
            head_commit: String::new(),
            base_ref: None,
            head_branch: None,
        };
        Self {
            file,
            rows,
            syntax: syntax::highlight(&diff),
        }
    }
}

enum ViewerState {
    Empty,
    Loading(String),
    Loaded(Rc<PreparedFileDiff>),
    Failed { path: String, message: String },
}

pub(crate) struct DiffViewer {
    state: ViewerState,
    list: ListState,
}

impl DiffViewer {
    pub(crate) fn new() -> Self {
        Self {
            state: ViewerState::Empty,
            list: ListState::new(0, ListAlignment::Top, px(300.)),
        }
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.state = ViewerState::Empty;
        self.list.reset(0);
        cx.notify();
    }

    pub(crate) fn set_loading(&mut self, path: String, cx: &mut Context<Self>) {
        self.state = ViewerState::Loading(path);
        self.list.reset(0);
        cx.notify();
    }

    pub(crate) fn set_loaded(&mut self, prepared: PreparedFileDiff, cx: &mut Context<Self>) {
        self.list.reset(prepared.rows.len());
        self.state = ViewerState::Loaded(Rc::new(prepared));
        cx.notify();
    }

    pub(crate) fn set_error(&mut self, path: String, message: String, cx: &mut Context<Self>) {
        self.state = ViewerState::Failed { path, message };
        self.list.reset(0);
        cx.notify();
    }

    fn message(title: String, detail: String, color: u32) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(rgb(color))
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(560.))
                    .text_center()
                    .text_sm()
                    .text_color(rgb(0x858989))
                    .child(detail),
            )
            .into_any_element()
    }
}

impl Render for DiffViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            ViewerState::Empty => Self::message(
                "Select a changed file".into(),
                "Its staged or working-tree diff will appear here.".into(),
                0xe7e7e7,
            ),
            ViewerState::Loading(path) => {
                Self::message("Loading diff…".into(), path.clone(), 0xe7e7e7)
            }
            ViewerState::Failed { path, message } => {
                Self::message(format!("Could not load {path}"), message.clone(), 0xf87171)
            }
            ViewerState::Loaded(loaded) => {
                let loaded = loaded.clone();
                let dark = cx.theme().is_dark();
                list(self.list.clone(), move |index, _, _| {
                    render_row(&loaded, loaded.rows[index], dark)
                })
                .size_full()
                .into_any_element()
            }
        }
    }
}

fn rows_for(file: &ChangedFile) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    match &file.content {
        FileContent::Text { hunks, truncated } => {
            for (hunk, item) in hunks.iter().enumerate() {
                if item.collapsed_before > 0 {
                    rows.push(DiffRow::Collapsed(hunk));
                }
                rows.push(DiffRow::HunkHeader(hunk));
                rows.extend(
                    item.lines
                        .iter()
                        .enumerate()
                        .map(|(line, _)| DiffRow::Line { hunk, line }),
                );
            }
            if *truncated {
                rows.push(DiffRow::Truncated);
            }
            if hunks.is_empty() && !truncated {
                rows.push(DiffRow::NoChanges);
            }
        }
        FileContent::Unavailable(_) => rows.push(DiffRow::Unavailable),
    }
    rows
}

fn render_row(loaded: &PreparedFileDiff, row: DiffRow, dark: bool) -> AnyElement {
    match row {
        DiffRow::HunkHeader(hunk) => hunk_header(&hunks(&loaded.file)[hunk]),
        DiffRow::Collapsed(hunk) => message_row(
            format!(
                "··· {} unchanged lines ···",
                hunks(&loaded.file)[hunk].collapsed_before
            ),
            0x555a5a,
        ),
        DiffRow::Line { hunk, line } => line_row(
            &hunks(&loaded.file)[hunk].lines[line],
            loaded.syntax.line(dark, 0, hunk, line),
        ),
        DiffRow::Truncated => message_row("··· diff truncated for performance ···", 0xd9a648),
        DiffRow::Unavailable => {
            let reason = match loaded.file.content {
                FileContent::Unavailable(reason) => reason,
                FileContent::Text { .. } => UnavailableReason::Missing,
            };
            message_row(format!("○ {}", reason.label()), 0x737878)
        }
        DiffRow::NoChanges => message_row("No line changes", 0x555a5a),
    }
}

fn hunks(file: &ChangedFile) -> &[Hunk] {
    match &file.content {
        FileContent::Text { hunks, .. } => hunks,
        FileContent::Unavailable(_) => &[],
    }
}

fn hunk_header(hunk: &Hunk) -> AnyElement {
    div()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .px_3()
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

fn line_row(line: &HunkLine, spans: &[super::syntax::SyntaxSpan]) -> AnyElement {
    let (background, sign, sign_color) = match line.tag {
        LineTag::Context => (None, " ", 0x555a5a),
        LineTag::Deletion => (Some(DELETION_BG), "-", 0xf87171),
        LineTag::Addition => (Some(ADDITION_BG), "+", 0x4ade80),
    };
    div()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(TERMINAL_FONT_FAMILY)
        .text_size(px(review_font_size()))
        .when_some(background, |row, color| row.bg(rgb(color)))
        .child(line_number(line.old_no))
        .child(line_number(line.new_no))
        .child(
            div()
                .w(px(20.))
                .flex_none()
                .text_color(rgb(sign_color))
                .child(sign),
        )
        .child(div().flex_1().min_w_0().child(
            StyledText::new(line.text.clone()).with_highlights(syntax::styled_highlights(spans)),
        ))
        .into_any_element()
}

fn line_number(number: Option<u32>) -> AnyElement {
    div()
        .w(px(44.))
        .flex_none()
        .text_color(rgb(0x555a5a))
        .child(number.map_or_else(|| "     ".to_owned(), |number| format!("{number:>5}")))
        .into_any_element()
}

fn message_row(message: impl Into<String>, color: u32) -> AnyElement {
    div()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .px_3()
        .text_sm()
        .text_color(rgb(color))
        .child(message.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::{FileStatus, diff_text};

    #[test]
    fn prepared_diff_flattens_hunks_for_virtual_rendering() {
        let (hunks, truncated) = diff_text("old\n", "new\n");
        let prepared = PreparedFileDiff::new(ChangedFile {
            path: "src/main.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            content: FileContent::Text { hunks, truncated },
        });
        assert!(matches!(prepared.rows[0], DiffRow::HunkHeader(0)));
        assert!(prepared.rows.len() >= 3);
    }
}
