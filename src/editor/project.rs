//! Bounded checkout discovery for the built-in editor. The same index feeds
//! the file tree, fuzzy opener, and text search, so all three honor ignores.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

const MAX_FILES: usize = 100_000;
const MAX_SEARCH_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 200;

#[derive(Clone, Debug)]
pub(super) struct ProjectFile {
    pub path: PathBuf,
    pub label: String,
}

#[derive(Clone, Debug)]
pub(super) struct TextMatch {
    pub path: PathBuf,
    pub label: String,
    pub line: usize,
    pub column: usize,
    pub preview: String,
}

#[derive(Default)]
pub(super) struct TextMatches {
    pub items: Vec<TextMatch>,
    pub truncated: bool,
    pub error: Option<String>,
}

struct LiteralMatcher(regex::bytes::Regex);

impl fff_grep::Matcher for LiteralMatcher {
    type Error = fff_grep::NoError;

    fn find_at(&self, bytes: &[u8], at: usize) -> Result<Option<fff_grep::Match>, Self::Error> {
        Ok(self
            .0
            .find_at(bytes, at)
            .map(|m| fff_grep::Match::new(m.start(), m.end())))
    }

    fn line_terminator(&self) -> Option<fff_grep::LineTerminator> {
        Some(fff_grep::LineTerminator::byte(b'\n'))
    }
}

struct TextSink<'a, F> {
    file: &'a ProjectFile,
    out: &'a mut TextMatches,
    cancelled: &'a F,
    matcher: &'a LiteralMatcher,
}

impl<F: Fn() -> bool> fff_grep::Sink for TextSink<'_, F> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _: &fff_grep::Searcher,
        m: &fff_grep::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if (self.cancelled)() {
            return Ok(false);
        }
        if self.out.items.len() == MAX_RESULTS {
            self.out.truncated = true;
            return Ok(false);
        }
        self.out.items.push(TextMatch {
            path: self.file.path.clone(),
            label: self.file.label.clone(),
            line: m.line_number().unwrap_or(1) as usize,
            column: self.matcher.0.find(m.bytes()).map_or(0, |m| m.start()),
            preview: String::from_utf8_lossy(m.bytes())
                .trim()
                .chars()
                .take(160)
                .collect(),
        });
        Ok(true)
    }
}

/// `ignore` applies .gitignore, global excludes, and parent rules. Hidden
/// source files remain visible; repository metadata and nested checkouts do
/// not enter the index. Symlinks are deliberately not followed.
pub(super) fn scan(root: &Path) -> Vec<ProjectFile> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let root = root.as_path();
    let filter_root = root.to_owned();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .filter_entry(move |entry| {
            if entry.file_name() == ".git" {
                return false;
            }
            !(entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry.path() != filter_root
                && entry.path().join(".git").exists())
        })
        .build();
    let mut files = Vec::new();
    for entry in walker.flatten() {
        if files.len() >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        files.push(ProjectFile {
            path: entry.path().to_owned(),
            label: relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        });
    }
    files.sort_unstable_by(|a, b| a.label.cmp(&b.label));
    files
}

/// Search only indexed files and cap memory, per-file I/O, and result count.
/// Results are line based so choosing one can open at that location.
pub(super) fn search_text(
    root: &Path,
    files: &[ProjectFile],
    query: &str,
    cancelled: impl Fn() -> bool,
) -> Option<TextMatches> {
    if query.is_empty() {
        return Some(TextMatches::default());
    }
    let Ok(root) = root.canonicalize() else {
        return Some(TextMatches::default());
    };
    // Literal, Unicode-aware case-insensitive matching. fff's slice grep
    // engine runs over our bounded reads instead of reopening checkout paths.
    let matcher = match regex::bytes::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
    {
        Ok(regex) => LiteralMatcher(regex),
        Err(error) => {
            return Some(TextMatches {
                error: Some(format!("Search unavailable: {error}")),
                ..Default::default()
            });
        }
    };
    let searcher = fff_grep::SearcherBuilder::new().line_number(true).build();
    let mut out = TextMatches::default();
    for file in files {
        if cancelled() {
            return None;
        }
        if out.truncated {
            break;
        }
        let Ok(bytes) = read_indexed_file(&root, file) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(_) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let _ = searcher.search_slice(
            &matcher,
            &bytes,
            TextSink {
                file,
                out: &mut out,
                cancelled: &cancelled,
                matcher: &matcher,
            },
        );
    }
    if cancelled() { None } else { Some(out) }
}

