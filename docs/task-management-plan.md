# Task management plan

Status: agreed product scope and implementation sequence; PRs 1–3's task/artifact stores, linking, sync coordination, and headless CLI are implemented. Concrete contracts are in [`task-storage.md`](task-storage.md) and [`task-cli.md`](task-cli.md), with shared [agent instructions](task-agent-instructions.md). PRs 4–5 remain planned; no task/artifact UI is implemented yet. This document is authoritative for the Rust task/artifact feature and supersedes the historical task model in `feature-parity.md` §§2, 9, and 11 and the task-specific assumptions in `cli-plan.md`. Existing review-comment and review-tutorial contracts are unchanged.

## Goal and boundaries

Build a personal, agent-managed planning workspace with persistent ideas, cross-repository tasks, standalone Markdown artifacts, and visible subtask progress. Keep the desktop read-focused and let agents create and iterate on records through a shared headless CLI. OpenCode and Claude CLI use the same interface; neither requires a provider-specific runtime integration.

Tasks and artifacts live in Devcroft's portable directory, independent of source repositories. Sharing documents, gathering team feedback, and obtaining approval happen outside Devcroft and must not shape the first-release design. Agents may read or update records whether launched in the Agent tab, another Devcroft terminal, or an external terminal; the desktop need not be running.

## Representative workflow

1. Ask an agent to persist an idea as a task with a title and requirements in its description. Repositories and subtasks may be unknown.
2. Later, research the idea with an agent and optionally create a standalone RFC artifact. An RFC can also be created before any task exists.
3. Read the RFC in Devcroft, copy its ID, and ask the agent to revise it. Any team discussion occurs outside Devcroft.
4. Ask the agent to prepare a Markdown plan, link the relevant RFC and plan to the task, and create PR-sized subtasks with repository assignments and dependencies.
5. Implement, for example, an API contract in `public-api`, a handler in `backend`, and configuration in `deployment`. Subtasks are small but need not be independently executable.
6. Ask the agent to update subtask status. Completion does not wait for a PR to merge and is independent of PR or deployment lifecycle.
7. If an RFC or plan changes, explicitly ask the agent to adjust task/subtask records. There is no automatic reconciliation.
8. Archive finished or abandoned work when it should leave active lists. Archived records remain readable by ID and through links.

## Domain model

### Task

- Stable ID, title, Markdown description, creation/update timestamps, and archive flag.
- Description holds requirements and context; no mandatory separate requirement objects, research reports, or result schema.
- Optional explicit repository keys, artifact links, and an ordered collection of subtasks.
- Creation permits no repositories, no artifacts, and no subtasks. Research, RFC creation, planning, and implementation are optional activities, not enforced task stages.
- Derived repository membership is the union of explicit task repository keys and all subtask repository keys. Explicit associations let an early task appear in a repository before breakdown.
- No subtasks means **Not planned**, not complete. Otherwise progress is completed subtasks divided by total subtasks; individual statuses remain visible. Do not maintain a second editable overall progress value.

### Subtask

- Stable task-local ID, title, Markdown description, one repository key, optional dependencies on other subtasks in the same task, artifact links, and status.
- Description contains objective, scope, completion criteria, and optional completion or blocker notes without requiring separate structured reports.
- Status is `todo`, `doing`, `blocked`, or `done`. No PR-review stage, automatic completion, or enforced execution state machine.
- Dependencies communicate ordering, not orchestration. Reject unknown dependencies, self-dependencies, and cycles; do not prevent an explicit status change merely because a dependency is unfinished.
- Allow subtask removal to support plan iteration. Reject removal while another subtask depends on it; the agent must adjust references first. Preserve existing IDs through updates and reordering, and do not reuse removed task-local IDs.

### Artifact

- Stable ID, title, kind (`rfc`, `plan`, or `note` initially), Markdown content, creation/update timestamps, and archive flag.
- Standalone lifecycle: no required task and no repository ownership. Multiple tasks or subtasks may link the same artifact.
- References always resolve to current content. No frozen revisions, approval states, or automatic propagation into linked tasks.
- A plan explains the approach; structured subtasks are the authoritative execution/progress record. Do not maintain synchronized completion checkboxes in the plan.
- Archive hides an artifact from default browsing but does not break ID lookup or existing links. Task archive does not archive its artifacts or mark unfinished subtasks done.

## Persistence and identifiers

Reuse the existing `DataRoot` and portable Git sync; do not introduce a database, source-repository copies, or a separate sync service. Expected layout:

```text
portable/
  tasks/
    task-k7m2q9d4/
      task.json
  artifacts/
    art-r3w8n6hp/
      artifact.json
```

