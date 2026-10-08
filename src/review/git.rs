//! Git access for the Review tab.
//!
//! This is the only review module that depends on `gix`. It compares one
//! base tree against the working tree and produces a [`ReviewDiff`](super::model::ReviewDiff).
//!
//! Two scopes share one code path: the default full-diff scope compares the
//! merge-base of `<remote>/<baseBranch>` (or the local base branch) against
//! the working tree, while uncommitted-changes mode compares `HEAD` against
//! the working tree. Staging state is intentionally ignored: both modes show
//! the union of committed and uncommitted changes.
//!
//! Simplifications for this milestone: rename detection pairs only
//! exact-content matches (similarity renames surface as delete + add, and
//! copies surface as plain additions); submodule contents are opaque.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use sha2::{Digest as _, Sha256};

use super::model::{
    ChangedFile, FileContent, FileStatus, ReviewDiff, UnavailableReason, compare_review_paths,
    count_changes, diff_text,
};

/// Maximum bytes read per side before content becomes unavailable.
pub(crate) const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// How the base tree for a review is chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewScope {
    /// Merge-base of the base branch against `HEAD`, per portable metadata.
    FullDiff { base_branch: String, remote: String },
    /// `HEAD` against the working tree.
    UncommittedChanges,
}

impl ReviewScope {
    pub(crate) fn full_diff(base_branch: impl Into<String>, remote: impl Into<String>) -> Self {
        Self::FullDiff {
            base_branch: base_branch.into(),
            remote: remote.into(),
        }
    }
}

/// Load a review for a checkout in the given scope.
pub(crate) fn load_review(repo_path: &Path, scope: &ReviewScope) -> Result<ReviewDiff> {
    let repo = open(repo_path)?;
    let head = repo.head_id().context("reading HEAD")?.detach();
    let (base_commit, base_ref) = match scope {
        ReviewScope::FullDiff {
            base_branch,
            remote,
        } => {
            let base_tip = resolve_base_commit(&repo, base_branch, remote)?;
            let merge_base = repo
                .merge_base(base_tip, head)
                .with_context(|| {
                    format!(
                        "finding merge-base of {base_branch} and HEAD (unrelated histories cannot be reviewed yet)"
                    )
                })?
                .detach();
            (merge_base, resolved_base_ref(&repo, base_branch, remote))
        }
        ReviewScope::UncommittedChanges => (head, None),
    };
    let base_tree = commit_tree_id(&repo, base_commit)?;
    assemble(
        &repo,
        base_tree,
        base_commit,
        head,
        base_ref,
        current_branch(&repo),
    )
}

/// Suggest a base branch name: the remote `HEAD` symref target first, then a
/// local `main` or `master` if present.
pub(crate) fn suggest_base_branch(repo_path: &Path, remote: &str) -> Option<String> {
    let repo = open(repo_path).ok()?;
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    if let Ok(reference) = repo.find_reference(&head_ref)
        && let Some(name) = reference.target().try_name()
    {
        let name = name.to_string();
        if let Some(short) = name
            .strip_prefix(&format!("refs/remotes/{remote}/"))
            .map(str::to_owned)
        {
            return Some(short);
        }
    }
    for candidate in ["main", "master"] {
        if repo
            .find_reference(format!("refs/heads/{candidate}").as_str())
            .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn open(repo_path: &Path) -> Result<gix::Repository> {
    gix::discover(repo_path)
        .with_context(|| format!("opening git repository at {}", repo_path.display()))
}

/// Read the complete side of an anchor, including lines outside visible hunks.
pub(crate) fn anchor_source(
    cwd: &Path,
    diff: &ReviewDiff,
    path: &str,
    side: super::comments::Side,
) -> Result<Option<String>> {
    let repo = open(cwd)?;
    let bytes = match side {
        super::comments::Side::New => {
            let root = repo
                .workdir()
                .context("review requires a worktree")?
                .canonicalize()?;
            if Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                bail!("Invalid anchor path");
            }
            match read_worktree(&root, path) {
                WorkContent::Bytes(bytes) | WorkContent::Symlink(bytes) => Some(bytes),
                _ => None,
            }
        }
        super::comments::Side::Old => {
            let tree =
                commit_tree_id(&repo, gix::ObjectId::from_hex(diff.base_commit.as_bytes())?)?;
            let entries = list_tree(&repo, tree)?;
            let old_path = diff
                .files
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.old_path.as_deref())
                .unwrap_or(path);
            entries
                .get(old_path)
                .and_then(|e| match read_blob(&repo, e.oid) {
                    BlobRead::Hit(bytes) => Some(bytes),
                    _ => None,
                })
        }
    };
    Ok(bytes
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|s| !s.contains('\0')))
}

fn current_branch(repo: &gix::Repository) -> Option<String> {
    repo.head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string())
}

/// Resolve the base branch to a commit, preferring the remote-tracking ref.
fn resolve_base_commit(
    repo: &gix::Repository,
    base_branch: &str,
    remote: &str,
) -> Result<gix::ObjectId> {
    let remote_ref = format!("refs/remotes/{remote}/{base_branch}");
    let local_ref = format!("refs/heads/{base_branch}");
    for candidate in [&remote_ref, &local_ref] {
        match repo.find_reference(candidate.as_str()) {
            Ok(mut reference) => {
                let id = reference
                    .peel_to_id()
                    .with_context(|| format!("resolving base ref {candidate}"))?;
                let object = repo
                    .find_object(id)
                    .with_context(|| format!("reading base commit for ref {candidate}"))?;
                if object.kind.is_commit() {
                    return Ok(id.detach());
                }
            }
            Err(gix::reference::find::existing::Error::NotFound { .. }) => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("looking up base ref {candidate}"));
            }
        }
    }
    bail!(
        "base branch `{base_branch}` not found as `{remote_ref}` or `{local_ref}`. Fetch the remote, create the branch, or choose a different base branch."
    );
}

