# Data directory plan (Rust rewrite)

Status: implemented in `src/data/` (items 1–4 below, with tests); item 5 intentionally skipped (no migration owed — fresh `git init`); item 6 is the `SyncOutcome::reload_required` contract plus `load_workspace`, with Home/Tasks/Review projection wiring to follow those surfaces. Single user, experimental; no migration from the Electron app and no multi-device upgrade path to preserve. Electron used capital `Devcroft` (`~/.config/Devcroft/device.json`); the Rust build uses lowercase `devcroft` everywhere below.

## Decisions

- One app-owned root resolved from the environment with OS defaults, overridable only by `DEVCROFT_DATA_DIR` (tests, smoke isolation, one-off moves). No in-app directory picker and no `dataDirectory` selection UI — a deliberate break from Electron parity (deletes directory-selection generation tracking and portable-access switching logic).
- Portable data lives at a fixed derived path `$DEVCROFT_DATA_DIR/portable`, which is itself the git repo root (`portable/.git`). Only this subtree is ever staged, committed, fetched, rebased, or pushed.
- `DEVCROFT_DATA_DIR` itself is never a git repo, so "sync only portable" is structural: `git -C portable/ ...` physically cannot see `device.json`. No `.gitignore` trust required, and `git clean -fdx` inside `portable/` cannot delete machine-local state.
- First run supports initializing `portable/` empty (`git init -b main` + seed), by cloning (`git clone <url> portable` into a root that may already hold `device.json`), or by attaching a remote later (`git remote add origin <url>`). All mutating git operations shell out to the git CLI so ssh-agent, credential helpers, and signing config come free; `gix` stays read-only for Review diffs.

## Layout

| OS | `DEVCROFT_DATA_DIR` default | `portable/` (git repo) | `device.json` |
| --- | --- | --- | --- |
| Linux | `$XDG_DATA_HOME/devcroft`, fallback `~/.local/share/devcroft` | `$DEVCROFT_DATA_DIR/portable` | `$DEVCROFT_DATA_DIR/device.json` |
| macOS | `~/Library/Application Support/devcroft` | `$DEVCROFT_DATA_DIR/portable` | `$DEVCROFT_DATA_DIR/device.json` |
| Windows | `%LOCALAPPDATA%\devcroft`, fallback `%USERPROFILE%\AppData\Local\devcroft` | `%LOCALAPPDATA%\devcroft\portable` | `%LOCALAPPDATA%\devcroft\device.json` |

Windows uses `Local`, not `Roaming`: `Roaming` replicates to domain controllers and is wrong for a git-synced tree with locks that can grow; `Local` is the designated spot for large or app-synced data.

```
$DEVCROFT_DATA_DIR/
  device.json          # machine-local only: checkout bindings, agent/editor settings, pins/recents, theme. Never committed.
  portable/            # git repo root: workspace.json, repositories/<key>/..., tasks/<id>/..., artifacts/<id>/...
  cache/ logs/ tmp/    # later, as needed. Never synced.
```

## Sync contract (portable only, cwd always `portable/`)

- Stage portable files, commit with `chore(devcroft): sync portable data` when dirty, fetch and rebase onto the exact fetched commit, push; stop with a surfaced error on conflict/auth and leave the working tree usable. First sync against a newly attached remote bootstraps the upstream: track `origin/<branch>` when the fetch brought it (refusing unrelated histories with guidance instead of a mid-rebase conflict), otherwise publish with `push -u` (plain push, never force).
- Sync status (`idle`/`syncing`/`error`) stays in memory, never persisted. Manual plus scheduled triggers as before; no auto-resolve UI.
- `device.json` keeps the tolerant `readJson` shape with unknown-field preservation, but drops the `dataDirectory` field (path is now derived, not selected).

## Implementation checklist

1. Add root resolution: `DEVCROFT_DATA_DIR` env wins, else per-OS default above; `mkdir -p` root and `portable`.
2. Add `device.json` load/save with atomic temp-sibling-plus-rename writes and serialized read-modify-write (same guarantees as Electron's `DeviceStateStore`, minus `dataDirectory`).
3. Add first-run init: if `portable/.git` absent, `git init -b main` (or `git clone <url> portable` when a URL is supplied) and seed `workspace.json`; command to set/show the `origin` remote.
4. Scope all sync git invocations to `portable/` and verify `device.json` is unreachable from them (add a test that `git -C portable status --porcelain` never lists `../device.json`).
5. Move the old `devcroft-data` repo contents into `portable/` once, preserving history if worth it (`git mv` or copy `.git`); otherwise fresh `git init` since no migration is owed.
6. Wire portable reload (Home/Tasks/Review projections) after sync rebase, same as before.

## Explicit non-goals

Task/artifact scope is defined in [`task-management-plan.md`](task-management-plan.md), which reuses this root and portable-only sync boundary. PR 1's task store and machine-local locks are implemented as documented in [`task-storage.md`](task-storage.md); artifacts, CLI commands, and task UI remain planned.

- No Electron `userData` reuse, no `Devcroft` → `devcroft` migration, no selectable data directory, no `Roaming` support on Windows, no libgit2/`gix` writes, no committed `device.json`.
