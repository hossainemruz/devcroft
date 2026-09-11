# Recent agent sessions implementation plan

Status: implemented (v1), 2026-09-10. Catalog, Home Recent Activity, Agent sidebar, exact-resume navigation, and the local cache are in place. Both proposed defaults below were adopted as specified. Known limitations are listed at the end.

## Outcome and scope

Show recent local conversations from OpenCode, Codex, and Claude Code in two places, using one shared session catalog:

- Home: add **Recent Activity** between Recent Projects and Recent Tasks, with up to **4 cards** across projects. Each card shows session title, repository label, and agent. Clicking anywhere on the card opens the matching checkout, selects Agent, and opens that exact conversation.
- Agent tab: a left sidebar showing the most recently worked-on sessions for the current checkout (25 by default, adjustable in Settings > Agent), merged across supported agents. Each row shows title, agent, and relative last activity time. Clicking switches the visible agent and conversation. Highlight the selected row across its full width.
- Both lists sort by last session activity descending, independent of repository opening time. Home's four is a named constant; the sidebar limit persists per machine in `device.json` (`recent_sessions_limit`).
- Session information remains device-local. Provider stores own transcripts; Devcroft stores only a disposable local metadata cache.
- OMP gets no discovery or resume adapter in this work. Removing or disabling its existing launcher is a separate change.

## Proposed defaults awaiting feedback

Two questions were sent during planning. Until answered, use these as explicit proposals, not confirmed requirements:

1. Include sessions started outside Devcroft for repositories registered on this device. Also show sessions for an ad-hoc checkout while it is open in Devcroft. Do not automatically register every directory discovered in an agent's history.
2. Keep the current session running when another is selected. Reuse a retained terminal for an already-open session; otherwise launch its exact resume target. No silent termination to enforce a cache limit.

For repository scope, v1 means the canonical **checkout**, matching existing workspace ownership. Sessions started in a subdirectory belong to that checkout. Linked worktrees and separate clones remain separate destinations even when they share a remote. Broadening the sidebar to all worktrees can follow later without changing session identity.

Selecting a historical session changes the active agent, but does not change the saved default used for new sessions. Add a small **New session** action that asks which harness to launch, showing the saved default as a hint and leaving it untouched. Change the settings description and behavior so changing the default affects future launches instead of destroying the visible session.

Entering a checkout starts its most recent catalog session rather than a fresh shell, reusing the shared open path (so retained-pane reuse and stale-click protection apply). The auto-start runs once per checkout; afterwards the user owns the pane. Checkouts without history, or entered before the first catalog load, open a fresh pane as before, and an explicit **New session** cancels an in-flight auto-resume.

## Current implementation and required seams

| Existing code | Relevant behavior | Planned change |
| --- | --- | --- |
| `src/home.rs` | Recent Projects and Recent Tasks; `HomeEvent::OpenRepository` | Render session cards from catalog snapshots; emit a stable session target |
| `src/workspace.rs` | `RepositoryTabs` retains one Agent pane per checkout; `switch_repository` restores it | Own a per-checkout session host and route exact session navigation |
| `src/agent.rs`, `src/data/workspace_agents.rs` | Agent identity and device-local default by canonical checkout | Keep defaults separate from active conversation selection |
| `src/pane.rs` | Creates an activity launch and PTY together | Accept new/resume launch intent and expose lifecycle failures |
| `src/session.rs` | `TerminalSession` is a PTY, not a conversation; startup is typed into a login shell | Preserve PTY behavior; accept the composed resume command through existing startup plumbing |
| `src/agent_activity/mod.rs` | `start` inserts activity keyed by checkout; another launch overwrites it | Track launch identities, project by checkout for existing consumers |
| `src/agent_activity/providers.rs` | OpenCode observer arguments/environment and Claude hooks | Compose observation configuration with resume arguments |
| `src/data/` | Local data root and separate `portable/` sync tree | Locate session cache outside portable storage |

Create `src/agent_sessions/` as a deep module. Home and the sidebar should know about normalized session summaries, refresh state, and open targets. Provider paths, pagination, protocol schemas, title extraction, cache invalidation, and resume flags stay inside this module. The three providers justify a private adapter seam; a static registry is sufficient.

Keep historical catalog data separate from live agent activity. Finding a conversation on disk does not mean it is running, idle, or finished.

## Shared model and interface

Suggested design sketch; finalize Rust signatures during implementation:

