# Devcroft

Devcroft is a small GPUI desktop workspace with three persistent, Ghostty-powered terminal tabs:

- **Agent** launches the workspace's default agent (`opencode` unless changed to `claude`, `codex`, or `omp` in the workspace settings sheet) in your default shell. Use **New session…** in the Agent sidebar to pick a harness for a new session without changing the saved default.
- **Editor** launches `nvim .` in your default shell.
- **Terminal** launches your default login shell.

Each tab owns an independent PTY session. Switching tabs keeps the underlying process alive. Devcroft starts the default shell as an interactive login shell in every tab, then enters the Agent or Editor command through that shell. This matches a normal terminal launch and makes shell startup files, environment changes, aliases, functions, and tool-manager activation available to the agent harness and `nvim`.

The terminal UI bundles JetBrains Mono NL Nerd Font Mono v3.5.1 (regular, bold, italic, and bold italic), so Nerd Font symbols work without a separate system font installation. Attribution and license files are in [`assets/`](assets/).

## Home

Devcroft opens on Home without starting terminal processes. Open a recent project or use the shared command bar (`Cmd/Ctrl+K` for actions, `Cmd/Ctrl+P` for projects) to enter a repository workspace. The **Home** button and **Go Home** command return to the dashboard without stopping existing sessions.

- **Recent Projects:** up to four linked repositories, ordered by last opened time. Each card shows its checkout's branch (or detached commit), clean/modified state, and available ahead/behind counts. Status refreshes in the background every five seconds while Home is active; this reads local Git state and does not fetch remotes. Click anywhere on a card to open its workspace; **Add project** registers another checkout.
- **Resources:** repository Markdown artifacts with originating sessions, editing, and agent-accessible comments.
- **Pull Requests:** track a GitHub PR by URL and choose a **Personal** or **Work** group. Titles are fetched automatically from GitHub; no title input is needed. The URL is shown until the first successful fetch. Cards show approval status, CI passed/failed (or pending/no checks), and open/draft/merged/closed state using your existing `gh auth login`. Status refreshes in the background every minute while Home or the PR board is active; **Refresh status** retries immediately. Failed fetches retain the last result with a stale indicator. **View all** opens a kanban board with **Waiting for Approval**, **To Review**, and **Watching** columns and All/Personal/Work filtering. Drag cards between columns or use their menu to move them. Edit and remove tracked entries locally; **Open** launches GitHub in your browser. Existing entries default to Personal and retain their column.
- **Todos:** add/edit a title, optional description, and optional label. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit**, **Delete**, and keyboard-accessible **Move up / Move down**. Drag a card onto another incomplete todo to reorder it. Enable **Show completed** to restore completed items.
- **To Read:** save an HTTP(S) link with an optional manual title, open it in a browser, mark it read, or delete it. No metadata is fetched.

Home lists and todo ordering are saved in `portable/dashboard.json` and participate in the existing portable Git sync. The header displays the portable directory's Git status, not the active project's status. Lists reload on returning Home, after adding a repository, and after sync/branch changes. Stale snapshots and malformed JSON are rejected rather than silently overwritten. The Pull Requests and Projects **View all** destinations are implemented; Todos and To Read destinations remain placeholders. Fetched PR statuses stay in memory and do not create portable-sync changes. GitHub CLI must be installed on the desktop app's PATH and authenticated with access to the tracked repositories. GitHub approval rules take precedence over individual reviews; CI combines check runs and commit statuses, treating cancelled/timed-out checks as failed and neutral/skipped checks as successful. Merged and closed PRs remain in their assigned column until removed manually.

### Keyboard navigation

Use **Tab / Shift+Tab** to move through Home's controls, including the command bar and whole project cards, and **Enter / Space** to activate buttons or toggle checkboxes. Focused project cards support **arrow keys** (following the current grid) and **Home / End** (first/last project). **Page Up / Page Down** scroll the dashboard. Existing command-palette shortcuts remain available, and dialogs retain gpui-kit's keyboard focus handling.

Press **Cmd+M** on macOS (**Super+M** on Linux) anywhere in the workspace to enter navigation mode: nothing edits while it owns the keyboard, and a `NAVIGATION` pill appears right after the command bar. The bottom-right list shows the keys available at the current location: tab and creation commands in a repository, back navigation on the Artifacts page, and edit/comment/save/cancel commands when a resource selection or draft allows them. Palettes stay on their direct shortcuts (`Cmd/Ctrl+K`, `Cmd/Ctrl+P`), which also work inside navigation mode. An action key runs once and returns to normal mode; `h`/`l` move between visible panes and keep the mode open; `j`/`k` move within the focused list (sessions sidebar, Home cards in visual order, resource lists, review files, review diff scroll) and keep the mode open; `Enter` opens the highlighted session or Home card, otherwise it keeps the focused pane; `Escape`, the toggle again, or a click exits without acting. The only direct shortcuts in normal mode are the palettes: `Cmd/Ctrl+K` actions, `Cmd/Ctrl+P` projects. See the [keyboard reference](docs/keyboard-reference.md).

