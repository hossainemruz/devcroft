# Task and artifact storage contract (PRs 1–2)

The task domain/store is implemented in `src/data/tasks.rs`, standalone artifacts in `src/data/artifacts.rs`, and machine-local locking in `src/data/store_lock.rs`. This is the storage foundation from [the task-management plan](task-management-plan.md), not an agent-facing CLI or desktop feature yet.

## Serialized records

Each task is stored once at `portable/tasks/<task-id>/task.json`, as two-space UTF-8 JSON with a trailing newline. Schema version **3** deliberately distinguishes this format from the historical Electron versions 1 and 2. Missing/other schema versions are reported as unsupported, not migrated or overwritten. Unknown top-level and subtask fields round-trip through mutations.

```json
{
  "schemaVersion": 3,
  "id": "task-k7m2q9d4",
  "title": "Expose the API",
  "description": "## Requirements\n\nExpose the agreed capability.",
  "repositories": ["deployment"],
  "subtasks": [
    {
      "id": "s1",
      "title": "Define the API contract",
      "description": "Implement the contract and verify compatibility.",
      "repository": "public-api",
      "dependencies": [],
      "status": "todo"
    },
    {
      "id": "s2",
      "title": "Implement the handler",
      "description": "Implement and test the backend handler.",
      "repository": "backend",
      "dependencies": ["s1"],
      "status": "todo"
    }
  ],
  "nextSubtaskId": 3,
  "archived": false,
  "createdAt": 1788825600000,
  "updatedAt": 1788825600000
}
```

- Task IDs have the prefix `task-` and eight characters from `23456789abcdefghjkmnpqrstuvwxyz`. The generator uses Rust's independently randomized `RandomState` hash keys; these IDs are collision-checked names, not security tokens. Creation retries occupied IDs up to 64 times and never adopts an existing directory, including an incomplete creation. There is no portable sequence file.
- Task-local subtask IDs are canonical positive integers prefixed with `s`. `nextSubtaskId` is a persisted high-water mark, greater than every existing subtask ID; removal does not reduce it. Reordering retains identities, dependencies, descriptions, and unknown fields.
- Timestamps are Unix epoch milliseconds. Creation sets both; a successful mutation advances `updatedAt` by at least one millisecond, retaining `createdAt`. Revisions do not rely on these timestamps.
- Missing optional descriptions, repositories, subtasks, dependencies, archive flags, and subtask statuses use empty/false/`todo` defaults. Titles, record identity, schema, timestamps, high-water mark, and each subtask's repository are required. Task creation itself accepts no repositories or subtasks.
- Titles must be nonblank; mutation inputs trim titles but preserve Markdown verbatim. Repository keys follow existing repository-key syntax. Repository lists, dependency lists, and reorder inputs cannot contain duplicates.
- A record is limited to 4 MiB and 1,000 subtasks. Dependency validation is iterative, rejects unknown/self/cyclic references, and does not impose status transitions or require dependencies to be complete before marking work done.

## Shared API behavior

`TaskStore` exposes create/get/list, task patching, archive/unarchive through `set_archived`, and subtask create/update/remove/reorder. Patches distinguish omitted fields from explicit empty strings/lists. Store-controlled IDs, timestamps, counters, and schema cannot be replaced through patch APIs. No task deletion API is provided.

Reads and successful mutations return a `Snapshot` containing the task, an opaque revision token, and repository/artifact-reference warnings. The token currently uses `git-blob-sha1:<digest>` over the complete persisted bytes using the existing gix dependency, without invoking Git or creating an object database. Consumers must treat it as opaque. Whitespace and unknown-field changes invalidate it, even when timestamps are unchanged. All updates, including subtask and archive operations, require the containing task's last-read token. A mismatch produces `task_changed` with reread/retry guidance.

New repository associations require a readable, parseable portable `repository.json`; a machine-local checkout is not required. Existing missing/malformed repository references produce warnings rather than making a task unreadable. Status/title/archive edits may preserve them, but introducing a new task or subtask association to the unavailable repository is rejected. References are never silently dropped.

