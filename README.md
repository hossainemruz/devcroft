# Devcroft

Devcroft is a small GPUI desktop workspace with Agent, Editor, Terminal, Review, and Resources tabs:

- **Agent** resumes the checkout's most recent session when history exists, otherwise it launches your default agent (`opencode` unless changed in Settings → Agent) in your default shell. Use **New session…** in the Agent sidebar to pick a harness (`opencode`, `claude`, `codex`, or `omp`) explicitly for a new session.
- **Editor** offers Neovim or Devcroft's built-in editor in **Settings → Editor**. New installations default to the built-in editor; existing installations retain their saved choice, or Neovim when unset. The built-in editor has file tabs, a checkout tree, fuzzy file opening, project text search, syntax highlighting, find/replace, and explicit save; it is intended for small edits, not a full IDE.
- **Terminal** launches your default login shell.

Terminal-backed tabs own independent PTY sessions. Switching tabs keeps the underlying process alive. Devcroft starts the default shell as an interactive login shell, then enters the Agent or Neovim command through that shell. This makes shell startup files, environment changes, aliases, functions, and tool-manager activation available to those commands.

You can select the built-in editor from Settings on Home before opening your first project; Neovim is not required. In the Editor tab, the project sidebar shows checkout files and its search icon opens the fuzzy finder. The **…** editor actions menu contains **Search project**, **Go to line**, **Find in file**, **Replace in file**, and save commands. `Cmd+J`, then `o` (`Ctrl+J`, then `o` on Linux/Windows) opens the file finder from a workspace. The finder accepts `↑`/`↓` and `Enter`. **Find in file** and **Replace in file** use the editor's in-file search. **Save** or `Cmd+S` (`Ctrl+S` elsewhere) saves the active tab. Tabs retain their buffers and undo history; closing a dirty tab offers Save, Discard, and Cancel.

Clean buffers reload after external edits. A dirty buffer stays open when its file changes; **Compare** and **Reload from disk** appear with the conflict; **Save as…** is available in the **…** menu. Save checks the disk again before replacing it. Unsaved buffers are kept in local recovery journals under Devcroft's data directory and restored as dirty tabs after a restart, without changing source files. LF and CRLF styles are preserved on save, and existing file permissions are retained. Deleted files cannot be silently recreated; renamed or changed files require review before saving. Binary, non-UTF-8, and files over 2 MiB need an external editor. The checkout index and search are bounded; use **Refresh files** after adding files. In Review, **Open ↗** and new-file line numbers open the current checkout file in the built-in editor. The built-in editor's **… → Open in Zed / VS Code** actions launch the checkout separately from the editor preference. Enable installed Zed or VS Code launchers in Settings → Editor. Paths are passed as process arguments. Changing editor choice keeps existing Neovim sessions and built-in drafts alive.

The terminal UI bundles JetBrains Mono NL Nerd Font Mono v3.5.1 (regular, bold, italic, and bold italic), so Nerd Font symbols work without a separate system font installation. Attribution and license files are in [`assets/`](assets/).

Rust semantic features are opt-in per checkout through **… → Trust checkout and enable Rust tooling**. See [editor setup and everyday use](docs/editor.md) for editor choice, shortcuts, external launchers, and file limits, and the [language/server matrix and trust model](docs/language-support.md) for Rust prerequisites and limitations.

## Home

Devcroft opens on Home without starting terminal processes. Open a recent project or use the shared command bar (`Cmd+K` for actions, `Cmd+P` for projects on macOS; `Ctrl+K` / `Ctrl+P` on Linux/Windows) to enter a repository workspace. The **Home** button and **Go Home** command return to the dashboard without stopping existing sessions.

