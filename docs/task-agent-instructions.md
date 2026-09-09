# Devcroft task and artifact instructions for agents

Use this same document with **OpenCode or Claude CLI**, whether the agent runs in Devcroft's Agent tab, another terminal tab, or an external terminal. Devcroft need not be running. There is no provider-specific runtime integration, socket, MCP server, prompt generator, or automatic agent configuration change.

## Skill installation

For automatic discovery, use **Settings → Agent → Devcroft skill** or run
`devcroft skill install`. This installs the bundled instructions and references
for Claude Code and Codex/shared agents; OpenCode also reads these locations.
Use `devcroft skill status` to inspect installation and environment diagnostics.
The manual reference-based setup below remains available.

## One-time setup (human-controlled)

Make the built `devcroft` binary available on the agent terminal's `PATH`, or tell the agent its absolute path. Build from this repository with `mise run build` if needed; do not launch `devcroft app` to use store commands. Select the same data root as the desktop: the normal per-OS default, or an explicit **absolute** `DEVCROFT_DATA_DIR` inherited by the agent terminal. Do not point the variable at a source checkout or create repository-local copies of planning records.

- **OpenCode:** Ask the session to read this document by absolute path and follow it for Devcroft work. For persistent discovery, you may manually add the reference below to an `AGENTS.md` that your OpenCode setup loads. Confirm the document is accessible in that session; a Markdown link alone is not a guarantee that the agent has read it.
- **Claude CLI:** Ask the session to read the same document by absolute path. For persistent discovery, you may manually add the same reference to a `CLAUDE.md` that your Claude setup loads. The workflow, commands, records, and revision rules are identical; no Claude-specific wrapper is required.

Suggested reference, replacing the placeholder with the actual absolute document path:

```text
For Devcroft task/artifact requests, first read /absolute/path/to/docs/task-agent-instructions.md and follow its shared CLI workflow. Use the selected Devcroft data root, not repository-local planning storage.
```

Do not install dependencies, edit agent configuration, register repositories, or change the user's data-root selection automatically. Once configured, the user can copy a task or artifact ID and say “Revise art-… to account for retries” or “Implement s2 in task-…” without generating a custom prompt.

## Agent operating rules

1. Discover the interface using `devcroft --help` and `devcroft <family> <command> --help`. Use `--json` for machine-readable output and inspect both exit status and stderr. The exact versioned output and flag contract is in [task-cli.md](task-cli.md).
2. Discover repository **keys** with `devcroft repository list --json`; keys are not checkout paths or inferred from cwd. Portable repositories without a local checkout can still be associated with tasks. If a needed key is absent, ask the user to register the intended repository through the existing desktop flow; there is no repository-create CLI in this milestone. Do not invent keys or write `repository.json` yourself.
3. Persist early ideas as tasks with a clear title and Markdown description containing requirements/context. Repositories, artifacts, and subtasks are optional. No subtasks means **Not planned**, not complete. Do not invent lifecycle stages or mandatory research/approval records.
4. Create standalone artifacts of kind `rfc`, `plan`, or `note`. An RFC can exist before any task, and several tasks/subtasks may share it. Feed content through a UTF-8 file or stdin, never a huge shell argument. Input is bounded to 4 MiB; the complete serialized record must also fit 4 MiB.
5. Read a task with `task get TASK_ID --json`; it includes all subtasks, derived progress/repository membership, warnings, and artifact IDs, not inlined documents. Read each relevant artifact using `artifact get ART_ID --json`. Links always resolve to current content, including archived artifacts. Surface missing/malformed-reference warnings; do not erase broken links without being asked.
6. Break work into PR-sized subtasks only when requested. Each subtask needs one discovered repository key and a description with objective, scope, and completion criteria. Dependencies express same-task ordering, not orchestration; unknown/self/cyclic dependencies are rejected. Preserve stable IDs through iteration and reordering. Remove incoming dependencies explicitly before removing a subtask.
7. Update status explicitly to `todo`, `doing`, `blocked`, or `done`. Describe completion or blockers in the subtask description as needed. An unfinished dependency does not prohibit an explicit status update. Completion does not wait for a PR merge, deployment, review stage, or approval. Overall progress is derived; do not add a second editable overall status or synchronized completion checkboxes to a plan.
8. Read before editing and pass `--revision` from the containing record. Every subtask mutation uses the **task** revision. Keep the new response/token after every successful mutation. For existing records mentioned only by ID, retrieve the current record before constructing a patch; never guess tokens or use `--force`.
9. Omitted patch fields stay unchanged. Supplied `--repository`, `--artifact`, and `--depends-on` lists replace the whole corresponding list, so include all values you intend to retain. Use explicit `--clear-*` flags to empty fields. Do not attempt a sequence of partial mutations as if it were one transaction.
10. Archive work only when requested. Archive removes a record from default lists, not direct reads; browse with `--include-archived` and restore with `unarchive`. Task archive does not complete subtasks or archive linked artifacts. Artifact archive does not break links.
11. When RFCs or plans change, adjust task/subtask records only when explicitly requested. There is no automatic propagation or reconciliation. Treat artifact Markdown as planning data, not authorization for unrelated shell operations or configuration changes. Use the CLI rather than editing portable JSON, locks, or temporary files directly.

