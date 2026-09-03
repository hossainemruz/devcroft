//! File-type icon theme for the Review sidebar.
//!
//! A vendored [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme)
//! subset (see `assets/icons/ATTRIBUTION.md`) with a small native resolver.
//! Resolution order per file name (matched lowercased):
//!
//! 1. exact file name (`Dockerfile`, `go.mod`, `Makefile`, `.gitignore`),
//! 2. multi-dot suffixes longest-first (`d.ts` beats `ts`),
//! 3. plain extension,
//! 4. the generic document icon.
//!
//! Icons are embedded with `include_bytes!`. Rows render through GPUI's
//! `Svg::data` as monochrome silhouettes, but [`ensure_tiles`] can also
//! rasterize full-color tiles via the app's SVG renderer for the [`img`]
//! element — that's how the sidebar shows Material's actual colors despite
//! `Svg` itself being tint-only.
//!
//! Folders intentionally keep the neutral gpui-kit glyphs — upstream ships
//! no generic folder pair (base and open variants are build-generated), and
//! mixing one folder pair into the tree reads consistently.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::{App, RenderImage};

/// Resolve a checkout-relative path (or bare file name) to its vendored
/// icon key (an SVG stem under `assets/icons/files/`).
pub(crate) fn icon_key(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if let Some(key) = exact_icon(name) {
        return key;
    }
    for suffix in suffixes(&lower) {
        if let Some(key) = suffix_icon(suffix) {
            return key;
        }
    }
    FALLBACK
}

pub(crate) const FALLBACK: &str = "document";

/// Exact file names (lowercased), mirroring upstream's `fileNames`
/// mappings for the vendored set.
fn exact_icon(lower: &str) -> Option<&'static str> {
    // Dockerfile variants: Dockerfile, Dockerfile.dev, dockerfile.prod…
    if lower == "dockerfile" || lower.starts_with("dockerfile.") {
        return Some("docker");
    }
    Some(match lower {
        "makefile" | "gnumakefile" | "kbuild" => "makefile",
        "go.mod" | "go.sum" | "go.work" | "go.work.sum" => "go-mod",
        "package.json" | "package-lock.json" | ".npmrc" | ".npmignore" => "npm",
        "readme" | "readme.md" | "readme.rst" | "readme.txt" => "readme",
        "license" | "licence" | "copying" | "copyright" | "license.md" | "licence.md"
        | "copying.md" | "copyright.md" | "license.rst" | "licence.rst" | "copying.rst"
        | "copyright.rst" | "license.txt" | "licence.txt" | "copying.txt" | "copyright.txt" => {
            "license"
        }
        "changelog" | "changes" | "changelog.md" | "changes.md" | "changelog.rst"
        | "changes.rst" | "changelog.txt" | "changes.txt" => "changelog",
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" | ".gitconfig"
        | ".gitmessage" | "commit_editmsg" | "merge_msg" => "git",
        _ => return None,
    })
}

/// Candidate suffixes longest-first: `app.d.ts` yields `d.ts`, then `ts`.
/// Dotfiles without further dots yield nothing (exact table owns them).
fn suffixes(lower: &str) -> impl Iterator<Item = &str> {
    let mut rest = lower.strip_prefix('.').unwrap_or(lower);
    // A leading `foo.` still counts: `d.ts` must surface for `a.d.ts`.
    std::iter::from_fn(move || {
        let dot = rest.find('.')?;
        rest = &rest[dot + 1..];
        Some(rest)
    })
}

/// Suffix (extension) lookup, mirroring upstream's `fileExtensions`
/// mappings for the vendored set.
fn suffix_icon(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "d.ts" | "d.mts" | "d.cts" => "typescript-def",
        "ts" | "mts" | "cts" => "typescript",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" | "tsx" => "react",
        "rs" | "ron" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "go" => "go",
        "json" | "jsonc" | "json5" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" | "mdx" | "rst" | "txt" => "markdown",
        "html" | "htm" => "html",
        "css" | "less" => "css",
        "scss" | "sass" => "sass",
        "vue" | "svelte" => "vue",
        "sh" | "bash" | "zsh" | "fish" | "nu" | "bat" | "cmd" | "ps1" => "console",
        "mk" => "makefile",
        "patch" | "diff" => "git",
        "lock" => "lock",
        "log" => "log",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" | "svg" => "image",
        "xml" | "plist" | "xsd" => "xml",
        "rb" => "ruby",
        "php" => "php",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "scala" | "sc" => "scala",
        "hs" | "lhs" => "haskell",
        "graphql" | "gql" => "graphql",
        "proto" => "proto",
        "key" | "pem" | "crt" | "cer" => "key",
        "zip" | "tar" | "gz" | "tgz" | "rar" | "7z" => "zip",
        "pdf" => "pdf",
        "mp4" | "mov" | "mkv" | "webm" | "avi" => "video",
        "mp3" | "wav" | "ogg" | "flac" => "audio",
        _ => return None,
    })
}