fn resolved_base_ref(repo: &gix::Repository, base_branch: &str, remote: &str) -> Option<String> {
    let remote_ref = format!("refs/remotes/{remote}/{base_branch}");
    if repo.find_reference(&remote_ref).is_ok() {
        Some(format!("{remote}/{base_branch}"))
    } else {
        Some(base_branch.to_owned())
    }
}

fn commit_tree_id(repo: &gix::Repository, commit: gix::ObjectId) -> Result<gix::ObjectId> {
    let object = repo
        .find_object(commit)
        .with_context(|| format!("reading commit {commit}"))?;
    let commit = object
        .try_into_commit()
        .map_err(|_| anyhow!("{commit} is not a commit"))?;
    Ok(commit.tree_id()?.detach())
}

/// One base-tree entry relevant to the review.
struct BaseEntry {
    oid: gix::ObjectId,
    mode: gix::object::tree::EntryMode,
}

/// List every blob/symlink/gitlink in a tree, keyed by forward-slash path.
fn list_tree(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
) -> Result<BTreeMap<String, BaseEntry>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(tree_id, String::new())];
    while let Some((id, prefix)) = stack.pop() {
        let object = repo
            .find_object(id)
            .with_context(|| format!("reading tree {id}"))?;
        let tree = object
            .try_into_tree()
            .map_err(|_| anyhow!("{id} is not a tree"))?;
        // Collect children first so the tree borrow ends before recursing.
        let mut children = Vec::new();
        for entry in tree.iter() {
            let entry = entry.context("decoding tree entry")?;
            let name = entry.filename().to_string();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let mode = entry.mode();
            if mode.is_tree() {
                children.push((entry.object_id(), path));
            } else {
                out.insert(
                    path,
                    BaseEntry {
                        oid: entry.object_id(),
                        mode,
                    },
                );
            }
        }
        drop(tree);
        stack.extend(children);
    }
    Ok(out)
}

/// A capped blob read: missing or oversized content is data, not failure.
enum BlobRead {
    Hit(Vec<u8>),
    TooLarge,
    Missing,
}

fn read_blob(repo: &gix::Repository, oid: gix::ObjectId) -> BlobRead {
    let Ok(header) = repo.find_header(oid) else {
        return BlobRead::Missing;
    };
    if header.size() > MAX_FILE_BYTES {
        return BlobRead::TooLarge;
    }
    let Ok(object) = repo.find_object(oid) else {
        return BlobRead::Missing;
    };
    let Ok(blob) = object.try_into_blob() else {
        return BlobRead::Missing;
    };
    if blob.data.len() as u64 > MAX_FILE_BYTES {
        BlobRead::TooLarge
    } else {
        BlobRead::Hit(blob.data.to_owned())
    }
}

/// Raw worktree content at one path.
enum WorkContent {
    Bytes(Vec<u8>),
    Symlink(Vec<u8>),
    TooLarge,
    Dir,
    Gone,
}

#[cfg(not(unix))]
fn read_worktree(root: &Path, name: &str) -> WorkContent {
    use std::io::Read as _;
    // Never traverse a PR-controlled directory link while capturing local
    // source. A link at the leaf is captured as its target text, not followed.
    let mut path = root.to_path_buf();
    let mut components = Path::new(name).components().peekable();
    while let Some(component) = components.next() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return WorkContent::Dir;
        }
        path.push(component);
        if components.peek().is_some()
            && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
        {
            return WorkContent::Dir;
        }
    }
    let path = path.as_path();
    let meta = match std::fs::symlink_metadata(path) {
        Err(_) => return WorkContent::Gone,
        Ok(meta) => meta,
    };
    if meta.is_dir() {
        return WorkContent::Dir;
    }
    if meta.file_type().is_symlink() {
        return match std::fs::read_link(path) {
            Ok(target) => WorkContent::Symlink(target.as_os_str().as_encoded_bytes().to_vec()),
            Err(_) => WorkContent::Gone,
        };
    }
    let Some(file) = open_worktree_file(root, name) else {
        return WorkContent::Gone;
    };
    let Ok(meta) = file.metadata() else {
        return WorkContent::Gone;
    };
    if meta.len() > MAX_FILE_BYTES {
        return WorkContent::TooLarge;
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return WorkContent::Gone;
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        WorkContent::TooLarge
    } else {
        WorkContent::Bytes(bytes)
    }
}

#[cfg(unix)]
fn read_worktree(root: &Path, name: &str) -> WorkContent {
    use std::io::Read as _;
    let file = match open_worktree(root, name) {
        WorkEntry::File(file) => file,
        WorkEntry::Symlink(link) => return WorkContent::Symlink(link),
        WorkEntry::Dir => return WorkContent::Dir,
        WorkEntry::Gone => return WorkContent::Gone,
    };
    let Ok(metadata) = file.metadata() else {
        return WorkContent::Gone;
    };
    if metadata.len() > MAX_FILE_BYTES {
        return WorkContent::TooLarge;
    }
    let mut bytes = vec![];
    if file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return WorkContent::Gone;
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        WorkContent::TooLarge
    } else {
        WorkContent::Bytes(bytes)
    }
}

#[cfg(unix)]
enum WorkEntry {
    File(std::fs::File),
    Symlink(Vec<u8>),
    Dir,
    Gone,
}

