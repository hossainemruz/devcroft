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

/// Preview reads use the same no-symlink, bounded reader as project search.
pub(super) fn preview(root: &Path, file: &ProjectFile) -> String {
    let bytes = match project::read_indexed_file(root, file) {
        Ok(bytes) => bytes,
        Err(error) => return format!("Preview unavailable: {error}"),
    };
    if bytes.contains(&0) {
        return "Binary file — press Enter to open".into();
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return "Preview unavailable: file is not UTF-8".into();
    };
    let mut preview = String::new();
    for (index, line) in text.lines().enumerate() {
        if index >= 300 || preview.len() + line.len() > 40_000 {
            preview.push_str("\n… Preview truncated\n");
            break;
        }
        preview.push_str(line);
        preview.push('\n');
    }
    preview
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
            preview(dir.path(), &results.files[0]),
            "fn navigation() {}\n"
        );
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