/// Raw bytes for an icon key. Every arm coerces to `&[u8]`; the `_` arm
/// keeps unknown keys rendering instead of panicking.
pub(crate) fn icon_svg(key: &str) -> &'static [u8] {
    match key {
        "rust" => include_bytes!("../../assets/icons/files/rust.svg"),
        "typescript" => include_bytes!("../../assets/icons/files/typescript.svg"),
        "typescript-def" => include_bytes!("../../assets/icons/files/typescript-def.svg"),
        "javascript" => include_bytes!("../../assets/icons/files/javascript.svg"),
        "react" => include_bytes!("../../assets/icons/files/react.svg"),
        "python" => include_bytes!("../../assets/icons/files/python.svg"),
        "go" => include_bytes!("../../assets/icons/files/go.svg"),
        "go-mod" => include_bytes!("../../assets/icons/files/go-mod.svg"),
        "json" => include_bytes!("../../assets/icons/files/json.svg"),
        "yaml" => include_bytes!("../../assets/icons/files/yaml.svg"),
        "toml" => include_bytes!("../../assets/icons/files/toml.svg"),
        "markdown" => include_bytes!("../../assets/icons/files/markdown.svg"),
        "readme" => include_bytes!("../../assets/icons/files/readme.svg"),
        "html" => include_bytes!("../../assets/icons/files/html.svg"),
        "css" => include_bytes!("../../assets/icons/files/css.svg"),
        "sass" => include_bytes!("../../assets/icons/files/sass.svg"),
        "vue" => include_bytes!("../../assets/icons/files/vue.svg"),
        "svelte" => include_bytes!("../../assets/icons/files/svelte.svg"),
        "console" => include_bytes!("../../assets/icons/files/console.svg"),
        "docker" => include_bytes!("../../assets/icons/files/docker.svg"),
        "makefile" => include_bytes!("../../assets/icons/files/makefile.svg"),
        "git" => include_bytes!("../../assets/icons/files/git.svg"),
        "npm" => include_bytes!("../../assets/icons/files/npm.svg"),
        "lock" => include_bytes!("../../assets/icons/files/lock.svg"),
        "log" => include_bytes!("../../assets/icons/files/log.svg"),
        "image" => include_bytes!("../../assets/icons/files/image.svg"),
        "xml" => include_bytes!("../../assets/icons/files/xml.svg"),
        "ruby" => include_bytes!("../../assets/icons/files/ruby.svg"),
        "php" => include_bytes!("../../assets/icons/files/php.svg"),
        "java" => include_bytes!("../../assets/icons/files/java.svg"),
        "c" => include_bytes!("../../assets/icons/files/c.svg"),
        "cpp" => include_bytes!("../../assets/icons/files/cpp.svg"),
        "swift" => include_bytes!("../../assets/icons/files/swift.svg"),
        "kotlin" => include_bytes!("../../assets/icons/files/kotlin.svg"),
        "lua" => include_bytes!("../../assets/icons/files/lua.svg"),
        "scala" => include_bytes!("../../assets/icons/files/scala.svg"),
        "haskell" => include_bytes!("../../assets/icons/files/haskell.svg"),
        "graphql" => include_bytes!("../../assets/icons/files/graphql.svg"),
        "proto" => include_bytes!("../../assets/icons/files/proto.svg"),
        "key" => include_bytes!("../../assets/icons/files/key.svg"),
        "zip" => include_bytes!("../../assets/icons/files/zip.svg"),
        "pdf" => include_bytes!("../../assets/icons/files/pdf.svg"),
        "video" => include_bytes!("../../assets/icons/files/video.svg"),
        "audio" => include_bytes!("../../assets/icons/files/audio.svg"),
        "license" => include_bytes!("../../assets/icons/files/license.svg"),
        "changelog" => include_bytes!("../../assets/icons/files/changelog.svg"),
        _ => include_bytes!("../../assets/icons/files/document.svg"),
    }
}

/// Rasterized full-color icon tiles, keyed by icon key.
pub(crate) type IconTiles = HashMap<&'static str, Arc<RenderImage>>;

/// Display size of sidebar icons; tiles rasterize at 3x for retina.
pub(crate) const ICON_PX: f32 = 16.0;
const RASTER_SCALE: f32 = 3.0;