Progress is derived as completed/total subtasks; zero subtasks is Not planned and is not complete. Repository membership is the union of explicit task repositories and all subtask repositories. Listing optionally filters by that union but returns the entire task. Lists default to active records and 50 results, expose truncation, and sort by descending update time then task ID. Archived records remain readable by ID and can be included in lists. Malformed records are reported separately without hiding valid siblings; historical `sequence.json` is ignored while legacy task directories are reported as unsupported IDs. A list scan is not a transaction across every task: each record is coherent, but independent task mutations may occur between record reads.

Subtask removal rejects incoming dependency references and missing IDs; the caller must update dependencies before removal. Reorder requires a complete permutation. Archive changes no subtask statuses and has no effect on completion; completed tasks stay in active lists until explicitly archived.

## Locking and portable Git coordination

```text
<data-root>/cache/
  portable-store.lock
  task-locks/<task-id>.lock
```

Task reads and mutations acquire the portable gate in shared mode, then a per-task shared read or exclusive write lock. Creation reserves its candidate under the same per-task lock. Different tasks can be written concurrently. Readers of a task wait for its writer, and writers reread and check revisions while exclusively locked. All cooperating callers use this lock order; do not nest public store calls while holding these locks.

Built-in portable sync acquires the gate exclusively for its entire Git pipeline, including stage/commit/fetch/rebase/push; built-in branch checkout does the same for branch resolution and switching. This deliberately favors a simple, coherent boundary over allowing task edits during network sync. The existing in-process sync serializer remains. Lock acquisition is blocking, so callers must run these operations on worker threads; branch switching already does so. Sync error handling releases the gate and preserves the existing reload/error behavior.

Locks use Rust's OS-backed file-lock API, are released on handle close/process exit, and remain structurally outside portable Git. Lock files are never deleted or recreated during normal operation, because waiters may retain their inode. Paths under the configured data root reject static symlinks for store directories, record files, and locks; IDs are validated before deriving record paths. The root is a trusted personal directory, not a sandbox against a hostile process swapping paths concurrently.

This gate coordinates the task and artifact stores with built-in sync and branch switching. It is not a retrofit of every existing portable writer. Existing repository/dashboard writes, manual editors, and external Git do not acquire it. External edits made before a mutation are detected by revision checks; simultaneous non-cooperating edits/Git operations are not made transactional and should be avoided. Other new stores should adopt the gate. Independent offline ID collisions or same-record Git conflicts remain ordinary explicit Git conflicts, never automatically resolved by this layer.

## Atomicity and failure behavior

Validate the full candidate record and newly added repository references before persistence. Serialize and prepare the returned snapshot before the commit point. Write to a unique, exclusively created hidden sibling file, flush it with `sync_all`, close it, and rename over `task.json`. Normal failures before rename preserve the original bytes and clean up the temporary file. A failed initial creation also removes its newly reserved directory if empty. Never overwrite an unsupported or malformed record to recover it.

A process interrupted before rename can leave a hidden `.task-*.tmp` sibling or an incomplete creation directory; the old record remains authoritative, and incomplete records are reported rather than adopted. Inspect/remove such leftovers with ordinary filesystem tools before sync if necessary, because the existing sync stages all portable files. There is no recovery journal or automatic adoption. This provides whole-file replacement, not a guarantee of power-loss durability of the directory entry; the parent directory is not fsynced. No post-rename fallible step is reported as a failed mutation.

## Validation

PR 2 adds artifact round trips, metadata-only/content-only patches, external Markdown edits, unknown-field preservation, shared live task/subtask links, archived and missing links, explicit link clearing, collision retry, malformed/unsupported siblings, path rejection, concurrent stale writers/coherent readers, and injected pre-rename failure with interrupted temporary-file recovery behavior. A subprocess exits after flushing its candidate but before rename, exercising old-record recovery and automatic lock release without cleanup; its ignored helper is explicitly run by the parent test.

Tests use temporary data roots only. Coverage includes idea-only tasks, field clearing, repository membership and missing-reference warnings, every status, zero-subtask progress, dependency validation, stable IDs through removal/reorder, archive/list behavior, malformed and unsupported siblings, unknown-field preservation, external edits, collision retry, size limits, symlink/traversal rejection, and injected pre-rename failure. Concurrency tests cover one winner for stale writers, independent record writes, subprocess lock compatibility/release, and sync/checkout waiting for task operations without committing lock files. The ignored subprocess helper is invoked explicitly by the parent lock test; it is not skipped concurrency coverage.

