# Repository resources

The Resources tab replaces task tracking. Its sidebar lists the current repository's artifacts by most recent update, selects the first by default, and renders Markdown. Selection remains stable across refreshes. Browse artifacts in the command palette provides a global view, including legacy artifacts that do not yet have a repository.

Use Edit Markdown to change a document and Save or Cancel. The comment panel supports creating, editing, resolving, reopening, and deleting document-level feedback. Originating sessions appear as links and open through the existing session navigator in their associated checkout. Missing sessions report an error without starting a different session.

## Storage and concurrency

`portable/artifacts/<art-id>/artifact.md` is the atomic record. Schema version 4 metadata is JSON (a YAML subset), enclosed by `---` lines. The Markdown body follows the closing delimiter. Metadata includes ID, title, kind, repository key, originating sessions, comments, archive state, and creation/update timestamps. Unknown metadata round-trips unchanged.

App and CLI mutations share a portable gate and per-artifact lock. A hash of the complete file is the revision. Saves check the revision, write and sync a temporary file, then atomically rename it. Conflicts leave editor drafts intact. Files must be regular UTF-8 files, no symlinks, at most 4 MiB including metadata.

Existing schema 3 JSON artifacts are read without modification. The first explicit edit writes Markdown; old JSON is retained as a recovery copy, and Markdown takes precedence afterward. Unassociated artifacts remain available through Browse artifacts and can be assigned with `artifact update --repository KEY`. Old task records are left on disk but are no longer read or exposed by the application.

## CLI

See the bundled [agent resource instructions](../assets/skills/devcroft/references/planning.md) for creation, editing, session links, and the full comment lifecycle. All operations are headless. `DEVCROFT_DATA_DIR` selects the same root used by the desktop.

## Manual desktop check

With an isolated data root and registered checkouts, create artifacts for two repositories. Open Resources and check filtering, ordering, default selection, Markdown rendering, long sidebar scrolling, and archive filtering. Edit Markdown, cancel once, then save. Add feedback in the desktop, read/resolve/reopen it with the CLI, and verify the polling update. Edit concurrently in the CLI and desktop and verify the desktop retains its draft after a conflict. Link sessions from both repositories and verify each opens the correct workspace. Check empty repositories, malformed records, and missing sessions.
