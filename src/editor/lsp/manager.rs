//! Checkout-scoped session identity and document ownership. No GUI or subprocess I/O.
use super::{
    Client,
    catalog::{Preference, ServerId, root_for},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    pub server: ServerId,
    pub root: PathBuf,
}
pub(crate) struct Slot {
    pub generation: u64,
    pub attempts: u32,
    pub client: Option<Arc<Client>>,
    pub starting: bool,
    pub status: String,
    pub preference: Preference,
    pub touched: Instant,
    pub started: Instant,
}
#[derive(Default)]
pub(crate) struct Manager {
    pub slots: HashMap<Key, Slot>,
    pub documents: HashMap<PathBuf, Key>,
    next_generation: u64,
}
impl Manager {
    pub fn key(server: ServerId, path: &Path, checkout: &Path, pref: &Preference) -> Key {
        Key {
            server,
            root: root_for(server, path, checkout, pref),
        }
    }
    pub fn start(&mut self, key: Key, pref: Preference) -> u64 {
        let attempts = self.slots.get(&key).map_or(1, |s| s.attempts + 1);
        self.next_generation += 1;
        let generation = self.next_generation;
        self.slots.insert(
            key,
            Slot {
                generation,
                attempts,
                client: None,
                starting: true,
                status: "Starting…".into(),
                preference: pref,
                touched: Instant::now(),
                started: Instant::now(),
            },
        );
        generation
    }
    pub fn accepts(&self, key: &Key, generation: u64) -> bool {
        self.slots
            .get(key)
            .is_some_and(|s| s.generation == generation)
    }
    pub fn accepts_document(&self, key: &Key, generation: u64, path: &Path) -> bool {
        self.accepts(key, generation) && self.documents.get(path) == Some(key)
    }
    pub fn client(&self, path: &Path) -> Option<&Arc<Client>> {
        self.documents
            .get(path)
            .and_then(|k| self.slots.get(k))
            .and_then(|s| s.client.as_ref())
    }
    pub fn bind(&mut self, path: PathBuf, key: Key) {
        if let Some(old) = self
            .documents
            .get(&path)
            .filter(|old| **old != key)
            .cloned()
            && let Some(client) = self.slots.get(&old).and_then(|s| s.client.as_ref())
            && let Ok(uri) = super::file_uri(&path)
        {
            client.did_close(uri.as_str());
        }
        self.documents.insert(path, key.clone());
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.touched = Instant::now();
        }
    }
    pub fn close(&mut self, path: &Path) {
        if let Some(key) = self.documents.remove(path)
            && let Some(slot) = self.slots.get_mut(&key)
        {
            if let Some(client) = &slot.client
                && let Ok(uri) = super::file_uri(path)
            {
                client.did_close(uri.as_str());
            }
            slot.touched = Instant::now();
        }
    }
    pub fn prune(&mut self) {
        self.slots.retain(|k, s| {
            self.documents.values().any(|d| d == k) || s.touched.elapsed() < Duration::from_secs(60)
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generation_and_roots_isolate_sessions() {
        let mut m = Manager::default();
        let a = Key {
            server: ServerId::Go,
            root: "/a".into(),
        };
        let b = Key {
            server: ServerId::Go,
            root: "/b".into(),
        };
        let first = m.start(a.clone(), Preference::default());
        let other = m.start(b.clone(), Preference::default());
        let next = m.start(a.clone(), Preference::default());
        assert!(!m.accepts(&a, first));
        assert!(m.accepts(&a, next));
        assert!(m.accepts(&b, other));
        let path = PathBuf::from("/a/main.go");
        m.bind(path.clone(), a.clone());
        assert!(m.accepts_document(&a, next, &path));
        m.bind(path.clone(), b.clone());
        assert!(!m.accepts_document(&a, next, &path));
        assert!(m.accepts_document(&b, other, &path));
    }
}