- **Spaces:** isolation profiles that partition your data — repositories, Home items (PRs, todos, reading), and artifacts. The Home titlebar shows a switcher for the active space (or press `s` in navigation mode), the app resumes the last space per device, and the projects palette only lists repositories in the active space. Agent sessions follow the space of the repository they ran in. Spaces live in `portable/spaces.json`, so they sync with the records they scope; create, rename, and delete them in Settings → Spaces. Existing data migrates on first load: Personal/Work groups become spaces, and any other repository group name becomes its own space rather than being reassigned.
- **Recent Projects:** up to four linked repositories, ordered by last opened time. Each card shows its checkout's branch (or detached commit), clean/modified state, and available ahead/behind counts. Status refreshes in the background every five seconds while Home is active; this reads local Git state and does not fetch remotes. Click anywhere on a card to open its workspace; **Add project** registers another checkout.
- **Repository Relationships:** open from Home, Projects, or the action palette. Connect providers to consumers on a native canvas, edit repository purposes/spaces, and query dependencies and dependents from the CLI. The canvas shows the active space's repositories; isolated and unlinked ones are included. Definitions sync through portable Git; per-space positions and viewports stay on this device.
- **Resources:** repository Markdown artifacts with originating sessions, editing, and agent-accessible comments.
- **Pull Requests:** track a GitHub PR by URL in the active space. Titles are fetched automatically from GitHub; no title input is needed. The URL is shown until the first successful fetch. Cards show approval status, CI passed/failed (or pending/no checks), and open/draft/merged/closed state using your existing `gh auth login`. Status refreshes in the background every minute while Home or the PR board is active; **Refresh status** retries immediately. Failed fetches retain the last result with a stale indicator. **View all** opens a kanban board with **Waiting for Approval**, **To Review**, and **Watching** columns for the active space. Drag cards between columns or use their menu to move them. Edit and remove tracked entries locally; **Open** launches GitHub in your browser. Existing entries default to Personal and retain their column.
- **Todos:** add/edit a title, optional description, the item's space, and an optional project. `http(s)` links in the description render clickable and open in a browser. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit**, **Delete**, and keyboard-accessible **Move up / Move down**. Cards show their space and project badges. Drag a card onto another incomplete todo to reorder it. The inbox filters by project within the active space. Enable **Show completed** to restore completed items. **View all** opens a kanban board with one column per project plus **Unscoped** for todos with no project (removed projects keep their own column so scoped todos are never hidden); drag cards between columns to re-scope them. The board respects the active space and **Show completed**.
- **To Read:** save an HTTP(S) link with an optional manual title, open it in a browser, mark it read, or delete it. No metadata is fetched. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit** and **Delete**. Enable **Show completed** to restore read items. **View all** opens a dedicated page with the full list.

Home lists and todo ordering are saved in `portable/dashboard.json` and participate in the existing portable Git sync. The header displays the portable directory's Git status, not the active project's status. Lists reload on returning Home, after adding a repository, and after sync/branch changes. Stale snapshots and malformed JSON are rejected rather than silently overwritten. The Pull Requests, Todos, To Read, and Projects **View all** destinations are implemented. Fetched PR statuses stay in memory and do not create portable-sync changes. GitHub CLI must be installed and authenticated with access to the tracked repositories; Devcroft finds `gh` via your PATH, common install locations (mise shims, `~/.local/bin`, Homebrew), or your login shell, so a terminal-only install still works for Finder-launched apps. GitHub approval rules take precedence over individual reviews; CI combines check runs and commit statuses, treating cancelled/timed-out checks as failed and neutral/skipped checks as successful. Merged and closed PRs remain in their assigned column until removed manually.

### Keyboard navigation

With navigation mode open, **Tab / Shift+Tab** move through the visible controls, including the command bar and whole project cards, and **Enter** leaves the mode on the focused control, where **Enter / Space** activates buttons or toggles checkboxes. Focused project cards support **arrow keys** (following the current grid) and **Home / End** (first/last project). **Page Up / Page Down** scroll the dashboard. Existing command-palette shortcuts remain available, and dialogs retain gpui-kit's keyboard focus handling.