/// Reopen every component relative to the checkout so a path swapped to a
/// symlink after indexing cannot make project search read outside files.
#[cfg(unix)]
pub(super) fn read_indexed_file(root: &Path, entry: &ProjectFile) -> std::io::Result<Vec<u8>> {
    use std::os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _};
    use std::{
        ffi::CString,
        os::fd::{AsRawFd as _, FromRawFd as _},
    };

    // Retain original OS path bytes, including non-UTF-8 filenames. The
    // display label is lossy and must never be used to reopen a file.
    // Normalize an alias root only when needed to align it with the scan's
    // canonical paths. A replaced canonical root still goes through the
    // O_NOFOLLOW open below instead of resolving a new symlink target.
    let canonical;
    let (root, relative) = match entry.path.strip_prefix(root) {
        Ok(relative) => (root, relative),
        Err(_) => {
            canonical = root.canonicalize()?;
            let relative = entry
                .path
                .strip_prefix(&canonical)
                .map_err(|_| std::io::Error::other("indexed path escaped checkout"))?;
            (canonical.as_path(), relative)
        }
    };
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(part) = part else {
            return Err(std::io::Error::other("invalid indexed path"));
        };
        let part = CString::new(part.as_bytes())
            .map_err(|_| std::io::Error::other("invalid indexed path"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if parts.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the parent descriptor is live and the component is NUL terminated.
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), part.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        let opened = unsafe { fs::File::from_raw_fd(descriptor) };
        if parts.peek().is_some() {
            directory = opened;
            continue;
        }
        let metadata = opened.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_SEARCH_BYTES {
            return Err(std::io::Error::other("not a searchable file"));
        }
        let mut bytes = Vec::new();
        opened.take(MAX_SEARCH_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SEARCH_BYTES {
            return Err(std::io::Error::other("file grew beyond search limit"));
        }
        return Ok(bytes);
    }
    Err(std::io::Error::other("empty indexed path"))
}

