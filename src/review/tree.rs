//! File-tree state for the Review tab.
//!
//! [`build_file_tree`] groups a [`ReviewDiff`](super::model::ReviewDiff)'s
//! files into folder items for gpui-kit's virtualized `tree`, alongside a
//! metadata map the row renderer uses for status badges and stats. All
//! folders start expanded: reviews are read top-to-bottom, and collapsing
//! is one click away.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gpui_kit::component::tree::TreeItem;

use super::model::{ChangedFile, FileStatus};

/// Item-id prefix for file rows; anything else is a folder id.
pub(crate) const FILE_ID_PREFIX: &str = "file:";
const DIR_ID_PREFIX: &str = "dir:";

/// Row metadata keyed by tree item id.
#[derive(Clone, Debug)]
pub(crate) struct TreeRowMeta {
    pub(crate) name: String,
    pub(crate) is_folder: bool,
    pub(crate) status: Option<FileStatus>,
}

pub(crate) fn file_item_id(path: &str) -> String {
    format!("{FILE_ID_PREFIX}{path}")
}

pub(crate) fn file_path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix(FILE_ID_PREFIX)
}

/// Build folder-grouped tree items plus per-row metadata.
///
/// `files` must arrive in [`compare_review_paths`](super::model::compare_review_paths)
/// order (as [`git`](super::git) produces): each level then lists subfolders
/// before files with no extra sorting, and the flattened pre-order reads in
/// exactly stream order.
pub(crate) fn build_file_tree(
    files: &[ChangedFile],
) -> (Vec<TreeItem>, HashMap<String, TreeRowMeta>) {
    let mut metas = HashMap::new();
    let mut dir_files: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut all_dirs: BTreeSet<String> = BTreeSet::new();
    for (index, file) in files.iter().enumerate() {
        let dir = parent_dir(&file.path);
        dir_files.entry(dir.clone()).or_default().push(index);
        // Register every ancestor so intermediate folders exist even when
        // no file sits directly in them.
        let mut prefix = String::new();
        for component in dir.split('/').filter(|part| !part.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            all_dirs.insert(prefix.clone());
        }
        let id = file_item_id(&file.path);
        metas.insert(
            id.clone(),
            TreeRowMeta {
                name: file_name(&file.path).to_owned(),
                is_folder: false,
                status: Some(file.status),
            },
        );
    }
    let items = dir_items("", files, &dir_files, &all_dirs, &mut metas);
    (items, metas)
}

fn dir_items(
    dir: &str,
    files: &[ChangedFile],
    dir_files: &BTreeMap<String, Vec<usize>>,
    all_dirs: &BTreeSet<String>,
    metas: &mut HashMap<String, TreeRowMeta>,
) -> Vec<TreeItem> {
    let mut items = Vec::new();
    for sub in all_dirs
        .iter()
        .filter(|candidate| parent_dir(candidate) == dir)
    {
        let id = format!("{DIR_ID_PREFIX}{sub}");
        metas.insert(
            id.clone(),
            TreeRowMeta {
                name: file_name(sub).to_owned(),
                is_folder: true,
                status: None,
            },
        );
        let children = dir_items(sub, files, dir_files, all_dirs, metas);
        items.push(
            TreeItem::new(id, file_name(sub))
                .expanded(true)
                .children(children),
        );
    }
    if let Some(indices) = dir_files.get(dir) {
        for &index in indices {
            let file = &files[index];
            items.push(TreeItem::new(
                file_item_id(&file.path),
                file_name(&file.path),
            ));
        }
    }
    items
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_owned(),
        None => String::new(),
    }
}

fn file_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[index + 1..],
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{FileContent, UnavailableReason};
    use super::*;

    fn file(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::Missing),
        }
    }

    #[test]
    fn groups_files_into_expanded_folders() {
        let files = vec![
            file("README.md"),
            file("src/main.rs"),
            file("src/ui/tree.rs"),
        ];
        let (items, metas) = build_file_tree(&files);
        // Root level: one file plus one folder.
        assert_eq!(items.len(), 2);
        assert_eq!(metas.len(), 5);
        let folder = items
            .iter()
            .find(|item| item.id.as_str() == "dir:src")
            .unwrap();
        assert!(folder.is_folder());
        assert!(folder.is_expanded());
        assert_eq!(metas["file:src/ui/tree.rs"].name, "tree.rs");
        assert!(metas["dir:src/ui"].is_folder);
    }

    #[test]
    fn file_ids_round_trip() {
        assert_eq!(file_path_from_id("file:src/a.rs"), Some("src/a.rs"));
        assert_eq!(file_path_from_id("dir:src"), None);
    }

    #[test]
    fn top_level_lists_folders_before_files_in_stream_order() {
        use super::super::model::compare_review_paths;
        let mut files = vec![
            file("README.md"),
            file("src-old.rs"),
            file("src/main.rs"),
            file("a/b.rs"),
        ];
        files.sort_by(|a, b| compare_review_paths(&a.path, &b.path));
        let (items, _) = build_file_tree(&files);
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["dir:a", "dir:src", "file:README.md", "file:src-old.rs"]
        );
    }
}
