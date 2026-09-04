//! The command bar model: searchable palette entries for navigation,
//! repository switching, settings, and portable-data sync.
//!
//! [`PaletteCommand`] is deliberately UI-free so filtering and the
//! section/row mapping stay unit-testable without a window. `Workspace`
//! renders one [`gpui_kit::component::command::CommandGroup`] per
//! [`GROUPS`] entry in order, so a confirmed `IndexPath` maps back through
//! [`command_at`] regardless of the current query filter.

gpui_kit::actions!(devcroft, [ToggleCommandPalette]);

/// Every command the bar can run, in canonical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PaletteCommand {
    GoAgent,
    GoEditor,
    GoTerminal,
    GoReview,
    GoHome,
    SwitchRepository,
    OpenSettings,
    SyncPortable,
}

impl PaletteCommand {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::GoAgent,
        Self::GoEditor,
        Self::GoTerminal,
        Self::GoReview,
        Self::GoHome,
        Self::SwitchRepository,
        Self::OpenSettings,
        Self::SyncPortable,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::GoAgent => "Go to Agent",
            Self::GoEditor => "Go to Editor",
            Self::GoTerminal => "Go to Terminal",
            Self::GoReview => "Go to Review",
            Self::GoHome => "Go to Home",
            Self::SwitchRepository => "Switch repository…",
            Self::OpenSettings => "Open settings…",
            Self::SyncPortable => "Sync portable data now",
        }
    }

    /// Extra search terms besides the label. These mirror the
    /// [`gpui_kit::component::command::CommandItem`] keywords installed by
    /// `Workspace`, so keep the two in sync when adding commands.
    pub(crate) fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::GoAgent => &["tab", "agent", "opencode"],
            Self::GoEditor => &["tab", "editor", "nvim"],
            Self::GoTerminal => &["tab", "terminal", "shell"],
            Self::GoReview => &["tab", "review", "diff"],
            Self::GoHome => &["tab", "home", "dashboard"],
            Self::SwitchRepository => &["repo", "repository", "project", "switch"],
            Self::OpenSettings => &["settings", "preferences", "config"],
            Self::SyncPortable => &["sync", "portable", "push", "pull", "backup"],
        }
    }

    /// Case-insensitive substring match over the label and keywords, matching
    /// the palette's own filtering so tests pin the search behavior users see.
    /// Test-only: production filtering lives in the `Command` widget itself.
    #[cfg(test)]
    pub(crate) fn matches(self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        self.label().to_lowercase().contains(&query)
            || self
                .keywords()
                .iter()
                .any(|keyword| keyword.to_lowercase().contains(&query))
    }
}

const GO_TO_COMMANDS: [PaletteCommand; 5] = [
    PaletteCommand::GoAgent,
    PaletteCommand::GoEditor,
    PaletteCommand::GoTerminal,
    PaletteCommand::GoReview,
    PaletteCommand::GoHome,
];

const REPOSITORY_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::SwitchRepository];

const SETTINGS_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::OpenSettings];

const SYNC_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::SyncPortable];

/// Palette groups in render order. `Workspace` builds one `CommandGroup` per
/// entry with no ungrouped items, so the group's position is the `IndexPath`
/// section and the item's position within it is the row.
pub(crate) const GROUPS: [(&str, &[PaletteCommand]); 4] = [
    ("Go to", &GO_TO_COMMANDS),
    ("Repository", &REPOSITORY_COMMANDS),
    ("Settings", &SETTINGS_COMMANDS),
    ("Sync", &SYNC_COMMANDS),
];

/// Resolve a confirmed `IndexPath` (section/row in the model installed by the
/// latest `Command` render, before filtering) back to its command.
pub(crate) fn command_at(section: usize, row: usize) -> Option<PaletteCommand> {
    GROUPS.get(section)?.1.get(row).copied()
}

/// Searchable subset for `query`, preserving canonical order. An empty query
/// returns everything. Test-only mirror of the widget's filtering.
#[cfg(test)]
pub(crate) fn filter_commands(query: &str) -> Vec<PaletteCommand> {
    PaletteCommand::ALL
        .into_iter()
        .filter(|command| command.matches(query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn groups_cover_every_command_exactly_once() {
        let grouped: Vec<PaletteCommand> =
            GROUPS.iter().flat_map(|(_, items)| items.iter().copied()).collect();
        assert_eq!(grouped.len(), PaletteCommand::ALL.len());
        let unique: HashSet<_> = grouped.iter().collect();
        assert_eq!(unique.len(), PaletteCommand::ALL.len());
        for command in PaletteCommand::ALL {
            assert!(grouped.contains(&command), "{command:?} is missing a group");
        }
    }

    #[test]
    fn index_paths_round_trip_through_groups() {
        // (section, row) must match declaration order: groups render in
        // GROUPS order with no ungrouped section, so section is the group
        // index.
        assert_eq!(command_at(0, 0), Some(PaletteCommand::GoAgent));
        assert_eq!(command_at(0, 4), Some(PaletteCommand::GoHome));
        assert_eq!(command_at(1, 0), Some(PaletteCommand::SwitchRepository));
        assert_eq!(command_at(2, 0), Some(PaletteCommand::OpenSettings));
        assert_eq!(command_at(3, 0), Some(PaletteCommand::SyncPortable));
        assert_eq!(command_at(0, 5), None);
        assert_eq!(command_at(4, 0), None);
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        assert_eq!(filter_commands(""), PaletteCommand::ALL.to_vec());
        assert_eq!(filter_commands("   "), PaletteCommand::ALL.to_vec());
    }

    #[test]
    fn filter_matches_labels_case_insensitively() {
        assert_eq!(
            filter_commands("TERMINAL"),
            vec![PaletteCommand::GoTerminal]
        );
        assert_eq!(
            filter_commands("go to"),
            vec![
                PaletteCommand::GoAgent,
                PaletteCommand::GoEditor,
                PaletteCommand::GoTerminal,
                PaletteCommand::GoReview,
                PaletteCommand::GoHome,
            ]
        );
    }

    #[test]
    fn filter_matches_keywords_for_each_purpose() {
        assert!(filter_commands("repo").contains(&PaletteCommand::SwitchRepository));
        assert!(filter_commands("preferences").contains(&PaletteCommand::OpenSettings));
        assert!(filter_commands("backup").contains(&PaletteCommand::SyncPortable));
        assert!(filter_commands("opencode").contains(&PaletteCommand::GoAgent));
    }

    #[test]
    fn filter_with_no_match_returns_empty() {
        assert!(filter_commands("zzz-no-such-command").is_empty());
    }
}