## Repository resources

Use the Resources tab to browse plans, RFCs, and notes for the current repository. Edit Markdown, comment on documents, and reopen originating agent sessions. Plans can track implementation phases using checkboxes.

```sh
devcroft repository list --json
devcroft artifact create --repository KEY --title "Implementation plan" --kind plan --content-file plan.md --json
devcroft artifact list --repository KEY --json
devcroft artifact comment list ART_ID --json
```

The CLI works without a running desktop. Records use the selected `DEVCROFT_DATA_DIR` or the normal per-OS root. See [Resources](docs/resources.md) and the [agent instructions](assets/skills/devcroft/references/resources.md).

## Code review

The Review tab syntax-highlights recognized source files and supports persistent
line and range comments. Click a line or
Shift-click a range, then save your feedback. Use the sidebar to navigate, edit,
resolve, reopen, or delete comments. Agents can use `devcroft review list --open`,
`devcroft review resolve ID`, and `devcroft review delete ID` without the desktop.
Comments persist locally per branch pair and scope; changed code is re-anchored
when possible, with removed or ambiguous ranges marked outdated. See the
[review workflow and CLI](docs/review-comments.md).

## Requirements

[mise](https://mise.jdx.dev/) manages the required Rust and Zig toolchains. The application expects `nvim` and at least one agent harness (`opencode`, `claude`, `codex`, or `omp`) to already be available in the environment inherited by your default shell.

```sh
mise install
mise run build
mise run app
```

Install as the **Devcroft** desktop app (replaces the old Electron-based install):

```sh
mise run install          # release build; use --debug for a faster debug install
```

On Linux this installs `~/.local/bin/devcroft`, the `devcroft.desktop` launcher with the icon from `assets/icons/logo.png`, and removes the previous `Devcroft.desktop` and legacy `com.devcroft.desktop.desktop` entries plus leftover Electron bundle files. The lowercase launcher filename matches the window app ID so Wayland taskbars can find its icon. Your data in `~/.local/share/devcroft` (`device.json`, `portable/`, caches) is kept. On macOS it (re)creates `/Applications/Devcroft.app` (`~/Applications` fallback when `/Applications` is not writable) with an `AppIcon.icns` generated from the same logo.

Useful development tasks:

```sh
mise run fmt
mise run check
mise run test
```

The initial terminal renderer supports ANSI color, keyboard input, bracketed paste, focus, resizing, scrollback, and scroll-wheel reporting to full-screen applications. Keyboard input automatically returns a scrolled terminal viewport to the active prompt. It shapes each terminal row as one GPUI text layout with Ghostty styles applied as highlights, avoiding both per-cell elements and clipping at style boundaries. Image protocols and general mouse click/drag reporting are intentionally left for a later version. The Ghostty binding is pinned to a reviewed upstream revision so builds remain reproducible.

## Agent skill

In **Settings → Agent → Devcroft skill**, install or update the bundled CLI
skill for Claude Code and/or Codex/shared agents. OpenCode also discovers these
locations. The skill explains repository artifacts and artifact/review
comments, with examples; all operations use the existing CLI.

The same installer works without the desktop:

```sh
devcroft skill install
devcroft skill status
devcroft skill install --target claude
devcroft skill uninstall --target agents
```

Without `--target`, commands process both `~/.claude/skills/devcroft` and
`~/.agents/skills/devcroft`. Install also updates an older managed bundle.
Status reports destination paths, bundle status, CLI availability on the current
process's PATH, and the selected data root. External agent shells may have a
different environment: ensure `devcroft` is available there and use the same
absolute `DEVCROFT_DATA_DIR` as the desktop when overriding the default.
Restart the agent if the skill does not appear.

Modified or unmanaged skill folders are preserved; move your custom copy aside
before reinstalling. Results are reported separately for both destinations; the
CLI exits nonzero if either fails. Remove only deletes an unmodified managed
bundle. Installation does not edit project instruction files or agent settings.

The maintained bundle lives in `assets/skills/devcroft/` and is embedded in the
binary, so installation requires neither network access nor a source checkout.
