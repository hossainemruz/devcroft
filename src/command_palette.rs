//! The command bar model: searchable palette entries for navigation,
//! repository switching, settings, and portable-data sync.
//!
//! [`PaletteItem`] is deliberately UI-free so filtering and the
//! section/row mapping stay unit-testable without a window. `Workspace`
//! rebuilds the sections on every palette opening (recent repositories
//! change with use), renders one [`gpui_kit::component::command::CommandGroup`]
//! per section in order, and resolves a confirmed `IndexPath` against the
//! same model it rendered.

use std::path::{Path, PathBuf};

use crate::agent_sessions::{SessionKey, SessionSummary};
use crate::data::RecentRepository;

gpui_kit::actions!(
    devcroft,
    [
        ToggleActionsPalette,
        ToggleProjectsPalette,
        ToggleSessionsPalette,
        GoToAgent,
        GoToEditor,
        GoToTerminal,
        GoToReview,
        GoToResources,
        NewAgentSession
    ]
);

/// Which filtered view of the command bar is open. `cmd/ctrl-k` opens the
/// action commands (navigation, settings, sync); `cmd/ctrl-p` opens project
/// navigation (recent repositories plus adding one); `cmd-s` opens recent
/// sessions across repositories, most recently worked first. `Add
/// repository…` lives only in projects mode, so every command has a single
/// home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteMode {
    Actions,
    Projects,
    Sessions,
}

impl PaletteMode {
    pub(crate) fn placeholder(self) -> &'static str {
        match self {
            Self::Actions => "Type a command or search…",
            Self::Projects => "Type a project name…",
            Self::Sessions => "Type a session title or repository…",
        }
    }
}

/// Match the command-bar shortcuts from a raw keystroke: `cmd/ctrl-k` opens
/// actions, `cmd/ctrl-p` opens projects, `cmd-s` opens sessions. Pure over the
/// keystroke pieces (not `KeyDownEvent`) so every focus site forwards
/// identically and the mapping stays unit-testable without a window. `alt`
/// combinations never match, so option-modified typing (e.g. `µ` on macOS)
/// keeps reaching the terminal. Sessions are platform-only: `ctrl-s` is XOFF
/// flow control, so it must keep reaching terminal applications.
pub(crate) fn palette_mode_for_shortcut(
    key: &str,
    platform: bool,
    control: bool,
    alt: bool,
) -> Option<PaletteMode> {
    if alt || !(platform || control) {
        return None;
    }
    if key.eq_ignore_ascii_case("k") {
        Some(PaletteMode::Actions)
    } else if key.eq_ignore_ascii_case("p") {
        Some(PaletteMode::Projects)
    } else if key.eq_ignore_ascii_case("s") {
        platform.then_some(PaletteMode::Sessions)
    } else {
        None
    }
}

/// Match the go-to-agent shortcut (`cmd-a`, Super on Linux) from a raw
/// keystroke. Same shape as [`palette_mode_for_shortcut`] so every focus
/// site forwards identically. Deliberately platform-only with no `ctrl`
/// fallback: `ctrl-a` is readline beginning-of-line, so it must keep
/// reaching the pty. Case-insensitive like the letter shortcuts above.
pub(crate) fn is_go_to_agent_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key.eq_ignore_ascii_case("a")
}

/// Match the go-to-editor shortcut (`cmd-e`, Super on Linux) from a raw
/// keystroke. Same shape as [`is_go_to_agent_shortcut`]: platform-only with
/// no `ctrl` fallback because `ctrl-e` is readline end-of-line.
pub(crate) fn is_go_to_editor_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key.eq_ignore_ascii_case("e")
}

