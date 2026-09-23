# Devcroft

Devcroft is a small GPUI desktop workspace with three persistent, Ghostty-powered terminal tabs:

- **Agent** resumes the checkout's most recent session when history exists, otherwise it launches your default agent (`opencode` unless changed in Settings → Agent) in your default shell. Use **New session…** in the Agent sidebar to pick a harness (`opencode`, `claude`, `codex`, or `omp`) explicitly for a new session.
- **Editor** launches `nvim .` in your default shell.
- **Terminal** launches your default login shell.

Each tab owns an independent PTY session. Switching tabs keeps the underlying process alive. Devcroft starts the default shell as an interactive login shell in every tab, then enters the Agent or Editor command through that shell. This matches a normal terminal launch and makes shell startup files, environment changes, aliases, functions, and tool-manager activation available to the agent harness and `nvim`.

The terminal UI bundles JetBrains Mono NL Nerd Font Mono v3.5.1 (regular, bold, italic, and bold italic), so Nerd Font symbols work without a separate system font installation. Attribution and license files are in [`assets/`](assets/).

## Home

Devcroft opens on Home without starting terminal processes. Open a recent project or use the shared command bar (`Cmd+K` for actions, `Cmd+P` for projects on macOS; `Ctrl+K` / `Ctrl+P` on Linux/Windows) to enter a repository workspace. The **Home** button and **Go Home** command return to the dashboard without stopping existing sessions.

- **Recent Projects:** up to four linked repositories, ordered by last opened time. Each card shows its checkout's branch (or detached commit), clean/modified state, and available ahead/behind counts. Status refreshes in the background every five seconds while Home is active; this reads local Git state and does not fetch remotes. Click anywhere on a card to open its workspace; **Add project** registers another checkout.
- **Repository Relationships:** open from Home, Projects, or the action palette. Connect providers to consumers on a native canvas, edit repository purposes/groups, and query dependencies and dependents from the CLI. Isolated and unlinked repositories are included. Definitions sync through portable Git; per-group positions and viewports stay on this device.
- **Resources:** repository Markdown artifacts with originating sessions, editing, and agent-accessible comments.
- **Pull Requests:** track a GitHub PR by URL and choose a **Personal** or **Work** group. Titles are fetched automatically from GitHub; no title input is needed. The URL is shown until the first successful fetch. Cards show approval status, CI passed/failed (or pending/no checks), and open/draft/merged/closed state using your existing `gh auth login`. Status refreshes in the background every minute while Home or the PR board is active; **Refresh status** retries immediately. Failed fetches retain the last result with a stale indicator. **View all** opens a kanban board with **Waiting for Approval**, **To Review**, and **Watching** columns and All/Personal/Work filtering. Drag cards between columns or use their menu to move them. Edit and remove tracked entries locally; **Open** launches GitHub in your browser. Existing entries default to Personal and retain their column.
- **Todos:** add/edit a title, optional description, **Personal** or **Work** group, and an optional project. `http(s)` links in the description render clickable and open in a browser. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit**, **Delete**, and keyboard-accessible **Move up / Move down**. Cards show their group and project badges. Drag a card onto another incomplete todo to reorder it. The inbox filters by group and project. Enable **Show completed** to restore completed items. **View all** opens a kanban board with one column per project plus **Unscoped** for todos with no project (removed projects keep their own column so scoped todos are never hidden); drag cards between columns to re-scope them. The board shares the group filter and respects **Show completed**.
- **To Read:** save an HTTP(S) link with an optional manual title, open it in a browser, mark it read, or delete it. No metadata is fetched. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit** and **Delete**. Enable **Show completed** to restore read items. **View all** opens a dedicated page with the full list.

Home lists and todo ordering are saved in `portable/dashboard.json` and participate in the existing portable Git sync. The header displays the portable directory's Git status, not the active project's status. Lists reload on returning Home, after adding a repository, and after sync/branch changes. Stale snapshots and malformed JSON are rejected rather than silently overwritten. The Pull Requests, Todos, To Read, and Projects **View all** destinations are implemented. Fetched PR statuses stay in memory and do not create portable-sync changes. GitHub CLI must be installed and authenticated with access to the tracked repositories; Devcroft finds `gh` via your PATH, common install locations (mise shims, `~/.local/bin`, Homebrew), or your login shell, so a terminal-only install still works for Finder-launched apps. GitHub approval rules take precedence over individual reviews; CI combines check runs and commit statuses, treating cancelled/timed-out checks as failed and neutral/skipped checks as successful. Merged and closed PRs remain in their assigned column until removed manually.

### Keyboard navigation

With navigation mode open, **Tab / Shift+Tab** move through the visible controls, including the command bar and whole project cards, and **Enter** leaves the mode on the focused control, where **Enter / Space** activates buttons or toggles checkboxes. Focused project cards support **arrow keys** (following the current grid) and **Home / End** (first/last project). **Page Up / Page Down** scroll the dashboard. Existing command-palette shortcuts remain available, and dialogs retain gpui-kit's keyboard focus handling.

