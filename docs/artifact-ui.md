# Artifact browsing and reading

Open **Home → Browse artifacts** to read standalone RFCs, plans, and notes. No task or repository association is required. Creation and Markdown edits remain agent/CLI operations. [Task views](task-ui.md) use the same live viewer for task/subtask artifact links, with **Back to task** preserving the task selection and reading position.

## Browser and viewer

- Active artifacts are listed newest-updated first with title, kind, ID, and a **Copy ID** button. **Show archived too** includes archived records and labels them; toggle again to return to active-only browsing.
- The first page contains up to 100 records. **Load 100 more** increases the visible bound without introducing a separate catalog.
- Open a row to read rendered Markdown using the existing preview renderer and outline. The viewer has its own copyable ID and **Archive**/**Unarchive** control. Archiving an open record does not close it or hide its content.
- An empty Markdown string has an explicit empty-content message. Malformed or unsupported records produce visible diagnostics without hiding valid siblings. If an open record disappears or becomes malformed, its old rendered content is cleared and replaced with an error; a later successful refresh recovers the viewer.
- Use Tab/Shift-Tab to traverse controls and Enter/Space to activate buttons. **Read document** focuses the Markdown reader for keyboard scrolling. Embedded readers allow Tab to leave the document/outline for other controls; Escape returns to the browser controls (from the outline, the first Escape returns to the reader).

## Refresh and concurrency

While the artifact page is active, a two-second background poll rereads the store. **Refresh**, portable Git status changes, and the existing post-sync/branch-change reload path also request a reload. Hidden views stop polling. Scans and archive writes share a single in-flight slot; requests during an operation coalesce, and superseded scan results cannot replace a newer selection/filter. Filesystem work and revision-checked writes run off the GPUI UI thread.

The shared store scans records to derive each list; the visible page bound is not an incremental filesystem index. The selected ID is read independently of that bound and the archive filter. `ArtifactBrowser::open` is the reusable ID-based entry point used by task/subtask links; callers activate the browser while it is visible.

Unchanged Markdown retains its renderer, selection, and exact scroll position. Changed Markdown keeps keyboard focus and returns to the previously active heading if it still exists; text selection and exact pixel position are not preserved across content changes. List scroll is retained while reading and returning to browsing.

Archive changes use the displayed snapshot's revision, not a freshly fetched token that could silently accept an unseen edit. Stale writes display an actionable error and reload; the user explicitly retries. There is no optimistic local archive toggle, automatic conflict resolution, or artifact content editing.

## Validation

Automated coverage includes standalone browsing without a task, current content after a store revision, archived ID reads outside the list filter, stale archive rejection, malformed siblings, disappearing records, bounded results, and coalesced/superseded refreshes. `tests/task_cli.rs::standalone_artifact_browser_smoke_contract` exercises creation, revision, archive, ID retrieval, and unarchive through the shipped headless binary with no display and an isolated data root.

Run `mise run fmt`, `mise run check`, and `mise run test`. These do not establish interactive focus, clipboard, or visual-layout acceptance.

## Interactive acceptance checklist

Status: pending interactive execution. Use a temporary `DEVCROFT_DATA_DIR` for both the desktop and CLI, not the real portable directory. Do not configure a sync remote or change agent configuration. Prepare a UTF-8 Markdown file with several headings, a table, a code block, and enough prose to scroll, then build with `mise run build`.

1. Run `target/debug/devcroft artifact create --title "Standalone RFC" --kind rfc --content-file /absolute/path/to/rfc.md --json` in the isolated environment. Save its returned ID and revision; do not create a task.
2. Start `target/debug/devcroft app` with the same isolated data root. Open Home → Browse artifacts, find the RFC, and read its Markdown and outline.
3. Navigate using only Tab/Shift-Tab and Enter/Space. Copy the ID from both list and viewer; paste into a scratch text field and verify it exactly matches the returned ID, without title or whitespace. Focus **Read document**, scroll with the keyboard, navigate the outline, and return to archive/back controls without the mouse.
4. Keep the viewer open and run `target/debug/devcroft artifact update <id> --revision <revision> --content-file /absolute/path/to/revised.md --json`. Verify the open title/content refreshes within one poll after the write finishes, without navigation. Verify unchanged polling does not reset reading position.
5. Archive from the viewer. Verify the document stays readable, disappears from active-only browsing, appears with an Archived label when archived inclusion is enabled, and can be opened and unarchived. Confirm the ID and Markdown are unchanged.
6. In this disposable root only, externally make one artifact JSON malformed while leaving another valid record. Verify the diagnostic and valid sibling both appear. With a record open, temporarily move its JSON aside; verify stale content is replaced by the missing/unreadable message, then restore it and verify recovery.
7. Rapidly switch list filters, open/back, and manual refresh while issuing CLI revisions. Confirm the selected ID never shows another record's content and the page remains responsive. Check narrow-window layout and long titles, including access to Copy ID and Archive.
8. If testing portable branch switching, use disposable local branches only and verify the open record reloads after the switch. Do not contact a real sync remote.
