//! Bounded, opt-in Rust language support. NativeEditor owns one client per
//! checkout; transport I/O and process initialization/teardown run off the UI
//! thread. References, document symbols, custom repository commands, and
//! additional language servers are deferred. Failures leave plain editing usable.

pub(crate) mod catalog;
pub(crate) mod client;
pub(crate) mod install;
pub(crate) mod manager;
pub(crate) mod process;
pub(crate) mod providers;
pub(crate) mod settings;
pub(crate) mod transport;

pub(crate) use client::{Client, DiagnosticEvent};
pub(crate) use providers::LspProviders;

use std::path::PathBuf;

use anyhow::Result;

#[cfg(test)]
use super::find_executable;

/// Locate a `rust-analyzer` binary from the device environment: an explicit path wins when
/// it resolves, otherwise fall back to `PATH` (plus the GUI-launch
/// directories the external-editor launcher already knows about).
#[cfg(test)]
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
    anyhow::ensure!(
        path.is_absolute(),
        "Cannot make a file URI from a relative path"
    );
    encode_path(&path.to_string_lossy(), cfg!(windows))
}

fn encode_path(path: &str, windows: bool) -> Result<lsp_types::Uri> {
    use std::fmt::Write as _;
    let path = if windows {
        let path = path
            .strip_prefix(r"\\?\")
            .unwrap_or(path)
            .replace('\\', "/");
        anyhow::ensure!(
            path.as_bytes().get(1) == Some(&b':') && path.as_bytes().get(2) == Some(&b'/'),
            "Only local Windows drive paths are supported"
        );
        format!("/{path}")
    } else {
        path.to_owned()
    };
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

/// Decode only local file URIs; remote/virtual definitions never launch a handler.
pub(crate) fn path_from_uri(uri: &lsp_types::Uri) -> Result<PathBuf> {
    let path = uri
        .as_str()
        .strip_prefix("file:///")
        .ok_or_else(|| anyhow::anyhow!("Only local file definitions are supported"))?;
    let mut bytes = Vec::new();
    let mut input = path.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = input.next().and_then(|c| (c as char).to_digit(16));
            let b = input.next().and_then(|c| (c as char).to_digit(16));
            bytes.push(
                a.zip(b)
                    .map(|(a, b)| (a * 16 + b) as u8)
                    .ok_or_else(|| anyhow::anyhow!("Invalid file URI escape"))?,
            );
        } else {
            bytes.push(byte);
        }
    }
    let path = String::from_utf8(bytes)?;
    #[cfg(not(windows))]
    let path = format!("/{path}");
    anyhow::ensure!(!path.contains('\0'), "File URI contains NUL");
    #[cfg(windows)]
    let path = path.replace('/', "\\");
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn windows_drive_uri_uses_forward_slashes_and_three_slash_prefix() {
        assert_eq!(
            encode_path(r"C:\project space\src\main.rs", true)
                .unwrap()
                .as_str(),
            "file:///C:/project%20space/src/main.rs"
        );
        assert_eq!(
            encode_path(r"\\?\C:\project\main.rs", true)
                .unwrap()
                .as_str(),
            "file:///C:/project/main.rs"
        );
        assert!(encode_path(r"\\server\share\file.rs", true).is_err());
    }

    #[test]
    fn local_uri_roundtrip_and_nonlocal_rejection() {
        let file = std::env::temp_dir().join("unicode 🎉 file.rs");
        assert_eq!(path_from_uri(&file_uri(&file).unwrap()).unwrap(), file);
        assert!(path_from_uri(&"https://example.com/code".parse().unwrap()).is_err());
        assert!(path_from_uri(&"file://remote/path".parse().unwrap()).is_err());
    }

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
