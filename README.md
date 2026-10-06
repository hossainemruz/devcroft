# Devcroft: Your personal development workspace

Devcroft is a GPUI based desktop app for personal development workflow. It focuses on keeping development workflow same no matter which harness you use. Vim like keyboard navigation let you fly between projects with ease.

> Status: pre-release (0.1.0). Build from source; there are no packaged binaries yet because the release pipeline is still coming.

## Quickstart

Requirements: [mise](https://mise.jdx.dev/) (manages the pinned Rust and Zig toolchains) and at least one agent harness (`opencode`, `claude`, `codex`, or `omp`) on your default shell's PATH. Optional: `gh` (authenticated) for PR tracking, Neovim if you prefer it as your editor.

```sh
mise install
mise run build
mise run app
```

Install as the **Devcroft** desktop app:

```sh
mise run install          # release build; use --debug for a faster debug install
```

On Linux this installs `~/.local/bin/devcroft` plus a `devcroft.desktop` launcher; on macOS it (re)creates `/Applications/Devcroft.app` (`~/Applications` fallback when `/Applications` is not writable). Your data in `~/.local/share/devcroft` (`device.json`, `portable/`, caches) is kept. If the app ever aborts without a message, the report is written to `logs/panic.log` under the data root and is never synced.

## Features

- **Agent** runs your default harness (`opencode` unless changed in Settings → Agent) in your default login shell, resuming the checkout's most recent session when history exists. Use **New session** to pick a harness explicitly.
- **Editor** offers Neovim or the built-in editor (Settings → Editor; the default for new installations). The built-in editor covers file tabs, fuzzy finding, project search, syntax highlighting, and find/replace for small edits. See [editor setup and everyday use](docs/editor.md) and [language support](docs/language-support.md).
- **Terminal** tabs own independent PTY sessions backed by your login shell, with bundled Nerd Font symbols and mouse/link support. See [Terminal](docs/terminal.md).
- **Home** dashboard for recent projects, repository relationships, PRs, todos, and reading lists, partitioned by spaces. See [Home dashboard](docs/home.md).
- **Review** tab with syntax highlighting and persistent line/range comments, plus a headless CLI for agents. See [review workflow and CLI](docs/review-comments.md).
- **Resources** tab for repository plans, RFCs, notes, reviews, and visual tutorials with agent-accessible comments. See [Repository resources](docs/resources.md).
- **Relationship graph** to connect providers to consumers across the active space. See [relationship graph](docs/relationships.md).
- **Tools** (action palette → Tools): Format JSON, Diff Checker, Base64 encoder/decoder. See [Developer tools](docs/tools.md).
- Keyboard-first throughout: `Cmd+J` on macOS (`Ctrl+J` elsewhere) enters navigation mode; `Cmd+K` / `Cmd+P` open the action and project palettes. See the [keyboard reference](docs/keyboard-reference.md).

## CLI

The CLI works without a running desktop. Records use `DEVCROFT_DATA_DIR` or the normal per-OS root.

```sh
devcroft repository list --json
devcroft artifact create --repository KEY --title "Implementation plan" --kind plan --content-file plan.md --json
devcroft review list --open
```

## Agent skill

In **Settings → Agent → Devcroft skill**, install the bundled CLI skill for Claude Code and/or Codex/shared agents (OpenCode discovers these locations too). The same installer works headless:

```sh
devcroft skill install
devcroft skill status
```

Modified or unmanaged skill folders are preserved. The maintained bundle lives in `assets/skills/devcroft/` and is embedded in the binary, so installation needs neither network access nor a source checkout.

## Development

```sh
mise run fmt
mise run check
mise run test
```

## Docs

- [Home dashboard](docs/home.md)
- [Editor setup and everyday use](docs/editor.md)
- [Language support](docs/language-support.md)
- [Terminal](docs/terminal.md)
- [Repository resources](docs/resources.md)
- [Relationship graph](docs/relationships.md)
- [Review comments](docs/review-comments.md)
- [Developer tools](docs/tools.md)
- [Keyboard reference](docs/keyboard-reference.md)

## Contributing

GitHub Issues are disabled — please use GitHub Discussions for bug reports, feature requests, questions, ideas, and support. PRs are welcome, including AI-assisted ones: they must meet the same quality bar as any other contribution (run `mise run check`, describe what changed and how you verified it), and the author is responsible for everything in the PR. For security reports, see [Security Policy](SECURITY.md) instead of opening a discussion.

## License

MIT — see [LICENSE](LICENSE). Third-party font and icon attributions live in [assets/](assets/) (notably assets/icons/ATTRIBUTION.md).
