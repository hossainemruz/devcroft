# Devcroft

Devcroft is a small GPUI desktop workspace with three persistent, Ghostty-powered terminal tabs:

- **Agent** launches the workspace's default agent (`opencode` unless changed to `claude` in the workspace settings sheet) in your default shell.
- **Editor** launches `nvim .` in your default shell.
- **Terminal** launches your default login shell.

Each tab owns an independent PTY session. Switching tabs keeps the underlying process alive. Devcroft starts the default shell as an interactive login shell in every tab, then enters the Agent or Editor command through that shell. This matches a normal terminal launch and makes shell startup files, environment changes, aliases, functions, and tool-manager activation available to `opencode` and `nvim`.

The terminal UI bundles JetBrains Mono NL Nerd Font Mono v3.5.1 (regular, bold, italic, and bold italic), so Nerd Font symbols work without a separate system font installation. Attribution and license files are in [`assets/`](assets/).

## Home

Devcroft opens on Home without starting terminal processes. Open a recent project or use the shared command bar (`Cmd/Ctrl+K` for actions, `Cmd/Ctrl+P` for projects) to enter a repository workspace. The **Home** button and **Go Home** command return to the dashboard without stopping existing sessions.

- **Recent Projects:** up to four linked repositories, ordered by last opened time. Each card shows its checkout's branch (or detached commit), clean/modified state, and available ahead/behind counts. Status refreshes in the background every five seconds while Home is active; this reads local Git state and does not fetch remotes. Click anywhere on a card to open its workspace; **Add project** registers another checkout.
- **Recent Tasks:** clearly marked dummy cards until task planning is implemented.
- **Pull Requests:** manually add a GitHub PR URL and optional title, then choose **To Review**, **Waiting for Review**, or **Watching**. Edit, remove, and open tracked PRs in a browser. GitHub status fetching is not implemented yet; no GitHub login is required for this iteration.
- **Todos:** add/edit a title, optional description, and optional label. The checkbox sits in front of the title; the **⋯** menu in the top-right corner holds **Edit**, **Delete**, and keyboard-accessible **Move up / Move down**. Drag a card onto another incomplete todo to reorder it. Enable **Show completed todos and read links** to restore completed items.
- **To Read:** save an HTTP(S) link with an optional manual title, open it in a browser, mark it read, or delete it. No metadata is fetched.

Home lists and todo ordering are saved in `portable/dashboard.json` and participate in the existing portable Git sync. The header displays the portable directory's Git status, not the active project's status. Lists reload on returning Home, after adding a repository, and after sync/branch changes. Stale snapshots and malformed JSON are rejected rather than silently overwritten. Dedicated **View all** destinations are placeholders for now.

### Keyboard navigation

Use **Tab / Shift+Tab** to move through Home's controls, including the command bar and whole project cards, and **Enter / Space** to activate buttons or toggle checkboxes. Focused project cards support **arrow keys** (following the current grid) and **Home / End** (first/last project). **Page Up / Page Down** scroll the dashboard. Existing command-palette shortcuts remain available, and dialogs retain gpui-kit's keyboard focus handling.

## Requirements

[mise](https://mise.jdx.dev/) manages the required Rust and Zig toolchains. The application expects `opencode` and `nvim` to already be available in the environment inherited by your default shell.

```sh
mise install
mise run build
mise run app
```

Useful development tasks:

```sh
mise run fmt
mise run check
mise run test
```

The initial terminal renderer supports ANSI color, keyboard input, bracketed paste, focus, resizing, scrollback, and scroll-wheel reporting to full-screen applications. Keyboard input automatically returns a scrolled terminal viewport to the active prompt. It shapes each terminal row as one GPUI text layout with Ghostty styles applied as highlights, avoiding both per-cell elements and clipping at style boundaries. Image protocols and general mouse click/drag reporting are intentionally left for a later version. The Ghostty binding is pinned to a reviewed upstream revision so builds remain reproducible.