```rust
SessionKey { provider_id, local_store_id, native_session_id }
SessionSummary {
    key,
    checkout,       // canonical routing destination
    working_dir,    // original session cwd, preserved for resume
    title,
    last_activity_at,
    timestamp_source,
    availability,
}

catalog.refresh(scope)                 // asynchronous, coalesced
catalog.snapshot(scope, limit)         // in-memory projection + health
catalog.prepare_open(key)              // validates and returns typed target
// Catalog publishes snapshot changes; views subscribe once.

AgentLaunchIntent::New { agent, cwd }
AgentLaunchIntent::Resume { target }
```

Use stable provider strings in serialized cache data; unknown providers can be ignored without breaking cache loading. `local_store_id` distinguishes local profiles/data roots containing coincident native IDs. Repository display names are a projection joined from current bindings, never identity. Include source-root context in resume preparation so listing from one profile cannot resume from another.

Normalize timestamps to one UTC unit. Prefer provider last-conversation-activity metadata; use provider update time when that is the available contract, then transcript modification time as an explicitly marked fallback. Do not bump recency merely because a card was viewed or a refresh ran. Handle invalid/future times consistently; unknown times sort last. Resolve equal times by stable session key.

Titles prefer explicit/provider-generated titles, then a bounded first user-message preview, then `Untitled session`. Normalize whitespace and control characters, cap retained length, and truncate only visually in cards. Do not store tool output, full prompts, or transcripts in the cache.

Resolve ownership from the recorded cwd using canonical paths and Git checkout discovery. Use path-component containment, not string prefixes; nested repositories resolve to the nearest owning checkout. Never join by basename or remote URL alone. Preserve missing paths as unavailable records and revalidate on open rather than redirecting to a similarly named project.

Filter provider child/subagent, ephemeral, archived, and deleted conversations out of the default recent lists where their metadata supports it. Keep root resumable conversations. Record provider limitations when a source cannot distinguish these categories reliably.

## Provider discovery and exact resume

Prefer machine-readable local provider interfaces. No model request should run to discover a title or list history. Provider reads must have cancellation, timeouts, output limits, and independent error reporting.

| Provider | Initial discovery approach | Exact resume |
| --- | --- | --- |
| OpenCode | Read-only query of the local session store (`$XDG_DATA_HOME/opencode/opencode.db`, `session` table); verify returned cwd, title, timestamps, root/child markers, and coverage on the supported version. If required fields/filtering are absent, use its local session interface behind the same adapter. | `opencode --session <id>` with recorded cwd and existing observation arguments |
| Codex | Local app-server `thread/list`, initialized once for discovery, with pagination and update-time sorting; normalize name/preview and cwd. Explicitly select supported root conversation source kinds. | `codex resume <id>` in the recorded working directory, preserving source-root configuration |
| Claude Code | A version-tested reader of the local project session metadata/transcripts. Use available metadata indexes as hints; reconcile against transcript existence and updates. Stream only records needed for ID, cwd, title, and timestamp. | Validate exact-ID `claude --resume <id>` on the supported version; documented absolute transcript-path resume is an alternative if needed |

