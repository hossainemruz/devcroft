//! The agent harness launched in the Agent tab.
//!
//! Each workspace remembers one default harness ([`AgentKind`]) in
//! machine-local `device.json`, keyed by its canonical checkout path (see
//! [`crate::data`]). Picking a harness in the workspace settings sheet
//! persists it and restarts the Agent tab with [`AgentKind::command`].
//!
//! Only `opencode` and `claude` exist today. Adding a harness later is a
//! matter of extending this enum plus its id/command tables below — the
//! persistence layer already round-trips unknown ids as plain strings.

/// Agent harness launched in the Agent tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AgentKind {
    Opencode,
    Claude,
}

impl AgentKind {
    /// The default for workspaces without a stored preference.
    pub(crate) const DEFAULT: Self = Self::Opencode;

    /// Every harness the workspace settings sheet offers, in display order.
    pub(crate) const ALL: [Self; 2] = [Self::Opencode, Self::Claude];

    /// Stable id used in `device.json` and matched case-insensitively on
    /// read, so hand-edited values like `"Claude"` still resolve.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
        }
    }

    /// Human label for the settings sheet.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
        }
    }

    /// Short description for the settings sheet.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Opencode => "The default AI agent.",
            Self::Claude => "Anthropic's Claude Code CLI.",
        }
    }

    /// Shell command typed into the Agent pane's login shell at spawn.
    pub(crate) fn command(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
        }
    }

    /// Parse a stored id. Unknown or empty values return `None` so callers
    /// fall back to [`AgentKind::DEFAULT`] instead of rejecting the file.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "opencode" => Some(Self::Opencode),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_case_insensitively() {
        for agent in AgentKind::ALL {
            assert_eq!(AgentKind::parse(agent.id()), Some(agent));
        }
        assert_eq!(AgentKind::parse("Claude"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::parse("  OPENCODE  "), Some(AgentKind::Opencode));
    }

    #[test]
    fn unknown_ids_fall_back_to_default() {
        assert_eq!(AgentKind::parse(""), None);
        assert_eq!(AgentKind::parse("codex"), None);
        assert_eq!(
            AgentKind::parse("unknown").unwrap_or(AgentKind::DEFAULT),
            AgentKind::Opencode
        );
    }

    #[test]
    fn commands_match_supported_harnesses() {
        assert_eq!(AgentKind::Opencode.command(), "opencode");
        assert_eq!(AgentKind::Claude.command(), "claude");
    }

    #[test]
    fn sheet_labels_are_non_empty() {
        for agent in AgentKind::ALL {
            assert!(!agent.label().is_empty());
            assert!(!agent.description().is_empty());
        }
    }
}