## Artifact representation and coherence decision (PR 2)

The proposed `artifact.json` plus `content.md` layout is replaced by **one `portable/artifacts/<artifact-id>/artifact.json` file**. Metadata and verbatim Markdown share a single atomic replacement; there is no second authoritative content file, transaction journal, recovery manifest, or versioned document system. This intentionally trades direct `.md` editing for the same minimal, coherent commit boundary as tasks. Future CLI file/stdin inputs will supply Markdown to the `content` field, not create a second portable file. External edits to that field are Markdown edits and invalidate the whole-record revision even if timestamps do not change.

```json
{
  "schemaVersion": 3,
  "id": "art-r3w8n6hp",
  "title": "API design",
  "kind": "rfc",
  "content": "# Proposal\n\nExpose the agreed API.\n",
  "archived": false,
  "createdAt": 1788825600000,
  "updatedAt": 1788825600000
}
```

Artifact IDs use `art-` and the same eight-character alphabet and collision-checked allocation convention as tasks. Kinds are exactly `rfc`, `plan`, and `note`. Schema version 3 reserves a distinct contract from historical formats; missing/unsupported schemas, unknown kinds, missing content, and malformed records are errors, never reset or migrated. Unknown fields round-trip. A sibling `content.md` is reported as an unsupported split representation rather than silently ignored or imported. Title, kind, and content are required; content may be empty and is preserved verbatim. The complete serialized UTF-8 JSON record, including escaping, is bounded to 4 MiB. Timestamps and title validation match tasks.

`ArtifactStore` offers create/get/list/update and `set_archived`. Patches distinguish omission from explicit empty content. Reads and successful mutations return metadata, Markdown content, and a `git-blob-sha1:<digest>` token over all persisted bytes. IDs, schema, and timestamps are store-controlled. Stale mutations fail with `artifact_changed` and reread/retry guidance. Default lists exclude archived artifacts, limit results to 50, sort by descending update time then ID, expose truncation, and report malformed siblings independently. Direct reads include archived records. There is no deletion, ownership, history, or frozen revision API.

Artifact operations acquire the shared portable gate, then a shared/exclusive record lock at `<data-root>/cache/artifact-locks/<artifact-id>.lock`. Built-in sync and checkout already hold the gate exclusively and therefore cannot bypass artifact mutation protection. Task operations that validate or report artifact references hold the gate and task lock, then acquire one artifact read lock at a time without reacquiring the gate. Artifact operations never acquire task locks. Links do not constitute a cross-record transaction: resolving a linked ID returns the current artifact at that read, not content pinned to the task revision.

Artifact writes reuse the task atomic-replacement primitive: prepare the complete snapshot, write and sync a unique hidden sibling, then rename over `artifact.json`. A failure before rename leaves both old metadata and old content intact. Interruption before rename may leave `.task-*.tmp` files (the shared primitive's filename convention); readers ignore them and continue using `artifact.json`. Interruption after rename leaves the complete new record authoritative. Incomplete initial-creation directories are reported and never adopted. Recovery requires no replay: inspect/remove abandoned temporary files or incomplete directories manually before sync, which stages all portable files. As with tasks, this is process-interruption atomicity, not a power-loss durability guarantee; directory entries are not fsynced. Concurrent nonparticipating editors remain outside the advisory locking contract.

## Task and subtask artifact links

Task and subtask schema 3 records now accept an optional `artifacts` array of artifact IDs, defaulting to empty. Create and patch inputs support these links; `Some([])` explicitly clears them. IDs must have valid syntax and lists cannot contain duplicates. Each newly introduced association on a task or individual subtask must resolve to a supported, readable artifact, including archived artifacts. Multiple tasks and subtasks can share the same ID. Existing missing/malformed references remain intact and produce snapshot warnings; unrelated status/title/archive edits still work. Adding a missing artifact to a different subtask is a new association and fails even if the parent already references it.

Task reads return IDs, not inlined documents. Resolve each through `ArtifactStore::get` for live content. Artifact edits do not alter task revisions, progress, dependencies, or repository membership. Task archive does not archive artifacts; artifact archive does not break links. Removing links changes only the containing task and requires its revision token.
