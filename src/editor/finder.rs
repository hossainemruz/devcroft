//! fff ranking over the editor's bounded, ignore-aware checkout index.

use std::{collections::HashMap, path::Path};

use anyhow::{Result, bail};
use fff_search::{FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, QueryParser};

use super::project::{self, ProjectFile};

pub(super) struct Finder {
    pickers: Vec<FilePicker>,
    files: HashMap<usize, ProjectFile>,
    _index_root: tempfile::TempDir,
}

#[derive(Default)]
pub(super) struct Matches {
    pub files: Vec<ProjectFile>,
    pub total: usize,
}

impl Finder {
    /// Index only path names. fff's insertion API reads file contents for
    /// binary detection, so root its virtual entries in an empty directory
    /// and map results back to the real project index. Absent entries have
    /// zero metadata and require no file reads. This also avoids fff's own
    /// discovery rules overriding our ignore, hidden-file and checkout policy.
    /// The insertion API reserves 1024 entries per picker; merge their pages
    /// by fff's scores. The temporary directory stays empty throughout.
    pub fn new(files: &[ProjectFile]) -> Result<Self> {
        let index_root = tempfile::tempdir()?;
        let mut pickers = Vec::new();
        let mut originals = HashMap::new();
        for partition in files.chunks(fff_search::constants::MAX_OVERFLOW_FILES) {
            let mut picker = FilePicker::new(FilePickerOptions {
                base_path: index_root.path().to_string_lossy().into_owned(),
                watch: false,
                follow_symlinks: false,
                ..Default::default()
            })?;
            for file in partition {
                let Some(item) = picker.add_new_file(&index_root.path().join(&file.label)) else {
                    bail!("fff could not index {}", file.label);
                };
                // fff keeps inserted items in stable allocations and returns
                // those references from search. Identity preserves distinct
                // paths even when their lossy display labels are identical.
                originals.insert(item as *const fff_search::FileItem as usize, file.clone());
            }
            pickers.push(picker);
        }
        Ok(Self {
            pickers,
            files: originals,
            _index_root: index_root,
        })
    }

    pub fn search(&self, query: &str, cancelled: impl Fn() -> bool) -> Option<Matches> {
        let parser = QueryParser::default();
        let query = parser.parse(query);
        let mut total = 0;
        let mut ranked = Vec::new();
        for picker in &self.pickers {
            if cancelled() {
                return None;
            }
            let results = picker.fuzzy_search(
                &query,
                None,
                FuzzySearchOptions {
                    max_threads: 1,
                    pagination: PaginationArgs {
                        offset: 0,
                        limit: 200,
                    },
                    ..Default::default()
                },
            );
            total += results.total_matched;
            ranked.extend(
                results
                    .items
                    .iter()
                    .zip(&results.scores)
                    .filter_map(|(file, score)| {
                        self.files
                            .get(&(*file as *const fff_search::FileItem as usize))
                            .map(|file| (score.total, file.clone()))
                    }),
            );
        }
        ranked.sort_unstable_by(|(a_score, a), (b_score, b)| {
            b_score.cmp(a_score).then_with(|| a.label.cmp(&b.label))
        });
        Some(Matches {
            total,
            files: ranked.into_iter().take(200).map(|(_, file)| file).collect(),
        })
    }
}

#[derive(Default)]
pub(super) struct Preview {
    pub text: String,
    pub first_line: usize,
    cropped_line_prefix: Option<(usize, usize)>,
    is_source: bool,
}

