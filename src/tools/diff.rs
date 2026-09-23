//! Text comparison for the diff tool.
//!
//! The line diff itself lives in [`crate::review::model`], the Review tab's
//! model, so the tool and the review agree on line tags, numbering, and the
//! Myers algorithm. This module only adapts pasted text to it: line endings
//! are normalized first, so a paste that drops the final newline or mixes
//! CRLF does not report a change a reader cannot see; the listing keeps every
//! line of both documents rather than collapsing unchanged runs, so a reader
//! can reference any line of the pasted text; and the result carries the
//! counts the dialog shows.
//!
//! Highlighting comes from [`crate::review::syntax`] as well, but the review
//! resolves a syntax per file path while pasted text has none. The dialog
//! offers [`languages`] in a picker instead, and [`highlight`] paints the
//! comparison with the chosen one.

use crate::review::model::{self, Hunk, LineTag};
use crate::review::syntax::{self, SyntaxHighlights, SyntaxReference};

/// A computed comparison of two pasted texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffResult {
    pub(crate) hunks: Vec<Hunk>,
    /// True when the line cap cut the emitted hunks short.
    pub(crate) truncated: bool,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
}

/// Compare `old` against `new` into one merged listing.
pub(crate) fn compare(old: &str, new: &str) -> DiffResult {
    let old = normalize(old);
    let new = normalize(new);
    let (hunks, truncated) = model::diff_text_merged(&old, &new);
    let (additions, deletions) = model::count_changes(&old, &new);
    DiffResult {
        hunks,
        truncated,
        additions,
        deletions,
    }
}

/// A line of the merged listing, by index into the comparison's hunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LineRef {
    pub(crate) hunk: usize,
    pub(crate) line: usize,
}

/// One row of the side-by-side view: the old side's line and the new side's
/// line. Either is `None` when that side has nothing on this row — a removal
/// leaves the new cell blank, an addition leaves the old one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SplitRow {
    pub(crate) old: Option<LineRef>,
    pub(crate) new: Option<LineRef>,
}

/// Pair a merged listing into side-by-side rows.
///
/// A change run pairs its removals with its additions row by row (the listing
/// already orders removals before additions), and the shorter side is padded
/// with blank cells; a context line shares a row with itself. Every line of
/// both documents therefore keeps its own row and its own number, which is
/// what makes the columns line up against the pasted text.
pub(crate) fn split_rows(result: &DiffResult) -> Vec<SplitRow> {
    fn flush(rows: &mut Vec<SplitRow>, old: &mut Vec<LineRef>, new: &mut Vec<LineRef>) {
        for index in 0..old.len().max(new.len()) {
            rows.push(SplitRow {
                old: old.get(index).copied(),
                new: new.get(index).copied(),
            });
        }
        old.clear();
        new.clear();
    }

    let mut rows = Vec::new();
    let mut pending_old: Vec<LineRef> = Vec::new();
    let mut pending_new: Vec<LineRef> = Vec::new();
    for (hunk_ix, hunk) in result.hunks.iter().enumerate() {
        for (line_ix, line) in hunk.lines.iter().enumerate() {
            let reference = LineRef {
                hunk: hunk_ix,
                line: line_ix,
            };
            match line.tag {
                LineTag::Context => {
                    flush(&mut rows, &mut pending_old, &mut pending_new);
                    rows.push(SplitRow {
                        old: Some(reference),
                        new: Some(reference),
                    });
                }
                LineTag::Deletion => pending_old.push(reference),
                LineTag::Addition => pending_new.push(reference),
            }
        }
    }
    flush(&mut rows, &mut pending_old, &mut pending_new);
    rows
}

/// The language the picker starts on: no highlighting at all.
pub(crate) const PLAIN_LANGUAGE: &str = "Plain Text";

/// The languages the picker offers: plain text first, then every bundled
/// syntax in alphabetical order. The names are both the picker's values and
/// the lookup key for [`highlight`].
pub(crate) fn languages() -> Vec<&'static str> {
    let mut languages = vec![PLAIN_LANGUAGE];
    languages.extend(
        syntax::syntax_names()
            .into_iter()
            .filter(|name| *name != PLAIN_LANGUAGE),
    );
    languages
}

