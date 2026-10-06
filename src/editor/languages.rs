//! File identity is independent of both the highlighting grammar and server.
use super::lsp::catalog::ServerId;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Language {
    pub grammar: &'static str,
    pub id: &'static str,
    pub server: Option<ServerId>,
}

pub(crate) fn detect(path: &Path) -> Language {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let (grammar, id, server) = if path.file_name().is_some_and(|n| n == "Cargo.lock") {
        ("toml", "toml", None)
    } else {
        match ext {
            "rs" => ("rust", "rust", Some(ServerId::Rust)),
            "go" => ("go", "go", Some(ServerId::Go)),
            "js" | "mjs" | "cjs" => ("javascript", "javascript", Some(ServerId::TypeScript)),
            "jsx" => ("javascript", "javascriptreact", Some(ServerId::TypeScript)),
            "ts" | "mts" | "cts" => ("typescript", "typescript", Some(ServerId::TypeScript)),
            "tsx" => ("tsx", "typescriptreact", Some(ServerId::TypeScript)),
            "py" | "pyi" => ("python", "python", Some(ServerId::Python)),
            "toml" => ("toml", "toml", None),
            "sh" | "bash" => ("bash", "shellscript", None),
            "md" => ("markdown", "markdown", None),
            "html" => ("html", "html", None),
            "css" => ("css", "css", None),
            "json" => ("json", "json", None),
            "yml" | "yaml" => ("yaml", "yaml", None),
            "c" | "h" => ("c", "c", None),
            "cc" | "cpp" | "hpp" => ("cpp", "cpp", None),
            "java" => ("java", "java", None),
            "rb" => ("ruby", "ruby", None),
            _ => ("", "plaintext", None),
        }
    };
    Language {
        grammar,
        id,
        server,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grammar_and_protocol_ids_are_independent() {
        assert_eq!(detect(Path::new("app.tsx")).grammar, "tsx");
        assert_eq!(detect(Path::new("app.tsx")).id, "typescriptreact");
        assert_eq!(detect(Path::new("app.jsx")).id, "javascriptreact");
        assert_eq!(
            detect(Path::new("app.mts")).server,
            Some(ServerId::TypeScript)
        );
        assert!(detect(Path::new("README.md")).server.is_none());
    }
}
