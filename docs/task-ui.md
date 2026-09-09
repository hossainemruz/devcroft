# Desktop task views

## Navigation and scope

Home's **Recent Tasks** shows a single row of up to 3 real active records, newest-updated first (IDs break timestamp ties), with title, copyable ID, repository badges, and derived progress. Fewer fit on narrow windows; overflow stays reachable through **View all →** into the global list. Open a summary to read the task or choose **View all →** to browse the global list. Repository-less ideas appear globally with **No repositories** and **Not planned** when there are no subtasks.

The workspace **Tasks** tab filters by the linked repository key, using the union of explicit task repositories and all subtask repository keys. Checkout paths and artifact associations do not determine membership. An unlinked checkout displays an actionable message instead of silently showing every task. Switching repositories clears the old selected task and invalidates in-flight results for the previous filter.

Global and repository lists share one view implementation. Lists initially show 100 records, with **Load 100 more**, copy buttons, and **Show archived too**. The shared store still scans records to derive the list; pagination bounds displayed results, not filesystem indexing. Malformed siblings are reported alongside valid tasks, and tasks with missing references remain visible with warnings.

## Task details

- The complete task opens by ID, independently of the list filter and archive state. Every repository therefore shows the full API → backend → deployment breakdown, not just its own subtasks.
- Read the Markdown task description and each ordered subtask's stable ID, title, Markdown description, status (`todo`, `doing`, `blocked`, `done`), repository key, dependencies, and artifact links. Missing repository/artifact references are reported without dropping the references.
- Progress is derived only from completed/total subtasks. Zero subtasks means **Not planned**; all-done tasks say **Complete**. There is no editable overall status or connection to PR state, merge, or deployment lifecycle.
- Task and subtask artifact links open the shared live Markdown viewer, including archived artifacts. Missing or malformed artifacts display the viewer's explicit error state. **Back to task** retains task selection and detail scroll position; the task continues refreshing while the artifact is visible.
- **Archive**/**Unarchive** uses the revision that was displayed. A stale mutation is rejected, displays an actionable error, and reloads; retry is explicit. Archiving neither completes subtasks nor archives linked artifacts. The open archived task remains readable.
- Creation, Markdown editing, reordering, and status changes remain agent/CLI operations. There are no task edit forms, automatic dependency orchestration, or plan-checkbox reconciliation.

## Keyboard and refresh

Use Tab/Shift-Tab to traverse buttons and Enter/Space to activate them. Page Up/Page Down scroll task lists and details; Escape returns from detail to the list. Repository task views participate in keyboard Tab traversal rather than forwarding input to a terminal. The artifact reader retains its own keyboard reading/outline controls and normal embedded Tab traversal.

Visible task views, including Home summaries, refresh every two seconds. Filesystem reads and archive writes run off the GPUI UI thread. The same single-flight/generation guard as the artifact browser coalesces requests and rejects superseded scan results. Hidden views stop polling; activation, manual refresh, portable Git status changes, and the existing post-sync/branch-switch callback reload relevant views. The nested artifact viewer polls independently, avoiding repeated invalidation of its in-flight reads by the task timer.

Selected task IDs and list/detail scroll handles survive polling. Unchanged Markdown retains its rendering state. Changed descriptions replace the affected Markdown state without resetting the containing detail scroll handle. If a selected task disappears or becomes malformed, its old detail is cleared and an error is shown until a later successful read. Returning Home intentionally resets the global view to active recent summaries.

## Automated validation

`src/task_browser_tests.rs` covers repository-less ideas, explicit and derived membership, full detail from every repository, dependencies and all status labels, unlinked checkout behavior, real bounded recent summaries and revision updates, archived access, stale archive rejection, malformed/missing records, missing repository/artifact warnings, archived artifact links, and Not planned versus Complete. Existing shared-store tests cover safe mutations and sync/branch coordination. The headless binary workflow test asserts that repository-filtered results and ID reads preserve the complete cross-repository task and artifact links.

Run `mise run fmt`, `mise run check`, and `mise run test`. All fixtures use disposable data roots; no real portable directory, sync remote, or agent configuration is used by these tests.

## Interactive acceptance checklist

Status: pending interactive execution. Automated checks do not establish visual layout, clipboard contents, or actual focus traversal. Use a temporary `DEVCROFT_DATA_DIR` for both the desktop and CLI and disposable local repository checkouts. Do not configure a real sync remote, alter real agent configuration, or use the real portable directory.

1. Follow the [shared CLI workflow](task-agent-instructions.md) to create a repository-less idea plus the public-api → backend → deployment task, dependent subtasks, and linked RFC/plan artifacts. Use `devcroft repository list --json` for the exact repository keys after linking the disposable checkouts in the desktop.
2. Confirm Home summaries show real task titles, IDs, repository badges, and progress. Open a summary, return Home, and use View all. Confirm the idea says No repositories / Not planned rather than Complete.
3. Visit each repository's Tasks tab. Confirm only matching tasks are listed and opening the example shows all three subtasks and dependencies. Check an unrelated linked repository and an unlinked checkout; neither should show a global fallback list.
4. Read long task/subtask Markdown descriptions with headings, code, and tables. Use Tab, Shift-Tab, Enter/Space, Page Up/Page Down, and Escape without the mouse. Verify narrow-window layout, long titles/repository keys, and that copy/archive/back controls remain reachable. Copy IDs from Home, lists, and details and verify exact text in a disposable scratch field.
5. Open both task-level and subtask-level artifact links, including an archived RFC. Verify current Markdown, copy behavior, CLI revisions while open, and Back to task preserving detail position. In the disposable root, temporarily move an artifact JSON aside to verify the missing-artifact state and restore it to verify recovery.
6. While a task is open, update a subtask through the CLI from todo to doing, blocked, and done. Verify progress and statuses refresh without navigation, including changes to other repositories' subtasks. Mark all done and verify Complete; archive an unfinished task and confirm it stays unfinished and linked artifacts stay active.
7. Toggle archived inclusion, open archived tasks, and unarchive. Make a concurrent CLI update around an archive attempt; verify either a fresh revision-checked success or a visible stale-write error, never an overwrite of unrelated fields.
8. In the disposable root, make one task JSON malformed and temporarily remove a selected record. Confirm valid siblings remain visible and selected stale detail is replaced by an error, then restore and verify recovery. Rapidly switch repositories, lists, and manual refresh during CLI updates to check that old results never replace the new selection/filter.
9. With disposable local portable branches, change branch while relevant views are open and verify task/artifact refresh. Do not contact a real remote. Recheck that unrelated PR/todo dashboard edits do not change task progress.