/// Resolve a picked or persisted name to a language that exists. An unknown
/// name (a stale setting, a syntax dropped from the bundle) falls back to
/// plain text rather than highlighting nothing silently.
pub(crate) fn resolve_language(name: &str) -> &'static str {
    languages()
        .into_iter()
        .find(|language| *language == name)
        .unwrap_or(PLAIN_LANGUAGE)
}

/// The languages the picker offers that the paste editors can also color:
/// picker name to the editor highlighter's language id.
///
/// The tool colors two surfaces with two highlighters. The diff pane uses the
/// Review tab's syntect syntaxes, which the picker is built from; the paste
/// editors use the editor's tree-sitter grammars, which cover a subset of
/// those languages and are enabled in `Cargo.toml`. A picker language missing
/// here colors the pane only, and the editors stay plain.
pub(crate) const EDITOR_LANGUAGES: &[(&str, &str)] = &[
    ("Bourne Again Shell (bash)", "bash"),
    ("C", "c"),
    ("C#", "csharp"),
    ("C++", "cpp"),
    ("CSS", "css"),
    ("Diff", "diff"),
    ("Go", "go"),
    ("HTML", "html"),
    ("Java", "java"),
    ("JavaScript", "javascript"),
    ("JSON", "json"),
    ("Lua", "lua"),
    ("Makefile", "make"),
    ("Markdown", "markdown"),
    ("PHP", "php"),
    ("Python", "python"),
    ("Ruby", "ruby"),
    ("Rust", "rust"),
    ("Scala", "scala"),
    ("Shell-Unix-Generic", "bash"),
    ("YAML", "yaml"),
];

/// The editor highlighter's name for plain text: no highlighting at all.
pub(crate) const PLAIN_EDITOR_LANGUAGE: &str = "text";

/// The editor highlighter's name for a picker language: its id when the
/// bundled grammars cover it, and [`PLAIN_EDITOR_LANGUAGE`] otherwise.
pub(crate) fn editor_language(language: &str) -> &'static str {
    EDITOR_LANGUAGES
        .iter()
        .find(|(name, _)| *name == language)
        .map(|(_, id)| *id)
        .unwrap_or(PLAIN_EDITOR_LANGUAGE)
}

/// Highlight a comparison's hunks with `language`, for both appearance
/// modes. Plain text resolves to no syntax and therefore no spans, which is
/// exactly what "no highlighting" means.
pub(crate) fn highlight(hunks: &[Hunk], language: &str) -> SyntaxHighlights {
    let Some(syntax) = syntax_for(language) else {
        return SyntaxHighlights::default();
    };
    syntax::highlight_hunks(hunks, syntax)
}

/// The syntax behind a language name, or `None` for plain text (and for a
/// name the bundle does not know).
fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    if language == PLAIN_LANGUAGE {
        return None;
    }
    syntax::syntax_by_name(language)
}

