//! Block anchors are independent of the renderer and preserve the original
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

/// Tagged so future selection anchors can be added without changing existing comments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommentAnchor {
    Block(BlockAnchor),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BlockAnchor {
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
        Self::Block(BlockAnchor {
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
                .match_indices(&source[block.range.clone()])
                .take(2)
                .count()
                > 1,
        })
    }
    pub(crate) fn block(&self) -> &BlockAnchor {
        let Self::Block(block) = self;
        block
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let block = self.block();
        ensure!(
            block.start < block.end && block.end - block.start == block.source.len(),
            "invalid comment block range"
        );
        ensure!(
            block.start_line > 0 && block.end_line >= block.start_line,
            "invalid comment block lines"
        );
        Ok(())
    }
    pub(crate) fn relocate(&mut self, source: &str, blocks: &[MarkdownBlock]) {
        let Self::Block(anchor) = self;
        let candidates = blocks
            .iter()
            .filter(|b| source.get(b.range.clone()) == Some(anchor.source.as_str()))
            .collect::<Vec<_>>();
        let current_hash = document_hash(source);
        let candidate = if current_hash == anchor.document_hash {
            candidates
                .iter()
                .copied()
                .find(|b| b.range.start == anchor.start && b.range.end == anchor.end)
        } else if candidates.len() == 1 && !anchor.requires_context {
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
                        usize::from(prefix == anchor.prefix) + usize::from(suffix == anchor.suffix),
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
            anchor.start_line = block.start_line;
            anchor.end_line = block.end_line;
            (anchor.prefix, anchor.suffix) = context(source, block.range.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moves_blocks_and_retains_changed_excerpts() {
        let source = "# Header\n\nA **bold** λ paragraph.\n\n```rs\nlet x = 1;\n```\n";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[1], Some("bold".into()));
        let moved = format!("Intro.\n\n{source}");
        anchor.relocate(&moved, &blocks(&moved));
        assert_eq!(anchor.block().start_line, 5);
        assert!(!anchor.block().outdated);
        let edited = moved.replace("λ", "β");
        anchor.relocate(&edited, &blocks(&edited));
        assert!(anchor.block().outdated);
        assert!(anchor.block().source.contains('λ'));
        anchor.relocate(&moved, &blocks(&moved));
        assert!(!anchor.block().outdated);
        anchor.validate().unwrap();
    }
    #[test]
    fn deleting_one_duplicate_does_not_move_feedback_to_the_survivor() {
        let source = "First.\n\nRepeat.\n\nMiddle.\n\nRepeat.\n\nLast.";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[1], None);
        let changed = "Middle.\n\nRepeat.\n\nLast.";
        anchor.relocate(changed, &blocks(changed));
        assert!(anchor.block().outdated);
    }
    #[test]
    fn repeated_blocks_require_distinguishing_context() {
        let source = "First.\n\nRepeat.\n\nSecond.\n\nRepeat.\n\nLast.";
        let mut anchor = CommentAnchor::new(source, &blocks(source)[3], None);
        let moved = format!("Introduction.\n\n{source}");
        anchor.relocate(&moved, &blocks(&moved));
        assert_eq!(anchor.block().start_line, 9);
        let ambiguous = "Changed.\n\nRepeat.\n\nRepeat.\n\nChanged.";
        anchor.relocate(ambiguous, &blocks(ambiguous));
        assert!(anchor.block().outdated);
    }
}
