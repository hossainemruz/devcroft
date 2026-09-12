# Agent status in the project command panel

Status: implemented; automated verification passes. Interactive validation with real agent approval dialogs remains a release check.

## Implementation notes

The implementation lives in `src/agent_activity/{mod,providers,terminal}.rs`. The shared store owns checkout identity, launch generations, request correlation, background completion, and acknowledgement; provider adapters supply observations. Adding an agent requires a provider preparation/event mapping and/or terminal classifier, without changing the workspace UI.

Cmd+P project rows show agent state. The warning immediately after the command trigger counts blocked Agent panes, opens a filtered project picker, and disappears when no blockers remain. Selecting a warning result opens that checkout's Agent tab. Only agents launched in this window are tracked; OMP detection is intentionally unavailable.

The implementation uses a shell OSC exit sentinel instead of the proposed helper process, preserving the existing shell's job control. Detection requires the actual escape sequence, so echoed startup commands cannot signal an exit. Provider secrets are passed through the process environment and unset when the managed command returns. The Claude plugin is temporary and removed when its pane is dropped.

OpenCode uses authenticated loopback SSE and reconciles working sessions through `/session/status` every second independently of stream delivery. Structured status takes precedence over terminal working/idle hints, preventing late terminal frames from reviving completed turns. Claude uses observational HTTP hooks. Codex uses current title and screen evidence. Terminal observations read the active grid independently of scrollback, at most ten times per second after output. Terminal wording can change between CLI versions; no claim is made that synthetic fixtures replace interactive compatibility testing.

Installed versions inspected: OpenCode 1.18.30, Claude Code 2.1.266, and Codex CLI 0.153.4. OpenCode's port/hostname options and Claude's plugin-directory option were verified locally. Tests cover request resolution and reordering, concurrent sessions, interruptions, background completion acknowledgement, stale generations, exit markers, fragmented SSE/UTF-8, and live-screen reads while scrolled back.

The sections below retain the original investigation and proposed sequence for context. The helper-process design and separate provider-file layout were superseded by the implementation described above. Real CLI approval, cancellation, suspend/resume, and GUI interaction checks remain manual release validation.

Investigated on 2026-09-09 against Rust Devcroft `dc445ae` and Electron Devcroft `23b4de3`.

## Outcome and scope

Show each running project's agent status in the project picker opened with Cmd+P, and show `⚠ 2 agents need attention` immediately to the right of the persistent command-bar trigger. Update both while repositories are in the background. Remove the warning as soon as the last actual blocker is resolved, canceled, or its agent exits.

Initial detection supports OpenCode, Claude Code, and Codex. Keep the existing OMP launcher and saved preferences working, but give it no detection adapter. Its row says `Status unavailable` while running.

Scope assumption: track Agent panes launched by this Devcroft window, including those retained in `inactive_repositories`. Agents launched in unrelated terminals, other applications, other Devcroft processes, or remote machines are a later discovery feature. Provider subagents contribute to their owning Agent pane; they are not additional agents in the warning count. This matches the current one-Agent-pane-per-checkout model while allowing more sessions later.

## What the existing implementations tell us

### Rust Devcroft

- `src/agent.rs` already identifies the four launchers. Detection capability must be separate from launcher availability.
- `src/session.rs::TerminalSession::spawn` starts a login shell, writes the agent command into its PTY, and feeds output into Ghostty. The stored child is the shell, so shell liveness/PTY EOF cannot tell us when an agent command has exited back to its prompt.
- `src/pane.rs::TerminalPane::new` consumes output even while hidden. Its paced presentation loop is an existing place to request coalesced terminal observations, but detection must not depend on drawing a visible pane.
- `src/workspace.rs::RepositoryTabs` and `restore_repository_tabs` keep background terminals alive. Their checkout identity must also retain activity ownership and subscriptions.
- `render_command_bar` builds project rows from `src/command_palette.rs`; the persistent trigger is rendered separately in the workspace header. Both need a shared activity projection.
- Palette confirmations resolve against `palette_model`. Live status changes must preserve this render/confirm agreement and the selected repository.
- The pinned Ghostty wrapper exposes `Terminal::title`, `on_title_changed`, and `active_screen`. The installed command widget supports `CommandItem::child` for custom row content while retaining a searchable label and keywords.

