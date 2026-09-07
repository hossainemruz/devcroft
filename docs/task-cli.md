# Task and artifact CLI contract (PR 3)

These headless commands call the shared [task/artifact stores](task-storage.md) before GPUI initialization. They work from any checkout or unrelated working directory, with no running desktop, socket, agent provider, or Git remote required. `DEVCROFT_DATA_DIR` selects the data root; otherwise the existing per-OS default applies. Use an absolute override when changing working directories. Commands create required directories but do not initialize, commit, or sync a portable Git repository.

## Commands and arguments

Run `devcroft --help`, then `devcroft <family> <command> --help` for discovery. All four families support `--json`, placed anywhere after the family name. IDs are positional. Arguments below use uppercase placeholders, not literal IDs.

```text
devcroft repository list [--limit N]

devcroft task list [--repository KEY] [--include-archived] [--limit N]
devcroft task get TASK_ID
devcroft task create --title TITLE [--description-file PATH|-] [--repository KEY ...] [--artifact ART_ID ...]
devcroft task update TASK_ID --revision TOKEN [--title TITLE] [--description-file PATH|- | --clear-description] [--repository KEY ... | --clear-repositories] [--artifact ART_ID ... | --clear-artifacts]
devcroft task archive TASK_ID --revision TOKEN
devcroft task unarchive TASK_ID --revision TOKEN

devcroft subtask create TASK_ID --revision TOKEN --title TITLE --repository KEY [--description-file PATH|-] [--depends-on SUBTASK_ID ...] [--artifact ART_ID ...]
devcroft subtask update TASK_ID SUBTASK_ID --revision TOKEN [--title TITLE] [--repository KEY] [--description-file PATH|- | --clear-description] [--depends-on SUBTASK_ID ... | --clear-dependencies] [--artifact ART_ID ... | --clear-artifacts] [--status todo|doing|blocked|done]
devcroft subtask remove TASK_ID SUBTASK_ID --revision TOKEN
devcroft subtask reorder TASK_ID [SUBTASK_ID ...] --revision TOKEN

devcroft artifact list [--include-archived] [--limit N]
devcroft artifact get ART_ID
devcroft artifact create --title TITLE --kind rfc|plan|note --content-file PATH|-
devcroft artifact update ART_ID --revision TOKEN [--title TITLE] [--kind rfc|plan|note] [--content-file PATH|- | --clear-content]
devcroft artifact archive ART_ID --revision TOKEN
devcroft artifact unarchive ART_ID --revision TOKEN
```

Repeat each list flag once per value, for example `--repository public-api --repository backend`. Values may also be comma-separated. On updates, supplied lists replace the entire corresponding list; omitted fields remain unchanged. Explicit `--clear-*` flags set empty strings/lists and conflict with the corresponding input flag. A subtask's required repository cannot be cleared. Reorder takes a complete positional permutation of existing subtask IDs; no IDs is valid only for an empty task. Removal rejects incoming dependency references. New subtasks start at `todo`; update status explicitly afterward if necessary. All subtask commands return the complete containing task and its new revision.

Descriptions on creation default to empty. Artifact creation requires a content input, which may be an empty file/stdin. Markdown inputs must be regular files or `-` for stdin, contain valid UTF-8, and be at most 4,194,304 bytes. Reads stop after the limit plus one byte. Input is consumed before any record mutation and Markdown is preserved verbatim. The store separately enforces a 4 MiB limit on the complete serialized record, so JSON escaping and metadata can make a near-limit document too large to persist. Reduce the document if that error occurs. Input file paths, unlike record IDs, resolve against the process working directory. No command launches an editor or interprets Markdown as shell code.

All list commands default to 50 results and accept `--limit` from 1 through 1000. Task/artifact lists exclude archived records unless `--include-archived` is supplied. Direct ID reads include archived records. Task filtering uses derived repository membership and returns the full cross-repository task. Repository discovery lists readable portable records, not only locally linked checkouts; the directory key is authoritative. Lists report truncation; increasing the limit does not repair malformed records. Limits bound returned records, not the underlying filesystem scan.

## Output and exit status

Human-readable text is the default. Single-record output includes the ID, revision, metadata, and Markdown; task output includes progress, derived repository membership, all subtasks, and artifact IDs without inlining their content. List output is concise. Diagnostics always go to stderr.

`--json` emits exactly one JSON object on stdout, followed by a newline. Every response has `formatVersion: 1`. Additive fields may be introduced without changing this version; consumers must ignore unknown fields and treat revisions as opaque. Stored records retain their own independent `schemaVersion`. The stable response shapes are:

```text
Task read/create/update/archive/unarchive and subtask mutations:
{formatVersion: 1, task: <whole task>, revision: string,
 progress: {completed: number, total: number},
 involvedRepositories: [string], warnings: [string]}

Subtask creation additionally returns subtaskId: string.

Artifact read/create/update/archive/unarchive:
{formatVersion: 1, artifact: <whole artifact including content>, revision: string}

Task list:
{formatVersion: 1, tasks: [<task response without formatVersion>], errors: [string], truncated: boolean}

Artifact list:
{formatVersion: 1, artifacts: [{artifact: <whole artifact>, revision: string}], errors: [string], truncated: boolean}

Repository list:
{formatVersion: 1, repositories: [{key: string, displayName: string|null, warnings: [string]}], errors: [string], truncated: boolean}
```

Creation returns generated identities in `task.id`, `artifact.id`, or `subtaskId`. Zero total subtasks means **Not planned**, never complete. Repository membership is the sorted union of task and subtask repository keys. Artifact updates do not change task revisions; retrieve linked artifact IDs explicitly for current content.

Exit codes are **0** for success/help, **1** for runtime failures, and **2** for parser usage errors (missing arguments, conflicting flags, invalid enums or limits). These commands never use the live-UI app-not-running exit code. Runtime errors go to stderr with operation context and the store's underlying cause, including stale `task_changed`/`artifact_changed`, invalid input, missing records/references, malformed/unsupported records, and storage errors. Error text is diagnostic, not a versioned machine-readable error schema. A failed single-record operation emits no JSON. A partial list emits its valid records and `errors`, prints those errors to stderr, and exits 1. Missing-reference warnings do not fail an otherwise readable task; warnings appear in JSON and on stderr. Do not discard stdout solely because a list exits 1.

Mutation tokens must come from a fresh read of the containing record. On a stale error, reread, inspect the intervening changes, and recompute the patch; never blindly substitute the new token or retry a possibly successful creation. A failure to deliver stdout after a successful write does not roll the write back: reread/list to establish the result before retrying. Stores remain the only validation and persistence implementation; no command performs multi-record transactions or automatic plan/status reconciliation.

See [the shared agent instructions](task-agent-instructions.md) for the workflow and thin OpenCode/Claude setup guidance.