OpenCode's `session list --format json` is scoped to the single project containing the caller's working directory (verified on 1.18.30: listing from $HOME hides a repository's sessions and vice versa), so it cannot back a global catalog. Discovery reads the local session store directly instead and filters by checkout afterwards, which also removes the recent-count truncation concern. [OpenCode CLI documentation](https://opencode.ai/docs/cli/).

Codex documents paginated `thread/list`, source filters, and update-time sorting. Discovery must only list metadata; it must not start/resume threads or turns. Verify that a local helper sees the same store and IDs as the interactive CLI. Local `codex resume --help` was inspected during planning and accepts a session ID. [Official OpenAI app-server documentation](https://developers.openai.com/codex/app-server/).

Claude documents continuously saved local sessions and JSONL transcripts beneath `~/.claude/projects/`, plus direct named/path resume. Directory encoding alone is insufficient for ownership; recover cwd from metadata. Honor the supported configuration-root override after checking the installed version. [Claude session documentation](https://code.claude.com/docs/en/sessions).

Compatibility spike before implementation: record executable versions, sanitized metadata fixtures, custom-root behavior, native ID semantics, title precedence, timestamp units, subagent filtering, and exact resume behavior for all three providers. OpenCode and Claude local help could not be inspected during planning because their mise launchers attempted unavailable installation/cache work. Do not treat the documentation as a completed local smoke test. Avoid adding a Node/Python runtime solely to list sessions; keep readers/protocol handling in Rust. Add dependencies only when the chosen source requires them.

## Cache, refresh, and failure handling

Store a versioned, rebuildable index at `$DEVCROFT_DATA_DIR/cache/agent-sessions/index.json`, with provider/source fingerprints and refresh checkpoints. This path is outside `portable/`, following [the data directory plan](data-directory-plan.md). Never put session data in portable repository metadata or task artifacts. If no data root is available, use an in-memory catalog.

Use serialized atomic writes with appropriate local file permissions. Multiple windows/processes must not corrupt the cache; either give one writer ownership or lock and merge by source generation. A corrupt cache triggers a rebuild. Never modify provider-owned histories as part of discovery.

Load cached summaries immediately, then refresh in the background on startup, Home/Agent activation, repository binding changes, and managed session lifecycle changes. Coalesce repeated triggers. While either session surface is visible, refresh approximately every 15 seconds; suspend periodic work when hidden and refresh when returning. Provider events can invalidate metadata sooner. Relative-time labels update on a lightweight minute timer.

Use one scan per provider/source, shared by both UI surfaces. Incremental file readers track modification/size and offsets where safe, detect truncation/replacement, and tolerate incomplete final JSONL lines. No filesystem or subprocess work belongs in rendering. Long initial scans publish progressive results with a loading indicator; never present a truncated scan as a complete top-N list. Bound worker concurrency and per-read allocations.

An unavailable provider leaves other providers usable. Retain its last good cache with a stale indication and Retry; an authoritative successful scan can remove deleted entries, but a timeout/partial scan cannot. Missing binaries, unsupported schemas, missing checkouts, and deleted sessions get distinct actionable errors. A failed resume must never silently create a new conversation or select the provider's latest session.

## Session host and navigation

Replace the single Agent slot's ownership with a per-checkout session host, retaining terminal entities by `SessionKey` and an active selection. Give newly created conversations a temporary launch identity until the native session ID is authoritatively observed. Do not guess identity from title, newest timestamp, or cwd. Reconcile that temporary entry with the discovered native key without creating duplicates. In-agent `/new` or `/resume` transitions also need identity reconciliation; verify each provider's usable identity signal in the spike.

Both UI surfaces dispatch the same `OpenAgentSession(SessionKey)` action:

1. Resolve fresh catalog metadata and validate provider, store, session, cwd, and checkout. Preserve the current view on validation failure.
2. Resolve the existing repository binding, or an already-open ad-hoc checkout, using canonical identity. Do not reinterpret a stale binding as permission to launch in a different checkout.
3. If this exact session already has a retained live pane, switch to it without spawning. Coalesce repeated clicks and ignore stale asynchronous navigation completions.
4. Otherwise prepare the exact resume intent and create its managed terminal. Route directly so opening a new workspace does not also eagerly launch an unused default agent.
5. Activate the destination workspace's Agent tab, highlight the session, and focus its terminal. Retain the previous session and its output handling. Surface spawn/CLI resume failures with Retry and a route back to the previous pane.

Resume arguments must compose with existing OpenCode ports/environment, Claude hooks, and process-exit markers. Keep executable/arguments/environment structured until the existing shell-quoting seam. Treat IDs/paths as data; never interpolate raw titles or metadata into shell text. Preserve login-shell initialization and ordinary terminal keyboard/resize behavior.

Track activity by launch identity rather than overwriting the checkout record. A session host associates each launch with its session key; activity snapshots can still provide a checkout aggregate for Cmd+P. Update warning navigation so multiple blocked sessions in one checkout are individually reachable. Visibility acknowledgment applies only to the visible launch. Closing one pane must not erase another pane's activity.

Provide an explicit close action for retained sessions; this releases the managed PTY, not provider history. Confirm closing a working/blocked session. Do not evict live sessions automatically when the recent-sessions limit is exceeded. Keep open sessions reachable in a small separate **Open sessions** group if they fall outside the recent list; avoid duplicating rows already in Recents. Window shutdown owns cleanup. Discovery of an externally running session is not attachment to its terminal; do not claim external process reuse. Cross-process duplicate-open detection is a compatibility check, and a known conflict must be surfaced instead of launching a competing writer.

## UI details

- Reuse Home card spacing and responsive grid sizing. Add keyboard focus and Enter activation; card/row event payloads carry stable keys, never list indexes.
- Sidebar is a fixed 260 px and scrolls independently; it is not collapsible. The terminal fills the remaining width and sizes its PTY grid from its own painted bounds, not the window viewport.
- Show `Codex · 12m ago` under the title, with the full title and timestamp available in a tooltip. Agent labels identify the coding harness, not its model.
- Keep selection/focus stable as rows reorder with new activity. Clicking an entry must still open that entry if a refresh completes during the click.
- Provide loading, empty, stale/partial-provider, and retry states. No sessions is a normal state, not an error. New session remains available.

## Implementation sequence

1. **Compatibility fixtures and decisions.** Complete the provider spike and resolve the two proposed defaults. Prove list-to-resume identity for each provider, including externally created sessions and new managed-session identity capture. Document supported versions and any limitations.
2. **Catalog module.** Add `src/agent_sessions/{mod,model,cache}.rs` and private `providers/{mod,opencode,codex,claude}.rs`. Implement normalization, checkout resolution, complete top-N merging, health, and refresh generations. Register the module in the crate root.
3. **Launch and lifetime.** Add typed launch intent, exact resume preparation, per-checkout session host, duplicate-click protection, close behavior, and per-launch activity ownership. Update `workspace.rs`, `pane.rs`, `session.rs`, and `agent_activity/`; preserve new-session behavior and settings persistence.
4. **Agent sidebar.** Add `src/agent_sessions/view.rs` (or the project's equivalent view module), shared snapshot subscription, active selection, relative times, collapse behavior, and New session. Validate mixed-provider navigation before adding Home.
5. **Home routing.** Add four Recent Activity cards and `HomeEvent::OpenAgentSession`; connect to the same navigation path. Query all registered checkout bindings, not just Home's four recent projects.
6. **Integration and documentation.** Exercise refresh/deletion/failure paths, run the required checks, and document provider compatibility, local-cache behavior, and the adapter checklist. No history migration is required.

## Verification and acceptance

- Catalog tests with an injected clock and temporary provider roots: cross-provider recency, timestamp units/ties/fallbacks, duplicate IDs across stores, title fallback, malformed/truncated input, child filtering, pagination, deleted records, partial failures, cache rebuild, and stale refresh rejection.
- Repository tests: symlink aliases, subdirectory cwd, nested repos, linked worktrees, two clones with the same remote/name, missing checkout, changed binding, and sessions in registered projects outside Home's project-card limit.
- Launch tests: exact IDs and cwd, custom provider roots, shell metacharacters, composed observer settings, repeated clicks, same-session reuse, different sessions of the same provider, cross-provider switch, temporary-to-native identity reconciliation, and failed resume preserving a recoverable previous pane.
- Activity/lifetime tests: two launches in one checkout do not overwrite each other; only the visible launch acknowledges completion; blockers remain individually reachable; closing one leaves the other observed; selecting history never changes the default agent.
- UI tests where practical: shared ordering, stable-key navigation during refresh, full-row selection, loading/empty/partial states, and collapse/resize behavior.
- Desktop smoke test with all three actual CLIs: create sessions inside and outside Devcroft in at least two repositories; verify Home's newest four and each sidebar's configured recent sessions. Open every provider from both surfaces, continue the exact conversation, switch back to a retained running session, restart Devcroft, and verify local rediscovery. Include busy background sessions and stale/deleted targets.
- Measure a large history fixture: first cached paint requires no provider scan, refresh never blocks scrolling/typing, and polling does not repeatedly parse unchanged transcripts. Record actual timings and scanned bytes before release.
- Run touched Rust formatting checks, relevant tests, `cargo test`, strict Clippy, and `git diff --check` during implementation. Distinguish automated checks from the required desktop smoke test. This Markdown-only planning change needs document/link review and diff checks.

To add another agent later, register a provider adapter, normalize its metadata, implement exact resume preparation, and supply compatibility fixtures. The catalog, checkout routing, Home cards, sidebar, and local-storage contract should remain unchanged.

## Compatibility and known limitations (v1, verified 2026-09-10)

- Inspected versions: OpenCode 1.18.30, Claude 2.1.266, Codex with `resume <id>` (picker filters by cwd by default; `--all` disables it). OpenCode discovery reads `$XDG_DATA_HOME/opencode/opencode.db` read-only (`session` rows with no `parent_id` and no `time_archived`), with millisecond `time_updated` timestamps and `id`/`title`/`directory` fields; the store query covers every project regardless of this process's working directory.
- Launcher shims (mise) print a notice to stdout ahead of the Codex app-server stream. Discovery sets `MISE_QUIET=1` and still skips preamble lines defensively.
- Codex has no terminal activity observer, so a newly created Codex session reconciles identity only when launched through exact resume (native ID known upfront). An in-terminal `/new` keeps the launch's temporary identity instead of adopting the new thread.
- No cross-process duplicate-open detection yet: opening a session that is already running in another window or process launches a competing writer instead of surfacing the conflict.
- The cache at `$DEVCROFT_DATA_DIR/cache/agent-sessions/index.json` is versioned (v1), atomically written with owner-only permissions, and rebuilt when corrupt. Claude transcripts are fingerprinted by size/mtime and fully re-parsed on change; byte-offset resumption is future work.