Use `task-` and `art-` prefixes with eight random lowercase alphanumeric characters drawn from a visually unambiguous alphabet. Check for collisions at creation and retry rather than overwriting. This avoids a global sequence counter and makes offline multi-machine creation practical, but does not promise mathematical uniqueness or automatic Git conflict resolution. Use task-local subtask IDs such as `s1`, addressed together with the task ID.

One shared storage/domain implementation serves CLI and desktop. Validate identifier syntax before deriving paths, reject traversal, validate new repository/artifact references, preserve unknown fields where practical, and surface malformed records independently without hiding valid siblings. Existing missing references after external edits or sync must be visible and must not be silently removed. Lists are derived from records, not an authoritative catalog.

Use atomic writes, brief cross-process locking, and optimistic revision checks. Reads return opaque revision tokens; mutations require the token for the containing record, check it under the lock, and return the updated record and token. Subtask mutations check the containing task revision. Concurrent updates to different records can succeed; stale edits to the same record fail clearly and require rereading. Archived state is part of the same mutation contract. Revision detection must account for external content changes, not rely only on timestamps.

PR 2 settled coherence by revisiting the proposed two-file representation: artifact metadata and verbatim Markdown content are stored together in `artifact.json`, using a single atomic replacement and a revision over the entire record. There is no separate `content.md`, journal, or versioned document system. Failure-injection tests cover failed and interrupted writes; the concrete recovery and machine-local lock/sync contract is documented in [`task-storage.md`](task-storage.md).

No migration from the historical Electron task schemas is required by this feature. Unexpected legacy or unsupported records must be reported and never silently reset, migrated, or overwritten. Portable Git can retain committed history, but the first release does not expose artifact history or revision pinning.

## Headless CLI contract

Dispatch before GPUI initialization through the existing CLI scaffold. Command handlers call shared stores rather than implementing a second validation or persistence path.

```text
devcroft repository list
devcroft task list|get|create|update|archive|unarchive
devcroft subtask create|update|remove
devcroft artifact list|get|create|update|archive|unarchive
```

The command families above are scope commitments, not literal shell invocations. PR 3's exact argument names and JSON shapes are documented in [`task-cli.md`](task-cli.md), following these requirements:

- Human-readable output by default, stable machine-readable output via `--json`, diagnostics on stderr, and existing CLI success/runtime/usage exit conventions. No socket or app-not-running error for store operations.
- Markdown input accepts a file or stdin so agents need not place large documents in shell arguments. Specify UTF-8 and bounded input sizes with actionable errors.
- Creation returns the generated ID. Reads and successful mutations return revision tokens. Mutation input supports explicit field clearing without conflating omitted fields with empty values.
- Task retrieval returns the whole task, derived progress/repository membership, all subtasks, and artifact references. It does not silently inline every referenced document; agents retrieve artifact content by ID.
- Artifact retrieval returns current metadata and Markdown content. Resolve IDs independently of cwd, using the selected Devcroft data root.
- Task listing supports an explicit repository filter using derived membership. Default task/artifact lists exclude archived records; provide archived inclusion and bounded listing. Direct ID reads include archived records.
- Support modifying task repository associations, task/subtask artifact links, subtask ordering, dependencies, and statuses without recreating stable identities.
- Errors identify stale revisions, invalid input, missing records/references, malformed data, and storage failures. Do not silently clobber or partially apply a failed record mutation.

Provide one shared agent instruction document with thin OpenCode and Claude setup guidance. Explain discovery via `--help`, repository keys, creating ideas, standalone artifacts, linking documents, PR-sized breakdown, explicit progress updates, archive semantics, and stale-write recovery. Do not automatically edit user agent configuration. Once configured, copying an ID and referring to it in the agent conversation is sufficient; no custom prompt generator is needed.

## Desktop experience

### Global Tasks and Home

- List active tasks, including repository-less ideas, with title, copyable ID, repository badges, and progress. Support archived browsing and archive/unarchive.
- Show task description, all subtasks with status/repository/dependencies, and task/subtask artifact links in a detail view.
- Replace Home's dummy recent tasks with real summaries opening the task detail view.
- Keep the first release read-focused: creation and content/status iteration primarily happen through agents. Rich task forms and a Markdown editor are not required.

### Repository Tasks

- Filter the task list by derived repository membership, not artifact associations or checkout paths.
- Opening a matching task shows the full cross-repository breakdown, including dependencies and work in other repositories.
- Artifact links open the shared Markdown viewer/popup. No independent repository-artifact association model or repository artifact catalog is needed.

### Global Artifacts

- Browse standalone artifacts by title, kind, and copyable ID, including an archived view.
- Open rendered Markdown with a copyable artifact ID; reuse existing Markdown rendering where suitable.
- Use the same viewer for task/subtask links, including links to archived artifacts. Show a clear missing-artifact state rather than hiding broken links.
- No inline feedback, generated revision instructions, approval workflow, or history browser.