/// Match the go-to-terminal shortcut (`cmd-/`, Super on Linux) from a raw
/// keystroke. Same shape as [`palette_mode_for_shortcut`] so every focus
/// site forwards identically. Deliberately platform-only with no `ctrl`
/// fallback: `ctrl-/` is a working key inside terminal applications, so it
/// must keep reaching the pty. Both `/` and `?` match: shift+/ reports the
/// shifted glyph on most layouts, and the shortcut intentionally stays
/// shift-lenient just like the (case-insensitive) letter shortcuts above.
pub(crate) fn is_go_to_terminal_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key == "/" || key == "?"
}

/// Match the go-to-review shortcut (`cmd-d`, Super on Linux) from a raw
/// keystroke. Same shape as [`is_go_to_agent_shortcut`]: platform-only with
/// no `ctrl` fallback, because `ctrl-d` is end-of-file and must keep reaching
/// terminal applications.
pub(crate) fn is_go_to_review_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key.eq_ignore_ascii_case("d")
}

/// Match the go-to-resources shortcut (`cmd-t`, Super on Linux) from a raw
/// keystroke. Same shape as [`is_go_to_agent_shortcut`]: platform-only with
/// no `ctrl` fallback so the keystroke never collides with terminal input.
pub(crate) fn is_go_to_resources_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key.eq_ignore_ascii_case("t")
}

/// Match the new-session shortcut (`cmd-n`, Super on Linux) from a raw
/// keystroke. Same shape as [`is_go_to_agent_shortcut`]: platform-only with
/// no `ctrl` fallback, because `ctrl-n` is readline next-line and must keep
/// reaching terminal applications.
pub(crate) fn is_new_session_shortcut(key: &str, platform: bool, alt: bool) -> bool {
    if alt || !platform {
        return false;
    }
    key.eq_ignore_ascii_case("n")
}

/// Every static command the bar can run, in canonical order. Repository
/// switching is dynamic (one [`PaletteItem::SwitchRepository`] per recent
/// repository) and lives outside this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PaletteCommand {
    GoAgent,
    GoEditor,
    GoTerminal,
    GoReview,
    GoResources,
    GoHome,
    BrowseArtifacts,
    AddRepository,
    OpenSettings,
    SyncPortable,
}

impl PaletteCommand {
    /// Workspace tab jumps: they select a repository tab, so they only make
    /// sense inside a repository workspace — never on Home.
    pub(crate) fn is_workspace_tab(self) -> bool {
        matches!(
            self,
            Self::GoAgent | Self::GoEditor | Self::GoTerminal | Self::GoReview | Self::GoResources
        )
    }

    #[cfg(test)]
    pub(crate) const ALL: [Self; 10] = [
        Self::GoAgent,
        Self::GoEditor,
        Self::GoTerminal,
        Self::GoReview,
        Self::GoResources,
        Self::GoHome,
        Self::BrowseArtifacts,
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
            Self::GoResources => "Go to Resources",
            Self::GoHome => "Go to Home",
            Self::BrowseArtifacts => "Browse artifacts",
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
            Self::GoResources => &["tab", "resources", "artifacts"],
            Self::GoHome => &["tab", "home", "dashboard"],
            Self::BrowseArtifacts => &[
                "artifact",
                "artifacts",
                "rfc",
                "plan",
                "note",
                "browse",
                "read",
            ],
            Self::AddRepository => &["repo", "repository", "project", "add", "new", "checkout"],
            Self::OpenSettings => &["settings", "preferences", "config"],
            Self::SyncPortable => &["sync", "portable", "push", "pull", "backup"],
        }
    }
}

/// One rendered palette row: a static command, a switch target for a recent
/// repository, or a historical session. The key is the stable identity; the
/// label is display text (display name when set, otherwise the key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PaletteItem {
    Command(PaletteCommand),
    SwitchRepository {
        key: String,
        label: String,
    },
    /// A live checkout whose portable repository metadata is unavailable.
    /// This keeps a running agent reachable without inventing a persisted key.
    OpenCheckout {
        checkout_path: PathBuf,
        label: String,
    },
    /// A historical conversation across repositories. Selecting it switches
    /// to the owning checkout's workspace and opens that exact session.
    OpenSession {
        key: SessionKey,
        title: String,
        repository: String,
        agent: String,
        age: String,
    },
}