#[cfg(unix)]
fn open_worktree(root: &Path, name: &str) -> WorkEntry {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _};
    // Directory descriptors pin each ancestor. O_NOFOLLOW closes the race
    // between checking a directory link and opening the source beneath it.
    let Ok(mut directory) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
    else {
        return WorkEntry::Gone;
    };
    let mut parts = Path::new(name).components().peekable();
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(part) = part else {
            return WorkEntry::Dir;
        };
        let Ok(part) = CString::new(part.as_bytes()) else {
            return WorkEntry::Gone;
        };
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if parts.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the directory FD is live and the name is NUL terminated.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), part.as_ptr(), flags) };
        if fd < 0 {
            if parts.peek().is_none() {
                let mut link = vec![0u8; 4096];
                // readlinkat reads the leaf itself, never the linked target.
                let count = unsafe {
                    libc::readlinkat(
                        directory.as_raw_fd(),
                        part.as_ptr(),
                        link.as_mut_ptr().cast(),
                        link.len(),
                    )
                };
                if count >= 0 {
                    link.truncate(count as usize);
                    return WorkEntry::Symlink(link);
                }
            }
            return WorkEntry::Gone;
        }
        // SAFETY: openat returned a newly owned file descriptor.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        if parts.peek().is_some() {
            directory = file;
            continue;
        }
        let Ok(metadata) = file.metadata() else {
            return WorkEntry::Gone;
        };
        if !metadata.is_file() {
            return WorkEntry::Dir;
        }
        return WorkEntry::File(file);
    }
    WorkEntry::Dir
}

fn assemble(
    repo: &gix::Repository,
    base_tree: gix::ObjectId,
    base_commit: gix::ObjectId,
    head: gix::ObjectId,
    base_ref: Option<String>,
    head_branch: Option<String>,
) -> Result<ReviewDiff> {
    let workdir = repo
        .workdir()
        .context("review requires a working tree (bare repositories are not supported)")
        .and_then(|p| p.canonicalize().map_err(Into::into))?;
    let base_entries = list_tree(repo, base_tree)?;
    let untracked = discover_untracked(&workdir, &base_entries);

    let mut files: Vec<ChangedFile> = Vec::new();
    let mut work_versions = HashMap::new();
    // Deleted/added byte payloads awaiting exact-content rename pairing.
    let mut deleted: Vec<(String, Vec<u8>)> = Vec::new();
    let mut added: Vec<(String, Vec<u8>)> = Vec::new();

    for (path, entry) in &base_entries {
        let work = read_worktree(&workdir, path);
        if let Some(version) = worktree_version(&workdir, path, &work) {
            work_versions.insert(path.clone(), version);
        }
        classify_tracked(repo, path, entry, work, &mut files, &mut deleted);
    }
    for path in untracked {
        let work = read_worktree(&workdir, &path);
        if let Some(version) = worktree_version(&workdir, &path, &work) {
            work_versions.insert(path.clone(), version);
        }
        match work {
            WorkContent::Bytes(bytes) | WorkContent::Symlink(bytes) => {
                added.push((path, bytes));
            }
            WorkContent::TooLarge => files.push(ChangedFile {
                path,
                old_path: None,
                status: FileStatus::Added,
                additions: 0,
                deletions: 0,
                content: FileContent::Unavailable(UnavailableReason::TooLarge),
            }),
            WorkContent::Dir | WorkContent::Gone => {}
        }
    }

    pair_exact_renames(&mut files, &mut deleted, &mut added);
    files.extend(
        added
            .into_iter()
            .map(|(path, bytes)| added_file(&path, &bytes)),
    );
    files.extend(
        deleted
            .into_iter()
            .map(|(path, bytes)| deleted_file(&path, &bytes)),
    );
    // Canonical review order: folders first, matching the sidebar tree's
    // pre-order exactly (see `compare_review_paths`).
    files.sort_by(|a, b| compare_review_paths(&a.path, &b.path));

    let file_versions = files
        .iter()
        .filter_map(|file| {
            let work = work_versions.get(&file.path)?;
            let mut hash = Sha256::new();
            hash.update(work);
            let old_path = file.old_path.as_deref().unwrap_or(&file.path);
            hash.update(old_path.as_bytes());
            if let Some(base) = base_entries.get(old_path) {
                hash.update(base.oid.as_bytes());
                hash.update(base.mode.value().to_le_bytes());
            }
            Some((file.path.clone(), hash.finalize().into()))
        })
        .collect();

    Ok(ReviewDiff {
        files,
        file_versions,
        base_commit: base_commit.to_hex().to_string(),
        head_commit: head.to_hex().to_string(),
        base_ref,
        head_branch,
    })
}

/// Hash the source captured for this load, rather than just rendered hunks.
/// Binary files and edits beyond the hunk cap must also invalidate progress.
fn worktree_version(root: &Path, path: &str, work: &WorkContent) -> Option<[u8; 32]> {
    let mut hash = Sha256::new();
    match work {
        WorkContent::Bytes(bytes) => {
            hash.update(b"file");
            hash.update(bytes);
        }
        WorkContent::Symlink(bytes) => {
            hash.update(b"link");
            hash.update(bytes);
        }
        WorkContent::TooLarge => {
            // Stream oversized files without lifting the rendering memory cap.
            #[cfg(unix)]
            {
                use std::io::Read as _;
                let WorkEntry::File(mut file) = open_worktree(root, path) else {
                    return None;
                };
                hash.update(b"file");
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer).ok()?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
            }
            #[cfg(not(unix))]
            {
                use std::io::Read as _;
                let mut file = open_worktree_file(root, path)?;
                hash.update(b"file");
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer).ok()?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
            }
        }
        WorkContent::Dir => {
            hash.update(b"directory");
            if let Ok(sub) = gix::discover(root.join(path))
                && let Ok(head) = sub.head_id()
            {
                hash.update(head.as_bytes());
            }
        }
        WorkContent::Gone => hash.update(b"absent"),
    }
    #[cfg(unix)]
    if matches!(work, WorkContent::Bytes(_) | WorkContent::TooLarge) {
        use std::os::unix::fs::PermissionsExt as _;
        let WorkEntry::File(file) = open_worktree(root, path) else {
            return None;
        };
        hash.update([u8::from(
            file.metadata().ok()?.permissions().mode() & 0o111 != 0,
        )]);
    }
    Some(hash.finalize().into())
}