### Refresh

Reflect CLI mutations while relevant views are open without requiring navigation away and back. Prefer a bounded background refresh mechanism consistent with existing app patterns, plus reload after portable sync/branch changes and manual refresh. Keep filesystem work off the GPUI UI thread, avoid overlapping unbounded scans, and preserve selection and scroll position where practical. A desktop-specific IPC server is not required for this feature.

## PR-sized implementation sequence

### PR 1 — Task storage and domain model (Done)

Implement records, ID allocation, task/subtask mutations, repository references, dependency validation, progress derivation, archive, revision checks, and cross-process mutation protection. Inspect existing data-store and sync primitives before deciding which can be reused safely across processes.

Acceptance: isolated tests cover empty ideas, unknown repositories, multi-repository membership, stable subtask identities, ordering/removal, dependencies and cycles, all statuses, zero-subtask progress, archive, stale writes, collision retry, malformed siblings, unsupported records, and atomic failure behavior. Document concrete serialized fields and lock/sync coordination.

### PR 2 — Artifact storage and linking (Done)

Implement standalone Markdown artifacts, kinds, safe metadata/content updates, revision tokens, archive, and task/subtask links. Depends on PR 1's shared ID/mutation conventions.

Acceptance: tests cover standalone/shared artifacts, live content resolution, metadata-only/content-only changes, stale writes including externally edited Markdown, coherent reads and interrupted writes, missing references, archived links, and path validation. Settle the multi-file persistence decision before treating this storage contract as complete.

### PR 3 — Headless CLI and agent instructions (Done)

Expose repository discovery and task/subtask/artifact command families with JSON output, Markdown input, explicit filters, and shared agent instructions. Depends on PRs 1–2.

Acceptance: binary integration tests use an isolated data root to create an idea, create/link RFC and plan artifacts, associate repositories, create dependent subtasks, update progress, revise content, remove/reorder subtasks safely, archive/unarchive, and retrieve archived IDs without starting GPUI. Test invalid input and stale-revision errors. Document and exercise the same workflow for both agent CLIs without provider-specific runtime code.

This is the first usable milestone: agents can manage persistent tasks and artifacts before desktop browsing is complete.

### PR 4 — Artifact browsing and viewer

Add global artifact browsing, Markdown viewer/popup, copy IDs, archive/unarchive, archived access, and refresh. Depends on PR 2; use PR 3 for agent-driven smoke validation when available.

Acceptance: find and read an RFC before any task exists, view archived artifacts, display malformed/missing content clearly, and see CLI revisions reflected in the open viewer. Verify keyboard navigation and copy behavior.

### PR 5 — Global and repository task views

Add global/repository task lists, complete task detail, subtask progress/status/dependencies, artifact links, archive controls, refresh, and real Home recent-task summaries. Depends on task stores and integrates PR 4's viewer; task-view development can otherwise overlap artifact-view development.

Acceptance: exercise the API → backend → deployment example from every repository, showing only relevant tasks in lists but all subtasks in detail. Verify repository-less ideas globally, Not planned versus complete, archived browsing, task/subtask artifact links, stale/missing records, and CLI/sync refresh. Confirm task progress is unaffected by PR state.

## Validation and implementation touchpoints

Expected touchpoints include `src/data/` for shared stores, `src/cli.rs` and headless dispatch for commands, the existing Home/workspace navigation, and Markdown preview/rendering. These are guidance, not a strict file allowlist; inspect current source before each PR. Existing plans and comments are evidence of intent, not proof that a capability is already implemented.

Run bounded tests during development and the repository's applicable `mise run fmt`, `mise run check`, and `mise run test` tasks for each implementation PR. Add headless binary integration coverage and manual UI smoke checks for desktop PRs. Use temporary `DEVCROFT_DATA_DIR` roots; never mutate the real portable directory, contact its sync remote, or alter real agent configuration during validation. Any extra dependency installation or external writes require approval.

## Non-goals

- Team collaboration, publishing/export integration, approval states, or comments.
- PR synchronization, automatic completion, or deployment tracking.
- Agent orchestration, assignments, claiming, provider-specific runtime adapters, or mandatory lifecycle stages.
- Automatic plan/subtask reconciliation or duplicated progress in Markdown.
- Artifact repository associations, frozen revisions, or a history UI.
- Task/artifact deletion, legacy data migration, or automatic Git conflict resolution.
- A new MCP server, desktop-only access restrictions, or mandatory live-UI IPC.
- Rich manual task editing, inline artifact feedback, or generated agent prompts.
