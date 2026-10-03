//! Phase 2 feasibility spike: a minimal language client for the built-in
//! editor.
//!
//! The framework (`gpui-base` 0.7.0) owns the editing UI: provider hooks
//! (`CompletionProvider`, `HoverProvider`, `DefinitionProvider`),
//! diagnostics storage, and UTF-16-agnostic text. Devcroft owns everything
//! on the other side of those hooks here: server discovery and process
//! lifetime ([`transport`]), the initialize/didOpen/didChange/request
//! lifecycle ([`client`]), and the hook implementations ([`providers`]).
//!
//! What this module deliberately does NOT do yet (Phase 4): per-workspace
//! server reuse, incremental sync, capability-gated requests, trusted-command
//! prompts, restart/timeout UX, reference/symbol follow-ups, or additional
//! languages. Rust is the only wired language, and any server failure
//! degrades to plain editing.

pub(crate) mod client;
pub(crate) mod providers;
pub(crate) mod transport;

pub(crate) use client::{Client, DiagnosticEvent};
pub(crate) use providers::LspProviders;

use std::path::PathBuf;

use anyhow::Result;

use super::find_executable;

/// Locate a `rust-analyzer` binary for the spike: an explicit path wins when
/// it resolves, otherwise fall back to `PATH` (plus the GUI-launch
/// directories the external-editor launcher already knows about).
pub(crate) fn discover_rust_analyzer(explicit: Option<&str>) -> Option<PathBuf> {
    explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(find_executable)
        .or_else(|| find_executable("rust-analyzer"))
}

/// File path to a `file://` URI with uppercase percent-encoding, matching
/// what servers built on the `url` crate (rust-analyzer) send back. Only
/// absolute paths are accepted. Encoding follows the `url` crate's path
/// rules: `/` separators and unreserved plus sub-delim characters pass
/// through, everything else is encoded byte-wise (UTF-8 for non-ASCII
/// names), so diagnostics URIs compare equal as strings.
pub(crate) fn file_uri(path: &std::path::Path) -> Result<lsp_types::Uri> {
    use std::fmt::Write as _;

    if !path.is_absolute() {
        anyhow::bail!("Cannot make a file URI from a relative path");
    }
    let path = path.to_string_lossy();
    let mut encoded = String::with_capacity(path.len() + "file://".len());
    encoded.push_str("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:@/".contains(&byte) {
            encoded.push(byte as char);
        } else {
            write!(encoded, "%{byte:02X}").expect("formatting a byte cannot fail");
        }
    }
    encoded
        .parse()
        .map_err(|_| anyhow::anyhow!("Checkout path is not a valid file URI"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn file_uri_matches_url_crate_path_encoding() {
        let uri = file_uri(Path::new("/tmp/project space/src/বাংলা+file.rs")).unwrap();
        assert_eq!(
            uri.as_str(),
            "file:///tmp/project%20space/src/%E0%A6%AC%E0%A6%BE%E0%A6%82%E0%A6%B2%E0%A6%BE+file.rs"
        );
        assert!(file_uri(Path::new("relative/path")).is_err());
    }
}
