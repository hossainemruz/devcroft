//! The command bar model: searchable palette entries for navigation,
//! repository switching, settings, and portable-data sync.
//!
//! [`PaletteItem`] is deliberately UI-free so filtering and the
//! section/row mapping stay unit-testable without a window. `Workspace`
//! rebuilds the sections on every palette opening (recent repositories
//! change with use), renders one [`gpui_kit::component::command::CommandGroup`]
//! per section in order, and resolves a confirmed `IndexPath` against the
//! same model it rendered.

use crate::data::RecentRepository;

gpui_kit::actions!(devcroft, [ToggleCommandPalette]);

/// Every static command the bar can run, in canonical order. Repository
/// switching is dynamic (one [`PaletteItem::SwitchRepository`] per recent
/// repository) and lives outside this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PaletteCommand {
    GoAgent,
    GoEditor,
    GoTerminal,
    GoReview,
    GoHome,
    AddRepository,
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
        Self::AddRepository,
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
            Self::AddRepository => "Add repository…",
            Self::OpenSettings => "Open settings…",
            Self::SyncPortable => "Sync portable data now",
        }
    }

    /// Extra search terms besides the label, installed on the widget by
    /// `Workspace` straight from [`PaletteItem::keywords`], so the model
    /// stays the single source of truth for what the widget matches.
    pub(crate) fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::GoAgent => &["tab", "agent", "opencode"],
            Self::GoEditor => &["tab", "editor", "nvim"],
            Self::GoTerminal => &["tab", "terminal", "shell"],
            Self::GoReview => &["tab", "review", "diff"],
            Self::GoHome => &["tab", "home", "dashboard"],
            Self::AddRepository => &["repo", "repository", "project", "add", "new", "checkout"],
            Self::OpenSettings => &["settings", "preferences", "config"],
            Self::SyncPortable => &["sync", "portable", "push", "pull", "backup"],
        }
    }
}

/// One rendered palette row: either a static command or a switch target
/// for a recent repository. The key is the stable identity; the label is
/// display text (display name when set, otherwise the key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PaletteItem {
    Command(PaletteCommand),
    SwitchRepository { key: String, label: String },
}

impl PaletteItem {
    pub(crate) fn switch_target(repository: &RecentRepository) -> Self {
        Self::SwitchRepository {
            key: repository.key.clone(),
            label: repository.label().to_owned(),
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Command(command) => command.label(),
            Self::SwitchRepository { label, .. } => label,
        }
    }

    /// Search terms installed on the widget alongside the label. The key is
    /// always searchable, so a repository found by display name still
    /// answers to its key and vice versa.
    pub(crate) fn keywords(&self) -> Vec<&str> {
        match self {
            Self::Command(command) => command.keywords().to_vec(),
            Self::SwitchRepository { key, .. } => {
                vec![key.as_str(), "repo", "repository", "project", "switch"]
            }
        }
    }

    /// Case-insensitive substring match over the label and keywords, matching
    /// the palette's own filtering so tests pin the search behavior users see.
    /// Test-only: production filtering lives in the `Command` widget itself.
    #[cfg(test)]
    pub(crate) fn matches(&self, query: &str) -> bool {
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

/// One rendered palette section: a static heading with its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaletteSection {
    pub(crate) heading: &'static str,
    pub(crate) items: Vec<PaletteItem>,
}

/// Build the rendered sections: static command groups plus the recent
/// repositories (already ordered, most-recent first) ahead of the static
/// repository commands. `Workspace` renders exactly this and resolves
/// confirmations against it, so the two can never disagree.
pub(crate) fn palette_sections(recents: &[RecentRepository]) -> Vec<PaletteSection> {
    let commands = |commands: &[PaletteCommand]| {
        commands
            .iter()
            .map(|command| PaletteItem::Command(*command))
            .collect::<Vec<_>>()
    };
    vec![
        PaletteSection {
            heading: "Go to",
            items: commands(&GO_TO_COMMANDS),
        },
        PaletteSection {
            heading: "Repositories",
            items: recents
                .iter()
                .map(PaletteItem::switch_target)
                .chain(commands(&REPOSITORY_COMMANDS))
                .collect(),
        },
        PaletteSection {
            heading: "Settings",
            items: commands(&SETTINGS_COMMANDS),
        },
        PaletteSection {
            heading: "Sync",
            items: commands(&SYNC_COMMANDS),
        },
    ]
}

/// Resolve a confirmed `IndexPath` (section/row in the model installed by the
/// latest `Command` render, before filtering) back to its item.
pub(crate) fn item_at(
    sections: &[PaletteSection],
    section: usize,
    row: usize,
) -> Option<PaletteItem> {
    sections.get(section)?.items.get(row).cloned()
}

const GO_TO_COMMANDS: [PaletteCommand; 5] = [
    PaletteCommand::GoAgent,
    PaletteCommand::GoEditor,
    PaletteCommand::GoTerminal,
    PaletteCommand::GoReview,
    PaletteCommand::GoHome,
];

const REPOSITORY_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::AddRepository];

const SETTINGS_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::OpenSettings];

const SYNC_COMMANDS: [PaletteCommand; 1] = [PaletteCommand::SyncPortable];

