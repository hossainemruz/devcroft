# Review tab — build plan (diffs.com-style, on gpui-kit)

> September 2026 update: the user's current requirements supersede M2's portable
> storage requirement below. Comments now use a local Git-common-directory store
> per branch pair and scope, with a desktop sidebar and headless lifecycle CLI.
> See [the implemented workflow](review-comments.md). The remaining portable,
> export, and tutorial items below are historical planning, not acceptance
> requirements for this comment implementation.

This document plans the Review tab for the Rust/GPUI rewrite: a diffs.com-style review experience built on gpui-kit tree components, informed by reviu, hunk (both of them — see below), and a prior exploration chat on Rust git-diff libraries (shared as "Rust Git Diff Libraries", whose recoverable findings are incorporated in the reference section). The binding capability spec is `feature-parity.md` §§6–7; this plan describes sequencing, architecture, crate choices, and acceptance criteria, not visual pixel-fidelity.

## Agreed decisions

- Phasing is phased parity: **M1** live diff viewer (tree + review stream) → **M2** portable comments + sidebar → **M3** agent-authored tutorial surface.
- Diffs and file status are computed with a **pure-Rust git library**, isolated behind one module so the choice stays reversible.
- Comments persist as **portable branch-pair JSON** exactly as `feature-parity.md` §§6–7 describe.
- Milestone 1 prioritizes the **file tree + multi-file review stream** slice of the diffs.com experience.

## Reference projects: what each one gives us

- `gpui-kit 0.6.0` (verified in the Cargo registry cache) ships a virtualized `tree` component with keyboard navigation (delegating to `gpui-base`), a `highlighter` module with tree-sitter languages, `list`/`ListItem`, `sidebar`, resizable panels, and `scroll`. There is no diff component, so line and hunk rendering is custom `div`-based work that reuses the `StyledText` patterns already established in `src/pane.rs`.
- reviu (Rust + GPUI, source-available under FSL) is the closest architectural cousin: split/inline toggle, hunk-level actions, and an inline-comments → send-back-to-agent loop confirm GPUI can carry this UX natively. Take the shape, not the code (license is non-competing-use only).
- hunk (TypeScript/OpenTUI + Pierre diffs, `modem-dev/hunk`) is a design reference only: multi-file review stream with sidebar, watch-mode auto-reload, and the agent-skill pattern (`hunk skill path` ≈ our `review-tutorial` skill distribution). No Rust code reuse.
- A second, unrelated project shares the name: `smolcars/hunk` ("A GPUI based diff viewer and Codex orchestrator", Rust, ~74 stars). Its crate split is the strongest structural precedent for our module plan: `hunk-git` (git access over a `hunk-domain` model), `hunk-text` / `hunk-language` (text and highlighting), `hunk-app` (feature-gated `diff` → `comments` → `comment-store` progression that mirrors our M1 → M2 phasing). Two caveats, both verified against the repo: its current desktop shell (`hunk-desktop`) is Qt Quick rather than GPUI, so take plumbing patterns (git access, domain modeling, store feature-gating) rather than view code; and it is licensed GPL-3.0, so study the architecture but write our own code (our crate is MIT).
- Arbor (Rust + GPUI, `penso/arbor`) is a secondary implementation reference for side-by-side diffs, changed-file navigation, and native inline PR comment actions; GitComet (`Auto-Explore/GitComet`, GPUI-native, gitoxide-based) is useful for git plumbing and diff/merge tooling rather than review UX. Both corroborate that GPUI carries this workload.
- diffs.com / Pierre define the interaction vocabulary to match: annotation framework for line widgets, split/stacked layouts, word-level inline change highlighting, and hunk separators. Web tech; treat as the UX bar, not the implementation.

## Module architecture

New code lives under `src/review/`; `src/workspace.rs` stays a thin shell that swaps the active content to the `ReviewView` entity (the `Option<Entity<TerminalPane>>` seam already supports non-terminal tabs).

- `src/review/mod.rs` — `ReviewView` entity: layout (toolbar, tree + stream, sidebar), review-mode state, Diff-vs-Tutorial surface toggle (toggle UI lands in M3; hold the state here from M1).
- `src/review/git.rs` — the only module that knows the git library: status, merge-base resolution, blob reads, worktree reads/hashing.
- `src/review/model.rs` — plain UI-ready data (`ChangedFile`, `Hunk`, `HunkLine`, `DiffStats`), independent of the git library so it is unit-testable with fixtures.
- `src/review/stream.rs` — multi-file diff stream rendering (unified first, split second) over the model.
- `src/review/tree.rs` — file-tree state built on gpui-kit `tree` + `TreeState`.
- `src/review/comments.rs` — M2: branch-pair store, comment anchoring, sidebar, copy-for-agent.
- `src/review/tutorial.rs` — M3: artifact load, freshness fingerprint, lesson reader.

## Milestone 1 — tree + review stream ✅ COMPLETE

> Status: implemented and verified (`cargo test`, `cargo clippy` clean).
> Delivered: `gix`-backed `git.rs` (both scopes, actionable missing-ref
> error), `model.rs` (hunks with full-file line numbers, truncation flag),
> folder-grouped `TreeState` sidebar, virtualized unified stream,
> Full-diff/Uncommitted toggle, manual refresh plus reload on tab
> activation, Material-icon file glyphs with A/M/D/R/T status cues,
> bidirectional tree↔stream scroll sync, folders-first ordering in both
> panes. Explicitly deferred to later milestones: tree search filter,
> split (side-by-side) layout, syntax highlighting (rows use the terminal
> font for now), and filesystem watching (refresh is manual plus
> on-activation).

