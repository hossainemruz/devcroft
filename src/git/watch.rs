use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct FileChanges {
    watcher: Option<RecommendedWatcher>,
    changed: Arc<AtomicBool>,
}

impl FileChanges {
    pub(super) fn new(root: &Path) -> Self {
        let changed = Arc::new(AtomicBool::new(false));
        let pending = changed.clone();
        let checkout = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        let (root, git_dirs) = match gix::discover(&checkout) {
            Ok(repo) => (
                repo.workdir().unwrap_or(&checkout).to_owned(),
                vec![repo.git_dir().to_owned(), repo.common_dir().to_owned()],
            ),
            Err(_) => (checkout, Vec::new()),
        };
        let watcher = (|| -> notify::Result<RecommendedWatcher> {
            let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                if event.map_or(true, |event| !matches!(event.kind, EventKind::Access(_))) {
                    pending.store(true, Ordering::Release);
                }
            })?;
            watcher.watch(&root, RecursiveMode::Recursive)?;
            for git_dir in deduplicated_paths(git_dirs) {
                if !git_dir.starts_with(&root) {
                    watcher.watch(&git_dir, RecursiveMode::Recursive)?;
                }
            }
            Ok(watcher)
        })()
        .ok();
        Self { watcher, changed }
    }

    pub(super) fn take_changed(&self) -> bool {
        self.watcher.is_none() || self.changed.swap(false, Ordering::AcqRel)
    }

    pub(super) fn mark_changed(&self) {
        self.changed.store(true, Ordering::Release);
    }
}

fn deduplicated_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for path in paths {
        if !result.contains(&path) {
            result.push(path);
        }
    }
    result
}