/// Validate the opened handle before reading any bytes. Checking components
/// alone leaves a race where an ancestor can be replaced with a directory link.
#[cfg(windows)]
fn open_worktree_file(root: &Path, path: &str) -> Option<std::fs::File> {
    use std::os::windows::{ffi::OsStringExt as _, fs::OpenOptionsExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, GetFinalPathNameByHandleW,
    };

    if Path::new(path)
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let file = std::fs::File::options()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root.join(path))
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    // SAFETY: the owned file keeps this handle live; the first call only queries
    // the required UTF-16 buffer length, and the second has that full capacity.
    let size =
        unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), std::ptr::null_mut(), 0, 0) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; size as usize];
    let written =
        unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), size, 0) };
    if written == 0 || written >= size {
        return None;
    }
    buffer.truncate(written as usize);
    let opened_path = PathBuf::from(std::ffi::OsString::from_wide(&buffer));
    // `root` was canonicalized at load start. Both it and the handle path use
    // Windows' extended absolute form; validate the actual opened file, rather
    // than resolving the mutable pathname again.
    opened_path.starts_with(root).then_some(file)
}

#[cfg(not(any(unix, windows)))]
fn open_worktree_file(_: &Path, _: &str) -> Option<std::fs::File> {
    // A platform without a handle-bound containment check cannot retain a mark.
    None
}

/// A tracked path (present in the base tree) compared against the worktree.
///
/// Pure deletions and (re)additions accumulate byte payloads for rename
/// pairing; everything else is emitted directly.
fn classify_tracked(
    repo: &gix::Repository,
    path: &str,
    entry: &BaseEntry,
    work: WorkContent,
    files: &mut Vec<ChangedFile>,
    deleted: &mut Vec<(String, Vec<u8>)>,
) {
    if entry.mode.is_commit() {
        classify_submodule(repo, path, entry, work, files);
        return;
    }
    let base_is_link = entry.mode.is_link();
    let work_is_link = matches!(work, WorkContent::Symlink(_));
    match work {
        WorkContent::Gone => match read_blob(repo, entry.oid) {
            BlobRead::Hit(bytes) if !is_binary(&bytes) => {
                deleted.push((path.to_owned(), bytes));
            }
            BlobRead::Hit(_) => {
                files.push(deleted_file_unavailable(path, UnavailableReason::Binary))
            }
            BlobRead::TooLarge => {
                files.push(deleted_file_unavailable(path, UnavailableReason::TooLarge))
            }
            BlobRead::Missing => {
                files.push(deleted_file_unavailable(path, UnavailableReason::Missing))
            }
        },
        WorkContent::Dir => {
            files.push(ChangedFile {
                path: path.to_owned(),
                old_path: None,
                status: FileStatus::TypeChanged,
                additions: 0,
                deletions: 0,
                content: FileContent::Unavailable(UnavailableReason::Unsupported),
            });
        }
        WorkContent::TooLarge => {
            // The rendering cap says nothing about whether the file changed.
            // Compare bytes without allocating another large worktree buffer.
            if !base_is_link
                && !mode_changed(repo, path, entry)
                && large_worktree_matches(repo, path, entry.oid)
            {
                return;
            }
            files.push(ChangedFile {
                path: path.to_owned(),
                old_path: None,
                status: if base_is_link {
                    FileStatus::TypeChanged
                } else {
                    FileStatus::Modified
                },
                additions: 0,
                deletions: 0,
                content: FileContent::Unavailable(UnavailableReason::TooLarge),
            });
        }
        WorkContent::Bytes(bytes) | WorkContent::Symlink(bytes) => {
            if base_is_link != work_is_link {
                files.push(ChangedFile {
                    path: path.to_owned(),
                    old_path: None,
                    status: FileStatus::TypeChanged,
                    additions: 0,
                    deletions: 0,
                    content: FileContent::Unavailable(UnavailableReason::Unsupported),
                });
                return;
            }
            match read_blob(repo, entry.oid) {
                BlobRead::Hit(base_bytes) if base_bytes == bytes => {
                    // Unchanged content: only a mode change (e.g. ±x) keeps
                    // the file visible, with empty hunks.
                    if mode_changed(repo, path, entry) {
                        files.push(ChangedFile {
                            path: path.to_owned(),
                            old_path: None,
                            status: FileStatus::Modified,
                            additions: 0,
                            deletions: 0,
                            content: FileContent::Text {
                                hunks: Vec::new(),
                                truncated: false,
                            },
                        });
                    }
                }
                BlobRead::Hit(base_bytes) => {
                    classify_modified(path, &base_bytes, &bytes, files);
                }
                BlobRead::TooLarge => files.push(ChangedFile {
                    path: path.to_owned(),
                    old_path: None,
                    status: FileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                    content: FileContent::Unavailable(UnavailableReason::TooLarge),
                }),
                BlobRead::Missing => files.push(ChangedFile {
                    path: path.to_owned(),
                    old_path: None,
                    status: FileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                    content: FileContent::Unavailable(UnavailableReason::Missing),
                }),
            }
        }
    }
}

/// Compare oversized content independently of the diff rendering limit.
/// Failed reads remain visible as unavailable changes.
#[cfg(unix)]
fn large_worktree_matches(repo: &gix::Repository, path: &str, oid: gix::ObjectId) -> bool {
    use std::io::Read as _;

    let Some(root) = repo.workdir() else {
        return false;
    };
    let WorkEntry::File(mut file) = open_worktree(root, path) else {
        return false;
    };
    let Ok(object) = repo.find_object(oid) else {
        return false;
    };
    let Ok(blob) = object.try_into_blob() else {
        return false;
    };
    let mut buffer = [0_u8; 64 * 1024];
    for chunk in blob.data.chunks(buffer.len()) {
        if file.read_exact(&mut buffer[..chunk.len()]).is_err() || buffer[..chunk.len()] != *chunk {
            return false;
        }
    }
    matches!(file.read(&mut buffer[..1]), Ok(0))
}

