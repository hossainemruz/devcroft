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
use crate::agent::AgentKind;
use crate::agent_sessions::{DEFAULT_SIDEBAR_LIMIT, snap_sidebar_limit};
use crate::metrics::{DEFAULT_APP_FONT_SIZE, clamp_app_font_size};

/// Selectable automatic portable-sync intervals, in minutes.
/// `None` (absent in `device.json`) means Off.
/// Reads stay tolerant — any positive stored value schedules — while the
/// Settings UI only ever writes these options (or clears to Off).
pub(crate) const SYNC_INTERVAL_OPTIONS: [u64; 4] = [2, 5, 15, 30];

/// Whether `minutes` is one of [`SYNC_INTERVAL_OPTIONS`]. Pure so the
/// Settings highlight stays unit-testable without a window.
pub(crate) fn is_supported_sync_interval(minutes: u64) -> bool {
    SYNC_INTERVAL_OPTIONS.contains(&minutes)
}

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
    /// Copy terminal selections on release; absent preserves the historical default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_copy_on_select: Option<bool>,
    /// Recent-session rows per repository in the Agent sidebar (Agents
    /// settings). Absent means the default; out-of-range values are clamped
    /// on read, never rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recent_sessions_limit: Option<u32>,
    /// Default agent harness id (Settings > Agent). Absent or unknown means
    /// [`AgentKind::DEFAULT`]; stored as a plain string so a harness added
    /// later can never break older builds' startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_agent: Option<String>,
    /// Enabled agent harness ids (Settings > Agent). Absent or empty means
    /// all harnesses; unknown ids are ignored on read so a harness removed
    /// later can never break startup. Stored as plain strings like
    /// `default_agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) enabled_agents: Option<Vec<String>>,
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

    /// Effective Agent sidebar limit: the stored value snapped to the
    /// Settings slider step and clamped to range, or the default when unset.
    pub(crate) fn recent_sessions_limit_or_default(&self) -> usize {
        self.recent_sessions_limit
            .map_or(DEFAULT_SIDEBAR_LIMIT, |limit| {
                snap_sidebar_limit(limit as usize)
            })
    }

    /// Effective default agent harness: the stored id, or
    /// [`AgentKind::DEFAULT`] when unset or unknown. Never fails — a
    /// hand-edited or future id must not break startup.
    pub(crate) fn default_agent_or_default(&self) -> AgentKind {
        self.default_agent
            .as_deref()
            .and_then(AgentKind::parse)
            .unwrap_or(AgentKind::DEFAULT)
    }

    /// Effective enabled harnesses in display order: the stored ids with
    /// unknown entries dropped, or every harness when unset, empty, or fully
    /// unknown. Never fails and never returns empty — at least one harness
    /// must stay selectable so the Agent tab and New-session picker cannot
    /// dead-end.
    pub(crate) fn enabled_agents_or_default(&self) -> Vec<AgentKind> {
        match self.enabled_agents.as_ref() {
            None => AgentKind::ALL.to_vec(),
            Some(ids) => {
                let enabled: Vec<AgentKind> = AgentKind::ALL
                    .into_iter()
                    .filter(|agent| {
                        ids.iter()
                            .any(|id| AgentKind::parse(id).is_some_and(|parsed| parsed == *agent))
                    })
                    .collect();
                if enabled.is_empty() {
                    AgentKind::ALL.to_vec()
                } else {
                    enabled
                }
            }
        }
    }

    /// Whether `agent` is in the effective enabled set.
    pub(crate) fn is_agent_enabled(&self, agent: AgentKind) -> bool {
        self.enabled_agents_or_default().contains(&agent)
    }

    /// Drop legacy selection fields: derived paths are never honored, and
    /// must not linger back into the file on the next save. The removed
    /// per-workspace default agent map (`workspace_agents`) is dropped the
    /// same way: fresh checkouts start with the first harness and returning
    /// checkouts resume their last session.
    fn scrub_legacy(&mut self) {
        self.extra.remove("dataDirectory");
        self.extra.remove("data_directory");
        self.extra.remove("workspace_agents");
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
    fn terminal_copy_preference_preserves_false_and_unknown_fields() {
        let state: DeviceState =
            serde_json::from_str(r#"{"terminal_copy_on_select":false,"future_setting":123}"#)
                .unwrap();
        assert_eq!(state.terminal_copy_on_select, Some(false));
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(encoded["terminal_copy_on_select"], false);
        assert_eq!(encoded["future_setting"], 123);
        assert_eq!(DeviceState::default().terminal_copy_on_select, None);
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
            recent_sessions_limit: Some(40),
            default_agent: Some("claude".to_owned()),
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
        assert_eq!(loaded.recent_sessions_limit, Some(40));
        assert_eq!(loaded.recent_sessions_limit_or_default(), 40);
        assert_eq!(loaded.default_agent.as_deref(), Some("claude"));
        assert_eq!(
            loaded.default_agent_or_default(),
            crate::agent::AgentKind::Claude
        );
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
    fn recent_sessions_limit_defaults_and_snaps_on_read() {
        use crate::agent_sessions::{DEFAULT_SIDEBAR_LIMIT, MAX_SIDEBAR_LIMIT, MIN_SIDEBAR_LIMIT};
        assert_eq!(
            DeviceState::default().recent_sessions_limit_or_default(),
            DEFAULT_SIDEBAR_LIMIT
        );
        let huge = DeviceState {
            recent_sessions_limit: Some(9999),
            ..DeviceState::default()
        };
        assert_eq!(huge.recent_sessions_limit_or_default(), MAX_SIDEBAR_LIMIT);
        let tiny = DeviceState {
            recent_sessions_limit: Some(1),
            ..DeviceState::default()
        };
        assert_eq!(tiny.recent_sessions_limit_or_default(), MIN_SIDEBAR_LIMIT);
        let exact = DeviceState {
            recent_sessions_limit: Some(40),
            ..DeviceState::default()
        };
        assert_eq!(exact.recent_sessions_limit_or_default(), 40);
        // Off-step values snap to the slider step.
        let off_step = DeviceState {
            recent_sessions_limit: Some(23),
            ..DeviceState::default()
        };
        assert_eq!(off_step.recent_sessions_limit_or_default(), 25);
    }

    #[test]
    fn enabled_agents_default_to_all_and_tolerate_unknown_ids() {
        use crate::agent::AgentKind;
        assert_eq!(
            DeviceState::default().enabled_agents_or_default(),
            AgentKind::ALL.to_vec()
        );
        let exact = DeviceState {
            enabled_agents: Some(vec!["opencode".to_owned(), "claude".to_owned()]),
            ..DeviceState::default()
        };
        assert_eq!(
            exact.enabled_agents_or_default(),
            vec![AgentKind::Opencode, AgentKind::Claude]
        );
        // Display order follows AgentKind::ALL, not stored order.
        let reversed = DeviceState {
            enabled_agents: Some(vec!["claude".to_owned(), "opencode".to_owned()]),
            ..DeviceState::default()
        };
        assert_eq!(
            reversed.enabled_agents_or_default(),
            vec![AgentKind::Opencode, AgentKind::Claude]
        );
        for stored in [None, Some(vec![]), Some(vec!["gemini".to_owned()])] {
            let state = DeviceState {
                enabled_agents: stored,
                ..DeviceState::default()
            };
            assert_eq!(state.enabled_agents_or_default(), AgentKind::ALL.to_vec());
        }
    }

    #[test]
    fn default_agent_defaults_and_tolerates_unknown_ids() {
        use crate::agent::AgentKind;
        assert_eq!(
            DeviceState::default().default_agent_or_default(),
            AgentKind::DEFAULT
        );
        let exact = DeviceState {
            default_agent: Some("claude".to_owned()),
            ..DeviceState::default()
        };
        assert_eq!(exact.default_agent_or_default(), AgentKind::Claude);
        // Case-insensitive like the rest of device.json reads.
        let mixed = DeviceState {
            default_agent: Some("Claude".to_owned()),
            ..DeviceState::default()
        };
        assert_eq!(mixed.default_agent_or_default(), AgentKind::Claude);
        // Unknown or empty ids fall back instead of failing the load.
        for id in ["gemini", "", "  "] {
            let unknown = DeviceState {
                default_agent: Some(id.to_owned()),
                ..DeviceState::default()
            };
            assert_eq!(
                unknown.default_agent_or_default(),
                AgentKind::DEFAULT,
                "id {id:?} should fall back"
            );
        }
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
    fn supported_sync_intervals_match_parity_options() {
        assert_eq!(SYNC_INTERVAL_OPTIONS, [2, 5, 15, 30]);
        for minutes in SYNC_INTERVAL_OPTIONS {
            assert!(is_supported_sync_interval(minutes));
        }
        assert!(!is_supported_sync_interval(0));
        assert!(!is_supported_sync_interval(10));
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