impl PaletteItem {
    pub(crate) fn switch_target(repository: &RecentRepository) -> Self {
        Self::SwitchRepository {
            key: repository.key.clone(),
            label: repository.label().to_owned(),
        }
    }

    pub(crate) fn open_checkout(checkout_path: &Path) -> Self {
        let label = checkout_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| checkout_path.display().to_string());
        Self::OpenCheckout {
            checkout_path: checkout_path.to_owned(),
            label,
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Command(command) => command.label(),
            Self::SwitchRepository { label, .. } | Self::OpenCheckout { label, .. } => label,
            Self::OpenSession { title, .. } => title,
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
            Self::OpenCheckout { checkout_path, .. } => vec![
                checkout_path.to_str().unwrap_or_default(),
                "repo",
                "repository",
                "project",
                "open",
            ],
            Self::OpenSession {
                repository, agent, ..
            } => vec![
                repository.as_str(),
                agent.as_str(),
                "session",
                "sessions",
                "conversation",
                "repo",
                "repository",
            ],
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

/// Build the sessions-mode rows from the catalog snapshot, preserving the
/// snapshot's recency order (most recently worked first). Sessions whose
/// checkout has no reachable repository are dropped: selecting them could
/// not complete a switch, so the model never offers a row confirmation
/// cannot resolve. `repository` resolves one checkout's display label.
pub(crate) fn session_items(
    sessions: &[SessionSummary],
    repository: impl Fn(&Path) -> Option<String>,
) -> Vec<PaletteItem> {
    sessions
        .iter()
        .filter_map(|session| {
            let repository = repository(&session.checkout)?;
            Some(PaletteItem::OpenSession {
                key: session.key.clone(),
                title: session.title.clone(),
                repository,
                agent: session.provider_label().to_owned(),
                age: session.age(),
            })
        })
        .collect()
}

/// One rendered palette section: a static heading with its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaletteSection {
    pub(crate) heading: &'static str,
    pub(crate) items: Vec<PaletteItem>,
}

/// Build the rendered sections: static command groups plus the recent
/// repositories (already ordered, most-recent first) ahead of the static
/// repository commands. Test-only: production renders per-mode through
/// [`palette_sections_for_mode`], and this full model pins the shared
/// section/item ordering the modes are built from.
#[cfg(test)]
pub(crate) fn palette_sections(recents: &[RecentRepository]) -> Vec<PaletteSection> {
    vec![
        PaletteSection {
            heading: "Go to",
            items: command_items(&GO_TO_COMMANDS),
        },
        PaletteSection {
            heading: "Repositories",
            items: repository_items(recents),
        },
        PaletteSection {
            heading: "Settings",
            items: command_items(&SETTINGS_COMMANDS),
        },
        PaletteSection {
            heading: "Sync",
            items: command_items(&SYNC_COMMANDS),
        },
    ]
}

/// Build the rendered sections for one [`PaletteMode`]: actions show the
/// go-to, settings, and sync groups (no repositories group at all);
/// projects show just the repositories group (switch targets plus the add
/// command); sessions show the prebuilt session rows in recency order. On
/// Home (`home_visible`) the workspace tab jumps are omitted — they select a
/// repository tab, so offering them there would silently change `active_tab`
/// behind the still-visible Home. Confirmations resolve against exactly this
/// model, same contract as [`palette_sections`].
pub(crate) fn palette_sections_for_mode(
    recents: &[RecentRepository],
    sessions: &[PaletteItem],
    mode: PaletteMode,
    home_visible: bool,
) -> Vec<PaletteSection> {
    match mode {
        PaletteMode::Actions => {
            let mut go_to = command_items(&GO_TO_COMMANDS);
            if home_visible {
                go_to.retain(|item| {
                    !matches!(item, PaletteItem::Command(command) if command.is_workspace_tab())
                });
            }
            vec![
                PaletteSection {
                    heading: "Go to",
                    items: go_to,
                },
                PaletteSection {
                    heading: "Settings",
                    items: command_items(&SETTINGS_COMMANDS),
                },
                PaletteSection {
                    heading: "Sync",
                    items: command_items(&SYNC_COMMANDS),
                },
            ]
        }
        PaletteMode::Projects => vec![PaletteSection {
            heading: "Repositories",
            items: repository_items(recents),
        }],
        PaletteMode::Sessions => vec![PaletteSection {
            heading: "Recent sessions",
            items: sessions.to_vec(),
        }],
    }
}

fn command_items(commands: &[PaletteCommand]) -> Vec<PaletteItem> {
    commands
        .iter()
        .map(|command| PaletteItem::Command(*command))
        .collect()
}

fn repository_items(recents: &[RecentRepository]) -> Vec<PaletteItem> {
    recents
        .iter()
        .map(PaletteItem::switch_target)
        .chain(command_items(&REPOSITORY_COMMANDS))
        .collect()
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

const GO_TO_COMMANDS: [PaletteCommand; 7] = [
    PaletteCommand::GoAgent,
    PaletteCommand::GoEditor,
    PaletteCommand::GoTerminal,
    PaletteCommand::GoReview,
    PaletteCommand::GoResources,
    PaletteCommand::GoHome,
    PaletteCommand::BrowseArtifacts,
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
                description: None,
                group: None,
                owner: None,
                name: None,
            },
            RecentRepository {
                key: "bbb-second".to_owned(),
                display_name: None,
                checkout_path: PathBuf::from("/tmp/second"),
                last_opened_at: Some("2024-01-01T00:00:00Z".to_owned()),
                description: None,
                group: None,
                owner: None,
                name: None,
            },
        ]
    }

    fn fixture_session(id: &str, checkout: &str, title: &str, updated: i64) -> SessionSummary {
        SessionSummary {
            key: SessionKey {
                provider: "opencode".to_owned(),
                store: PathBuf::from("/store"),
                id: id.to_owned(),
            },
            cwd: PathBuf::from(checkout),
            checkout: PathBuf::from(checkout),
            title: title.to_owned(),
            updated,
            timestamp_source: "test".to_owned(),
        }
    }

    #[test]
    fn sections_cover_every_command_exactly_once() {
        let commands: Vec<PaletteCommand> = palette_sections(&[])
            .into_iter()
            .flat_map(|section| section.items)
            .filter_map(|item| match item {
                PaletteItem::Command(command) => Some(command),
                PaletteItem::SwitchRepository { .. }
                | PaletteItem::OpenCheckout { .. }
                | PaletteItem::OpenSession { .. } => None,
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
    fn actions_mode_holds_every_command_but_add_repository() {
        let sections =
            palette_sections_for_mode(&fixture_recents(), &[], PaletteMode::Actions, false);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Go to");
        assert_eq!(sections[1].heading, "Settings");
        assert_eq!(sections[2].heading, "Sync");
        let commands: Vec<PaletteCommand> = sections
            .iter()
            .flat_map(|section| section.items.iter())
            .filter_map(|item| match item {
                PaletteItem::Command(command) => Some(*command),
                PaletteItem::SwitchRepository { .. }
                | PaletteItem::OpenCheckout { .. }
                | PaletteItem::OpenSession { .. } => None,
            })
            .collect();
        // Every static command except `AddRepository`, which lives only in
        // projects mode — and no switch targets even with recents present.
        let expected: Vec<PaletteCommand> = PaletteCommand::ALL
            .into_iter()
            .filter(|command| *command != PaletteCommand::AddRepository)
            .collect();
        assert_eq!(commands.len(), expected.len());
        for command in expected {
            assert!(
                commands.contains(&command),
                "{command:?} missing in actions mode"
            );
        }
        assert!(
            !commands.contains(&PaletteCommand::AddRepository),
            "AddRepository must live only in projects mode"
        );
    }

    #[test]
    fn home_hides_workspace_tab_jumps_but_keeps_home_destinations() {
        let sections =
            palette_sections_for_mode(&fixture_recents(), &[], PaletteMode::Actions, true);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Go to");
        let commands: Vec<PaletteCommand> = sections[0]
            .items
            .iter()
            .filter_map(|item| match item {
                PaletteItem::Command(command) => Some(*command),
                PaletteItem::SwitchRepository { .. }
                | PaletteItem::OpenCheckout { .. }
                | PaletteItem::OpenSession { .. } => None,
            })
            .collect();
        // Tab jumps make no sense on Home: selecting one would flip
        // `active_tab` behind the still-visible dashboard.
        for tab in [
            PaletteCommand::GoAgent,
            PaletteCommand::GoEditor,
            PaletteCommand::GoTerminal,
            PaletteCommand::GoReview,
            PaletteCommand::GoResources,
        ] {
            assert!(
                !commands.contains(&tab),
                "{tab:?} must stay out of Home's command bar"
            );
            assert!(tab.is_workspace_tab());
        }
        // Home destinations and global commands stay reachable everywhere.
        assert_eq!(
            commands,
            vec![
                PaletteCommand::GoHome,
                PaletteCommand::BrowseArtifacts,
            ]
        );
        // Projects mode is page-agnostic: switching checkouts is how you
        // leave Home, so recents stay put there too.
        let sections =
            palette_sections_for_mode(&fixture_recents(), &[], PaletteMode::Projects, true);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Repositories");
        assert_eq!(sections[0].items.len(), 3);
    }

    #[test]
    fn projects_mode_holds_only_the_repositories_group() {
        let sections =
            palette_sections_for_mode(&fixture_recents(), &[], PaletteMode::Projects, false);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Repositories");
        assert_eq!(
            sections[0].items,
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
    }

    #[test]
    fn sessions_mode_holds_only_session_rows_in_recency_order() {
        let sessions = vec![
            fixture_session("newer", "/repos/api", "Fix palette", 300),
            fixture_session("older", "/repos/web", "Add sessions", 60),
        ];
        let rows = session_items(&sessions, |checkout| {
            Some(format!("label:{}", checkout.display()))
        });
        let sections =
            palette_sections_for_mode(&fixture_recents(), &rows, PaletteMode::Sessions, true);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Recent sessions");
        // Recency order comes straight from the catalog snapshot; the mode
        // must not reorder or append anything.
        assert_eq!(sections[0].items, rows);
        assert_eq!(sections[0].items[0].label(), "Fix palette");
        assert_eq!(sections[0].items[1].label(), "Add sessions");
    }

    #[test]
    fn session_items_drop_unreachable_checkouts_and_stay_searchable() {
        let sessions = vec![
            fixture_session("known", "/repos/api", "Fix palette", 300),
            fixture_session("orphan", "/gone", "Unreachable", 200),
            fixture_session("also-known", "/repos/web", "Add sessions", 60),
        ];
        let rows = session_items(&sessions, |checkout| {
            (checkout != Path::new("/gone")).then(|| checkout.display().to_string())
        });
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label(), "Fix palette");
        assert_eq!(rows[1].label(), "Add sessions");
        // The owning repository and the harness both stay searchable even
        // though the row shows the session title.
        assert!(rows[0].matches("fix palette"));
        assert!(rows[0].matches("api"));
        assert!(rows[0].matches("opencode"));
        assert!(rows[0].matches("session"));
        assert!(!rows[0].matches("unreachable"));
    }

    #[test]
    fn shortcut_matcher_routes_k_p_and_s_to_their_modes() {
        use PaletteMode::{Actions, Projects, Sessions};

        assert_eq!(
            palette_mode_for_shortcut("k", true, false, false),
            Some(Actions)
        );
        assert_eq!(
            palette_mode_for_shortcut("K", false, true, false),
            Some(Actions)
        );
        assert_eq!(
            palette_mode_for_shortcut("p", true, false, false),
            Some(Projects)
        );
        assert_eq!(
            palette_mode_for_shortcut("P", false, true, false),
            Some(Projects)
        );
        assert_eq!(
            palette_mode_for_shortcut("s", true, false, false),
            Some(Sessions)
        );
        assert_eq!(
            palette_mode_for_shortcut("S", true, false, false),
            Some(Sessions)
        );
        // No `ctrl` fallback for sessions: `ctrl-s` is XOFF flow control and
        // must keep reaching terminal applications.
        assert_eq!(palette_mode_for_shortcut("s", false, true, false), None);
        assert_eq!(palette_mode_for_shortcut("S", false, true, false), None);
        // No modifier, alt held, or any other key never toggles the bar.
        assert_eq!(palette_mode_for_shortcut("k", false, false, false), None);
        assert_eq!(palette_mode_for_shortcut("p", false, false, false), None);
        assert_eq!(palette_mode_for_shortcut("s", false, false, false), None);
        assert_eq!(palette_mode_for_shortcut("k", true, false, true), None);
        assert_eq!(palette_mode_for_shortcut("p", true, true, true), None);
        assert_eq!(palette_mode_for_shortcut("s", true, false, true), None);
        assert_eq!(palette_mode_for_shortcut("o", true, false, false), None);
        assert_eq!(palette_mode_for_shortcut("Enter", true, false, false), None);
    }

    #[test]
    fn agent_shortcut_matches_a_with_platform_modifier_only() {
        assert!(is_go_to_agent_shortcut("a", true, false));
        assert!(is_go_to_agent_shortcut("A", true, false));
        // No `ctrl` fallback: `ctrl-a` is readline beginning-of-line and
        // must keep reaching the terminal.
        assert!(!is_go_to_agent_shortcut("a", false, false));
        assert!(!is_go_to_agent_shortcut("A", false, false));
        // Alt held, or any other key, never jumps to agent.
        assert!(!is_go_to_agent_shortcut("a", true, true));
        assert!(!is_go_to_agent_shortcut("e", true, false));
        assert!(!is_go_to_agent_shortcut("k", true, false));
        assert!(!is_go_to_agent_shortcut("Enter", true, false));
    }

    #[test]
    fn editor_shortcut_matches_e_with_platform_modifier_only() {
        assert!(is_go_to_editor_shortcut("e", true, false));
        assert!(is_go_to_editor_shortcut("E", true, false));
        // No `ctrl` fallback: `ctrl-e` is readline end-of-line and must
        // keep reaching the terminal.
        assert!(!is_go_to_editor_shortcut("e", false, false));
        assert!(!is_go_to_editor_shortcut("E", false, false));
        // Alt held, or any other key, never jumps to editor.
        assert!(!is_go_to_editor_shortcut("e", true, true));
        assert!(!is_go_to_editor_shortcut("a", true, false));
        assert!(!is_go_to_editor_shortcut("k", true, false));
        assert!(!is_go_to_editor_shortcut("Enter", true, false));
    }

    #[test]
    fn terminal_shortcut_matches_slash_with_platform_modifier_only() {
        assert!(is_go_to_terminal_shortcut("/", true, false));
        // Shift+/ reports the shifted glyph; the shortcut stays shift-lenient.
        assert!(is_go_to_terminal_shortcut("?", true, false));
        // No `ctrl` fallback: `ctrl-/` must keep reaching the terminal.
        assert!(!is_go_to_terminal_shortcut("/", false, false));
        assert!(!is_go_to_terminal_shortcut("?", false, false));
        // Alt held, or any other key, never jumps to terminal.
        assert!(!is_go_to_terminal_shortcut("/", true, true));
        assert!(!is_go_to_terminal_shortcut("k", true, false));
        assert!(!is_go_to_terminal_shortcut("p", true, false));
        assert!(!is_go_to_terminal_shortcut("Enter", true, false));
    }

    #[test]
    fn review_shortcut_matches_d_with_platform_modifier_only() {
        assert!(is_go_to_review_shortcut("d", true, false));
        assert!(is_go_to_review_shortcut("D", true, false));
        // No `ctrl` fallback: `ctrl-d` is end-of-file and must keep reaching
        // the terminal.
        assert!(!is_go_to_review_shortcut("d", false, false));
        assert!(!is_go_to_review_shortcut("D", false, false));
        // Alt held, or any other key, never jumps to review.
        assert!(!is_go_to_review_shortcut("d", true, true));
        assert!(!is_go_to_review_shortcut("r", true, false));
        assert!(!is_go_to_review_shortcut("t", true, false));
        assert!(!is_go_to_review_shortcut("k", true, false));
        assert!(!is_go_to_review_shortcut("Enter", true, false));
    }

    #[test]
    fn resources_shortcut_matches_t_with_platform_modifier_only() {
        assert!(is_go_to_resources_shortcut("t", true, false));
        assert!(is_go_to_resources_shortcut("T", true, false));
        // No `ctrl` fallback: the keystroke must keep reaching the terminal.
        assert!(!is_go_to_resources_shortcut("t", false, false));
        assert!(!is_go_to_resources_shortcut("T", false, false));
        // Alt held, or any other key, never jumps to Resources.
        assert!(!is_go_to_resources_shortcut("t", true, true));
        assert!(!is_go_to_resources_shortcut("d", true, false));
        assert!(!is_go_to_resources_shortcut("k", true, false));
        assert!(!is_go_to_resources_shortcut("Enter", true, false));
    }

    #[test]
    fn new_session_shortcut_matches_n_with_platform_modifier_only() {
        assert!(is_new_session_shortcut("n", true, false));
        assert!(is_new_session_shortcut("N", true, false));
        // No `ctrl` fallback: `ctrl-n` is readline next-line and must keep
        // reaching the terminal.
        assert!(!is_new_session_shortcut("n", false, false));
        assert!(!is_new_session_shortcut("N", false, false));
        // Alt held, or any other key, never starts a session.
        assert!(!is_new_session_shortcut("n", true, true));
        assert!(!is_new_session_shortcut("t", true, false));
        assert!(!is_new_session_shortcut("k", true, false));
        assert!(!is_new_session_shortcut("Enter", true, false));
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
            Some(PaletteItem::Command(PaletteCommand::GoResources))
        );
        assert_eq!(
            item_at(&sections, 0, 5),
            Some(PaletteItem::Command(PaletteCommand::GoHome))
        );
        assert_eq!(
            item_at(&sections, 0, 6),
            Some(PaletteItem::Command(PaletteCommand::BrowseArtifacts))
        );
        assert_eq!(
            item_at(&sections, 0, 7), None
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
        assert_eq!(item_at(&sections, 0, 8), None);
        assert_eq!(item_at(&sections, 4, 0), None);
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        let sections = palette_sections(&fixture_recents());
        let all = filter_items(&sections, "");
        // 7 go-to + 2 switch + 1 add + 1 settings + 1 sync.
        assert_eq!(all.len(), 12);
        assert_eq!(filter_items(&sections, "   ").len(), 12);
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
                PaletteItem::Command(PaletteCommand::GoResources),
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
        assert!(
            filter_items(&sections, "browse artifacts")
                .contains(&PaletteItem::Command(PaletteCommand::BrowseArtifacts))
        );
        assert_eq!(
            filter_items(&sections, "BROWSE ARTIFACTS"),
            vec![PaletteItem::Command(PaletteCommand::BrowseArtifacts)]
        );
    }

    #[test]
    fn filter_with_no_match_returns_empty() {
        let sections = palette_sections(&fixture_recents());
        assert!(filter_items(&sections, "zzz-no-such-command").is_empty());
    }
}
