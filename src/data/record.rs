use super::store_lock::reject_symlink;
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::hash_map::RandomState,
    fs::{self, OpenOptions},
    hash::BuildHasher,
    io::{Read as _, Write as _},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
pub(super) const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn revision(bytes: &[u8]) -> Result<String> {
    Ok(format!(
        "v1-{}",
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)?
    ))
}
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
pub(super) fn nonblank(value: &str, name: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{name} must not be blank");
    Ok(())
}

pub(super) fn timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub(super) fn random_id() -> String {
    // RandomState supplies independently randomized hash keys from std. These
    // are short collision-checked identifiers, not secrets or access tokens.
    let mut bits = RandomState::new().hash_one((SystemTime::now(), std::process::id()));
    let mut id = String::from("art-");
    for _ in 0..8 {
        id.push(ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char);
        bits /= ALPHABET.len() as u64;
    }
    id
}

pub(super) fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    reject_symlink(path)?;
    ensure!(
        fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .is_file(),
        "not a regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "{} exceeds {MAX_BYTES} byte limit",
        path.display()
    );
    Ok(bytes)
}

pub(super) fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    reject_symlink(path)?;
    let tmp = path.with_file_name(format!(".record-{}.tmp", random_id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let result: Result<()> = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_rename()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("atomically writing {}", path.display()))
}