#[cfg(not(unix))]
fn large_worktree_matches(_: &gix::Repository, _: &str, _: gix::ObjectId) -> bool {
    // Do not infer equality by following an unchecked directory link.
    false
}

/// Whether the worktree executable bit differs from the base tree mode.
/// Symlinks have no executable bit; non-Unix platforms report no change.
fn mode_changed(repo: &gix::Repository, path: &str, entry: &BaseEntry) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let Some(workdir) = repo.workdir() else {
            return false;
        };
        let WorkEntry::File(file) = open_worktree(workdir, path) else {
            return !entry.mode.is_link();
        };
        let Ok(meta) = file.metadata() else {
            return true;
        };
        let work_executable = meta.permissions().mode() & 0o111 != 0;
        let base_executable = entry.mode.is_executable();
        work_executable != base_executable
    }
    #[cfg(not(unix))]
    {
        let _ = (repo, path, entry);
        false
    }
}

/// Compare base bytes against work bytes for a path present on both sides.
fn classify_modified(
    path: &str,
    base_bytes: &[u8],
    work_bytes: &[u8],
    files: &mut Vec<ChangedFile>,
) {
    if is_binary(base_bytes) || is_binary(work_bytes) {
        files.push(ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::Binary),
        });
        return;
    }
    let (Ok(base_text), Ok(work_text)) = (
        std::str::from_utf8(base_bytes),
        std::str::from_utf8(work_bytes),
    ) else {
        files.push(ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::InvalidUtf8),
        });
        return;
    };
    let (additions, deletions) = count_changes(base_text, work_text);
    if additions == 0 && deletions == 0 {
        // Bytes differ but lines do not (e.g. line-ending-only change):
        // keep the file visible with empty hunks.
        files.push(ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            content: FileContent::Text {
                hunks: Vec::new(),
                truncated: false,
            },
        });
        return;
    }
    let (hunks, truncated) = diff_text(base_text, work_text);
    files.push(ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status: FileStatus::Modified,
        additions,
        deletions,
        content: FileContent::Text { hunks, truncated },
    });
}

fn deleted_file_unavailable(path: &str, reason: UnavailableReason) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status: FileStatus::Deleted,
        additions: 0,
        deletions: 0,
        content: FileContent::Unavailable(reason),
    }
}

fn added_file(path: &str, bytes: &[u8]) -> ChangedFile {
    if is_binary(bytes) {
        return ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Added,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::Binary),
        };
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Added,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::InvalidUtf8),
        };
    };
    let line_count = text.lines().count() as u32;
    let (hunks, truncated) = diff_text("", text);
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status: FileStatus::Added,
        additions: line_count,
        deletions: 0,
        content: FileContent::Text { hunks, truncated },
    }
}

fn deleted_file(path: &str, bytes: &[u8]) -> ChangedFile {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Deleted,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::InvalidUtf8),
        };
    };
    let line_count = text.lines().count() as u32;
    let (hunks, truncated) = diff_text(text, "");
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status: FileStatus::Deleted,
        additions: 0,
        deletions: line_count,
        content: FileContent::Text { hunks, truncated },
    }
}

/// Pair deleted and added files with byte-identical content into renames.
///
/// Pairing is greedy in sorted order on both sides (both lists arrive
/// sorted: deletions in base-tree order, additions sorted after discovery).
/// Removals from each list are descending so indices stay valid.
fn pair_exact_renames(
    files: &mut Vec<ChangedFile>,
    deleted: &mut Vec<(String, Vec<u8>)>,
    added: &mut Vec<(String, Vec<u8>)>,
) {
    if deleted.is_empty() || added.is_empty() {
        return;
    }
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, (_, bytes)) in added.iter().enumerate() {
        by_hash.entry(hash_bytes(bytes)).or_default().push(index);
    }
    let mut consumed = vec![false; added.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (deleted_index, (_, bytes)) in deleted.iter().enumerate() {
        if let Some(candidates) = by_hash.get(&hash_bytes(bytes))
            && let Some(&added_index) = candidates.iter().find(|&&i| !consumed[i])
        {
            consumed[added_index] = true;
            pairs.push((deleted_index, added_index));
        }
    }
    if pairs.is_empty() {
        return;
    }
    // Remove from each list independently in descending index order: a
    // joint ordering would let one list's removals invalidate the other's
    // indices when pairs cross (e.g. a.txt→m.txt alongside z.txt→b.txt).
    pairs.sort_unstable_by_key(|pair| std::cmp::Reverse(pair.0));
    let mut removed: Vec<((String, Vec<u8>), usize)> = Vec::with_capacity(pairs.len());
    for (deleted_index, added_index) in pairs {
        let entry = deleted.remove(deleted_index);
        removed.push((entry, added_index));
    }
    removed.sort_unstable_by_key(|removed| std::cmp::Reverse(removed.1));
    let mut renames = Vec::with_capacity(removed.len());
    for ((old_path, old_bytes), added_index) in removed {
        let (new_path, new_bytes) = added.remove(added_index);
        debug_assert_eq!(hash_bytes(&old_bytes), hash_bytes(&new_bytes));
        renames.push(renamed_file(&old_path, &new_path, &old_bytes, &new_bytes));
    }
    files.extend(renames);
}

fn renamed_file(old_path: &str, new_path: &str, old_bytes: &[u8], new_bytes: &[u8]) -> ChangedFile {
    // Exact-content pairing only admits payloads both sides could read;
    // fall back to unavailable rather than panicking on races.
    match (
        std::str::from_utf8(old_bytes),
        std::str::from_utf8(new_bytes),
    ) {
        (Ok(old_text), Ok(new_text)) => {
            let (additions, deletions) = count_changes(old_text, new_text);
            let (hunks, truncated) = diff_text(old_text, new_text);
            ChangedFile {
                path: new_path.to_owned(),
                old_path: Some(old_path.to_owned()),
                status: FileStatus::Renamed,
                additions,
                deletions,
                content: FileContent::Text { hunks, truncated },
            }
        }
        _ => ChangedFile {
            path: new_path.to_owned(),
            old_path: Some(old_path.to_owned()),
            status: FileStatus::Renamed,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::InvalidUtf8),
        },
    }
}