impl Preview {
    /// Locate the literal hit in the displayed line, after any preview
    /// cropping. Use the same Unicode case folding as project grep.
    pub fn match_range(&self, line: usize, query: &str) -> Option<std::ops::Range<usize>> {
        if !self.is_source || query.is_empty() || line < self.first_line {
            return None;
        }
        let start = self
            .text
            .split_inclusive('\n')
            .take(line - self.first_line)
            .map(str::len)
            .sum::<usize>();
        let text = self.text.get(start..)?.lines().next()?;
        let prefix = self
            .cropped_line_prefix
            .filter(|(cropped_line, _)| *cropped_line == line)
            .map_or(0, |(_, bytes)| bytes);
        let text = text.get(prefix..)?;
        let matcher = regex::bytes::RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()
            .ok()?;
        let found = matcher.find(text.as_bytes())?;
        Some(start + prefix + found.start()..start + prefix + found.end())
    }
}

impl From<String> for Preview {
    fn from(text: String) -> Self {
        Self {
            text,
            first_line: 1,
            ..Default::default()
        }
    }
}

/// Preview reads use the same no-symlink, bounded reader as project search.
pub(super) fn preview(
    root: &Path,
    file: &ProjectFile,
    line: Option<usize>,
    column: usize,
) -> Preview {
    let bytes = match project::read_indexed_file(root, file) {
        Ok(bytes) => bytes,
        Err(error) => return format!("Preview unavailable: {error}").into(),
    };
    if bytes.contains(&0) {
        return "Binary file — press Enter to open".to_owned().into();
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return "Preview unavailable: file is not UTF-8".to_owned().into();
    };
    preview_text(text, line, column)
}

