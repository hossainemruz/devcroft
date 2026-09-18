# Repository resources

The Resources tab replaces task tracking. Its sidebar lists the current repository's artifacts by most recent update, selects the first by default, and renders Markdown. Selection remains stable across refreshes. Browse artifacts in the command palette provides a global view, including legacy artifacts that do not yet have a repository.

Use Edit Markdown to change a document and Save or Cancel. The comment panel supports creating, editing, resolving, reopening, and deleting document-level feedback. Originating sessions appear as links and open through the existing session navigator in their associated checkout. Missing sessions report an error without starting a different session.

Keyboard navigation (`Cmd+J` on macOS, `Ctrl+J` on Linux/Windows) offers `m` Edit Markdown and `c` Add comment for a selected resource, or `w` Save draft and `q` Cancel draft while drafting, alongside `h`/`l` pane movement. See the [keyboard reference](keyboard-reference.md).

## Markdown references

The shared reader renders these links as inline references with local hover
details. The author-supplied label remains visible and copyable:

```markdown
[Backend](devcroft:repository/backend)
[Design](devcroft:artifact/art-23456789)
[Discussion](devcroft:session/backend/codex/native-session-id)
```

Use real keys from `devcroft repository list --json`, artifact IDs from
`devcroft artifact list --json`, and provider/session IDs from
`devcroft session list --json`. Percent-encode session IDs as a URL path segment.
Session references support `opencode`, `codex`, and `claude`; they contain no
checkout or provider-store paths. The repository binding and session must be
available on this device, and ambiguous matches are rejected.

Click or keyboard-activate a reference to open it. Artifact links open in global
Browse artifacts, including archived records and records outside the current
page or kind filter. Existing drafts are retained; an active draft in the
destination browser must be saved or cancelled first. Standalone previews open
a workspace using `devcroft app --open-reference URL`.

Hover metadata is read in the background for up to 128 unique references per
document revision. Session details use the saved local catalog. These details
may be older than the destination; opening rechecks availability. Missing
references remain readable and explain failures. Use short labels for the
atomic inline elements. Inline links are supported; reference-style Markdown
definitions are not enriched. Ordinary web links remain ordinary links.

Projects, Resources, and recent-session views use shared empty states, with
actions for adding repositories or clearing filters and the existing session
creation control. Loading and failures remain separate from empty data.

Mermaid and math typesetting are not enabled. Future plugin candidates are
source-file links, GitHub-style note/warning callouts, PR/issue hover cards,
and optional math rendering.

## Storage and concurrency

`portable/artifacts/<art-id>/artifact.md` is the atomic record. Schema version 4 metadata is JSON (a YAML subset), enclosed by `---` lines. The Markdown body follows the closing delimiter. Metadata includes ID, title, kind, repository key, originating sessions, comments, archive state, and creation/update timestamps. Unknown metadata round-trips unchanged.

App and CLI mutations share a portable gate and per-artifact lock. A hash of the complete file is the revision. Saves check the revision, write and sync a temporary file, then atomically rename it. Conflicts leave editor drafts intact. Files must be regular UTF-8 files, no symlinks, at most 4 MiB including metadata.

Existing schema 3 JSON artifacts are read without modification. The first explicit edit writes Markdown; old JSON is retained as a recovery copy, and Markdown takes precedence afterward. Unassociated artifacts remain available through Browse artifacts and can be assigned with `artifact update --repository KEY`. Old task records are left on disk but are no longer read or exposed by the application.

## CLI

See the bundled [agent resource instructions](../assets/skills/devcroft/references/resources.md) for creation, editing, session links, and the full comment lifecycle. All operations are headless. `DEVCROFT_DATA_DIR` selects the same root used by the desktop.

## Manual desktop check

With an isolated data root and registered checkouts, create artifacts for two repositories. Open Resources and check filtering, ordering, default selection, Markdown rendering, long sidebar scrolling, and archive filtering. Edit Markdown, cancel once, then save. Add feedback in the desktop, read/resolve/reopen it with the CLI, and verify the polling update. Edit concurrently in the CLI and desktop and verify the desktop retains its draft after a conflict. Link sessions from both repositories and verify each opens the correct workspace. Check empty repositories, malformed records, and missing sessions.