/// Searchable subset for `query`, preserving canonical order. An empty query
/// returns everything. Test-only mirror of the widget's filtering.
#[cfg(test)]
pub(crate) fn filter_items(sections: &[PaletteSection], query: &str) -> Vec<PaletteItem> {
    sections
        .iter()
        .flat_map(|section| section.items.iter())
        .filter(|item| item.matches(query))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn fixture_recents() -> Vec<RecentRepository> {
        vec![
            RecentRepository {
                key: "aaa-first".to_owned(),
                display_name: Some("First Repo".to_owned()),
                checkout_path: PathBuf::from("/tmp/first"),
                last_opened_at: Some("2024-06-01T00:00:00Z".to_owned()),
            },
            RecentRepository {
                key: "bbb-second".to_owned(),
                display_name: None,
                checkout_path: PathBuf::from("/tmp/second"),
                last_opened_at: Some("2024-01-01T00:00:00Z".to_owned()),
            },
        ]
    }

    #[test]
    fn sections_cover_every_command_exactly_once() {
        let commands: Vec<PaletteCommand> = palette_sections(&[])
            .into_iter()
            .flat_map(|section| section.items)
            .filter_map(|item| match item {
                PaletteItem::Command(command) => Some(command),
                PaletteItem::SwitchRepository { .. } => None,
            })
            .collect();
        assert_eq!(commands.len(), PaletteCommand::ALL.len());
        let unique: HashSet<_> = commands.iter().collect();
        assert_eq!(unique.len(), PaletteCommand::ALL.len());
        for command in PaletteCommand::ALL {
            assert!(
                commands.contains(&command),
                "{command:?} is missing a group"
            );
        }
    }

    #[test]
    fn sections_list_recents_ahead_of_static_repository_commands() {
        let sections = palette_sections(&fixture_recents());
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[1].heading, "Repositories");
        assert_eq!(
            sections[1].items,
            vec![
                PaletteItem::SwitchRepository {
                    key: "aaa-first".to_owned(),
                    label: "First Repo".to_owned(),
                },
                PaletteItem::SwitchRepository {
                    key: "bbb-second".to_owned(),
                    label: "bbb-second".to_owned(),
                },
                PaletteItem::Command(PaletteCommand::AddRepository),
            ]
        );
        // No recents: the group holds just the static commands, never empty.
        let sections = palette_sections(&[]);
        assert_eq!(
            sections[1].items,
            vec![PaletteItem::Command(PaletteCommand::AddRepository)]
        );
    }

    #[test]
    fn index_paths_round_trip_through_sections() {
        let sections = palette_sections(&fixture_recents());
        // (section, row) matches render order: sections render in order
        // with no ungrouped rows, so section is the group index.
        assert_eq!(
            item_at(&sections, 0, 0),
            Some(PaletteItem::Command(PaletteCommand::GoAgent))
        );
        assert_eq!(
            item_at(&sections, 0, 4),
            Some(PaletteItem::Command(PaletteCommand::GoHome))
        );
        assert_eq!(
            item_at(&sections, 1, 0),
            Some(PaletteItem::SwitchRepository {
                key: "aaa-first".to_owned(),
                label: "First Repo".to_owned(),
            })
        );
        assert_eq!(
            item_at(&sections, 1, 2),
            Some(PaletteItem::Command(PaletteCommand::AddRepository))
        );
        assert_eq!(
            item_at(&sections, 2, 0),
            Some(PaletteItem::Command(PaletteCommand::OpenSettings))
        );
        assert_eq!(
            item_at(&sections, 3, 0),
            Some(PaletteItem::Command(PaletteCommand::SyncPortable))
        );
        assert_eq!(item_at(&sections, 0, 5), None);
        assert_eq!(item_at(&sections, 4, 0), None);
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        let sections = palette_sections(&fixture_recents());
        let all = filter_items(&sections, "");
        // 5 go-to + 2 switch + 1 add + 1 settings + 1 sync.
        assert_eq!(all.len(), 10);
        assert_eq!(filter_items(&sections, "   ").len(), 10);
    }

    #[test]
    fn filter_matches_labels_case_insensitively() {
        let sections = palette_sections(&fixture_recents());
        assert_eq!(
            filter_items(&sections, "TERMINAL"),
            vec![PaletteItem::Command(PaletteCommand::GoTerminal)]
        );
        assert_eq!(
            filter_items(&sections, "first repo"),
            vec![PaletteItem::SwitchRepository {
                key: "aaa-first".to_owned(),
                label: "First Repo".to_owned(),
            }]
        );
        assert_eq!(
            filter_items(&sections, "go to"),
            vec![
                PaletteItem::Command(PaletteCommand::GoAgent),
                PaletteItem::Command(PaletteCommand::GoEditor),
                PaletteItem::Command(PaletteCommand::GoTerminal),
                PaletteItem::Command(PaletteCommand::GoReview),
                PaletteItem::Command(PaletteCommand::GoHome),
            ]
        );
    }

    #[test]
    fn filter_matches_keywords_for_each_purpose() {
        let sections = palette_sections(&fixture_recents());
        // The key stays searchable even when the label shows a display name.
        assert!(
            filter_items(&sections, "aaa-first").contains(&PaletteItem::SwitchRepository {
                key: "aaa-first".to_owned(),
                label: "First Repo".to_owned(),
            })
        );
        assert!(
            filter_items(&sections, "repo")
                .contains(&PaletteItem::Command(PaletteCommand::AddRepository))
        );
        assert!(
            filter_items(&sections, "preferences")
                .contains(&PaletteItem::Command(PaletteCommand::OpenSettings))
        );
        assert!(
            filter_items(&sections, "backup")
                .contains(&PaletteItem::Command(PaletteCommand::SyncPortable))
        );
        assert!(
            filter_items(&sections, "opencode")
                .contains(&PaletteItem::Command(PaletteCommand::GoAgent))
        );
    }

    #[test]
    fn filter_with_no_match_returns_empty() {
        let sections = palette_sections(&fixture_recents());
        assert!(filter_items(&sections, "zzz-no-such-command").is_empty());
    }
}