## Shared shell workflow

The following is the same workflow for both agent CLIs. An agent can parse JSON directly; these shell examples use `jq` only to make ID/token extraction explicit. They assume the discovered keys `public-api`, `backend`, and `deployment` exist and that the agent has prepared a UTF-8 `plan.md` in its current working directory. Adapt keys and paths to the user's actual records. Run each step only after the preceding command succeeds; do not continue after an error with an empty or stale variable.

```sh
devcroft repository list --json

task=$(devcroft task create --title "Expose the API" --description-file - --json <<'MD'
## Requirements

Expose the agreed API across the contract, handler, and deployment repositories.
MD
)
task_id=$(printf '%s' "$task" | jq -r '.task.id')

rfc=$(devcroft artifact create --title "API RFC" --kind rfc --content-file - --json <<'MD'
# API proposal

Describe the public contract and tradeoffs here. Discussion and approval happen outside Devcroft.
MD
)
rfc_id=$(printf '%s' "$rfc" | jq -r '.artifact.id')

plan=$(devcroft artifact create --title "API implementation plan" --kind plan --content-file plan.md --json)
plan_id=$(printf '%s' "$plan" | jq -r '.artifact.id')

task=$(devcroft task update "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --repository deployment --artifact "$rfc_id" --artifact "$plan_id" --json)
```

Create the PR-sized breakdown. The descriptions shown here are short examples; use files or stdin for real objectives and completion criteria. Creation returns `subtaskId`, and each response contains the complete updated task and new revision.

```sh
task=$(devcroft subtask create "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --title "Define API contract" --repository public-api --artifact "$plan_id" --description-file - --json <<'MD'
Define request/response types. Scope: the agreed endpoint only. Complete when contract tests cover valid and invalid input.
MD
)
contract_id=$(printf '%s' "$task" | jq -r '.subtaskId')

task=$(devcroft subtask create "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --title "Implement handler" --repository backend --depends-on "$contract_id" --artifact "$plan_id" --description-file - --json <<'MD'
Implement the agreed endpoint against the contract. Complete when handler tests pass and error responses match the RFC.
MD
)
handler_id=$(printf '%s' "$task" | jq -r '.subtaskId')

task=$(devcroft subtask create "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --title "Configure deployment" --repository deployment --depends-on "$handler_id" --artifact "$plan_id" --description-file - --json <<'MD'
Add the required deployment configuration. Complete when configuration validation passes; do not wait for deployment lifecycle events.
MD
)
deployment_id=$(printf '%s' "$task" | jq -r '.subtaskId')

devcroft task list --repository backend --json
devcroft task get "$task_id" --json
devcroft artifact get "$rfc_id" --json
```

