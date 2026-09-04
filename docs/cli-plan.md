# CLI plan (`devcroft` binary)

Status: planning — no code yet. This document records the agreed CLI shape, transport, and phasing so implementation (Phase 0+) has a stable contract to build against.

## Goal

Give the project a first-class CLI where `devcroft app` starts the current GPUI workspace, and later subcommands let the agent (running in the Agent tab), Neovim (running in the Editor tab), and other independent local processes perform Devcroft-specific work: task management, review-comment management, Markdown preview, and adding line/selection references from diff view/editor into the agent input queue.

## Relation to existing plans

- `docs/feature-parity.md` §11 already requires the rewrite to expose the thirteen original agent operations as a CLI rather than a loopback MCP server, preserving filters, validation, atomicity, optimistic-concurrency contracts, the human-owned comment-body boundary, the tutorial source-inspection contract, error vocabulary, and live-refresh semantics adapted to a non-running-desktop model. This plan is the concrete shape of that requirement.
- `docs/data-directory-plan.md` owns root resolution (`DEVCROFT_DATA_DIR` override else per-OS default), `portable/` as the only synced subtree, and atomic JSON writes. The CLI reuses it verbatim and derives its transport from the same root.
- `docs/review-plan.md` M2 (portable comments, human-owns-bodies store API) and M3 (tutorial get/write with fingerprint + ETag concurrency) define the store semantics the CLI must reuse, not reimplement.

## Agreed decisions

- Binary name is `devcroft`; `devcroft app` boots the existing GUI unchanged. Bare `devcroft` with no args prints help (explicit `app` keeps the surface greppable for agents and leaves room for future default behavior without breaking scripts).
- Commands split into two categories: headless store commands that operate directly on `portable/` with no running app, and live-UI commands that require notifying the running parent process.
- Transport for live-UI commands is a local socket derived from the data root: Unix domain socket at `<data_root>/cache/devcroft.sock` on macOS/Linux, Named Pipe `\\.\pipe\devcroft-<hash-of-data-root>` on Windows, behind one API via the `interprocess` crate's `local_socket` abstraction. No TCP ports, no CORS, no port conflicts.
- Socket identity follows data-root identity: different `DEVCROFT_DATA_DIR` means a different socket, so a dev instance built and run inside Devcroft never collides with the outer production instance. Same-root second `app` fails fast (single instance per data root).
- Children discover the parent via injected env (`DEVCROFT_SOCK`), with explicit `--sock` overriding and derived-from-data-root as the fallback. The app strips inherited `DEVCROFT_SOCK` on startup, derives fresh, and re-injects for its own children (same strip-and-reinject pattern as the editor-reference token).
- Agent-facing output contract from day one: human-pretty default, `--json` for stable machine output, errors to stderr with path + cause, exit codes `0` success / `1` runtime / `2` usage / `3` app-not-running (live-UI commands only).
- The human-owns-bodies boundary (agents may list, set status, and delete explicit comment lists, but never create or edit bodies) is enforced in the shared store API so the CLI inherits it for free.
- `clap` v4 derive for parsing; CLI dispatch happens in `main()` before `gpui_kit::application()` so headless commands never initialize a window and stay fast.

## Command tree

Phase 0 (scaffold): `devcroft app [--checkout <path>]`, `devcroft status | doctor`, `devcroft --help`.

Phase 1 (headless, no socket needed): `devcroft task list|get|create|update`, `devcroft subtask create|update|delete`, `devcroft review comment list|set-status|delete`, `devcroft review tutorial get|write`, `devcroft sync` (thin wrapper over `src/data/sync.rs`, later).

Phase 2 (live-UI, socket required): `devcroft preview <markdown-file>`, `devcroft ref add --file <checkout-relative> --lines <a[:b]> [--repo <key>] [--id <uuid>]`, plus `devcroft focus --tab agent|review|editor|terminal` as the small navigation primitive that `ref add` and tutorial starter prompts reuse.

Explicit non-goals for v1: no remote access, no second transport, no daemon mode, no `devcroft app` auto-focus-existing (second same-root `app` errors; focus-via-socket comes later), no `preview` fallback renderer when the app is not running (fail with actionable `app-not-running`).

## Architecture

- New top-level module `src/cli.rs` owns arg definitions (`clap` derive), resolution order (`--sock` > `$DEVCROFT_SOCK` > derived-from-data-root; `--repo` > cwd-via-`device.json` checkout bindings > explicit path), output formatting (human vs `--json`), and exit-code mapping. Per-resource handlers live under `src/commands/` (`tasks.rs`, `review_comments.rs`, `review_tutorial.rs`, `preview.rs`, `ref_add.rs`, `doctor.rs`) and call into the existing `src/data/` and (once built) task/review store modules — no duplicated validation, no second persistence path.
- `main()` dispatches: if `argv[1]` is a CLI subcommand other than `app`, run it headless and exit without touching GPUI; `app` runs the current boot path verbatim (data-root ensure, device load, window open). This keeps incremental builds fast (top-level crate stays unoptimized) and CLI startup latency independent of font/GPU init.
- Live-UI IPC is newline-delimited JSON `{id, method, params}` over the local socket, served by a background thread spawned in `Workspace::new` (off the GPUI main thread, notifying via `cx.update` only on state change, mirroring the git-status poll). Methods are `preview.open`, `ref.add`, `tabs.focus`; responses carry `{ok, data}` or `{ok: false, error: {code, message}}` with the same error vocabulary as §11. Every `ref add` carries an idempotency key (`--id`, auto-UUID default) with bounded pending admissions + TTL so retries and window-reload replays never double-paste into the Agent FIFO — preserving the exactly-once semantics of the Electron reference pipeline (`feature-parity.md` §8).
- PTY env injection: Agent/Editor children get `DEVCROFT_SOCK=<this-instance-sock>` (plus the existing reference-token triple for editor terminals); inherited copies of those names are stripped before injection so nested processes cannot forward stale credentials. `DEVCROFT_DATA_DIR` is inherited, not overwritten, so normal children operate on the parent's root while a dev run explicitly overrides it before `cargo run`.

