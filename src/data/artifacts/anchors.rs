//! Source anchors are independent of the renderer and preserve the original
//! excerpt when a document edit makes the location uncertain.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug)]
pub(crate) struct MarkdownBlock {
    pub range: Range<usize>,
    pub definition: bool,
    pub start_line: usize,
    pub end_line: usize,
}

pub(crate) fn blocks(source: &str) -> Vec<MarkdownBlock> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.frontmatter = true;
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    let Ok(root) = markdown::to_mdast(source, &options) else {
        return Vec::new();
    };
    root.children()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let position = node.position()?;
            Some(MarkdownBlock {
                definition: matches!(node, markdown::mdast::Node::Definition(_)),
                range: position.start.offset..position.end.offset,
                start_line: position.start.line,
                end_line: position.end.line,
            })
        })
        .collect()
}

/// One-based inclusive lines for an end-exclusive UTF-8 source span.
pub(crate) fn source_span(source: &str, range: Range<usize>) -> MarkdownBlock {
    let line_at = |offset: usize| {
        source.as_bytes()[..offset]
            .iter()
            .enumerate()
            .filter(|(index, byte)| {
                **byte == b'\n'
                    || (**byte == b'\r' && source.as_bytes().get(index + 1) != Some(&b'\n'))
            })
            .count()
            + 1
    };
    MarkdownBlock {
        start_line: line_at(range.start),
        end_line: line_at(range.end.saturating_sub(1)),
        range,
        definition: false,
    }
}

/// Existing block anchors retain their serialized representation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommentAnchor {
    Block(SourceAnchor),
    Selection(SourceAnchor),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceAnchor {
    /// UTF-8 byte offsets in the artifact body, end exclusive.
    pub start: usize,
    pub end: usize,
    /// One-based inclusive source lines (last known location when outdated).
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
    pub prefix: String,
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    pub outdated: bool,
    /// Duplicate excerpts must retain identifying context even if only one survives.
    #[serde(default)]
    pub requires_context: bool,
    #[serde(default)]
    pub document_hash: String,
}

fn document_hash(source: &str) -> String {
    gix::objs::compute_hash(
        gix::hash::Kind::Sha1,
        gix::objs::Kind::Blob,
        source.as_bytes(),
    )
    .expect("hashing in-memory Markdown")
    .to_string()
}

fn context(source: &str, range: Range<usize>) -> (String, String) {
    let prefix = source[..range.start]
        .chars()
        .rev()
        .take(120)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let suffix = source[range.end..].chars().take(120).collect();
    (prefix, suffix)
}

