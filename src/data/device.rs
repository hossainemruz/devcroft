//! Machine-local `device.json`: tolerant load, atomic save, serialized RMW.
//!
//! Same guarantees as Electron's `DeviceStateStore` (atomic
//! temp-sibling-plus-rename writes serialized per path), minus the
//! `dataDirectory` field — the path is now derived, not selected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DataRoot, write_json_atomic};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, clamp_app_font_size};

/// Machine-local state: checkout bindings, agent/editor settings,
/// pins/recents, theme. Never committed.
///
/// Optional known fields plus flattened unknown-field preservation, so
/// older/newer builds round-trip values they don't understand. There is
/// deliberately no `dataDirectory` field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DeviceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sync_interval_minutes: Option<u64>,
    /// App-wide font size (General settings). Absent means the default;
    /// out-of-range values are clamped on read, never rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) app_font_size: Option<f32>,
    /// Per-workspace default agent harness, keyed by canonical checkout
    /// path string (see `workspace_agents`). Machine-local like the rest of
    /// this file: different machines may have different harnesses
    /// installed. Unknown ids are tolerated on read (callers fall back to
    /// the default) and preserved across edits via plain-string storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_agents: Option<HashMap<String, String>>,
    /// Machine-local checkout bindings by repository key: the linked local
    /// checkout plus remote alias. Portable metadata lives in
    /// `portable/repositories/<key>/repository.json`; only the binding that
    /// ties a key to this machine lives here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repositories: Option<HashMap<String, DeviceRepositoryBinding>>,
    /// Unknown fields, preserved across edits where practical.
    #[serde(flatten)]
    pub(crate) extra: HashMap<String, Value>,
}

/// Machine-local binding for one portable repository: where its checkout
/// lives on this machine and which remote alias Review prefers.
/// `snake_case` matches the rest of this file; portable `repository.json`
/// stays `camelCase` for Electron parity (see `repositories`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DeviceRepositoryBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkout_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_opened_at: Option<String>,
    #[serde(flatten, default)]
    pub(crate) extra: HashMap<String, Value>,
}

impl DeviceState {
    /// Effective app-wide font size: the stored value clamped to the
    /// settable range, or the default when unset.
    pub(crate) fn app_font_size_or_default(&self) -> f32 {
        self.app_font_size
            .map_or(DEFAULT_APP_FONT_SIZE, clamp_app_font_size)
    }

    /// Drop legacy selection fields: derived paths are never honored, and
    /// must not linger back into the file on the next save.
    fn scrub_legacy(&mut self) {
        self.extra.remove("dataDirectory");
        self.extra.remove("data_directory");
    }
}

