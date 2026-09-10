use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    agent::AgentKind,
    relative_time::{current_unix_secs, relative_duration_label},
};

pub(crate) const HOME_LIMIT: usize = 4;
pub(crate) const SIDEBAR_LIMIT: usize = 15;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SessionKey {
    pub provider: String,
    pub store: PathBuf,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionSummary {
    pub key: SessionKey,
    pub cwd: PathBuf,
    #[serde(default)]
    pub checkout: PathBuf,
    pub title: String,
    pub updated: i64,
    pub timestamp_source: String,
}

impl SessionSummary {
    pub fn agent(&self) -> Option<AgentKind> {
        AgentKind::parse(&self.key.provider)
    }
    /// Harness label for cards and rows, never the raw stored id.
    pub fn provider_label(&self) -> &str {
        self.agent()
            .map(|agent| agent.label())
            .unwrap_or(&self.key.provider)
    }
    pub fn age(&self) -> String {
        if self.updated <= 0 {
            return "Unknown activity time".into();
        }
        relative_duration_label(current_unix_secs().saturating_sub(self.updated))
    }
    /// Absolute UTC timestamp for tooltips; the relative [`Self::age`]
    /// label stays the compact row text.
    pub fn absolute_time(&self) -> String {
        if self.updated <= 0 {
            return "Unknown activity time".into();
        }
        chrono::DateTime::from_timestamp(self.updated, 0)
            .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "Unknown activity time".into())
    }
    /// Full title plus timestamp for card/row tooltips; titles render
    /// truncated in place.
    pub fn tooltip(&self) -> String {
        format!(
            "{}\n{} · {} ({})",
            self.title,
            self.provider_label(),
            self.age(),
            self.absolute_time()
        )
    }
    pub fn validate(&self) -> Result<()> {
        if self.agent().is_none() || self.key.provider == "omp" {
            bail!("Unsupported session provider");
        }
        if self.key.id.is_empty()
            || self.key.id.starts_with('-')
            || self.key.id.chars().any(char::is_control)
        {
            bail!("Invalid session identity");
        }
        if !self.cwd.is_absolute() || !self.cwd.is_dir() {
            bail!(
                "Session working directory is unavailable: {}",
                self.cwd.display()
            );
        }
        if !self.key.store.is_dir() {
            bail!("Session store is unavailable: {}", self.key.store.display());
        }
        if self.key.provider == "claude" && !claude_transcript(self).is_some_and(|p| p.is_file()) {
            bail!("Claude transcript is no longer available");
        }
        Ok(())
    }
    pub fn arguments(&self) -> Vec<String> {
        match self.key.provider.as_str() {
            "codex" => vec!["resume".into(), self.key.id.clone()],
            "opencode" => vec!["--session".into(), self.key.id.clone()],
            "claude" => vec!["--resume".into(), self.key.id.clone()],
            _ => Vec::new(),
        }
    }
    pub fn environment(&self) -> Vec<(String, String)> {
        let name = match self.key.provider.as_str() {
            "codex" => "CODEX_HOME",
            "claude" => "CLAUDE_CONFIG_DIR",
            "opencode" => "XDG_DATA_HOME",
            _ => return Vec::new(),
        };
        vec![(name.into(), self.key.store.to_string_lossy().into_owned())]
    }
}

pub(super) fn claude_transcript(session: &SessionSummary) -> Option<PathBuf> {
    // Read only immediate project transcripts; subagent directories are excluded.
    std::fs::read_dir(session.key.store.join("projects"))
        .ok()?
        .filter_map(Result::ok)
        .map(|project| project.path().join(format!("{}.jsonl", session.key.id)))
        .find(|path| path.is_file())
}

pub(crate) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

pub(super) fn checkout_for(cwd: &Path) -> PathBuf {
    let cwd = canonical(cwd);
    cwd.ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(&cwd)
        .to_owned()
}

pub(super) fn title(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect();
    if value.is_empty() {
        "Untitled session".into()
    } else {
        value
    }
}

pub(super) fn sort(sessions: &mut Vec<SessionSummary>) {
    let now = current_unix_secs();
    for session in sessions.iter_mut() {
        session.updated = session.updated.clamp(0, now);
        session.cwd = canonical(&session.cwd);
        session.checkout = checkout_for(&session.cwd);
    }
    sessions.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.key.cmp(&b.key)));
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|session| seen.insert(session.key.clone()));
}
