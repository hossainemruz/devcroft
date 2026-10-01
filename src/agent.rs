//! The agent harness launched in the Agent tab.
//!
//! Entering a checkout resumes its most recent session when history exists;
//! otherwise the Agent tab starts the default harness (Settings > Agent,
//! [`AgentKind::DEFAULT`] until changed). Use **New session…** to pick a
//! harness explicitly for a new session; open sessions keep running with
//! the harness they started with ([`AgentKind::command`]).
//!
//! Only `opencode`, `claude`, `codex`, and `omp` exist today. Adding a
//! harness later is a matter of extending this enum plus its id/command
//! tables below.

/// Agent harness launched in the Agent tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AgentKind {
    Opencode,
    Claude,
    Codex,
    Omp,
}

impl AgentKind {
    /// Interactive startup only: keep the ordinary configuration, tools and
    /// permission prompts, and leave the conversation open for follow-ups.
    pub(crate) fn prompt_arguments(self, prompt: &str) -> Vec<String> {
        match self {
            Self::Opencode => vec!["--prompt".into(), prompt.into()],
            Self::Claude | Self::Codex | Self::Omp => vec![prompt.into()],
        }
    }
    /// The fallback harness: fresh installs and unset or unknown stored
    /// preferences resolve here. It is also the first entry in the
    /// new-session picker.
    pub(crate) const DEFAULT: Self = Self::Opencode;

    /// Every harness the new-session picker offers, in display order.
    pub(crate) const ALL: [Self; 4] = [Self::Opencode, Self::Claude, Self::Codex, Self::Omp];

    /// Stable id matched case-insensitively when resolving historical
    /// sessions, so values like `"Claude"` still resolve.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    /// Human label for the new-session picker and session rows.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    /// Short description for the new-session picker and session rows.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Opencode => "Anomaly's opencode CLI.",
            Self::Claude => "Anthropic's Claude Code CLI.",
            Self::Codex => "OpenAI's Codex CLI.",
            Self::Omp => "The oh-my-pi agent harness.",
        }
    }

    /// Shell command typed into the Agent pane's login shell at spawn.
    pub(crate) fn command(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    /// Parse a stored id. Unknown or empty values return `None` so callers
    /// fall back to [`AgentKind::DEFAULT`] instead of rejecting the session.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "opencode" => Some(Self::Opencode),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "omp" | "oh-my-pi" | "ohmypi" => Some(Self::Omp),
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
        assert_eq!(AgentKind::parse("gemini"), None);
        assert_eq!(
            AgentKind::parse("unknown").unwrap_or(AgentKind::DEFAULT),
            AgentKind::Opencode
        );
    }

    #[test]
    fn omp_aliases_resolve_to_omp() {
        assert_eq!(AgentKind::parse("omp"), Some(AgentKind::Omp));
        assert_eq!(AgentKind::parse("oh-my-pi"), Some(AgentKind::Omp));
        assert_eq!(AgentKind::parse("OHMYPI"), Some(AgentKind::Omp));
    }

    #[test]
    fn commands_match_supported_harnesses() {
        assert_eq!(AgentKind::Opencode.command(), "opencode");
        assert_eq!(AgentKind::Claude.command(), "claude");
        assert_eq!(AgentKind::Codex.command(), "codex");
        assert_eq!(AgentKind::Omp.command(), "omp");
    }

    #[test]
    fn sheet_labels_are_non_empty() {
        for agent in AgentKind::ALL {
            assert!(!agent.label().is_empty());
            assert!(!agent.description().is_empty());
        }
    }
}
