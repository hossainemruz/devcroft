//! Local, unsynced recovery copies for dirty built-in editor buffers.

use std::{
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Draft {
    pub path: PathBuf,
    pub saved: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    pub root: PathBuf,
    pub drafts: Vec<Draft>,
}

pub(super) fn journal_path(data_root: &Path, checkout: &Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash as _, Hasher as _};
    checkout.hash(&mut hasher);
    data_root
        .join("editor-drafts")
        .join(format!("{:016x}.json", hasher.finish()))
}

pub(super) fn read(path: &Path, root: &Path) -> Vec<Draft> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(journal) = serde_json::from_slice::<Journal>(&bytes) else {
        return Vec::new();
    };
    if journal.root != root {
        return Vec::new();
    }
    let canonical_root = root.canonicalize().ok();
    journal
        .drafts
        .into_iter()
        .filter(|draft| {
            draft.path.starts_with(root)
                && !draft
                    .path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
                && draft.text != draft.saved
                && (draft.path.canonicalize().is_ok_and(|path| {
                    canonical_root
                        .as_ref()
                        .is_some_and(|root| path.starts_with(root))
                }) || (!draft.path.exists()
                    && draft
                        .path
                        .parent()
                        .and_then(|parent| parent.canonicalize().ok())
                        .is_some_and(|parent| {
                            canonical_root
                                .as_ref()
                                .is_some_and(|root| parent.starts_with(root))
                        })))
        })
        .collect()
}

pub(super) fn write(path: &Path, journal: &Journal) -> Result<()> {
    let parent = path.parent().context("Draft journal has no parent")?;
    fs::create_dir_all(parent)?;
    if journal.drafts.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, journal)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .context("Could not replace the draft journal")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_is_scoped_and_never_changes_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let file = root.join("main.rs");
        fs::write(&file, "saved\n").unwrap();
        let path = journal_path(dir.path(), &root);
        write(
            &path,
            &Journal {
                root: root.clone(),
                drafts: vec![Draft {
                    path: file.clone(),
                    saved: "saved\n".into(),
                    text: "draft\n".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(read(&path, &root).len(), 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "saved\n");
        assert!(read(&path, dir.path()).is_empty());
        write(
            &path,
            &Journal {
                root: root.clone(),
                drafts: Vec::new(),
            },
        )
        .unwrap();
        assert!(!path.exists());
    }
}
