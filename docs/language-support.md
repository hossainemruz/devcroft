# Built-in editor language support

The built-in editor supports completion, hover, definitions, and diagnostics for Rust, Go, JavaScript/TypeScript (including JSX/TSX), and Python. Other bundled languages retain syntax highlighting and plain editing.

## First use

Open a supported file and click the language indicator in the editor footer to **Trust checkout**. Trust lasts for this editor workspace's lifetime. It permits language servers to consult project configuration and tools; installing a server does not grant trust.

Devcroft uses your configured executable first, then its managed installation, then a system executable. When no server is available, an inline offer provides **Install**, **Use existing**, and **Not now**. Dismissals last for the workspace lifetime. Nothing downloads merely because a repository contains a language.

Manage support at **Settings → Editor → Language servers**, or click the editor footer. Each row shows its source and managed version. Expand **Use existing / configure** to select an absolute server executable, Node/Go runtime, and optional server settings as a JSON object. The editor's dialog also supports a project-root override within the checkout. Invalid explicit paths produce an error rather than silently selecting another server. Settings are device-local.

Executable discovery uses the desktop PATH, `~/.local/bin`, `~/.cargo/bin`, `~/go/bin`, mise shims, and Volta's bin directory; macOS also checks Homebrew locations. It does not invoke a login shell. Executables must resolve outside the checkout. A runtime override helps when a GUI environment cannot find Node or Go.

## Managed servers and prerequisites

| Languages | Pinned managed server | Prerequisites |
| --- | --- | --- |
| Rust | rust-analyzer 2026-09-28 | curl for acquisition; project's Rust toolchain for analysis |
| Go | gopls 0.23.0 | Go compatible with this gopls release, both to install and analyze |
| JavaScript, JSX, TypeScript, TSX | typescript-language-server 6.0.1 with TypeScript 6.0.3 | Node 22.22.2 or newer and npm |
| Python, Python stubs | basedpyright 1.40.2 | Node and npm for the managed package; project Python environment/dependencies for analysis |

Servers and caches live under the device data directory's `language-servers/`, outside portable sync. Installations use a neutral staging directory and never add project dependencies or global npm packages. Rust downloads use a pinned SHA-256 checksum; npm uses shipped integrity lockfiles with lifecycle scripts disabled; Go uses a pinned module version and the public checksum database. Installs have bounded output/time and can be cancelled.

**Check / update** installs the version bundled with Devcroft's catalog, not an arbitrary latest upstream release. Updates retain the previous selection for **Roll back**. Running sessions keep their current version until restart. **Remove** affects managed packages only and refuses versions still used by a running server; disable the server and close its workspaces first. Installation logs appear in the expanded configuration row. Missing prerequisites and installation failures remain retryable there.

## Roots, settings, and lifecycle

Instances are scoped to a checkout, server, and language root. Related tabs share a server, including inactive unsaved buffers. Independent projects can have separate servers, and one language's failure does not stop another.

Root selection recognizes Cargo workspace members/exclusions, Go modules and go.work members, package.json workspaces, and Python project markers. It never crosses the checkout boundary. Use the root override for layouts the detector does not recognize, including workspace formats not represented in package.json. Python environment and TypeScript SDK choices can be supplied through the server's settings. Configuration changes restart affected instances; explicit package updates wait for deliberate restart.

Use **… → Restart language server** to restart support for the active file's root. All matching open drafts are replayed. Repeated crashes use bounded retry delays and stop after three attempts; failed initial startup requires a restart or corrected settings. Unused instances stop after approximately one minute. Closing the workspace releases its processes and package leases.

Native file watching sends project changes to running servers, including changes outside open buffers. Generated dependency/build directories (`node_modules`, `target`, `.venv`, `__pycache__`) and `.git` are excluded. Notifications are batched, with a bounded event buffer. Recursive OS watches may still consume resources for excluded directories; OS watch-limit failures or event-buffer overflow remain visible until restart. Watcher failures are reported as server errors; restart after external changes if watching is unavailable. Changes to workspace membership/root layout may require reopening the file or restarting support.

Rust defaults keep check-on-save, build-script analysis, and proc macros disabled. Explicit server settings may override these defaults after checkout trust. Toolchains, package dependencies, interpreters, and SDKs remain the user's responsibility.

## Feature limits and validation

The client advertises UTF-16 positions, handles full or whole-document incremental sync, and cancels stale document requests. Old session generations and wrong document versions are rejected. Unversioned diagnostics are best-effort because freshness cannot be proven.

Completion applies a single plain-text edit; snippets and suggestions requiring extra edits (such as auto-imports) are excluded. Definitions stay within the checkout and preserve Back/Forward history. Refactoring, arbitrary workspace edits, references, document symbols, pull diagnostics, and dynamic registration are not implemented. Server-specific progress/indexing is not yet displayed.

All four managed recipes have passed real-server installation, initialization, completion, hover, definition, and pushed syntax-diagnostic checks on macOS arm64. Deterministic tests cover protocol behavior, language routing, roots, session generations, preferences, receipts, cancellation, and leases. Linux and Windows runtime validation remains open. Managed Rust assets are provided for macOS arm64/x86_64 and Linux GNU arm64/x86_64; other platforms require an existing Rust server. Windows drive URI normalization is tested; UNC/network paths are unsupported.

## Verification

```sh
mise exec -- cargo test --locked editor:: -- --test-threads=1
mise exec -- cargo test --locked data:: -- --test-threads=1
# Opt-in: downloads pinned packages into a disposable directory.
mise exec -- cargo test --locked managed_servers_live_smoke -- --ignored --nocapture --test-threads=1
mise exec -- cargo clippy --locked --all-targets -- -D warnings
```