Press **Cmd+J** on macOS (**Ctrl+J** on Linux/Windows) anywhere in the workspace to enter navigation mode: nothing edits while it owns the keyboard, and the keyboard shortcut badge right after the command bar becomes an amber `Navigation` indicator. The bottom-right list shows the keys available at the current location: tab and creation commands in a repository, back navigation on the Artifacts page, and edit/comment/save/cancel commands when a resource selection or draft allows them. Palettes stay on their direct shortcuts (`Cmd+K`, `Cmd+P` on macOS; `Ctrl+K`, `Ctrl+P` on Linux/Windows), which also work inside navigation mode. An action key runs once and returns to normal mode; **Tab / Shift+Tab** move through the visible components and keep the mode open; `h`/`l` move between visible panes and keep the mode open; `j`/`k` move within the focused list (sessions sidebar, Home cards in visual order, resource lists, review files, review diff scroll) and keep the mode open; `Enter` opens the highlighted session or Home card, otherwise it keeps the focused pane; `Escape`, the toggle again, or a click exits without acting. The only direct shortcuts in normal mode are the palettes: `Cmd+K` actions, `Cmd+P` projects on macOS (`Ctrl+K` / `Ctrl+P` on Linux/Windows); normal mode leaves **Tab** to the focused component, so terminals and the agent harness keep it (opencode switches agents/models/modes with it). See the [keyboard reference](docs/keyboard-reference.md).

## Repository resources

Use the Resources tab to browse plans, RFCs, notes, reviews, and visual tutorials for the current repository. Edit Markdown, comment on documents, and reopen originating agent sessions. Plans can track implementation phases using checkboxes. Reviews record agent review findings. Tutorials are self-contained HTML pages: they render in a sandboxed embedded view on macOS, open in the default browser elsewhere, and have no editor or comment UI.

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
node to edit its purpose/space, or an edge/label to edit endpoints, description,
or delete it. Selected edge endpoints can also be rewired by dragging their
handles; changes remain drafts until Save. Cycles are supported.

Drag the background to pan, scroll to zoom about the pointer, and use **Fit view**
or **Auto arrange**. In navigation mode, **Tab / Shift+Tab** move focus to nodes
or edge labels, then **Enter** selects the focused one; **Add relationship**
provides a keyboard form. **Escape** cancels the active gesture or dismisses the
draft. The active space filters the canvas; a node's
inspector lists connections across spaces, and its Space selector moves the
repository (and its artifacts) to another profile. Each space keeps its own
canvas layout. **Auto arrange** puts
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
for limits, space filters, unresolved repositories, and the full command set.

## Code review

The Review tab syntax-highlights recognized source files and supports persistent
line and range comments. Click a line or
Shift-click a range, then save your feedback. Use the sidebar to navigate, edit,
resolve, reopen, or delete comments. Agents can use `devcroft review list --open`,
`devcroft review resolve ID`, and `devcroft review delete ID` without the desktop.
Comments persist locally per branch pair and scope; changed code is re-anchored
when possible, with removed or ambiguous ranges marked outdated. See the
[review workflow and CLI](docs/review-comments.md).

