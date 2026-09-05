# Devcroft

Devcroft is a small GPUI desktop workspace with three persistent, Ghostty-powered terminal tabs:

- **Agent** launches `opencode` in your default shell.
- **Editor** launches `nvim .` in your default shell.
- **Terminal** launches your default login shell.

Each tab owns an independent PTY session. Switching tabs keeps the underlying process alive. Devcroft starts the default shell as an interactive login shell in every tab, then enters the Agent or Editor command through that shell. This matches a normal terminal launch and makes shell startup files, environment changes, aliases, functions, and tool-manager activation available to `opencode` and `nvim`.

The terminal UI bundles JetBrains Mono NL Nerd Font Mono v3.5.1 (regular, bold, italic, and bold italic), so Nerd Font symbols work without a separate system font installation. Attribution and license files are in [`assets/`](assets/).

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