impl CommentAnchor {
    pub(crate) fn new(source: &str, block: &MarkdownBlock, quote: Option<String>) -> Self {
        let (prefix, suffix) = context(source, block.range.clone());
        Self::Block(SourceAnchor {
            start: block.range.start,
            end: block.range.end,
            start_line: block.start_line,
            end_line: block.end_line,
            source: source[block.range.clone()].to_owned(),
            prefix,
            suffix,
            quote: quote.filter(|q| !q.trim().is_empty()),
            outdated: false,
            document_hash: document_hash(source),
            requires_context: source
                .char_indices()
                .filter(|(offset, _)| source[*offset..].starts_with(&source[block.range.clone()]))
                .take(2)
                .count()
                > 1,
        })
    }
    pub(crate) fn selection(
        source: &str,
        range: Range<usize>,
        quote: Option<String>,
    ) -> Result<Self> {
        ensure!(
            range.start < range.end && source.get(range.clone()).is_some(),
            "invalid comment selection range"
        );
        let block = source_span(source, range);
        let Self::Block(anchor) = Self::new(source, &block, quote) else {
            unreachable!()
        };
        Ok(Self::Selection(anchor))
    }
    pub(crate) fn location(&self) -> &SourceAnchor {
        let (Self::Block(block) | Self::Selection(block)) = self;
        block
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let block = self.location();
        ensure!(
            block.start < block.end && block.end - block.start == block.source.len(),
            "invalid comment source range"
        );
        ensure!(
            block.start_line > 0 && block.end_line >= block.start_line,
            "invalid comment source lines"
        );
        Ok(())
    }
    pub(crate) fn relocate(&mut self, source: &str, blocks: &[MarkdownBlock]) {
        let selection = matches!(self, Self::Selection(_));
        let (Self::Block(anchor) | Self::Selection(anchor)) = self;
        let current_hash = document_hash(source);
        if current_hash == anchor.document_hash
            && source.get(anchor.start..anchor.end) == Some(anchor.source.as_str())
        {
            anchor.outdated = false;
            return;
        }
        let spans;
        let blocks = if selection {
            // Include overlapping occurrences; offset alone never identifies a
            // surviving duplicate after a document edit.
            spans = source
                .char_indices()
                .filter(|(offset, _)| source[*offset..].starts_with(&anchor.source))
                .map(|(offset, _)| MarkdownBlock {
                    range: offset..offset + anchor.source.len(),
                    definition: false,
                    start_line: 0,
                    end_line: 0,
                })
                .collect::<Vec<_>>();
            &spans
        } else {
            blocks
        };
        let candidates = blocks
            .iter()
            .filter(|b| source.get(b.range.clone()) == Some(anchor.source.as_str()))
            .collect::<Vec<_>>();
        let candidate = if current_hash == anchor.document_hash {
            candidates
                .iter()
                .copied()
                .find(|b| b.range.start == anchor.start && b.range.end == anchor.end)
        } else if !selection && candidates.len() == 1 && !anchor.requires_context {
            candidates.first().copied()
        } else {
            // Position alone is not identity: deletion may leave another equal
            // block at exactly the old offset. Require distinguishing context.
            let scored = candidates
                .iter()
                .map(|block| {
                    let (prefix, suffix) = context(source, block.range.clone());
                    (
                        *block,
                        usize::from(prefix == anchor.prefix && (!selection || !prefix.is_empty()))
                            + usize::from(
                                suffix == anchor.suffix && (!selection || !suffix.is_empty()),
                            ),
                    )
                })
                .collect::<Vec<_>>();
            let best = scored.iter().map(|(_, score)| *score).max().unwrap_or(0);
            let winners = scored
                .iter()
                .filter(|(_, score)| *score == best)
                .collect::<Vec<_>>();
            if best > 0 && winners.len() == 1 {
                Some(winners[0].0)
            } else {
                None
            }
        };
        anchor.requires_context |= candidates.len() > 1;
        anchor.outdated = candidate.is_none();
        if let Some(block) = candidate {
            anchor.document_hash = current_hash;
            anchor.start = block.range.start;
            anchor.end = block.range.end;
            let span = if selection {
                source_span(source, block.range.clone())
            } else {
                block.clone()
            };
            anchor.start_line = span.start_line;
            anchor.end_line = span.end_line;
            (anchor.prefix, anchor.suffix) = context(source, block.range.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_lines_follow_commonmark_line_endings() {
        for source in ["a\rb", "a\r\nb", "a\nb"] {
            let start = source.find('b').unwrap();
            let span = source_span(source, start..start + 1);
            assert_eq!((span.start_line, span.end_line), (2, 2));
            let span = source_span(source, 0..start);
            assert_eq!((span.start_line, span.end_line), (1, 1));
        }
    }

    #[test]
    fn removed_selection_is_not_reassigned_to_a_new_unique_occurrence() {
        let source = "Selected token. Other.";
        let start = source.find("token").unwrap();
        let mut anchor = CommentAnchor::selection(source, start..start + 5, None).unwrap();
        let changed = "Removed. Other token.";
        anchor.relocate(changed, &blocks(changed));
        assert!(anchor.location().outdated);
        assert_eq!(anchor.location().start, start);
    }

    #[test]
    fn selection_preserves_exact_occurrence_and_survives_surrounding_edits() {
        let source = "# Title\n\nFirst **λ** and second **λ**.\nNext line.";
        let start = source.rfind('λ').unwrap();
        let mut anchor =
            CommentAnchor::selection(source, start..start + 'λ'.len_utf8(), Some("λ".into()))
                .unwrap();
        assert_eq!(anchor.location().start, start);
        assert_eq!(anchor.location().start_line, 3);
        let json = serde_json::to_string(&anchor).unwrap();
        assert!(json.contains("\"kind\":\"selection\""));
        assert_eq!(
            serde_json::from_str::<CommentAnchor>(&json).unwrap(),
            anchor
        );
        let changed = format!("Introduction.\n\n{source}");
        anchor.relocate(&changed, &blocks(&changed));
        assert!(!anchor.location().outdated);
        assert_eq!(anchor.location().start, changed.rfind('λ').unwrap());
        assert_eq!(anchor.location().start_line, 5);
        let deleted = "First **λ** only.";
        anchor.relocate(deleted, &blocks(deleted));
        assert!(anchor.location().outdated);
        assert_eq!(anchor.location().source, "λ");
    }

    #[test]
    fn selections_validate_utf8_and_end_exclusive_lines() {
        let source = "α\r\nβ\nlast";
        for range in [0..0, 1..2, 0..100, std::ops::Range { start: 4, end: 3 }] {
            assert!(CommentAnchor::selection(source, range, None).is_err());
        }
        let anchor = CommentAnchor::selection(source, 0..4, None).unwrap();
        assert_eq!(
            (anchor.location().start_line, anchor.location().end_line),
            (1, 1)
        );
        let anchor = CommentAnchor::selection(source, 4..6, None).unwrap();
        assert_eq!(
            (anchor.location().start_line, anchor.location().end_line),
            (2, 2)
        );
    }

    #[test]
    fn selection_survives_edits_elsewhere_in_same_block() {
        let source = "Before **selected text** after.";
        let start = source.find("selected").unwrap();
        let mut anchor = CommentAnchor::selection(source, start..start + 13, None).unwrap();
        let changed = "Changed before **selected text** after.";
        anchor.relocate(changed, &blocks(changed));
        assert!(!anchor.location().outdated);
        assert_eq!(
            &changed[anchor.location().start..anchor.location().end],
            "selected text"
        );
        anchor.relocate("Changed before **replacement** after.", &[]);
        assert!(anchor.location().outdated);
    }

    #[test]
    fn overlapping_duplicate_selection_is_not_reassigned() {
        let mut anchor = CommentAnchor::selection("aaa", 1..3, None).unwrap();
        assert!(anchor.location().requires_context);
        anchor.relocate("XaaY", &[]);
        assert!(anchor.location().outdated);
    }

    #[test]
    fn moves_blocks_and_retains_changed_excerpts() {
        let source = "# Header\n\nA **bold** λ paragraph.\n\n```rs\nlet x = 1;\n```\n";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[1], Some("bold".into()));
        let moved = format!("Intro.\n\n{source}");
        anchor.relocate(&moved, &blocks(&moved));
        assert_eq!(anchor.location().start_line, 5);
        assert!(!anchor.location().outdated);
        let edited = moved.replace("λ", "β");
        anchor.relocate(&edited, &blocks(&edited));
        assert!(anchor.location().outdated);
        assert!(anchor.location().source.contains('λ'));
        anchor.relocate(&moved, &blocks(&moved));
        assert!(!anchor.location().outdated);
        anchor.validate().unwrap();
    }
    #[test]
    fn deleting_one_duplicate_does_not_move_feedback_to_the_survivor() {
        let source = "First.\n\nRepeat.\n\nMiddle.\n\nRepeat.\n\nLast.";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[1], None);
        let changed = "Middle.\n\nRepeat.\n\nLast.";
        anchor.relocate(changed, &blocks(changed));
        assert!(anchor.location().outdated);
    }
    #[test]
    fn repeated_blocks_require_distinguishing_context() {
        let source = "First.\n\nRepeat.\n\nSecond.\n\nRepeat.\n\nLast.";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[3], None);
        let moved = format!("Introduction.\n\n{source}");
        anchor.relocate(&moved, &blocks(&moved));
        assert_eq!(anchor.location().start_line, 9);
        let ambiguous = "Changed.\n\nRepeat.\n\nRepeat.\n\nChanged.";
        anchor.relocate(ambiguous, &blocks(ambiguous));
        assert!(anchor.location().outdated);
    }
}