/// Normalize pasted text for comparison: CRLF and lone CR become LF, and a
/// missing final newline is added. Neither difference is visible in the diff
/// view, so without this a copy/paste that drops the last newline or mixes
/// line endings would report a change on the final line that a reader cannot
/// see. An empty side stays empty rather than becoming one blank line.
fn normalize(text: &str) -> String {
    let mut normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.is_empty() && !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_have_no_hunks() {
        let result = compare("one\ntwo\n", "one\ntwo\n");
        assert!(result.hunks.is_empty());
        assert!(!result.truncated);
        assert_eq!((result.additions, result.deletions), (0, 0));
    }

    #[test]
    fn changed_lines_carry_hunks_and_counts() {
        let result = compare("one\ntwo\nthree\n", "one\nTWO\nthree\nfour\n");
        assert_eq!((result.additions, result.deletions), (2, 1));
        assert_eq!(result.hunks.len(), 1);
        let changed: Vec<_> = result.hunks[0]
            .lines
            .iter()
            .filter(|line| line.tag != LineTag::Context)
            .map(|line| (line.tag, line.text.as_str()))
            .collect();
        // Unified order: removals before additions at one position.
        assert_eq!(
            changed,
            vec![
                (LineTag::Deletion, "two"),
                (LineTag::Addition, "TWO"),
                (LineTag::Addition, "four"),
            ]
        );
    }

    #[test]
    fn line_ending_and_final_newline_differences_are_normalized() {
        // The same text pasted from a CRLF source, and without the final
        // newline, must not show a change.
        let result = compare("a\nb", "a\r\nb\r\n");
        assert!(result.hunks.is_empty(), "{result:?}");
        assert_eq!((result.additions, result.deletions), (0, 0));
        // A lone CR separates lines like LF does.
        let result = compare("a\rb\n", "a\nb\n");
        assert!(result.hunks.is_empty(), "{result:?}");
    }

    #[test]
    fn an_empty_side_diffs_as_removals_or_additions() {
        let result = compare("", "one\ntwo\n");
        assert_eq!((result.additions, result.deletions), (2, 0));
        let result = compare("one\ntwo\n", "");
        assert_eq!((result.additions, result.deletions), (0, 2));
        // Two empty sides are equal, not one blank line each.
        let result = compare("", "");
        assert!(result.hunks.is_empty());
        assert_eq!((result.additions, result.deletions), (0, 0));
    }

    #[test]
    fn languages_start_with_plain_text_and_resolve_by_name() {
        let languages = languages();
        assert_eq!(languages.first(), Some(&PLAIN_LANGUAGE));
        assert!(languages.contains(&"Rust"), "{languages:?}");
        assert!(languages.contains(&"JSON"), "{languages:?}");
        // Plain text is offered exactly once, and the rest is alphabetical.
        let mut sorted = languages[1..].to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, languages[1..], "the list must be sorted");
        assert_eq!(
            languages
                .iter()
                .filter(|name| **name == PLAIN_LANGUAGE)
                .count(),
            1
        );
        // A name the bundle does not know falls back to plain text instead of
        // silently highlighting nothing.
        assert_eq!(resolve_language("Rust"), "Rust");
        assert_eq!(resolve_language("No Such Language"), PLAIN_LANGUAGE);
        assert_eq!(resolve_language(""), PLAIN_LANGUAGE);
    }

    #[test]
    fn highlighting_paints_the_chosen_language_and_plain_text_paints_nothing() {
        let old = "{\n  \"a\": 1\n}\n";
        let new = "{\n  \"a\": 2\n}\n";
        let result = compare(old, new);
        let json = highlight(&result.hunks, "JSON");
        let colored = (0..result.hunks[0].lines.len())
            .any(|line| !json.document_line(true, 0, line).is_empty());
        assert!(colored, "JSON keys and values must produce spans");

        let plain = highlight(&result.hunks, PLAIN_LANGUAGE);
        for line in 0..result.hunks[0].lines.len() {
            assert!(plain.document_line(true, 0, line).is_empty());
            assert!(plain.document_line(false, 0, line).is_empty());
        }
        // An unknown language is as plain as the plain one.
        let unknown = highlight(&result.hunks, "No Such Language");
        for line in 0..result.hunks[0].lines.len() {
            assert!(unknown.document_line(true, 0, line).is_empty());
        }
    }

    #[test]
    fn huge_diffs_are_truncated_instead_of_emitted_whole() {
        // Every line changed, so the merged listing is the whole document —
        // over the merged cap. The listing keeps what fits and flags the
        // rest, so the pane shows a usable prefix plus the truncation notice.
        let old: String = (1..=(model::MAX_MERGED_LINES + 10))
            .map(|n| format!("line {n}\n"))
            .collect();
        let new: String = (1..=(model::MAX_MERGED_LINES + 10))
            .map(|n| format!("changed {n}\n"))
            .collect();
        let result = compare(&old, &new);
        assert!(result.truncated);
        let emitted: usize = result.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        assert_eq!(emitted, model::MAX_MERGED_LINES);
    }

    /// Resolve a side-by-side reference against the listing it points into.
    fn resolved(result: &DiffResult, reference: LineRef) -> (LineTag, Option<u32>, &str) {
        let line = &result.hunks[reference.hunk].lines[reference.line];
        (line.tag, line.new_no, line.text.as_str())
    }

    #[test]
    fn split_rows_pair_removals_with_additions() {
        let result = compare("a\nb\nc\n", "a\nB\nc\n");
        let rows = split_rows(&result);
        assert_eq!(rows.len(), 3, "{rows:?}");
        // Context shares a row with itself.
        assert_eq!(rows[0].old, rows[0].new);
        assert_eq!(
            resolved(&result, rows[0].old.unwrap()),
            (LineTag::Context, Some(1), "a")
        );
        // The replacement puts the removal and the addition on one row.
        let old = rows[1].old.expect("a removed line");
        let new = rows[1].new.expect("an added line");
        assert_eq!(resolved(&result, old), (LineTag::Deletion, None, "b"));
        assert_eq!(resolved(&result, new), (LineTag::Addition, Some(2), "B"));
        assert_eq!(resolved(&result, rows[2].old.unwrap()).2, "c");
    }

    #[test]
    fn split_rows_blank_the_side_that_has_nothing() {
        // A pure removal leaves the new side blank...
        let result = compare("a\nb\n", "a\n");
        let rows = split_rows(&result);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].old.is_some());
        assert!(rows[1].new.is_none(), "{rows:?}");

        // ...and a pure addition leaves the old side blank.
        let result = compare("a\n", "a\nb\n");
        let rows = split_rows(&result);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].old.is_none(), "{rows:?}");
        assert!(rows[1].new.is_some());

        // An unbalanced replacement pairs what it can and pads the shorter
        // side rather than dropping the extra removal.
        let result = compare("one\ntwo\nthree\n", "one\nTWO\n");
        let rows = split_rows(&result);
        let paired: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.old.map(|r| resolved(&result, r)),
                    row.new.map(|r| resolved(&result, r)),
                )
            })
            .collect();
        assert_eq!(paired.len(), 3, "{paired:?}");
        // Context first, then the replacement, then the leftover removal with
        // a blank cell opposite.
        assert!(paired[0].0.is_some() && paired[0].1.is_some());
        assert!(paired[1].0.is_some() && paired[1].1.is_some());
        assert!(paired[2].0.is_some() && paired[2].1.is_none(), "{paired:?}");
    }

    #[test]
    fn split_rows_keep_every_line_of_both_documents() {
        let old = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
        let new =
            old.replacen("line 3", "line THREE", 1)
                .replacen("line 17\n", "line 17\ninserted\n", 1);
        let result = compare(&old, &new);
        let rows = split_rows(&result);
        // Every line of both sides appears exactly once, on its own row.
        let old_lines = rows.iter().filter(|row| row.old.is_some()).count();
        let new_lines = rows.iter().filter(|row| row.new.is_some()).count();
        assert_eq!(old_lines, 20);
        assert_eq!(new_lines, 21);
    }

    #[test]
    fn the_editor_highlighter_language_map_covers_what_it_claims() {
        // Every mapped name is one the picker offers, so a language cannot
        // silently become unreachable, and the ids are the editor registry's
        // lowercase names.
        let offered = languages();
        for (name, id) in EDITOR_LANGUAGES {
            assert!(offered.contains(name), "{name} is not in the picker");
            assert_eq!(*id, id.to_lowercase(), "{id} is not a language id");
        }
        assert_eq!(editor_language("Rust"), "rust");
        assert_eq!(editor_language("Plain Text"), PLAIN_EDITOR_LANGUAGE);
        // A language the grammars cannot color falls back to plain text
        // rather than to a highlighter that would paint nothing.
        assert_eq!(editor_language("XML"), PLAIN_EDITOR_LANGUAGE);
        assert_eq!(editor_language(""), PLAIN_EDITOR_LANGUAGE);
    }
}