To understand a change before reviewing it, ask your agent for a visual tutorial
and open it from Resources (see [Repository resources](#repository-resources)).
Tutorials are agent-generated, self-contained HTML pages; review judgments stay in
the Review tab and its comments.

## Tools

Developer tools live in the action palette (`Cmd+K` on macOS, `Ctrl+K` on Linux/Windows) under **Tools**. Each tool opens a dialog with its own inputs.

- **Format JSON:** paste JSON into the syntax-highlighted editor, then **Format** to pretty-print it with two-space indentation. The formatted document replaces the input in place, so what stays in the dialog — and what is saved — is already laid out; one undo restores the text as you pasted it. Formatting lays the document out instead of rebuilding it, so key order, duplicate keys, number literals, and string escapes stay exactly as written, and invalid input reports the line and column while leaving the text untouched. **Copy** puts the current text on the clipboard. **Clear** empties the input and the saved copy.

- **Diff Checker:** the primary button toggles between **Edit** and **Diff**, and each mode owns the whole dialog. **Edit** holds the old text on the left and the new text on the right; **Diff** shows the same two documents side by side, lined up row by row. Both sides keep their own absolute line numbers, so a change can be referenced in the pasted text: removals are tinted red on the left, additions green on the right, and a side with nothing at a change keeps a blank cell so the columns stay aligned. Entering **Diff** compares the pastes and reveals the result; going back to **Edit** costs nothing, and a comparison that is still current is reused instead of recomputed. A global **Language** picker in the action row colors both the paste editors and the comparison, is remembered for next time, and defaults to plain text: pasted text has no file name to guess from, so nothing is assumed. The app bundles two highlighters: the comparison uses the Review tab's syntaxes, and the paste editors use the editor's grammars, which cover the common languages (Rust, Python, Go, C, C++, C#, Java, JavaScript, Ruby, PHP, Scala, Lua, shell, YAML, HTML, CSS, Markdown, Makefile, Diff, JSON). A picked language the editor grammars do not cover still colors the comparison; the paste areas stay plain. Trailing-newline and CRLF differences are normalized away, so a paste that drops the final newline is not reported as a change. **Clear** empties both sides, drops the comparison, and returns to **Edit**.

- **Base64 Encoder** and **Base64 Decoder:** each has its own saved input and a separate, read-only output. **Encode** converts UTF-8 text to standard padded Base64. **Decode** accepts standard padded Base64, including line breaks and spaces, and displays the decoded UTF-8 text. Invalid Base64 or decoded bytes that are not UTF-8 produce an error without changing the input. **Copy** copies the current output; editing or clearing the input removes the old output.

Tool inputs and the diff tool's language choice are machine-local: they autosave as you type, flush when the dialog closes, and reload the next time you open the tool. They live under `tools/<tool>/` in the data root (`~/.local/share/devcroft` by default), never inside `portable/`, so tool scratch content does not participate in portable sync.

## Requirements

[mise](https://mise.jdx.dev/) manages the required Rust and Zig toolchains. The application expects at least one agent harness (`opencode`, `claude`, `codex`, or `omp`) in the environment inherited by your default shell. Neovim is needed only when selected as the editor.

```sh
mise install
mise run build
mise run app
```

Install as the **Devcroft** desktop app (replaces the old Electron-based install):

```sh
mise run install          # release build; use --debug for a faster debug install
```

On Linux this installs `~/.local/bin/devcroft`, the `devcroft.desktop` launcher with the icon from `assets/icons/logo.png`, and removes the previous `Devcroft.desktop` and legacy `com.devcroft.desktop.desktop` entries plus leftover Electron bundle files. The lowercase launcher filename matches the window app ID so Wayland taskbars can find its icon. Your data in `~/.local/share/devcroft` (`device.json`, `portable/`, caches) is kept. On macOS it (re)creates `/Applications/Devcroft.app` (`~/Applications` fallback when `/Applications` is not writable) with an `AppIcon.icns` generated from the same logo. If the app ever aborts without a message (GPUI panics inside an OS event callback cannot unwind), the report — panic message plus backtrace — is written to `logs/panic.log` under the data root and is never synced.

Useful development tasks:

```sh
mise run fmt
mise run check
mise run test
```

The initial terminal renderer supports ANSI color, keyboard input, bracketed paste, focus, resizing, scrollback, and scroll-wheel reporting to full-screen applications. Keyboard input automatically returns a scrolled terminal viewport to the active prompt. It shapes each terminal row as one GPUI text layout with Ghostty styles applied as highlights, avoiding both per-cell elements and clipping at style boundaries. Mouse clicks, releases, drags, and hover are forwarded when the terminal application requests them. Hold **Shift** while dragging to select and copy text locally; double/triple-click selects a word/line. **Ctrl-click** (**Cmd-click** on macOS) opens HTTP(S) links, including OSC 8 links. Plain-text URL detection currently covers a single displayed row. Image protocols are not yet supported. The Ghostty binding is pinned to a reviewed upstream revision so builds remain reproducible.

## Agent skill

In **Settings → Agent → Devcroft skill**, install or update the bundled CLI
skill for Claude Code and/or Codex/shared agents. OpenCode also discovers these
locations. The skill explains repository artifacts, visual tutorial
generation, and artifact/review comments, with examples; all operations use the
existing CLI.

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