1. `git.rs`: resolve the base ref (prefer `<remote>/<baseBranch>` when the remote-tracking ref exists, otherwise local `<baseBranch>`, otherwise an actionable missing-ref error per `feature-parity.md` §6), resolve merge-base against `HEAD`, list changed files with statuses (added, modified, deleted, renamed, copied, type-changed), compute per-file additions/deletions, read base/head content from blobs and the working tree, and report binary, too-large, invalid-text, or missing content as `unavailable`.
2. `model.rs`: diff each file into hunks with context lines, keeping full-file line numbers on both sides from the start (comment anchoring in M2 needs them; never collapse to hunk-relative numbering).
3. `tree.rs`: `TreeState` with folder grouping, per-file status badge plus stats, search filter, selection that scrolls the stream to the file, and session-local viewed-file state (matching `feature-parity.md` §6).
4. `stream.rs`: virtualized multi-file unified stream with one section per changed file (header with status, stats, and viewed toggle; hunk separators; +/- gutters; line numbers) and syntax highlighting via the gpui-kit `highlighter`. Split view follows behind the same model.
5. Refresh: manual refresh button plus reload on tab activation first; filesystem watcher (e.g. `notify`, debounced) as a fast-follow within M1, since agent-driven worktrees change underneath the reviewer.
6. Guards from day one: binaries render as counts only, oversized diffs truncate with an explicit truncated flag, empty states cover `identical` and missing base refs, and the stream virtualizes so a 10k-line diff cannot hang the frame loop.

M1 acceptance: open any repository, see the live branch-diff tree and stream, toggle viewed state, switch unified/split layouts, refresh after external edits, and confirm no PTY is spawned on the Review tab.

## Milestone 2 — portable comments

- Branch-pair record lifecycle: readable `base-to-head` keys, load-on-branch-switch, create-on-first-comment, bump `base.commit`, `head.commit`, and `updatedAt` on new commits; no immutable snapshots or history infrastructure.
- Comment shape carries key, mode, checkout-relative path, side, line, context, Markdown body, status, and timestamps, per `feature-parity.md` §6.
- Placement is best-effort by design: try stored path and line, then search nearby for stored context, then surface the comment as unplaced (a normal UI state with a repair affordance, never an error).
- Sidebar offers open/resolved/unplaced filtering, navigation shortcuts, edit/lifecycle/copy/export, and **Copy comments for agent** (concise Markdown to the clipboard and into the Agent tab queue, reusing the existing FIFO admission pattern).
- Enforce the human-owns-bodies boundary in the store API now (agents may list, set status, and delete explicit lists, but never create or edit bodies) so the later CLI inherits it for free.
- Deliberate divergence from `smolcars/hunk` (whose `comment-store` feature is SQLite-backed): our store is portable branch-pair JSON per `feature-parity.md` §6, because comments must travel with the user's Git-backed portable directory across devices rather than live in a local database.

## Milestone 3 — tutorial surface

Diff-vs-Tutorial toggle held session-local per repository, artifact path derivation with ETag optimistic concurrency, `sha256/devcroft-live-full-diff-v1` fingerprint recompute-on-load with fresh/stale/malformed/no-changes states, agent starter-prompt handoff to the Agent tab, and a render-capability-gated block registry, all per `feature-parity.md` §7. M1/M2 must preserve two invariants for this milestone: the changed-file manifest stays bounded with an explicit truncation flag (the agent treats it as orientation only and inspects the worktree itself), and no diff snapshots are persisted anywhere.

## Crate choices and risks

- Git access: start with `gix` (pure Rust, no C build dependency) behind `git.rs`, with `git2` as the isolated fallback. This exact pairing is proven by `smolcars/hunk`'s `hunk-git`, which depends on `gix 0.81` (features `parallel`, `revision`, `sha1`, `status`) plus `git2 0.20` (vendored libgit2 on macOS) — adopt the same versions and feature set as the starting point. If the week-0 spike (below) shows `gix` diff/status friction, the fallback is a contained change inside `git.rs` only.
- Word-level inline highlighting (the diffs.com signature detail): the `similar` crate (mitsuhiko/similar) applied to paired removed/added lines, with no git involvement.
- Watching: `notify`, debounced, with manual refresh always available as the fallback.
- Highlighting: gpui-kit `highlighter`/tree-sitter; confirm 0.6.0 covers our file types in the spike.

Biggest risks, ordered: `gix` diff-API maturity (de-risk first), large-diff frame performance (virtualize, never render all rows), and untracked files (absent from every diff — enumerate via status, read from the worktree, and remember the tutorial fingerprint must hash them too).

## Verification

- Unit tests for model diffing, comment placement fallback, branch-pair key derivation, and fingerprint stability against fixture repos under `tests/`.
- Parity acceptance per `feature-parity.md` §15: a portable directory from the Electron app opens, both live modes reproduce, comments plus Markdown export work, tutorial states transition correctly, and the CLI reproduces the agent operations later.
- `cargo test` and `cargo clippy` clean per milestone, matching the current suite.

## Suggested next step

Milestone 1 is done; the next step is Milestone 2 (portable comments)
as scoped above. The original week-0 `gix` spike is subsumed: `gix`
proved out in production code behind `git.rs`, with `git2` retained only
as a documented fallback.