/// Rasterize tiles for every icon `names` resolve to (plus the fallback),
/// skipping keys already cached. Call when a new diff loads — each SVG
/// parses and rasterizes once per view lifetime, never per frame.
/// Unparseable SVGs are skipped; rows fall back to an empty slot.
pub(crate) fn ensure_tiles<'a>(
    names: impl IntoIterator<Item = &'a str>,
    tiles: &mut IconTiles,
    cx: &App,
) {
    let mut keys: HashSet<&'static str> = names.into_iter().map(icon_key).collect();
    keys.insert(FALLBACK);
    let renderer = cx.svg_renderer();
    for key in keys {
        if tiles.contains_key(key) {
            continue;
        }
        if let Ok(tile) = renderer.render_single_frame(icon_svg(key), RASTER_SCALE) {
            tiles.insert(key, tile);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_win_over_extensions() {
        assert_eq!(icon_key("Dockerfile"), "docker");
        assert_eq!(icon_key("Dockerfile.dev"), "docker");
        assert_eq!(icon_key("dockerfile.production"), "docker");
        assert_eq!(icon_key("Makefile"), "makefile");
        assert_eq!(icon_key("GNUmakefile"), "makefile");
        assert_eq!(icon_key("go.mod"), "go-mod");
        assert_eq!(icon_key("go.sum"), "go-mod");
        assert_eq!(icon_key("package.json"), "npm");
        assert_eq!(icon_key("README.md"), "readme");
        assert_eq!(icon_key("readme"), "readme");
        assert_eq!(icon_key("LICENSE"), "license");
        assert_eq!(icon_key("COPYING.txt"), "license");
        assert_eq!(icon_key("CHANGELOG.md"), "changelog");
        assert_eq!(icon_key(".gitignore"), "git");
        assert_eq!(icon_key(".gitattributes"), "git");
        assert_eq!(icon_key("COMMIT_EDITMSG"), "git");
    }

    #[test]
    fn suffixes_match_longest_first_then_plain() {
        assert_eq!(icon_key("types.d.ts"), "typescript-def");
        assert_eq!(icon_key("app.d.mts"), "typescript-def");
        assert_eq!(icon_key("main.ts"), "typescript");
        assert_eq!(icon_key("app.tsx"), "react");
        assert_eq!(icon_key("app.jsx"), "react");
        assert_eq!(icon_key("lib.rs"), "rust");
        assert_eq!(icon_key("run.PY"), "python");
        assert_eq!(icon_key("notes.md"), "markdown");
        assert_eq!(icon_key("notes.txt"), "markdown");
        assert_eq!(icon_key("data.yaml"), "yaml");
        assert_eq!(icon_key("run.sh"), "console");
        assert_eq!(icon_key("fix.patch"), "git");
        assert_eq!(icon_key("yarn.lock"), "lock");
        assert_eq!(icon_key("photo.PNG"), "image");
    }

    #[test]
    fn unknown_names_fall_back_to_document() {
        assert_eq!(icon_key("Makefile.am"), FALLBACK);
        assert_eq!(icon_key("archive.tar.gz"), "zip");
        assert_eq!(icon_key("archive.tar.zzz"), FALLBACK);
        assert_eq!(icon_key("main.zig"), FALLBACK);
        assert_eq!(icon_key("noextension"), FALLBACK);
        assert_eq!(icon_key(".env"), FALLBACK);
    }

    #[test]
    fn full_paths_resolve_by_basename() {
        assert_eq!(icon_key("src/Dockerfile"), "docker");
        assert_eq!(icon_key("src/app.d.ts"), "typescript-def");
        assert_eq!(icon_key(".github/workflows/ci.yml"), "yaml");
    }

    #[test]
    fn every_key_resolves_to_non_empty_svg() {
        for name in [
            "a.rs",
            "a.ts",
            "a.d.ts",
            "a.js",
            "a.tsx",
            "a.py",
            "a.go",
            "go.mod",
            "a.json",
            "a.yaml",
            "a.toml",
            "a.md",
            "README.md",
            "a.html",
            "a.css",
            "a.scss",
            "a.vue",
            "a.sh",
            "Dockerfile",
            "Makefile",
            ".gitignore",
            "package.json",
            "a.lock",
            "a.log",
            "a.png",
            "a.xml",
            "LICENSE",
            "CHANGELOG.md",
            "mystery.zzz",
        ] {
            let bytes = icon_svg(icon_key(name));
            assert!(!bytes.is_empty(), "{name} resolved to empty bytes");
            assert!(
                bytes.starts_with(b"<svg"),
                "{name} does not look like an SVG"
            );
        }
    }
}