## Dev-inside-dev (testing instance)

- The scenario is building and running Devcroft from inside Devcroft's own Agent tab: the outer (prod) app owns the default data root and socket, while the inner (dev) run uses an isolated root, e.g. `DEVCROFT_DATA_DIR=./.dev-data cargo run -- app` (to be wrapped as `mise run dev`, with `./.dev-data` gitignored and the dev window titled/status-barred `DEV` so the two instances are never confused).
- Because the socket derives from the data root, the two instances bind different endpoints and run side by side with no conflict. The inner app ignores the outer-injected `$DEVCROFT_SOCK` for its own listen address and re-injects its own for its children, so `ref add` from an inner Neovim reaches the inner Agent tab and outer Neovim still reaches the outer one.
- Startup rule: bind fails with `AddrInUse` → try-connect: accepted means another live instance owns this root → exit `1` with "already running for this data dir (socket …)"; refused means stale file → unlink, bind, continue. `devcroft doctor` prints `data_root + socket + pid + running?` and `ref add -v` logs which socket it dialed, making "which instance am I talking to?" always answerable.

## Security and boundaries

- Filesystem scoping is the first boundary: socket dir `0700`, `portable/`-only git scoping already structural per the data plan, temp-sibling-plus-rename atomic writes with per-path serialization reused by every CLI mutation.
- Inside-only enforcement (so arbitrary local processes cannot drive the UI) is a per-launch token injected only into Agent/Editor PTY env and validated by the socket server, mirroring the Electron loopback reference endpoint's bearer design without HTTP. Store commands need no token (they are just file operations under the user's own data dir).
- The comment-body boundary, tutorial dialect/ETag/fingerprint concurrency (`review_changed` vs `tutorial_changed`), canonical `task-0001` ID validation, and unknown-repository rejection all live in the shared modules, so GUI, CLI, and future skills cannot diverge.

## Output and errors

- Default output is concise human text; `--json` emits a stable schema (records plus ` pprint` counts, truncation flags, and ETags/fingerprints where applicable) intended for `opencode` and scripts. Errors go to stderr as `devcroft: <command>: <path?>: <message> (<cause>)`, exit `2` for clap usage errors, `1` for runtime failures (missing ref, malformed record reported per-record without hiding siblings, git/env failures), `3` for live-UI commands when no app answers on the resolved socket.

## Testing and verification

- Unit tests for arg parsing, resolution order (flag > env > derived), socket-path derivation per OS, stale-socket bind logic, and idempotency-key handling, using `tempfile` + `DEVCROFT_DATA_DIR` isolation exactly like the existing `src/data` tests.
- CLI integration tests run the binary against tempdir roots (never the real data dir), covering headless round-trips (task create → get → list `--json`, comment list/set-status concurrency rejection on stale tokens, tutorial get/write ETag mismatch) plus `doctor` reporting.
- Socket tests use tempdir sockets with an ephemeral server harness; smoke coverage launches an isolated `app` with a temp root (never killing or contacting a running production instance), dials `doctor`/`ref add` over its socket, and asserts the full tool inventory reachable — the Unix-socket analogue of the Electron `DEVCROFT_INTERNAL_MCP_SMOKE` pattern.
- Acceptance per `feature-parity.md` §15 adapted: a portable directory from the Electron app opens, every headless CLI operation matches the thirteen MCP operations' validation/atomicity/concurrency, and live-UI commands reproduce admission, preview, and focus behavior with synchronous errors.

## Phasing

- Phase 0 — scaffold: add `clap` + `interprocess` deps, `src/cli.rs` + `src/commands/doctor.rs`, move current GUI boot under `app`, add `mise run dev` (isolated root + DEV tag), `cargo test` + `cargo clippy` clean.
- Phase 1 — headless parity: task/subtask and review comment/tutorial store commands on `portable/` with `--json`, sharing M2/M3 store code as it lands; file-watcher/reload wiring so the GUI reflects CLI mutations live.
- Phase 2 — live UI: socket server + `preview`, `ref add` (with idempotency/TTL/replay), `focus`, per-launch token enforcement, `doctor` socket probing.
- Phase 3 — distribution: `review-tutorial`-style opencode skill docs describing the CLI so any Agent tab discovers it via `--help` + skill file rather than tribal knowledge.

## Open questions

- Should second same-root `devcroft app` eventually focus the existing window instead of erroring, and does that need a `preview`/`ref` payload piggyback (single-instance open-file)?
- Should `preview` accept stdin (`cat README.md | devcroft preview -`) in addition to a file path, and what is the size/UTF-8 limit relative to the 4 MiB repository file-API limit?
- What is the exact `--json` schema versioning rule (informational `formatVersion` never gating, per the `workspace.json` precedent)?
