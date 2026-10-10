//! Highlight queries for grammars whose bundled config ships without one.
//!
//! `gpui-component` links the protobuf grammar behind its `tree-sitter-proto`
//! feature but registers the language with an empty highlight query (as of
//! 0.7.1), so the registry parses a `.proto` buffer and paints nothing. The
//! grammar crate does not export its query, so a copy lives under
//! `assets/tree-sitter-proto/` and is installed over the bundled config here.
//! A query the bundle already has is left untouched, so a future upstream fix
//! retires this registration instead of being masked by it.
//!
//! Installation is a one-time process-wide side effect on the language
//! registry, so it hangs off the editor's grammar resolution
//! (`native::language_for`) rather than app startup: a highlighter cannot be
//! created before the query it needs is installed, and the tests below resolve
//! a `.proto` path through the same function the editor uses. A future surface
//! that assigns a proto highlighter without resolving a path through
//! `native::language_for` must call [`ensure_for`] itself.

use std::sync::Once;

/// Verbatim copy of `tree-sitter-proto` 0.2.0's `queries/highlights.scm`.
///
/// A query naming a node the linked grammar does not have fails to compile,
/// and the editor silently falls back to plain text, so the tests below pin
/// the highlight output rather than the query text.
const PROTOBUF_HIGHLIGHTS: &str = include_str!("../../assets/tree-sitter-proto/highlights.scm");

static PROTOBUF: Once = Once::new();

/// Install the vendored query for `grammar` when the bundled config has none.
///
/// Cheap on every grammar resolution: after the first call this only checks a
/// `Once`. A no-op for grammars that need nothing, and when the grammar is not
/// linked in.
pub(crate) fn ensure_for(grammar: &str) {
    if grammar == "proto" {
        PROTOBUF.call_once(install_protobuf);
    }
}

fn install_protobuf() {
    let registry = gpui_kit::component::highlighter::LanguageRegistry::singleton();
    let Some(mut config) = registry.language("proto") else {
        return;
    };
    if !config.highlights.is_empty() {
        return;
    }
    config.highlights = PROTOBUF_HIGHLIGHTS.into();
    registry.register("proto", &config);
}

#[cfg(test)]
mod tests {
    use crate::editor::native::language_for;
    use gpui_kit::base::input::Rope;
    use gpui_kit::component::highlighter::{HighlightTheme, LanguageRegistry, SyntaxHighlighter};
    use std::path::Path;

    const SOURCE: &str = r#"// The fleet's vehicles.
syntax = "proto3";

package fleet;

message Vehicle {
  string vin = 1;
  repeated int32 wheel_count = 2;
}

enum State {
  IDLE = 0;
  MOVING = 1;
}

service FleetService {
  rpc Get(GetRequest) returns (Vehicle);
}
"#;

    /// Resolve the grammar exactly as the editor does. The tests go through
    /// this path instead of calling [`ensure_for`] directly, so removing the
    /// installation from the editor's grammar resolution fails them rather
    /// than being masked by a test-only call.
    fn resolve_proto() {
        assert_eq!(language_for(Path::new("proto/fleet.proto")), "proto");
    }

    #[test]
    fn resolving_a_proto_path_installs_the_grammar_query() {
        resolve_proto();
        let config = LanguageRegistry::singleton()
            .language("proto")
            .expect("the tree-sitter-proto feature must register the language");
        assert!(
            config.has_grammar(),
            "the protobuf grammar must be linked in"
        );
        assert!(
            !config.highlights.is_empty(),
            "the registered protobuf config must carry a highlight query"
        );
    }

    #[test]
    fn protobuf_documents_highlight_keywords_comments_and_literals() {
        resolve_proto();
        let mut highlighter = SyntaxHighlighter::new("proto");
        let text = Rope::from_str(SOURCE);
        assert!(
            highlighter.update(None, &text, None),
            "the protobuf highlighter must parse the document"
        );
        // A query the grammar rejects makes `new` fall back to the inert
        // "text" highlighter, which paints nothing.
        assert_eq!(
            highlighter.language().as_ref(),
            "proto",
            "the highlighter must not fall back to plain text"
        );
        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..text.len()), &*theme);
        assert!(
            styles.len() > 1,
            "protobuf must produce distinct styled spans, got {styles:?}"
        );
        let colored = |from: usize, through: usize| {
            styles.iter().any(|(range, style)| {
                range.start <= from && range.end >= through && style.color.is_some()
            })
        };
        let message_at = SOURCE.find("message").unwrap();
        assert!(
            colored(message_at, message_at + "message".len()),
            "the `message` keyword must carry a syntax color, got {styles:?}"
        );
        let comment_end = SOURCE.find('\n').unwrap();
        assert!(
            colored(0, comment_end),
            "the line comment must carry a syntax color, got {styles:?}"
        );
        let package = SOURCE.find("package").unwrap();
        assert!(
            colored(package, package + "package".len()),
            "the `package` keyword must carry a syntax color, got {styles:?}"
        );
    }
}
