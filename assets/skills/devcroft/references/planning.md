# Tasks, subtasks, and artifacts

Examples use placeholders `TASK_ID`, `SUBTASK_ID`, `ART_ID`, `KEY`, and `TOKEN`; replace them with values from actual responses. Run dependent commands only after inspecting the preceding result.

## Read and create

```sh
devcroft repository list --json
devcroft task list --repository KEY --json
devcroft task get TASK_ID --json
devcroft artifact get ART_ID --json
devcroft task create --title "Implement retry handling" --repository KEY --description-file requirements.md --json
devcroft artifact create --title "Retry plan" --kind plan --content-file plan.md --json
```

Task responses contain `task`, `revision`, `progress`, and `involvedRepositories`. Artifact responses contain `artifact` and `revision`. Creation IDs are in `task.id` or `artifact.id`. Read linked artifacts separately; task responses do not inline their content. Artifact kinds are `rfc`, `plan`, and `note`. Markdown input accepts a UTF-8 regular file or `-` for stdin (4 MiB input limit).

## Update and track work

```sh
devcroft task get TASK_ID --json
devcroft task update TASK_ID --revision TOKEN --description-file requirements.md --json
devcroft subtask create TASK_ID --revision TOKEN --title "Add bounded retries" --repository KEY --description-file scope.md --json
devcroft subtask update TASK_ID SUBTASK_ID --revision TOKEN --status doing --json
# After implementing and validating the work, use the latest task revision:
devcroft subtask update TASK_ID SUBTASK_ID --revision TOKEN --status done --json
devcroft artifact get ART_ID --json
devcroft artifact update ART_ID --revision TOKEN --content-file revised-plan.md --json
```

Every subtask mutation uses the containing **task's** revision and returns the complete updated task with a new revision. Subtask creation also returns `subtaskId`. Read fresh state before a later edit; after each successful mutation use the returned revision for the next dependent mutation. Artifact revisions are independent.

Subtask statuses are `todo`, `doing`, `blocked`, and `done`. Overall task progress is derived; zero subtasks means not planned. Dependencies (`--depends-on SUBTASK_ID`, repeatable) refer to subtasks of the same task. Reordering requires all current subtask IDs exactly once. Remove incoming dependencies before removing a subtask. Plan text and status do not reconcile automatically.

Omitted update fields stay unchanged. Supplied `--repository`, `--artifact`, and `--depends-on` lists replace the entire corresponding list; include existing values you want to retain. Use the relevant `--clear-*` flag to empty a field. Discover all flags through command help.

Archive only within the user's requested scope:

```sh
devcroft task archive TASK_ID --revision TOKEN --json
devcroft task list --include-archived --json
devcroft task unarchive TASK_ID --revision TOKEN --json
```

Artifacts have matching archive/unarchive commands. Archiving does not complete subtasks or archive linked artifacts. Direct reads include archived records.

## Errors and concurrent edits

- On `task_changed` or `artifact_changed`, reread, inspect intervening changes, and recompute the patch. Do not blindly substitute a fresh token.
- Lists default to 50 results; inspect `truncated`, and increase `--limit` up to 1000 if needed. Partial lists can exit 1 while returning valid records and `errors`; inspect both.
- Surface missing-reference warnings; do not silently remove links. Do not repair internal records or delete locks to bypass an error.
- Multi-record edits are not transactions. If interrupted or output delivery fails, reread/list before retrying; a write may already have succeeded.
