//! Framework provider implementations backed by [`Client`].
//!
//! The editor engine calls these on the UI thread with the current buffer
//! and cursor offset. Each implementation snapshots the document version and
//! converts the offset to an LSP position while the buffer is in scope, then
//! moves the blocking request onto a background thread. Failures degrade to
//! empty results; the client records the failure (`last_error`) instead of
//! surfacing editor errors, so a dead server never breaks plain editing.
//!
//! Position handling: the engine's own `Position` counts Unicode scalar
//! values per line, while the protocol counts UTF-16 code units. The
//! conversion below is UTF-16 based (matching the negotiated encoding the
//! client records); astral characters such as emoji are the case that
//! distinguishes the two, and the tests pin it.

use std::{rc::Rc, sync::Arc};

use anyhow::Result;
use gpui_kit::{
    App, Task, Window,
    component::input::{CompletionProvider, DefinitionProvider, HoverProvider, Rope, RopeExt},
};
use lsp_types::{CompletionContext, CompletionResponse};

use super::client::Client;

/// One provider set per open document; the URI never changes for its
/// lifetime, so stale-document checks reduce to the client's version.
pub struct LspProviders {
    client: Arc<Client>,
    uri: lsp_types::Uri,
}

impl LspProviders {
    pub fn new(client: Arc<Client>, uri: lsp_types::Uri) -> Rc<Self> {
        Rc::new(Self { client, uri })
    }
}

/// Byte offset to an LSP (UTF-16) position, clamped into the buffer. The
/// engine's point column is byte-based, so the prefix is re-measured in
/// UTF-16 code units here.
pub fn offset_to_lsp_position(text: &Rope, offset: usize) -> lsp_types::Position {
    let point = text.offset_to_point(offset.min(text.len()));
    let line_start = text.line_start_offset(point.row);
    let character: usize = text
        .slice(line_start..line_start + point.column)
        .chars()
        .map(|c| c.len_utf16())
        .sum();
    lsp_types::Position::new(point.row as u32, character as u32)
}

/// LSP (UTF-16) position to a byte offset, clamped into the buffer. Columns
/// past the line content land on the content end. A trailing `\r` (the
/// `slice_line` contract keeps it) counts as the line break, not content.
///
/// Covered by tests; the spike's flows only convert offsets outward, while
/// edit/diagnostic application (Phase 4) converts positions back inward.
#[allow(dead_code)]
pub fn lsp_position_to_offset(text: &Rope, position: &lsp_types::Position) -> usize {
    let total = text.lines_len();
    if total == 0 {
        return 0;
    }
    let line = (position.line as usize).min(total - 1);
    let line_start = text.line_start_offset(line);
    let mut utf16 = 0usize;
    let mut offset = line_start;
    let mut chars = text.slice_line(line).chars().peekable();
    while let Some(c) = chars.next() {
        if utf16 >= position.character as usize {
            break;
        }
        if c == '\r' && chars.peek().is_none() {
            break;
        }
        utf16 += c.len_utf16();
        offset += c.len_utf8();
    }
    offset
}

impl CompletionProvider for LspProviders {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let client = Arc::clone(&self.client);
        let uri = self.uri.clone();
        let version = client.doc_version();
        let position = offset_to_lsp_position(text, offset);
        // The blocking request runs on a background thread; the engine
        // holds the Task and polls it without ever blocking the UI.
        cx.background_executor().clone().spawn(async move {
            // The server tracks the document through didChange; the request
            // carries the version the offset was read at so a concurrent
            // edit discards the answer as stale.
            Ok(client
                .completion(&uri, version, position)
                .unwrap_or(lsp_types::CompletionResponse::Array(Vec::new())))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        // Member access and Rust paths are the completions worth waking a
        // cold server for; ordinary identifier typing still resolves through
        // the explicit completion key.
        new_text.ends_with('.') || new_text.ends_with(':')
    }
}

impl HoverProvider for LspProviders {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        let client = Arc::clone(&self.client);
        let uri = self.uri.clone();
        let version = client.doc_version();
        let position = offset_to_lsp_position(text, offset);
        cx.background_executor()
            .clone()
            .spawn(async move { Ok(client.hover(&uri, version, position).unwrap_or(None)) })
    }
}

impl DefinitionProvider for LspProviders {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<lsp_types::LocationLink>>> {
        let client = Arc::clone(&self.client);
        let uri = self.uri.clone();
        let version = client.doc_version();
        let position = offset_to_lsp_position(text, offset);
        cx.background_executor().clone().spawn(async move {
            Ok(client
                .definition(&uri, version, position)
                .unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "a `<CJK><CJK><emoji>` test" — CJK is one UTF-16 unit per char, emoji
    /// is two. A scalar-counting position reads 5 at the end of 🎉; the
    /// protocol-correct answer is 6.
    fn sample() -> Rope {
        Rope::from("a 中文🎉 test\nsecond line\n")
    }

    #[test]
    fn positions_count_utf16_units_not_chars() {
        let text = sample();
        let emoji_end = "a 中文🎉".len();
        let position = offset_to_lsp_position(&text, emoji_end);
        assert_eq!(position, lsp_types::Position::new(0, 6));
        // And back again lands on the same byte offset.
        assert_eq!(lsp_position_to_offset(&text, &position), emoji_end);
    }

    #[test]
    fn positions_clamp_past_line_end_and_last_line() {
        let text = sample();
        let first_line_end = "a 中文🎉 test".len();
        assert_eq!(
            lsp_position_to_offset(&text, &lsp_types::Position::new(0, 100)),
            first_line_end
        );
        assert_eq!(
            offset_to_lsp_position(&text, text.len() + 50),
            lsp_types::Position::new(2, 0)
        );
        assert_eq!(
            lsp_position_to_offset(&text, &lsp_types::Position::new(99, 0)),
            text.len()
        );
    }

    #[test]
    fn empty_buffer_maps_to_origin() {
        let text = Rope::from("");
        assert_eq!(
            offset_to_lsp_position(&text, 0),
            lsp_types::Position::new(0, 0)
        );
        assert_eq!(
            lsp_position_to_offset(&text, &lsp_types::Position::new(3, 9)),
            0
        );
    }
}