/// `device.json` store with atomic writes serialized per path. Share one
/// instance per process so concurrent `update` calls serialize.
pub(crate) struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl DeviceStore {
    pub(crate) fn new(root: &DataRoot) -> Self {
        Self {
            path: root.device_path(),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Tolerant load: a missing file is default state; unknown fields are
    /// kept; legacy selection fields are dropped.
    pub(crate) fn load(&self) -> Result<DeviceState> {
        let _guard = self.lock.lock();
        load_from(&self.path)
    }

    pub(crate) fn save(&self, state: &DeviceState) -> Result<()> {
        let _guard = self.lock.lock();
        // Scrub on the way out too, so the invariant holds even if a caller
        // hand-inserts a legacy key into `extra`.
        let mut owned = state.clone();
        owned.scrub_legacy();
        save_to(&self.path, &owned)
    }

    /// Serialized read-modify-write: load, apply `f`, save atomically.
    pub(crate) fn update<R>(&self, f: impl FnOnce(&mut DeviceState) -> R) -> Result<R> {
        let _guard = self.lock.lock();
        let mut state = load_from(&self.path)?;
        let result = f(&mut state);
        state.scrub_legacy();
        save_to(&self.path, &state)?;
        Ok(result)
    }
}

fn load_from(path: &Path) -> Result<DeviceState> {
    let bytes = match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeviceState::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
        Ok(bytes) => bytes,
    };
    let mut state: DeviceState =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    state.scrub_legacy();
    Ok(state)
}

fn save_to(path: &Path, state: &DeviceState) -> Result<()> {
    write_json_atomic(path, state)
        .with_context(|| format!("saving device state to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_in(dir: &Path) -> DeviceStore {
        DeviceStore::new(&DataRoot::new(dir.to_owned()))
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = store_in(dir.path()).load().unwrap();
        assert_eq!(state, DeviceState::default());
    }

    #[test]
    fn round_trip_preserves_known_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut state = DeviceState {
            theme: Some("dark".to_owned()),
            sync_interval_minutes: Some(15),
            app_font_size: Some(17.0),
            ..DeviceState::default()
        };
        state
            .extra
            .insert("futureField".into(), json!({"v": [1, 2]}));
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.theme.as_deref(), Some("dark"));
        assert_eq!(loaded.sync_interval_minutes, Some(15));
        assert_eq!(loaded.app_font_size, Some(17.0));
        assert_eq!(loaded.app_font_size_or_default(), 17.0);
        assert_eq!(loaded.extra.get("futureField"), Some(&json!({"v": [1, 2]})));

        // The file itself is two-space JSON with a trailing newline.
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn app_font_size_defaults_and_clamps_on_read() {
        assert_eq!(
            DeviceState::default().app_font_size_or_default(),
            crate::metrics::DEFAULT_APP_FONT_SIZE
        );
        let huge = DeviceState {
            app_font_size: Some(99.0),
            ..DeviceState::default()
        };
        assert_eq!(
            huge.app_font_size_or_default(),
            crate::metrics::MAX_APP_FONT_SIZE
        );
        let tiny = DeviceState {
            app_font_size: Some(1.0),
            ..DeviceState::default()
        };
        assert_eq!(
            tiny.app_font_size_or_default(),
            crate::metrics::MIN_APP_FONT_SIZE
        );
    }

    #[test]
    fn legacy_data_directory_is_dropped_not_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        std::fs::write(
            store.path(),
            "{\"theme\":\"light\",\"dataDirectory\":\"/tmp/old\",\"data_directory\":\"/tmp/old2\"}\n",
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.theme.as_deref(), Some("light"));
        assert!(!loaded.extra.contains_key("dataDirectory"));
        assert!(!loaded.extra.contains_key("data_directory"));

        // A later save must not write it back either.
        store.save(&loaded).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("dataDirectory"));
        assert!(!text.contains("data_directory"));
    }

    #[test]
    fn save_scrubs_legacy_keys_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut state = DeviceState::default();
        state
            .extra
            .insert("dataDirectory".into(), json!("/tmp/old"));
        store.save(&state).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("dataDirectory"));
    }

    #[test]
    fn malformed_file_errors_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        std::fs::write(store.path(), "{not json").unwrap();
        let error = format!("{:#}", store.load().expect_err("must fail"));
        assert!(error.contains("device.json"), "{error}");
    }

    #[test]
    fn update_is_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .update(|state| {
                state.theme = Some("light".to_owned());
                state.extra.insert("kept".into(), json!(true));
            })
            .unwrap();
        let theme = store
            .update(|state| {
                state.last_tab = Some("Review".to_owned());
                state.theme.clone()
            })
            .unwrap();
        assert_eq!(theme.as_deref(), Some("light"));
        let loaded = store.load().unwrap();
        assert_eq!(loaded.last_tab.as_deref(), Some("Review"));
        assert_eq!(loaded.extra.get("kept"), Some(&json!(true)));
    }

    #[test]
    fn concurrent_updates_serialize_without_loss() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(store_in(dir.path()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    store
                        .update(|state| {
                            let next = state
                                .extra
                                .get("counter")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0)
                                + 1;
                            state.extra.insert("counter".into(), json!(next));
                        })
                        .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.extra.get("counter").and_then(|value| value.as_u64()),
            Some(200)
        );
    }
}
