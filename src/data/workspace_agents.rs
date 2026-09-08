//! Per-workspace default agent harness.
//!
//! The setting lives in machine-local `device.json` (`workspace_agents`,
//! keyed by canonical checkout path string) so it survives restarts without
//! ever syncing: different machines may have different harnesses installed.
//! Keying by checkout path — the same identity [`crate::workspace`] uses for
//! its per-repository tab cache — keeps this working for linked and ad-hoc
//! (`--checkout`) workspaces alike, including separate worktrees of one repo.
//!
//! Stored values are plain agent ids ([`AgentKind::id`]). Unknown ids fall
//! back to [`AgentKind::DEFAULT`] on read instead of failing the load, so a
//! harness added later can never break older builds' startup.

use std::path::Path;

use anyhow::{Context as _, Result};

use super::{DataRoot, DeviceStore};
use crate::agent::AgentKind;

/// Canonical storage key for a checkout: symlinks resolved so aliases share
/// one setting. Falls back to the unresolved path when canonicalization
/// fails (e.g. a checkout removed between switch and save).
pub(crate) fn workspace_key(checkout: &Path) -> String {
    std::fs::canonicalize(checkout)
        .unwrap_or_else(|_| checkout.to_owned())
        .to_string_lossy()
        .into_owned()
}

/// Default harness for a checkout: the stored id, or [`AgentKind::DEFAULT`]
/// when unset, unknown, or when device state is unavailable. Never fails —
/// a broken `device.json` must not break workspace startup.
pub(crate) fn resolve_workspace_agent(root: Option<&DataRoot>, checkout: &Path) -> AgentKind {
    let Some(root) = root else {
        return AgentKind::DEFAULT;
    };
    let state = DeviceStore::new(root).load().unwrap_or_default();
    state
        .workspace_agents
        .as_ref()
        .and_then(|agents| agents.get(&workspace_key(checkout)))
        .and_then(|id| AgentKind::parse(id))
        .unwrap_or(AgentKind::DEFAULT)
}

/// Persist the default harness for a checkout. No-op when portable data is
/// unavailable (`None` root): the live session keeps the selection, only the
/// restart persistence is lost.
pub(crate) fn set_workspace_agent(
    root: Option<&DataRoot>,
    checkout: &Path,
    agent: AgentKind,
) -> Result<()> {
    let Some(root) = root else {
        return Ok(());
    };
    let key = workspace_key(checkout);
    let id = agent.id().to_owned();
    DeviceStore::new(root)
        .update(|state| {
            let mut agents = state.workspace_agents.take().unwrap_or_default();
            agents.insert(key.clone(), id.clone());
            state.workspace_agents = Some(agents);
        })
        .context("saving workspace default agent to device.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataRoot;

    fn fresh_root() -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        (dir, root)
    }

    #[test]
    fn unset_checkout_resolves_to_default() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), checkout.path()),
            AgentKind::DEFAULT
        );
        // Missing device state entirely also falls back, never panics.
        assert_eq!(
            resolve_workspace_agent(None, checkout.path()),
            AgentKind::DEFAULT
        );
    }

    #[test]
    fn set_then_resolve_round_trips() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        set_workspace_agent(Some(&root), checkout.path(), AgentKind::Claude).unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), checkout.path()),
            AgentKind::Claude
        );
        set_workspace_agent(Some(&root), checkout.path(), AgentKind::Opencode).unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), checkout.path()),
            AgentKind::Opencode
        );
    }

    #[test]
    fn distinct_checkouts_stay_independent() {
        let (_dir, root) = fresh_root();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        set_workspace_agent(Some(&root), first.path(), AgentKind::Claude).unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), first.path()),
            AgentKind::Claude
        );
        assert_eq!(
            resolve_workspace_agent(Some(&root), second.path()),
            AgentKind::DEFAULT
        );
    }

    #[test]
    fn unknown_stored_ids_fall_back_to_default() {
        let (_dir, root) = fresh_root();
        let checkout = tempfile::tempdir().unwrap();
        let key = workspace_key(checkout.path());
        DeviceStore::new(&root)
            .update(|state| {
                state.workspace_agents = Some([(key, "gemini".to_owned())].into());
            })
            .unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), checkout.path()),
            AgentKind::DEFAULT
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_one_setting() {
        let (_dir, root) = fresh_root();
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&checkout).unwrap();
        std::os::unix::fs::symlink(&checkout, &alias).unwrap();
        set_workspace_agent(Some(&root), &alias, AgentKind::Claude).unwrap();
        assert_eq!(
            resolve_workspace_agent(Some(&root), &checkout),
            AgentKind::Claude
        );
    }
}
