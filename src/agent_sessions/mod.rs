//! Device-local conversation catalog. Provider formats never reach the views.
mod model;
mod process;
mod providers;

use crate::data::DataRoot;
pub(crate) use model::{HOME_LIMIT, SIDEBAR_LIMIT, SessionKey, SessionSummary};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub sessions: Vec<SessionSummary>,
    pub errors: Vec<String>,
    pub loaded: bool,
}
impl Snapshot {
    pub fn recent(&self, checkout: &std::path::Path, limit: usize) -> Vec<SessionSummary> {
        self.sessions
            .iter()
            .filter(|s| s.checkout == checkout)
            .take(limit)
            .cloned()
            .collect()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    sessions: Vec<SessionSummary>,
    transcripts: HashMap<PathBuf, providers::Transcript>,
}
#[derive(Clone)]
pub(crate) struct Catalog {
    snapshot: Arc<Mutex<Snapshot>>,
    scan: Arc<Mutex<Cache>>,
    cache: Option<PathBuf>,
}
impl Catalog {
    pub fn new(root: Option<&DataRoot>) -> Self {
        Self {
            snapshot: Arc::default(),
            scan: Arc::default(),
            cache: root.map(|r| r.root().join("cache/agent-sessions")),
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().clone()
    }
    /// Runs on a worker. Cached metadata is published before any provider calls.
    pub fn load_cache(&self) {
        let Some(dir) = &self.cache else {
            return;
        };
        let Ok(bytes) = fs::read(dir.join("index.json")) else {
            return;
        };
        if bytes.len() > 32 * 1024 * 1024 {
            return;
        }
        let Ok(mut cache) = serde_json::from_slice::<Cache>(&bytes) else {
            return;
        };
        if cache.version != 1 {
            return;
        }
        let sources = providers::sources();
        cache.sessions.retain(|s| {
            sources
                .iter()
                .any(|source| source.provider == s.key.provider && source.root == s.key.store)
        });
        model::sort(&mut cache.sessions);
        *self.snapshot.lock() = Snapshot {
            sessions: cache.sessions.clone(),
            errors: vec!["Refreshing saved sessions…".into()],
            loaded: false,
        };
        *self.scan.lock() = cache;
    }
    pub fn prepare_open(&self, key: &SessionKey) -> anyhow::Result<SessionSummary> {
        let source = providers::sources()
            .into_iter()
            .find(|s| s.provider == key.provider && s.root == key.store)
            .ok_or_else(|| anyhow::anyhow!("Session source is no longer configured"))?;
        let mut files = HashMap::new();
        let sessions = providers::discover(&source, &mut files)?;
        let mut rows: Vec<_> = sessions.into_iter().filter(|s| &s.key == key).collect();
        model::sort(&mut rows);
        let session = rows.pop().ok_or_else(|| {
            anyhow::anyhow!("Session was deleted, archived, or is no longer resumable")
        })?;
        session.validate()?;
        Ok(session)
    }
    pub fn refresh(&self) {
        let Some(mut cache) = self.scan.try_lock() else {
            return;
        };
        let mut errors = Vec::new();
        let sources = providers::sources();
        cache.sessions.retain(|s| {
            sources
                .iter()
                .any(|source| source.provider == s.key.provider && source.root == s.key.store)
        });
        for source in sources {
            match providers::discover(&source, &mut cache.transcripts) {
                Ok(sessions) => {
                    cache.sessions.retain(|s| {
                        s.key.provider != source.provider || s.key.store != source.root
                    });
                    cache.sessions.extend(sessions);
                }
                Err(error) => errors.push(format!(
                    "{} sessions unavailable: {error}. Saved results may be stale.",
                    source.provider
                )),
            }
            model::sort(&mut cache.sessions);
            *self.snapshot.lock() = Snapshot {
                sessions: cache.sessions.clone(),
                errors: errors.clone(),
                loaded: false,
            };
        }
        cache.version = 1;
        if let Some(dir) = &self.cache
            && let Err(error) = save_cache(dir, &cache)
        {
            errors.push(format!("Could not save local session cache: {error}"));
        }
        *self.snapshot.lock() = Snapshot {
            sessions: cache.sessions.clone(),
            errors,
            loaded: true,
        };
    }
}
fn save_cache(dir: &std::path::Path, cache: &Cache) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("index.lock"))?;
    lock.lock()?;
    // Disposable snapshots: the last completed scan wins, and every window refreshes.
    crate::data::write_json_atomic(&dir.join("index.json"), cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(provider: &str, id: &str, updated: i64) -> SessionSummary {
        SessionSummary {
            key: SessionKey {
                provider: provider.into(),
                store: "/store".into(),
                id: id.into(),
            },
            cwd: "/repo".into(),
            checkout: "/repo".into(),
            title: "test".into(),
            updated,
            timestamp_source: "test".into(),
        }
    }
    #[test]
    fn merges_by_recency_and_keeps_provider_and_store_identity() {
        let mut rows = vec![
            session("codex", "same", 20),
            session("claude", "same", 30),
            session("codex", "same", 10),
        ];
        model::sort(&mut rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key.provider, "claude");
        assert_eq!(rows[1].updated, 20);
    }
    #[test]
    fn checkout_matching_respects_components_and_nested_repos() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(model::checkout_for(&repo.join("src")), repo);
        assert_ne!(model::checkout_for(&root.path().join("repository")), repo);
        fs::create_dir(repo.join("src/.git")).unwrap();
        assert_eq!(model::checkout_for(&repo.join("src")), repo.join("src"));
    }
    #[test]
    fn corrupt_cache_is_disposable_and_outside_portable() {
        let root = tempfile::tempdir().unwrap();
        let data = DataRoot::new(root.path().into());
        let catalog = Catalog::new(Some(&data));
        let dir = catalog.cache.as_ref().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("index.json"), "broken").unwrap();
        catalog.load_cache();
        assert!(catalog.snapshot().sessions.is_empty());
        assert!(!dir.starts_with(data.portable_dir()));
    }
    #[test]
    fn resume_arguments_never_use_titles_or_latest() {
        for (provider, flag) in [
            ("codex", "resume"),
            ("claude", "--resume"),
            ("opencode", "--session"),
        ] {
            let mut row = session(provider, "native-id", 1);
            row.title = "$(touch /tmp/no)".into();
            assert_eq!(row.arguments(), vec![flag, "native-id"]);
        }
    }
}