#[cfg(windows)]
pub(super) fn read_indexed_file(root: &Path, entry: &ProjectFile) -> std::io::Result<Vec<u8>> {
    use std::os::windows::{ffi::OsStringExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let file = fs::File::open(&entry.path)?;
    // Check the opened handle, rather than a pathname that could be swapped
    // between canonicalization and open.
    let mut name = vec![0u16; 32_768];
    // SAFETY: the file handle is open and `name` provides its stated capacity.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            0,
        )
    } as usize;
    if length == 0 || length >= name.len() {
        return Err(std::io::Error::last_os_error());
    }
    let opened_path = PathBuf::from(std::ffi::OsString::from_wide(&name[..length]));
    if !opened_path.starts_with(root) {
        return Err(std::io::Error::other("indexed path escaped checkout"));
    }
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a searchable file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SEARCH_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SEARCH_BYTES {
        return Err(std::io::Error::other("file grew beyond search limit"));
    }
    Ok(bytes)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn read_indexed_file(_root: &Path, _entry: &ProjectFile) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other(
        "project search is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_and_search_respect_ignores() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("visible.rs"), "let needle = 1;\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/child.rs"), "child\n").unwrap();
        let files = scan(dir.path());
        assert!(files.iter().any(|file| file.label == "visible.rs"));
        assert!(files.iter().any(|file| file.label == "nested/child.rs"));
        assert!(!files.iter().any(|file| file.label == "ignored.txt"));
        let matches = search_text(dir.path(), &files, "needle", || false)
            .unwrap()
            .items;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 1);
    }

    #[test]
    fn fff_grep_matches_literal_unicode_text_and_preserves_line_locations() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.txt"),
            "before\r\n  CAFÉ [x].*\r\nlast [X].*",
        )
        .unwrap();
        fs::write(dir.path().join("b.txt"), "café [x].*\n").unwrap();
        fs::write(dir.path().join("binary.txt"), b"[x].*\0").unwrap();
        fs::write(dir.path().join("invalid.txt"), b"[x].*\xff").unwrap();
        fs::write(dir.path().join("huge.txt"), "[x].*".repeat(500_000)).unwrap();
        let files = scan(dir.path());
        let found = search_text(dir.path(), &files, "[x].*", || false).unwrap();
        assert_eq!(
            found
                .items
                .iter()
                .map(|m| (m.label.as_str(), m.line))
                .collect::<Vec<_>>(),
            vec![("a.txt", 2), ("a.txt", 3), ("b.txt", 1)]
        );
        assert_eq!(found.items[0].preview, "CAFÉ [x].*");
        assert_eq!(
            search_text(dir.path(), &files, "café", || false)
                .unwrap()
                .items
                .len(),
            2
        );
        assert!(
            search_text(dir.path(), &files, "", || false)
                .unwrap()
                .items
                .is_empty()
        );
        assert!(search_text(dir.path(), &files, "[x].*", || true).is_none());
    }

    #[test]
    fn fff_grep_caps_matching_lines_and_reports_more_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.txt");
        fs::write(&path, "needle\n".repeat(MAX_RESULTS)).unwrap();
        let files = scan(dir.path());
        let exact = search_text(dir.path(), &files, "needle", || false).unwrap();
        assert_eq!(exact.items.len(), MAX_RESULTS);
        assert!(!exact.truncated);
        fs::write(path, "needle\n".repeat(MAX_RESULTS + 1)).unwrap();
        let extra = search_text(dir.path(), &files, "needle", || false).unwrap();
        assert_eq!(extra.items.len(), MAX_RESULTS);
        assert!(extra.truncated);
    }

    // macOS filesystems reject filenames containing invalid UTF-8.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn grep_reads_original_non_utf8_filenames() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"bad-\xff.txt".to_vec());
        fs::write(dir.path().join(&name), "needle\n").unwrap();
        let files = scan(dir.path());
        let found = search_text(dir.path(), &files, "needle", || false).unwrap();
        assert_eq!(found.items.len(), 1);
        assert_eq!(found.items[0].path.file_name(), Some(name.as_os_str()));
    }

    #[cfg(unix)]
    #[test]
    fn grep_rejects_indexed_parent_directory_replaced_by_a_symlink() {
        use std::os::unix::fs::symlink;
        let checkout = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(checkout.path().join("src")).unwrap();
        fs::write(checkout.path().join("src/file.txt"), "safe\n").unwrap();
        let files = scan(checkout.path());
        fs::write(outside.path().join("file.txt"), "outside-secret\n").unwrap();
        fs::remove_file(checkout.path().join("src/file.txt")).unwrap();
        fs::remove_dir(checkout.path().join("src")).unwrap();
        symlink(outside.path(), checkout.path().join("src")).unwrap();
        assert!(
            search_text(checkout.path(), &files, "outside-secret", || false)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn search_skips_indexed_file_replaced_with_outside_symlink() {
        use std::os::unix::fs::symlink;
        let checkout = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let indexed = checkout.path().join("indexed.txt");
        fs::write(&indexed, "safe\n").unwrap();
        let files = scan(checkout.path());
        fs::write(outside.path().join("secret.txt"), "outside-secret\n").unwrap();
        fs::remove_file(&indexed).unwrap();
        symlink(outside.path().join("secret.txt"), &indexed).unwrap();
        assert!(
            search_text(checkout.path(), &files, "outside-secret", || false)
                .unwrap()
                .items
                .is_empty()
        );
    }
}
