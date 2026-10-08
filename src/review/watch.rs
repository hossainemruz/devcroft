//! Coalesce filesystem events so visible Review refreshes do no idle file I/O.
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
        let comment_dirs: Vec<PathBuf> = git_dirs
            .iter()
            .map(|dir| dir.join("devcroft-review"))
            .collect();
        let watcher = (|| -> notify::Result<RecommendedWatcher> {
            let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                if event.map_or(true, |event| affects_review(&comment_dirs, &event)) {
                    pending.store(true, Ordering::Release);
                }
            })?;
            watcher.watch(&root, RecursiveMode::Recursive)?;
            // Linked worktrees keep HEAD/index and shared refs outside the root.
            for git_dir in &git_dirs {
                if !git_dir.starts_with(&root) {
                    watcher.watch(git_dir, RecursiveMode::Recursive)?;
                }
            }
            Ok(watcher)
        })()
        .ok();
        Self { watcher, changed }
    }

    pub(super) fn take_changed(&self) -> bool {
        // Fall back to polling when the platform cannot watch this checkout.
        self.watcher.is_none() || self.changed.swap(false, Ordering::AcqRel)
    }

    pub(super) fn mark_changed(&self) {
        self.changed.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn is_watching(&self) -> bool {
        self.watcher.is_some()
    }
}

fn affects_review(comment_dirs: &[PathBuf], event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            // Local comment saves must not trigger a source reload. Git ref/index
            // changes and all worktree edits (including new paths) do trigger it.
            !comment_dirs.iter().any(|dir| path.starts_with(dir))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, ModifyKind};

    #[test]
    fn events_coalesce_and_reads_and_comment_writes_do_not_reload_source() {
        let root = Path::new("/repo");
        let comment_dirs = [root.join(".git/devcroft-review")];
        assert!(!affects_review(
            &comment_dirs,
            &Event::new(EventKind::Access(AccessKind::Read)).add_path(root.join("file.rs"))
        ));
        assert!(!affects_review(
            &comment_dirs,
            &Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.join(".git/devcroft-review/comments.json"))
        ));
        for path in [
            "file.rs",
            "new.rs",
            "devcroft-review/source.rs",
            ".git/HEAD",
            ".git/refs/heads/main",
        ] {
            assert!(affects_review(
                &comment_dirs,
                &Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.join(path))
            ));
        }
    }
}
