//! Advisory, cross-process locks outside portable/. Always acquire the portable
//! gate before a record lock. Never unlink lock files: waiters retain the inode.
use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

use super::DataRoot;

/// Task/artifact operations share the gate; sync/checkout hold it exclusively. Blocking
/// calls belong on worker threads, not GPUI's UI thread. Closing releases locks,
/// including when a process exits unexpectedly. External Git/editors are advisory
/// nonparticipants and must not run concurrently with application mutations.
pub(super) fn portable_gate(root: &DataRoot, exclusive: bool) -> Result<File> {
    std::fs::create_dir_all(root.root())?;
    let cache = root.root().join("cache");
    ensure_directory(&cache)?;
    lock_file(&cache.join("portable-store.lock"), exclusive)
}

pub(super) fn task_lock(root: &DataRoot, id: &str, exclusive: bool) -> Result<File> {
    let dir = root.root().join("cache/task-locks");
    ensure_directory(&dir)?;
    lock_file(&dir.join(format!("{id}.lock")), exclusive)
}

pub(super) fn artifact_lock(root: &DataRoot, id: &str, exclusive: bool) -> Result<File> {
    let dir = root.root().join("cache/artifact-locks");
    ensure_directory(&dir)?;
    lock_file(&dir.join(format!("{id}.lock")), exclusive)
}

fn lock_file(path: &Path, exclusive: bool) -> Result<File> {
    reject_symlink(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    if exclusive {
        file.lock()
    } else {
        file.lock_shared()
    }
    .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

pub(super) fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "symlinks are not supported for store paths: {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
}

pub(super) fn ensure_directory(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating directory {}", path.display())),
    }
}