/// The same bounded presentation can preview an unsaved open buffer.
pub(super) fn preview_text(text: &str, line: Option<usize>, column: usize) -> Preview {
    let target = line.unwrap_or(1).saturating_sub(1);
    // Reserve half the byte budget for the hit itself. Large preceding lines
    // must not consume the preview before we reach the selected match.
    let mut context = std::collections::VecDeque::new();
    let mut context_bytes = 0;
    for (index, line) in text.lines().take(target).enumerate() {
        context.push_back((index, line));
        context_bytes += line.len() + 1;
        while context.len() > 20 || context_bytes > 20_000 {
            if let Some((_, line)) = context.pop_front() {
                context_bytes -= line.len() + 1;
            }
        }
    }
    let first_line = context.front().map_or(target + 1, |(index, _)| index + 1);
    let mut preview = String::new();
    let mut cropped_line_prefix = None;
    let context_lines = context.len();
    for (_, line) in context {
        preview.push_str(line);
        preview.push('\n');
    }
    for (index, line) in text.lines().skip(target).enumerate() {
        if index + context_lines >= 300 || preview.len() + line.len() + 1 > 40_000 {
            if index == 0 {
                // Show the selected portion of a very long matching line,
                // with UTF-8 boundaries intact, rather than omit it entirely.
                let mut start = column.saturating_sub(200).min(line.len());
                while !line.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (start + 39_990 - preview.len()).min(line.len());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                if start > 0 {
                    preview.push_str("… ");
                    cropped_line_prefix = Some((target + 1, "… ".len()));
                }
                preview.push_str(&line[start..end]);
            }
            preview.push_str("\n… Preview truncated\n");
            break;
        }
        preview.push_str(line);
        preview.push('\n');
    }
    Preview {
        text: preview,
        first_line,
        cropped_line_prefix,
        is_source: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn fff_ranks_typos_and_stays_inside_the_project_index() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join(".hidden.rs"), "hidden\n").unwrap();
        fs::write(dir.path().join("src/navigation.rs"), "fn navigation() {}\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path().join("ignored.rs"), "ignored\n").unwrap();
        fs::create_dir_all(dir.path().join("nested/.git")).unwrap();
        fs::write(dir.path().join("nested/navigation.rs"), "nested\n").unwrap();
        let files = project::scan(dir.path());
        let finder = Finder::new(&files).unwrap();
        let results = finder.search("navigaton", || false).unwrap();
        assert_eq!(results.files[0].label, "src/navigation.rs");
        assert_eq!(
            finder.search(".hidden", || false).unwrap().files[0].label,
            ".hidden.rs"
        );
        let all = finder.search("", || false).unwrap();
        assert_eq!(all.total, files.len());
        assert!(
            all.files
                .iter()
                .all(|file| files.iter().any(|allowed| allowed.label == file.label))
        );
        assert_eq!(
            preview(dir.path(), &results.files[0], None, 0).text,
            "fn navigation() {}\n"
        );
    }
    #[test]
    fn grep_preview_keeps_the_hit_after_large_context_and_in_long_unicode_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        let prefix = "é".repeat(50_000);
        fs::write(&path, format!("{}\n{}needle\n", "x".repeat(50_000), prefix)).unwrap();
        let file = ProjectFile {
            path,
            label: "long.txt".into(),
        };
        let result = preview(dir.path(), &file, Some(2), prefix.len());
        assert_eq!(result.first_line, 2);
        assert!(result.text.contains("needle"));
        assert_eq!(
            &result.text[result.match_range(2, "needle").unwrap()],
            "needle"
        );
        assert!(result.text.len() < 40_100);
    }

    #[test]
    fn preview_word_ranges_preserve_utf8_case_folding_and_literal_punctuation() {
        let preview = Preview {
            text: "// café\nlet CAFÉ = [x].*;\n".into(),
            first_line: 30,
            is_source: true,
            ..Default::default()
        };
        let found = preview.match_range(31, "café").unwrap();
        assert_eq!(&preview.text[found], "CAFÉ");
        let found = preview.match_range(31, "[X].*").unwrap();
        assert_eq!(&preview.text[found], "[x].*");
        assert!(preview.match_range(29, "café").is_none());
        assert!(preview.match_range(32, "café").is_none());
        assert!(preview.match_range(31, "missing").is_none());
        assert!(preview.match_range(31, "").is_none());
        let status = Preview::from("Preview unavailable: file is not UTF-8".to_owned());
        assert!(status.match_range(1, "file").is_none());
    }

    #[test]
    fn clipped_preview_matches_file_content_instead_of_the_synthetic_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        let file = ProjectFile {
            path: path.clone(),
            label: "long.txt".into(),
        };
        let prefix = "é".repeat(50_000);
        for query in [" ", "…"] {
            fs::write(&path, format!("{prefix}{query}end\n")).unwrap();
            let preview = preview(dir.path(), &file, Some(1), prefix.len());
            assert!(preview.text.starts_with("… "));
            let range = preview.match_range(1, query).unwrap();
            assert!(
                range.start > 100,
                "must skip the generated ellipsis and space"
            );
            assert_eq!(&preview.text[range], query);
        }
    }

    #[test]
    fn search_merges_more_than_one_fff_insertion_partition() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for index in 0..1030 {
            let label = format!("file-{index:04}.rs");
            let path = dir.path().join(&label);
            fs::write(&path, "fn main() {}\n").unwrap();
            files.push(ProjectFile { path, label });
        }
        let finder = Finder::new(&files).unwrap();
        assert_eq!(finder.search("", || false).unwrap().total, 1030);
        assert_eq!(finder.search("", || false).unwrap().files.len(), 200);
        assert_eq!(
            finder.search("file-1029.rs", || false).unwrap().files[0].label,
            "file-1029.rs"
        );
    }
    #[cfg(unix)]
    #[test]
    fn display_label_collisions_preserve_distinct_original_paths() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for byte in [0x80, 0x81] {
            let name = std::ffi::OsString::from_vec(vec![byte, b'.', b'r', b's']);
            let path = dir.path().join(&name);
            files.push(ProjectFile {
                path,
                label: name.to_string_lossy().into_owned(),
            });
        }
        let finder = Finder::new(&files).unwrap();
        let all = finder.search("", || false).unwrap();
        assert_eq!(all.total, 2);
        assert_ne!(all.files[0].path, all.files[1].path);
        assert!(finder.search("", || true).is_none());
    }
}