fn classify_submodule(
    repo: &gix::Repository,
    path: &str,
    entry: &BaseEntry,
    work: WorkContent,
    files: &mut Vec<ChangedFile>,
) {
    let changed = match &work {
        WorkContent::Gone | WorkContent::Bytes(_) | WorkContent::Symlink(_) => true,
        WorkContent::TooLarge => true,
        WorkContent::Dir => {
            let sub_path = repo
                .workdir()
                .map(|root| root.join(path))
                .unwrap_or_else(|| PathBuf::from(path));
            match gix::discover(&sub_path)
                .ok()
                .and_then(|sub| sub.head_id().map(|id| id.detach()).ok())
            {
                Some(id) => id != entry.oid,
                None => true,
            }
        }
    };
    if changed {
        files.push(ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            content: FileContent::Unavailable(UnavailableReason::Submodule),
        });
    }
}

/// Discover untracked files, respecting ignores, excluding tracked paths.
///
/// Tracked paths are read directly and never need discovery; the walk only
/// finds paths absent from the base tree. `.git` and nested repositories
/// (submodule checkouts) are pruned.
fn discover_untracked(workdir: &Path, base_entries: &BTreeMap<String, BaseEntry>) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(workdir)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|entry| {
            if entry.file_name() == OsStr::new(".git") {
                return false;
            }
            if entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry.path().join(".git").symlink_metadata().is_ok()
            {
                return false;
            }
            true
        })
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workdir) else {
            continue;
        };
        let path = relative
            .components()
            .map(|component| component.as_os_str().as_encoded_bytes())
            .collect::<Vec<_>>()
            .join(&b'/');
        let Ok(path) = String::from_utf8(path) else {
            continue;
        };
        if !base_entries.contains_key(&path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Git's binary heuristic: a NUL byte in the first 8 KiB.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&byte| byte == 0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git CLI must be available for fixture setup");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn write(dir: &Path, path: &str, content: &[u8]) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", message]);
    }

    fn uncommitted() -> ReviewScope {
        ReviewScope::UncommittedChanges
    }

    fn find<'a>(diff: &'a ReviewDiff, path: &str) -> &'a ChangedFile {
        diff.files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("expected {path} in diff"))
    }

    #[test]
    fn file_versions_detect_changes_hidden_by_rendering_limits() {
        let dir = init_repo();
        let large = vec![b'a'; MAX_FILE_BYTES as usize + 1];
        write(dir.path(), "binary.bin", &[0, 1]);
        write(dir.path(), "large.txt", &large);
        write(dir.path(), "truncated.txt", "old\n".repeat(2500).as_bytes());
        commit_all(dir.path(), "base");
        write(dir.path(), "binary.bin", &[0, 2]);
        let mut large = vec![b'b'; MAX_FILE_BYTES as usize + 1];
        write(dir.path(), "large.txt", &large);
        let mut text = "new\n".repeat(2500);
        write(dir.path(), "truncated.txt", text.as_bytes());
        let before = load_review(dir.path(), &uncommitted()).unwrap();
        assert_eq!(before.file_versions.len(), 3);
        assert_eq!(before, load_review(dir.path(), &uncommitted()).unwrap());
        write(dir.path(), "binary.bin", &[0, 3]);
        *large.last_mut().unwrap() = b'c';
        let modified = fs::metadata(dir.path().join("large.txt"))
            .unwrap()
            .modified()
            .unwrap();
        write(dir.path(), "large.txt", &large);
        fs::File::options()
            .write(true)
            .open(dir.path().join("large.txt"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        text.truncate(text.len() - "new\n".len());
        text.push_str("last edit\n");
        write(dir.path(), "truncated.txt", text.as_bytes());
        let after = load_review(dir.path(), &uncommitted()).unwrap();
        for path in ["binary.bin", "large.txt", "truncated.txt"] {
            assert_eq!(
                find(&before, path),
                find(&after, path),
                "rendered diff is identical"
            );
            assert_ne!(
                before.file_versions[path], after.file_versions[path],
                "source version changed: {path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_versions_include_executable_mode_changes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = init_repo();
        write(dir.path(), "script.sh", b"original\n");
        commit_all(dir.path(), "base");
        write(dir.path(), "script.sh", b"modified\n");
        let before = load_review(dir.path(), &uncommitted()).unwrap();
        let path = dir.path().join("script.sh");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(permissions.mode() ^ 0o111);
        fs::set_permissions(path, permissions).unwrap();
        let after = load_review(dir.path(), &uncommitted()).unwrap();
        assert_eq!(before.files, after.files);
        assert_ne!(
            before.file_versions["script.sh"],
            after.file_versions["script.sh"]
        );
    }

    #[test]
    fn anchor_sources_cover_both_sides_renames_and_unchanged_files() {
        use super::super::comments::{Anchor, Side, Store};
        let dir = init_repo();
        write(dir.path(), "old.txt", b"one\ntwo\n");
        commit_all(dir.path(), "initial");
        let scope = uncommitted();
        let initial = load_review(dir.path(), &scope).unwrap();
        let store = Store::open(dir.path(), "main", "origin", &scope, &initial).unwrap();
        for side in [Side::Old, Side::New] {
            store
                .create(
                    Anchor {
                        path: "old.txt".into(),
                        side,
                        start: 2,
                        end: 2,
                        source: "one\ntwo\n".into(),
                        outdated: false,
                    },
                    "feedback".into(),
                )
                .unwrap();
        }
        fs::rename(dir.path().join("old.txt"), dir.path().join("new.txt")).unwrap();
        let diff = load_review(dir.path(), &scope).unwrap();
        let comments = store.refresh(dir.path(), &diff).unwrap();
        assert!(
            comments
                .iter()
                .all(|c| c.anchor.path == "new.txt" && !c.anchor.outdated)
        );
        fs::remove_file(dir.path().join("new.txt")).unwrap();
        let comments = store.refresh(dir.path(), &diff).unwrap();
        assert!(!comments[0].anchor.outdated);
        assert!(comments[1].anchor.outdated);
        git(dir.path(), &["checkout", "-b", "other"]);
        assert!(store.ensure_checkout(dir.path()).is_err());
    }

    #[test]
    fn clean_worktree_yields_no_files() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"hello\n");
        commit_all(dir.path(), "initial");
        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        assert!(diff.files.is_empty());
        assert_eq!(diff.head_branch.as_deref(), Some("main"));
    }

    #[test]
    fn uncommitted_diff_reports_modified_added_deleted_and_untracked() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"one\ntwo\nthree\n");
        write(dir.path(), "gone.txt", b"bye\n");
        commit_all(dir.path(), "initial");

        write(dir.path(), "a.txt", b"one\nTWO\nthree\nfour\n");
        fs::remove_file(dir.path().join("gone.txt")).unwrap();
        write(dir.path(), "new.txt", b"fresh\n");

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        assert_eq!(diff.files.len(), 3);

        let modified = find(&diff, "a.txt");
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!((modified.additions, modified.deletions), (2, 1));
        let FileContent::Text { hunks, truncated } = &modified.content else {
            panic!("expected text hunks");
        };
        assert!(!truncated);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 1);

        let added = find(&diff, "new.txt");
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!((added.additions, added.deletions), (1, 0));

        let deleted = find(&diff, "gone.txt");
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!((deleted.additions, deleted.deletions), (0, 1));
    }

    #[test]
    fn oversized_files_are_only_listed_when_changed() {
        let dir = init_repo();
        let original = vec![b'a'; MAX_FILE_BYTES as usize + 1];
        write(dir.path(), "fixture.yaml", &original);
        commit_all(dir.path(), "large fixture");
        git(dir.path(), &["checkout", "-b", "feature"]);
        let full = ReviewScope::full_diff("main", "origin");
        for scope in [uncommitted(), full.clone()] {
            assert!(load_review(dir.path(), &scope).unwrap().files.is_empty());
        }

        // Same length, differing final byte: size alone cannot detect this.
        let mut changed = original.clone();
        *changed.last_mut().unwrap() = b'b';
        write(dir.path(), "fixture.yaml", &changed);
        for scope in [uncommitted(), full.clone()] {
            let diff = load_review(dir.path(), &scope).unwrap();
            assert_eq!(diff.files.len(), 1);
            assert_eq!(diff.files[0].status, FileStatus::Modified);
            assert_eq!(
                diff.files[0].content,
                FileContent::Unavailable(UnavailableReason::TooLarge)
            );
        }
        commit_all(dir.path(), "change fixture");
        assert!(
            load_review(dir.path(), &uncommitted())
                .unwrap()
                .files
                .is_empty()
        );
        assert_eq!(load_review(dir.path(), &full).unwrap().files.len(), 1);

        // Restoring the merge-base contents clears Full diff, but differs from HEAD.
        write(dir.path(), "fixture.yaml", &original);
        assert!(load_review(dir.path(), &full).unwrap().files.is_empty());
        assert_eq!(
            load_review(dir.path(), &uncommitted()).unwrap().files.len(),
            1
        );

        write(dir.path(), "fixture.yaml", b"small now\n");
        assert_eq!(
            load_review(dir.path(), &uncommitted()).unwrap().files.len(),
            1
        );
        fs::remove_file(dir.path().join("fixture.yaml")).unwrap();
        assert_eq!(
            find(
                &load_review(dir.path(), &uncommitted()).unwrap(),
                "fixture.yaml"
            )
            .status,
            FileStatus::Deleted
        );
        write(dir.path(), "new.yaml", &original);
        assert_eq!(
            find(
                &load_review(dir.path(), &uncommitted()).unwrap(),
                "new.yaml"
            )
            .status,
            FileStatus::Added
        );
    }

    #[test]
    fn full_diff_uses_merge_base_scope() {
        let dir = init_repo();
        write(dir.path(), "shared.txt", b"v1\n");
        commit_all(dir.path(), "initial");
        git(dir.path(), &["checkout", "-b", "feature"]);
        write(dir.path(), "shared.txt", b"v2\n");
        write(dir.path(), "feature.txt", b"only here\n");
        commit_all(dir.path(), "feature work");
        // Diverge main after the branch point: invisible to the review.
        git(dir.path(), &["checkout", "main"]);
        write(dir.path(), "main-only.txt", b"not in review\n");
        commit_all(dir.path(), "main work");
        git(dir.path(), &["checkout", "feature"]);

        let scope = ReviewScope::full_diff("main", "origin");
        let diff = load_review(dir.path(), &scope).unwrap();
        assert_eq!(diff.base_ref.as_deref(), Some("main"));
        assert_eq!(diff.head_branch.as_deref(), Some("feature"));
        assert!(diff.files.iter().any(|f| f.path == "shared.txt"));
        assert!(diff.files.iter().any(|f| f.path == "feature.txt"));
        assert!(!diff.files.iter().any(|f| f.path == "main-only.txt"));
        let shared = find(&diff, "shared.txt");
        assert_eq!(shared.status, FileStatus::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn local_capture_reads_leaf_links_without_traversing_parent_links() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.txt", b"outside source");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("parent")).unwrap();
        assert!(!matches!(
            read_worktree(dir.path(), "parent/secret.txt"),
            WorkContent::Bytes(_)
        ));
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("leaf"))
            .unwrap();
        assert!(matches!(
            read_worktree(dir.path(), "leaf"),
            WorkContent::Symlink(_)
        ));
        assert!(!matches!(
            read_worktree(dir.path(), "../secret.txt"),
            WorkContent::Bytes(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_comparison_and_mode_checks_reject_replaced_ancestors() {
        let dir = init_repo();
        let outside = tempfile::tempdir().unwrap();
        let bytes = vec![b'a'; MAX_FILE_BYTES as usize + 1];
        write(dir.path(), "parent/large.txt", &bytes);
        commit_all(dir.path(), "large tracked fixture");
        let repo = gix::open(dir.path()).unwrap();
        let tree = repo.head_commit().unwrap().tree_id().unwrap().detach();
        let entries = list_tree(&repo, tree).unwrap();
        let entry = &entries["parent/large.txt"];
        assert!(matches!(
            read_worktree(dir.path(), "parent/large.txt"),
            WorkContent::TooLarge
        ));
        assert!(large_worktree_matches(&repo, "parent/large.txt", entry.oid));
        write(outside.path(), "large.txt", &bytes);
        fs::rename(dir.path().join("parent"), dir.path().join("original")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("parent")).unwrap();
        assert!(!large_worktree_matches(
            &repo,
            "parent/large.txt",
            entry.oid
        ));
        assert!(mode_changed(&repo, "parent/large.txt", entry));
    }

    #[test]
    fn missing_base_branch_errors_actionably() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"hi\n");
        commit_all(dir.path(), "initial");
        let error = load_review(
            dir.path(),
            &ReviewScope::full_diff("no-such-branch", "origin"),
        )
        .expect_err("must fail");
        let message = format!("{error:#}");
        assert!(message.contains("no-such-branch"), "{message}");
        assert!(message.contains("Fetch the remote"), "{message}");
    }

    #[test]
    fn exact_content_move_is_a_rename() {
        let dir = init_repo();
        write(dir.path(), "old.txt", b"same content\n");
        commit_all(dir.path(), "initial");
        fs::rename(dir.path().join("old.txt"), dir.path().join("new.txt")).unwrap();

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        assert_eq!(diff.files.len(), 1);
        let renamed = &diff.files[0];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.path, "new.txt");
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        assert_eq!((renamed.additions, renamed.deletions), (0, 0));
    }

    #[test]
    fn multiple_crossing_renames_pair_correctly() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"content X\n");
        write(dir.path(), "z.txt", b"content Y\n");
        commit_all(dir.path(), "initial");
        // Crossing pairs: deleted order [a, z], added order [b, m].
        fs::rename(dir.path().join("a.txt"), dir.path().join("m.txt")).unwrap();
        fs::rename(dir.path().join("z.txt"), dir.path().join("b.txt")).unwrap();

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        assert_eq!(diff.files.len(), 2);
        let first = find(&diff, "b.txt");
        assert_eq!(first.status, FileStatus::Renamed);
        assert_eq!(first.old_path.as_deref(), Some("z.txt"));
        let second = find(&diff, "m.txt");
        assert_eq!(second.status, FileStatus::Renamed);
        assert_eq!(second.old_path.as_deref(), Some("a.txt"));
    }

    #[test]
    fn stream_lists_files_in_sidebar_order() {
        let dir = init_repo();
        write(dir.path(), "README.md", b"docs\n");
        write(dir.path(), "src-old.rs", b"old\n");
        commit_all(dir.path(), "initial");
        // Nested new files plus a root change: stream order must match the
        // folders-first sidebar pre-order exactly.
        write(dir.path(), "src/main.rs", b"main\n");
        write(dir.path(), "a/b.rs", b"b\n");
        write(dir.path(), "README.md", b"docs!\n");
        write(dir.path(), "src-old.rs", b"old, changed\n");

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["a/b.rs", "src/main.rs", "README.md", "src-old.rs"]
        );
    }

    #[test]
    fn binary_and_invalid_utf8_are_unavailable() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"ok\n");
        commit_all(dir.path(), "initial");
        let mut blob = b"PK\x03\x04".to_vec();
        blob.extend([0, 1, 2, 3]);
        write(dir.path(), "archive.zip", &blob);
        write(dir.path(), "broken.txt", &[0xff, 0xfe, b'x', b'\n']);

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        let binary = find(&diff, "archive.zip");
        assert_eq!(binary.status, FileStatus::Added);
        assert_eq!(
            binary.content,
            FileContent::Unavailable(UnavailableReason::Binary)
        );
        let broken = find(&diff, "broken.txt");
        assert_eq!(
            broken.content,
            FileContent::Unavailable(UnavailableReason::InvalidUtf8)
        );
    }

    #[test]
    fn ignored_files_are_not_untracked() {
        let dir = init_repo();
        write(dir.path(), ".gitignore", b"*.log\n");
        write(dir.path(), "kept.txt", b"kept\n");
        commit_all(dir.path(), "initial");
        write(dir.path(), "debug.log", b"noise\n");
        write(dir.path(), "notes.txt", b"visible\n");

        let diff = load_review(dir.path(), &uncommitted()).unwrap();
        assert!(!diff.files.iter().any(|f| f.path == "debug.log"));
        assert!(!diff.files.iter().any(|f| f.path == ".git"));
        assert!(diff.files.iter().any(|f| f.path == "notes.txt"));
    }

    #[test]
    fn suggest_base_branch_prefers_remote_head() {
        let dir = init_repo();
        write(dir.path(), "a.txt", b"hi\n");
        commit_all(dir.path(), "initial");
        assert_eq!(
            suggest_base_branch(dir.path(), "origin").as_deref(),
            Some("main")
        );
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            dir.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(
            suggest_base_branch(dir.path(), "origin").as_deref(),
            Some("main")
        );
    }
}
