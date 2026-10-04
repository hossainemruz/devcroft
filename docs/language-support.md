# Built-in editor language support

Syntax highlighting is available for the bundled grammars. Semantic features currently support **Rust with an installed rust-analyzer** only. Devcroft never installs a language server automatically.

## Enable Rust tooling

Open a Rust file, then click the disabled Rust indicator in the editor status bar and confirm **Trust checkout**. This decision lasts for that editor workspace's lifetime; reopening the app requires trusting again. Plain editing and syntax highlighting work without trust or a server.

Trust permits starting the installed server in this checkout. Rust tooling can consult Cargo/toolchain configuration and execute project-configured tools. Devcroft disables rust-analyzer's check-on-save, build-script analysis, and proc macros. Repository-supplied server commands, shell command strings, and custom server configuration are not supported. The server executable must resolve outside the checkout; relative executable paths are rejected.

Devcroft looks for `rust-analyzer` in the desktop process's inherited `PATH`, then `$HOME/.local/bin` and `$HOME/.cargo/bin`; macOS also checks `/opt/homebrew/bin` and `/usr/local/bin`. It does not invoke a login shell. If rustup manages Rust, install its rust-analyzer component yourself (`rustup component add rust-analyzer`), then use **… → Restart language server**. Restart does not grant trust. If a PATH entry selects a checkout-local executable, remove that entry from the desktop environment and restart the app.

## Supported and tested matrix

| Language / server | Semantic features | Validation |
| --- | --- | --- |
| Rust / rust-analyzer 1.97.1 (`8bab26f4`, 2026-07-14) | Completion, hover, definitions within the checkout, pushed diagnostics | macOS: real-server handshake, hover, definition, completion, and versioned syntax diagnostics; deterministic client and editor tests |
| Other highlighted languages | Syntax highlighting and plain editing | No semantic server integration |

Linux and Windows runtime LSP validation remains open. Windows local drive URI normalization is covered by platform-independent tests; UNC/network paths are unsupported. Desktop popover, pointer, and IME inspection is separate from these headless checks.

## Lifecycle and failure behavior

One server belongs to each trusted checkout editor workspace. Rust tabs share that server, including inactive unsaved buffers. Opening, changing, saving, and closing documents follows the server's advertised capabilities. Switching to another file does not restart the process. Explicit restart reopens the current contents of all Rust tabs without discarding drafts.

The status footer reports startup, readiness, missing binaries, crashes, and request failures. Use **… → Restart language server** to recover. Startup (20-second timeout), requests (10 seconds), writes, and process teardown run off the UI thread. A full outbound queue marks the server unavailable instead of blocking typing. Closing the workspace releases the server; Unix teardown also terminates its process group.

The client advertises UTF-16 positions and rejects servers that select another encoding. Incoming ranges are converted for the editor. Versions are tracked per URI, with non-reused version numbers after close/reopen. Edits and closes cancel outstanding document requests; late or stale responses are discarded. Versioned diagnostics must match the current buffer. Unversioned server diagnostics cannot be proven fresh and are applied as best-effort updates.

Full-sync servers receive full text. Incremental-sync servers receive a valid replacement range covering the previous document; fine-grained diff sync is deferred. Completion currently applies a single plain-text edit; suggestions requiring additional edits (such as auto-imports) or snippets are excluded. Build-script/proc-macro-dependent analysis can be incomplete with the defaults above. The client does not run Cargo checks on save.

Definitions open checkout files and preserve Back/Forward history. Definitions outside the checkout, remote URLs, and virtual documents are rejected. References and document symbols are follow-up features. Broad refactoring, arbitrary workspace edits, custom server commands, additional servers, and automatic installation are deferred.

Protocol reference: [Language Server Protocol 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).

## Verification

```sh
mise exec -- cargo test --locked editor:: -- --test-threads=1
mise exec -- cargo test --locked live_rust_analyzer_hover_and_definition -- --ignored
mise exec -- cargo clippy --locked --all-targets -- -D warnings
```