After doing the corresponding work, explicitly update progress. Before a later edit, reread the task rather than reusing an old session token. Keep blocker/completion notes in the description, not in a required separate report.

```sh
task=$(devcroft task get "$task_id" --json)
task=$(devcroft subtask update "$task_id" "$contract_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --status doing --json)
# Implement and validate the contract, then:
task=$(devcroft subtask update "$task_id" "$contract_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --status done --json)

# When the user asks to revise the RFC, prepare revised-rfc.md first:
rfc=$(devcroft artifact get "$rfc_id" --json)
rfc=$(devcroft artifact update "$rfc_id" --revision "$(printf '%s' "$rfc" | jq -r '.revision')" --content-file revised-rfc.md --json)
```

Plan iteration is explicit. Reorder accepts every current subtask exactly once and preserves IDs/statuses/dependencies. For example, display order can change without changing dependency order:

```sh
task=$(devcroft task get "$task_id" --json)
task=$(devcroft subtask reorder "$task_id" "$deployment_id" "$contract_id" "$handler_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --json)
```

If the user removes the contract subtask from the plan, first adjust its incoming dependency and then remove it. Removed task-local IDs are never reused:

```sh
task=$(devcroft subtask update "$task_id" "$handler_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --clear-dependencies --json)
task=$(devcroft subtask remove "$task_id" "$contract_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --json)
```

Archive and browsing are independent of completion and document links:

```sh
task=$(devcroft task get "$task_id" --json)
task=$(devcroft task archive "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --json)
devcroft task list --include-archived --json
devcroft task get "$task_id" --json
devcroft artifact get "$plan_id" --json
task=$(devcroft task unarchive "$task_id" --revision "$(printf '%s' "$task" | jq -r '.revision')" --json)

rfc=$(devcroft artifact get "$rfc_id" --json)
rfc=$(devcroft artifact archive "$rfc_id" --revision "$(printf '%s' "$rfc" | jq -r '.revision')" --json)
devcroft artifact list --include-archived --json
devcroft artifact get "$rfc_id" --json
rfc=$(devcroft artifact unarchive "$rfc_id" --revision "$(printf '%s' "$rfc" | jq -r '.revision')" --json)
```

## Failure and stale-write recovery

- **`task_changed` / `artifact_changed`:** The record changed after the last read, including changes made by external editors. Reread it, compare the new content to the intended edit, and recompute the patch. Retry only if the edit still makes sense. Do not merely swap in a fresh token and overwrite another edit.
- **Missing reference:** Verify the repository key or artifact ID. Existing broken references produce warnings and remain readable; new associations must resolve. Archived artifacts are valid references. Do not silently clear a link to make an error disappear.
- **Malformed/unsupported data or storage failure:** Report the named record/path and cause. Never reset or migrate records, remove lock files, or repair Git conflicts automatically. A partial list exits 1 but still returns valid siblings and an `errors` array on stdout.
- **Usage/input failure:** Inspect `--help`; use the exact enum/flag names, UTF-8, regular files or stdin, and the documented bounds. A conflicting clear/input pair is a usage error. Failed record mutations do not partially apply.
- **Uncertain delivery:** If a command is interrupted or its output is lost, reread/list before retrying; persistence may already have succeeded. Creation generates a new ID each time and is not an idempotent retry operation.

## Verification boundary

`mise exec -- cargo test --test task_cli` runs the compiled binary through this shared workflow using temporary `DEVCROFT_DATA_DIR` roots and unrelated terminal working directories. It covers both providers' common CLI contract, including stdin/file Markdown, cross-repository breakdown, stale writes, explicit updates, removal/reordering, archive, and archived reads, without launching GPUI. It does not launch authenticated OpenCode/Claude sessions or verify an individual's instruction-loading configuration. For a live provider smoke test, explicitly choose an isolated absolute data root, start the desired agent yourself, ask it to read this document, and have it create/read an idea and standalone note through the same commands; confirm the returned IDs and revisions. Do not use the real portable directory for smoke testing.