### Electron Devcroft

Reference root: `/home/emruz/Projects/personal/hossainemruz/devcroft/apps/desktop/src/`.

Read `shared/agent-types.ts` and `main/agents/{agent-activity-service,agent-activity-reducer,agent-provider,opencode-agent-adapter,claude-agent-adapter}.ts`.

The old implementation uses an OpenCode loopback SSE connection and a temporary Claude plugin with hook callbacks. A shared reducer tracks sessions, unresolved requests, launch generations, visibility, and unseen completion. It already rejects obsolete launch events and remembers resolved request IDs to handle duplicate or reordered delivery. Codex is not a dedicated provider there.

Reuse these concepts and the adapter fixtures, not the Electron IPC or renderer architecture. Improve two behaviors: do not clear every pending request on any tool activity when tools can run concurrently; and distinguish failed/interrupted turns from successful completion. The old Claude adapter resolves all requests for a provider session on several activity hooks, which needs narrower correlation in this implementation.

### Herdr

Herdr's [agent-detection directory](https://github.com/herdrdev/herdr/tree/master/distribution/agent-detection) provides versioned, provider-specific rules with priority, terminal regions, and state evidence. The inspected files were [Codex](https://github.com/herdrdev/herdr/blob/master/distribution/agent-detection/codex.toml) (`2026.08.28.1`), [Claude](https://github.com/herdrdev/herdr/blob/master/distribution/agent-detection/claude.toml) (`2026.09.04.1`), and [OpenCode](https://github.com/herdrdev/herdr/blob/master/distribution/agent-detection/opencode.toml) (`2026.06.10.1`).

Codex rules use title activity, an action-required title, and live approval/question controls. Claude also has exclusions for transcript views and menus; OpenCode recognizes permission controls and interrupt/progress hints. This demonstrates why scanning accumulated output for words like “permission” is insufficient. Adopt region-aware observations as a fallback and for Codex initially. The distribution files alone do not establish Herdr's complete runtime state machine. Verify upstream licensing and preserve attribution before copying rule expressions; do not download executable rules at runtime.

## Recommended approach

Use a hybrid detector behind one provider-neutral activity module. Prefer structured evidence where the existing CLI workflow exposes it; use bounded terminal observations for missing signals.

| Provider | Initial adapter | Why / limitation |
| --- | --- | --- |
| OpenCode | Authenticated SSE from the launched TUI's local server; snapshot reconciliation on connect/reconnect | Reuses the old implementation's approach and supplies request identities. Requires verifying current event schemas and server startup flags. |
| Claude Code | Per-launch plugin with observational hooks; terminal evidence for startup dialogs and gaps in resolution events | Keeps the interactive CLI and user settings. Hook ordering and parallel requests need explicit handling. |
| Codex | Current terminal title plus region-aware live-screen classifier | Requires no hook installation and fits the existing PTY. Detection is inferred and version-sensitive; unknown layouts must remain unknown. |