Press **Cmd+J** on macOS (**Ctrl+J** on Linux/Windows) anywhere in the workspace to enter navigation mode: nothing edits while it owns the keyboard, and the keyboard shortcut badge right after the command bar becomes an amber `Navigation` indicator. The bottom-right list shows the keys available at the current location: tab and creation commands in a repository, back navigation on the Artifacts page, and edit/comment/save/cancel commands when a resource selection or draft allows them. Palettes stay on their direct shortcuts (`Cmd+K`, `Cmd+P` on macOS; `Ctrl+K`, `Ctrl+P` on Linux/Windows), which also work inside navigation mode. An action key runs once and returns to normal mode; **Tab / Shift+Tab** move through the visible components and keep the mode open; `h`/`l` move between visible panes and keep the mode open; `j`/`k` move within the focused list (sessions sidebar, Home cards in visual order, resource lists, review files, review diff scroll) and keep the mode open; `Enter` opens the highlighted session or Home card, otherwise it keeps the focused pane; `Escape`, the toggle again, or a click exits without acting. The only direct shortcuts in normal mode are the palettes: `Cmd+K` actions, `Cmd+P` projects on macOS (`Ctrl+K` / `Ctrl+P` on Linux/Windows); normal mode leaves **Tab** to the focused component, so terminals and the agent harness keep it (opencode switches agents/models/modes with it). See the [keyboard reference](docs/keyboard-reference.md).

## Repository resources

Use the Resources tab to browse plans, RFCs, notes, and reviews for the current repository. Edit Markdown, comment on documents, and reopen originating agent sessions. Plans can track implementation phases using checkboxes. Reviews record agent review findings.

```sh
devcroft repository list --json
devcroft artifact create --repository KEY --title "Implementation plan" --kind plan --content-file plan.md --json
devcroft artifact list --repository KEY --json
devcroft artifact comment list ART_ID --json
```

The CLI works without a running desktop. Records use the selected `DEVCROFT_DATA_DIR` or the normal per-OS root. See [Resources](docs/resources.md) and the [agent instructions](assets/skills/devcroft/references/resources.md).

### Repository relationship graph

`api → backend` means **backend depends on api**. Drag from a provider's bottom
handle to a consumer's top handle, describe the connection, then Save. Select a
node to edit its purpose/group, or an edge/label to edit endpoints, description,
or delete it. Selected edge endpoints can also be rewired by dragging their
handles; changes remain drafts until Save. Cycles are supported.

Drag the background to pan, scroll to zoom about the pointer, and use **Fit view**
or **Auto arrange**. In navigation mode, **Tab / Shift+Tab** move focus to nodes
or edge labels, then **Enter** selects the focused one; **Add relationship**
provides a keyboard form. **Escape** cancels the active gesture or dismisses the
draft. Groups change the view only; a node's
inspector lists connections across groups. Choose a group using the radio buttons;
the last selected group and its layout are remembered. **Auto arrange** puts
unconnected repositories in a compact grid. Click **Open repository** to enter
a linked checkout explicitly.

```sh
devcroft repository relationships backend --json
devcroft repository relationships backend --depth 2 --json
devcroft repository relationship create --from api --to backend --description 'backend implements api contracts' --revision GRAPH_REV --json
devcroft repository get backend --json
devcroft repository update backend --description 'Service implementation' --revision METADATA_REV --json
```

Read before editing and use the returned revision. Stale saves preserve UI drafts;
review the latest values before retrying. See the [agent relationship reference](assets/skills/devcroft/references/relationships.md)
for limits, group exclusions, unresolved repositories, and the full command set.

## Code review

The Review tab syntax-highlights recognized source files and supports persistent
line and range comments. Click a line or
Shift-click a range, then save your feedback. Use the sidebar to navigate, edit,
resolve, reopen, or delete comments. Agents can use `devcroft review list --open`,
`devcroft review resolve ID`, and `devcroft review delete ID` without the desktop.
Comments persist locally per branch pair and scope; changed code is re-anchored
when possible, with removed or ambiguous ranges marked outdated. See the
[review workflow and CLI](docs/review-comments.md).

## Tools

Developer tools live in the action palette (`Cmd+K` on macOS, `Ctrl+K` on Linux/Windows) under **Tools**. Each tool opens a dialog with its own inputs.

- **Format JSON:** paste JSON into the syntax-highlighted editor, then **Format** to pretty-print it with two-space indentation. The formatted document replaces the input in place, so what stays in the dialog — and what is saved — is already laid out; one undo restores the text as you pasted it. Formatting lays the document out instead of rebuilding it, so key order, duplicate keys, number literals, and string escapes stay exactly as written, and invalid input reports the line and column while leaving the text untouched. **Copy** puts the current text on the clipboard. **Clear** empties the input and the saved copy.

Tool inputs are machine-local: they autosave as you type, flush when the dialog closes, and reload the next time you open the tool. They live under `tools/<tool>/` in the data root (`~/.local/share/devcroft` by default), never inside `portable/`, so tool scratch content does not participate in portable sync.

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
