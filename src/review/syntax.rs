//! Cached syntax styles for review diff lines.
//!
//! Highlighting is computed once with the diff on the background executor.
//! The nested cache mirrors `files -> hunks -> lines`, so virtualized row
//! rendering only has to look up already-resolved byte ranges.

use std::path::Path;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::model::{FileContent, Hunk, LineTag, ReviewDiff};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

const DARK_THEME: &str = "base16-ocean.dark";
const LIGHT_THEME: &str = "InspiredGitHub";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyntaxSpan {
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) rgba: u32,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
}

#[derive(Default)]
struct FileHighlights {
    /// One span list per line, grouped by hunk.
    hunks: Vec<Vec<Vec<SyntaxSpan>>>,
}

#[derive(Default)]
struct PaletteHighlights {
    files: Vec<FileHighlights>,
}

/// Both appearance variants are cached because changing the application
/// theme does not reload the underlying git diff.
#[derive(Default)]
pub(crate) struct SyntaxHighlights {
    light: PaletteHighlights,
    dark: PaletteHighlights,
}

impl SyntaxHighlights {
    pub(crate) fn line(&self, dark: bool, file: usize, hunk: usize, line: usize) -> &[SyntaxSpan] {
        let palette = if dark { &self.dark } else { &self.light };
        palette
            .files
            .get(file)
            .and_then(|file| file.hunks.get(hunk))
            .and_then(|hunk| hunk.get(line))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

pub(crate) fn highlight(diff: &ReviewDiff) -> SyntaxHighlights {
    SyntaxHighlights {
        light: highlight_with_theme(diff, &THEMES.themes[LIGHT_THEME]),
        dark: highlight_with_theme(diff, &THEMES.themes[DARK_THEME]),
    }
}

fn highlight_with_theme(diff: &ReviewDiff, theme: &Theme) -> PaletteHighlights {
    let files = diff
        .files
        .iter()
        .map(|file| {
            let FileContent::Text { hunks, .. } = &file.content else {
                return FileHighlights::default();
            };
            let Some(syntax) = syntax_for_path(&file.path) else {
                return FileHighlights::default();
            };
            FileHighlights {
                hunks: hunks
                    .iter()
                    .map(|hunk| highlight_hunk(hunk, syntax, theme))
                    .collect(),
            }
        })
        .collect();
    PaletteHighlights { files }
}

fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let path = Path::new(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    SYNTAXES
        .find_syntax_by_extension(file_name)
        .or_else(|| SYNTAXES.find_syntax_by_extension(extension))
}

fn highlight_hunk(hunk: &Hunk, syntax: &SyntaxReference, theme: &Theme) -> Vec<Vec<SyntaxSpan>> {
    let mut old = HighlightLines::new(syntax, theme);
    let mut new = HighlightLines::new(syntax, theme);
    let base_color = theme.settings.foreground;

    hunk.lines
        .iter()
        .map(|line| {
            // The default syntax set expects line endings. Keep the diff model's
            // newline-free text untouched and clamp returned spans before it.
            let source = format!("{}\n", line.text);
            match line.tag {
                LineTag::Deletion => highlight_line(&mut old, &source, line.text.len(), base_color),
                LineTag::Addition => highlight_line(&mut new, &source, line.text.len(), base_color),
                LineTag::Context => {
                    // Advance both sides through shared context. Their parser
                    // states may differ after a changed multiline construct;
                    // the new side is what the worktree will retain.
                    let _ = old.highlight_line(&source, &SYNTAXES);
                    highlight_line(&mut new, &source, line.text.len(), base_color)
                }
            }
        })
        .collect()
}

fn highlight_line(
    highlighter: &mut HighlightLines<'_>,
    source: &str,
    text_len: usize,
    base_color: Option<syntect::highlighting::Color>,
) -> Vec<SyntaxSpan> {
    let Ok(parts) = highlighter.highlight_line(source, &SYNTAXES) else {
        return Vec::new();
    };
    let mut offset = 0;
    parts
        .into_iter()
        .filter_map(|(style, text)| {
            let start = offset.min(text_len);
            offset += text.len();
            let end = offset.min(text_len);
            (start < end && is_visible_style(style, base_color)).then(|| SyntaxSpan {
                range: start..end,
                rgba: u32::from_be_bytes([
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                    style.foreground.a,
                ]),
                bold: style.font_style.contains(FontStyle::BOLD),
                italic: style.font_style.contains(FontStyle::ITALIC),
                underline: style.font_style.contains(FontStyle::UNDERLINE),
            })
        })
        .collect()
}

fn is_visible_style(style: Style, base_color: Option<syntect::highlighting::Color>) -> bool {
    base_color != Some(style.foreground) || !style.font_style.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::model::{ChangedFile, FileStatus, HunkLine};

    fn rust_diff() -> ReviewDiff {
        ReviewDiff {
            files: vec![ChangedFile {
                path: "src/main.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                additions: 1,
                deletions: 1,
                content: FileContent::Text {
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 1,
                        collapsed_before: 0,
                        lines: vec![
                            HunkLine {
                                tag: LineTag::Deletion,
                                old_no: Some(1),
                                new_no: None,
                                text: "fn old() {}".into(),
                            },
                            HunkLine {
                                tag: LineTag::Addition,
                                old_no: None,
                                new_no: Some(1),
                                text: "fn new() {}".into(),
                            },
                        ],
                    }],
                    truncated: false,
                },
            }],
            base_commit: String::new(),
            head_commit: String::new(),
            base_ref: None,
            head_branch: None,
        }
    }

    #[test]
    fn recognizes_extensions_and_special_file_names() {
        assert_eq!(syntax_for_path("src/lib.rs").unwrap().name, "Rust");
        assert!(syntax_for_path("Makefile").is_some());
        assert!(syntax_for_path("notes.unknown-extension").is_none());
    }

    #[test]
    fn caches_both_sides_and_both_appearance_variants() {
        let highlights = highlight(&rust_diff());
        for dark in [false, true] {
            for line in [0, 1] {
                let spans = highlights.line(dark, 0, 0, line);
                assert!(
                    spans
                        .iter()
                        .any(|span| span.range.start == 0 && span.range.end >= 2),
                    "Rust's `fn` keyword should be highlighted"
                );
            }
        }
        assert_ne!(
            highlights.line(false, 0, 0, 1)[0].rgba,
            highlights.line(true, 0, 0, 1)[0].rgba,
        );
    }

    #[test]
    fn unknown_file_types_fall_back_to_plain_text() {
        let mut diff = rust_diff();
        diff.files[0].path = "data.unknown-extension".into();
        let highlights = highlight(&diff);
        assert!(highlights.line(false, 0, 0, 0).is_empty());
        assert!(highlights.line(true, 0, 0, 1).is_empty());
    }
}