OpenCode documents its TUI/server arrangement, event stream, server authentication, and session status endpoint in the [server reference](https://opencode.ai/docs/server/). Use the installed version's OpenAPI schema to verify permission/question reconciliation endpoints and event payloads before coding against them.

Claude documents lifecycle, permission, tool, notification, and stop hooks in the [hooks reference](https://code.claude.com/docs/en/hooks). Hook callbacks must observe only: return success without instructions, approvals, denials, or changes to tool input. Keep callback latency bounded.

Codex now documents lifecycle hooks, including prompt, tool, permission, interrupt, and stop events. Non-managed definitions require trust review, and hook coverage does not establish a complete question/approval-resolution stream. Therefore hooks are an optional subsequent adapter improvement, not a v1 prerequisite. Keep hook definitions stable if added later; never bypass trust automatically. See [Codex hooks](https://learn.chatgpt.com/docs/hooks). Its legacy `notify` mechanism only documents turn completion, so it cannot implement this feature alone; terminal notifications also do not provide a complete unblock lifecycle. See [advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications).

Alternatives considered: all-provider screen matching is easier to launch but more sensitive to layout changes; hooks for everything introduce setup and incomplete event coverage; switching to SDK/headless/app-server clients would expand this into a new agent UI. The hybrid keeps the existing interactive terminals and limits provider-specific maintenance to adapters.

## Status and attention semantics

Keep actual activity, observation health, and unseen completion separate internally. Derive display state from them.

| Display | Meaning | Counts as needing attention? |
| --- | --- | --- |
| Starting | Agent command started; readiness not yet observed | No |
| Idle | Ready for a prompt, with no active turn or unseen completion | No |
| Working | At least one tracked turn/subagent is active and no blocker is confirmed | No |
| Needs attention | An approval, question, trust dialog, or other supported input dialog is unresolved | Yes |
| Finished | An observed work cycle ended normally and has not been acknowledged | No |
| Status unavailable | Unsupported provider, lost observation, or unrecognized state | No; expose the uncertainty |
| No agent | No live managed agent for this checkout | No |

`Finished` means the agent's turn ended, not that its requested task was objectively completed. Add an outcome detail for interruption or failure; neither should create a successful-finish badge. A recoverable tool failure can still be part of a working turn.

Transition rules:

1. Launch begins a new generation and resets previous observations, requests, and completion.
2. Readiness produces Idle. Positive work evidence produces Working and clears unseen completion. Quiet output never means Finished.
3. Opening a request records `(provider_session_id, request_id)` and produces Needs attention. Repeated evidence must not increase the agent count.
4. Resolving/rejecting/canceling that request removes only that request. If another blocker remains, attention remains. Otherwise derive Working or Idle from fresh activity evidence.
5. A normal end of an observed work cycle sets unseen completion only after all tracked work ends and no requests remain. Root/child ownership prevents a child completion from finishing a working parent.
6. Acknowledge completion when its Agent pane is actually visible in a focused window, or clear it on new work. Merely opening Cmd+P, highlighting a row, switching to its Editor, or opening Home does not acknowledge completion. Opening the Agent pane never clears an unresolved request.
7. Agent exit, pane disposal, or replacement removes its live activity and blockers. Do not treat a successful process exit as proof of a successfully completed turn.
8. On observation failure, show Status unavailable and retain last-known state only as diagnostic detail. This is loss of knowledge, not evidence of unblocking. A reconnect alone must not restore Idle: reconcile state or await fresh authoritative evidence. If all previously blocked agents lose observation, replace the stale warning with a compact `Agent status unavailable` indication so it does not imply all blockers were resolved.

For terminal-derived blockers, replace the previous screen observation when a fresh, complete live dialog region shows it gone. Require a stable frame or positive work/idle evidence to avoid clearing during a partial redraw. Input keystrokes alone, timeouts, and output elsewhere are not proof of resolution.

## Module design and ownership

Create `src/agent_activity/` as a deep module. Its small external interface owns launch preparation/lifetime, provider normalization, the reducer, and read-only projections. UI callers should not know event names, regular expressions, request correlation rules, or transport reconnection behavior.

Suggested external shape (design sketch, not final Rust signatures):

```rust
prepare_launch(checkout, agent_kind) -> PreparedAgentLaunch
attach(prepared, terminal_identity) -> AgentActivityHandle
observe_terminal(handle, observation)
set_visible(handle, visible_and_window_focused)
snapshot() -> ActivitySnapshot
// Dropping the launch handle cancels observation and removes its generation.
```

`PreparedAgentLaunch` holds an executable, argument vector, environment additions, and owned runtime resources. Adapters sit at a private seam with three real implementations. A static registration table keyed by `AgentKind` is sufficient; no dynamic plugin engine or user-editable detection language is required initially.

Use a single workspace-owned activity store/entity, with a cancellable handle owned by each Agent pane. Send normalized events through a channel and reduce them on the GPUI side; perform networking and classification off the UI thread. The workspace subscribes once to projection changes, including updates from hidden panes. Avoid cycles by using weak callbacks.

Each record contains a unique launch ID/generation, canonical checkout path, agent kind, provider session/turn identities, pending requests, bounded resolved-request tombstones, current evidence/health, outcome, unseen completion, and timestamp. Reject events for retired generations. Use provider sequence IDs when available; serialize events per connection and never assume callback arrival order is causal order. Request tombstones expire when their turn/session is retired, with a hard bound for malformed sources.

Key activity by canonical checkout plus launch identity, not project display name or portable repository key. Map `RecentRepository.checkout_path` to this identity for rendering. Keep live records visible even if a portable record is renamed or temporarily absent: include an open-checkout row with a path fallback. Multiple registered aliases of the same checkout must not inflate counts. No activity state belongs in `device.json`, task status, or portable Git sync.

## Launch and terminal integration

Replace the Agent-tab-only command string with the structured launch description; retain existing Editor and Terminal startup behavior. Preserve login-shell initialization and PATH discovery. At the final shell seam, use deliberate shell quoting for executable/arguments and test paths with spaces and quotes.

Add a hidden headless `devcroft` helper to run the prepared agent with inherited terminal I/O and report command start/exit through authenticated local IPC. This gives an actual agent lifetime independent of the surrounding login shell. Resolve the helper via the running executable's absolute path. Dispatch it before GPUI initialization in `main.rs`/`cli.rs`.

Validate job control and signals in the initial spike: Ctrl+C must interrupt the agent as it does today, terminal resize and suspend/resume must survive the wrapper, and closing/restarting a pane must clean up the owned agent/process group and detector resources. PTY EOF and helper-channel closure are additional cleanup signals. Handle missing binaries and failures before telemetry attaches. Manually running a second agent after the managed command exits is outside v1 unless routed through the helper.

Extract a compact `TerminalObservation` from Ghostty: title, monotonically increasing revision, terminal dimensions, live screen text, and relevant prompt/dialog regions. Read the live application screen independently of user scrollback; do not mutate the visible viewport to inspect it. Reuse the existing VT parser rather than searching raw ANSI chunks or creating a second terminal emulator. Verify live-screen text extraction in the spike; `TerminalPane.rows` alone is not a valid substitute when the user scrolls up.

Coalesce observations to a target of at most 10 per second per active-output Agent pane, with one trailing observation after the last output. No screen extraction on idle polling and no classification during rendering. Keep current title evidence scoped to the current launch so a shell title cannot turn an exited agent into Idle. Emit GPUI notifications only when the projected status/detail changes.

## Adapter work

### OpenCode

Prepare a loopback-only server address and per-launch password, following the old adapter's `--hostname`, `--port`, and `OPENCODE_SERVER_PASSWORD` pattern after verifying supported flags. Do not override unrelated settings. Detect port allocation/bind races, limit startup retries, and fall back to an ordinary launch with unavailable telemetry if preparation fails.

Consume `/event`; normalize busy/retry to work, idle to turn end, and supported permission/question asked/replied/rejected events to request lifecycle events. Filter by instance, checkout, and session ownership; a dedicated server can still expose stored or unrelated sessions. Register children under their root and ignore historical idle sessions when deriving completion.

On initial connection and reconnect, obtain current session and pending-request snapshots using verified endpoints. Buffer events while reconciling and apply a documented ordering strategy; if the protocol cannot provide an atomic snapshot, converge with a follow-up snapshot rather than presenting a guessed Idle. Bound SSE frame sizes, back off on transport errors, and keep healthy quiet streams open. Never resend prompts or answer requests through this connection.

### Claude Code

Materialize a private per-launch plugin and add `--plugin-dir`, as in Electron. Keep user/global/project configuration intact. Prefer short observational HTTP hooks where supported, with a per-launch callback token; use the same headless helper for command hooks if required by the tested version. Receiver failures must not block agent work.

Map session start/end, prompt submission, tool activity, stop/failure, and permission hooks into normalized evidence. Track `AskUserQuestion` and supported elicitation dialogs explicitly; do not interpret every notification or ordinary idle prompt as a blocker. A permission hook can precede an automatic approval by another hook, so confirm actual waiting or reconcile immediately with subsequent evidence.

Correlate tool/request identities when present. When a hook lacks a resolution ID, clear only the affected dialog/session after verified continuation or terminal disappearance; do not copy the old “clear all on any tool event” behavior. Test two simultaneous requests and a denied request. A Stop hook may be followed by another hook continuing the turn, so corroborate completion with settled activity/idle evidence. Account for background children before showing Finished.

### Codex

Start with compiled, versioned rules for title work/attention and live approval, question, trust, working, and idle regions. Preserve observations during transcript viewers and unrelated menus. Prefer strong live controls to weak text matches; exclude prompt text, historical transcript content, and stale titles. Do not assume every non-empty title means Idle unless the observed version establishes an agent-owned title convention.

Use observed Working → settled Idle to infer completion, excluding interrupted/error layouts. Mark detection source as inferred in diagnostic details. Unknown layouts yield Status unavailable instead of false certainty. Keep any fallback expressions and fixtures inside this adapter so adding another provider does not affect Codex logic or UI code.

An implementation spike must demonstrate both opening and clearing approval/question warnings on the installed Codex version, including denial, cancel, and subsequent long-running tool execution. If passive detection cannot satisfy those cases, strengthen the adapter with tested lifecycle hooks before calling Codex support complete; do not silently ship a completion-only notifier. Hook setup would need an explicit user-facing opt-in/trust flow and stable definitions, and is a separate scope decision if necessary.

## UI changes

In Cmd+P, render a trailing agent label and icon/text status for each checkout row, for example `api-server    Codex · Working` and `web-app    Claude · Needs attention`. Use `CommandItem::child`, retaining labels/keywords for search and explicitly drawing the current-project check since custom content owns its presentation. Color supplements text: neutral idle, activity working, green finished, amber attention, muted unavailable.

Keep recency ordering stable during live updates. Do not reorder rows under the user's selection. Make provider names and status words searchable; preserve selection by stable checkout/item identity if a status-filtered result disappears. Normal repository selection keeps its current behavior.

Add the warning immediately after the persistent command trigger, visible even when Cmd+P is closed and on Home/Resources/Review. Use singular/plural forms and omit the element when no attention or observation-loss indication is needed. Count distinct live Agent panes with confirmed blockers, including the current pane. The count is independent of the palette's search filter.

Clicking the warning opens Cmd+P in an attention-only view using the same project model; choosing a row switches to that existing checkout and focuses its Agent pane. This explicit action does not acknowledge its blocker. Keep the filtered model and confirmation target synchronized if an agent unblocks while the picker is open. Ensure narrow headers truncate gracefully and expose accessible labels/tooltips with provider, state, and observation health.

## Implementation sequence

1. **Capture compatibility fixtures and settle launch mechanics.** Record the three installed CLI versions and sanitized observations for idle, working, finished, approvals, questions, denial, cancellation, interruption, exit, and background work. Inspect current OpenCode schemas, Claude hook payloads, and Codex title/layout behavior. Prove the helper preserves terminal behavior and Ghostty can read the live screen while scrolled back. Save the matrix in `docs/agent-status.md`; this is a release gate, not an assumption that all provider versions behave identically.
2. **Implement the module and reducer.** Add `src/agent_activity/{mod,model,reducer}.rs`, generation-scoped handles, snapshots, aggregation, and deterministic reducer tests with an injected clock. Keep provider/transport details private.
3. **Wire managed launch and lifetime.** Add preparation/runtime ownership, local IPC and helper dispatch; update Agent-pane launch paths in `session.rs`, `pane.rs`, `workspace.rs`, `cli.rs`, and `main.rs`. Verify hidden panes keep observations alive and replacement/drop retires the old generation.
4. **Add observations and adapters.** Add `terminal.rs`, `providers/{mod,opencode,claude,codex}.rs`, private transport helpers, packaged Claude hook assets, and provider fixtures under `tests/fixtures/agent-activity/`. Implement OpenCode reconciliation and Claude correlation, then validate Codex fallbacks. Choose only the HTTP/SSE/regex dependencies actually needed and record them in `Cargo.toml`/`Cargo.lock`; do not add a generic rule DSL.
5. **Connect both UI surfaces.** Extend the palette projection in `command_palette.rs`, use custom row rendering and the warning trigger in `workspace.rs`, wire focus/visibility acknowledgment, and handle live filtering without target drift. Use theme tokens rather than hard-coded provider colors.
6. **Verify end to end and document support.** Run relevant tests and manual multi-repository scenarios. Document tested versions, inferred versus structured detection, degraded-state behavior, cleanup, and the checklist for adding an adapter. OMP detection remains excluded.

Runtime files should use an application-owned temporary directory with restricted permissions and a separate namespace per process/launch. Authenticate callbacks, cap input size and queues, redact tokens and content, and retain only status metadata. Cancel receivers/readers and remove only owned resources on disposal. No global agent settings or repository files should be modified for v1 detection.

## Verification and acceptance criteria

- Reducer replay: idle → working → request → resolution → working → finished; two requests with one resolution; duplicate/reordered events; stale-generation events; root/child completion; interrupted/error outcomes; telemetry loss/recovery; focused versus background completion; removal and relaunch.
- Adapter replay: fragmented SSE, malformed/oversized input, disconnect and reconciliation; duplicate Claude permission notifications, concurrent tools, user questions, automatic approval, denial, and hook-driven continuation; split ANSI/title writes, resize, partial redraw, transcript/menu views, scrollback containing old approvals, unknown Codex layouts, and persistent background work.
- Lifetime: missing binary, helper/receiver failure, agent exits but shell stays alive, pane restart, window close, port conflict, two instances, checkout aliases, and switch-away/switch-back without duplicate registration. Verify Ctrl+C, suspend/resume, resize, and paste in each real CLI.
- UI/model: count agents rather than requests; exclude Finished/Idle/OMP from attention count; retain current-project marking and search; no wrong-target confirmation during updates; warning click focuses the existing Agent pane; merely opening a palette does not clear attention or completion.
- Manual scenario: run agents in three repositories, block two, and work in the third. The header says `⚠ 2 agents need attention`. Resolve one and it becomes `⚠ 1 agent needs attention`; resolve the other and it disappears without reopening the panel. Repeat with cancellation, denial, and exit. Completed background work shows Finished until its Agent pane is viewed or starts again.
- Performance target: structured events repaint on the next UI update; settled terminal evidence updates within roughly 250 ms under normal load, measured from receipt of provider evidence. Idle agents incur no periodic screen scans. Show degraded observation explicitly rather than claiming this latency when a provider emits no usable signal.

For implementation, run `cargo fmt --check` and the relevant reducer, adapter, palette, and lifecycle tests, then the repository's required checks. This planning change itself needs only Markdown/link and diff review; it does not launch agents or alter their configuration.

## Extending support

To add a provider: register its launcher if new, implement the private adapter seam, declare available evidence/capabilities, add versioned fixtures covering the acceptance matrix, and document supported versions. The shared reducer, repository ownership, warning count, and UI rendering should remain unchanged. External-session discovery and cross-window aggregation can later supply additional launch identities through the same module, but require their own ownership and focus-routing design.
