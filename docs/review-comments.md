# Review comments

Open Review, click a diff line, write feedback in the editor directly below it,
and choose **Save comment**. The saved comment stays highlighted. The editor receives keyboard focus automatically.
Shift-click another line to select an inclusive line range. Ranges stay within
one file and one side of the diff; deleted lines use the old side, and additions
and ordinary context clicks use the new side. A range beginning on a deleted
line can extend into old-side context. A subtle blue background and blue gutters
show the selected range; the editor appears below its final line. Gold
gutters identify commented lines.

Saved comments stay visible as threads below their anchored ranges. Each thread
provides **Edit**, **Resolve / Reopen**, and **Delete**, with editing inside the
thread. The sidebar is an index showing file locations and comment text only;
it has no code excerpts or per-comment controls. The toolbar button before Refresh shows the comment count and toggles this sidebar. Sidebar cards retain subtle
green backgrounds for resolved comments and yellow backgrounds for outdated
comments, including when resolved; inline threads in the main pane have no status tint.
Clicking anywhere on a comment card jumps to its inline thread, centering it in the pane, and highlights the anchored range. Edit, Resolve/Reopen,
and Delete act independently without jumping. Outdated or hidden anchors appear
under the file header; comments whose files are absent from the diff appear in
the main pane's "Comments outside the current diff" section. Outdated threads
retain their saved code excerpts. All controls remain available there, even when
the diff is empty. Delete permanently removes that
comment, including its body and anchor; resolve retains it for later review.

The open Review tab checks for code changes automatically. Marking a file
**Viewed** collapses it; if either side changes afterward, its viewed mark is
cleared and the file expands. Unchanged files keep their review progress,
including across new commits on the same branch. Manual collapse controls
remain independent of viewed marks.

Use **Refresh**, or return to the Review tab, after the agent changes
comments. Unsaved drafts survive same-pair refreshes. Switching branch pairs or
review scopes clears the editor. An edit based on an externally modified comment
fails with a refresh message instead of overwriting it; after refreshing, click
Edit again to use its current revision.

## Agent workflow

These commands run without the desktop. They use the current checkout by default;
`--checkout PATH`, `--base BRANCH`, and `--remote NAME` select another review.
The base defaults to the same suggestion used by the desktop, and the remote
defaults to `origin`. Add `--uncommitted` for comments made in that scope.

```sh
devcroft review list --open
devcroft review resolve c1 --revision 1
devcroft review list --resolved
devcroft review delete c1 --revision 2
```

`list` always emits a JSON array. Each element contains `comment` (including
`id`, `body`, `resolved`, `revision`, and `anchor`) and `excerpt`. Anchors include
the repository-relative path, `old` or `new` side, inclusive one-based `start`
and `end`, and an independent `outdated` flag. Full saved file contents are not
included in CLI output. `list --resolved` lists resolved comments; omitting
filters lists all comments. `reopen ID` returns a comment to open status.

Ask the agent to list open comments, address the feedback, run appropriate
checks, and resolve the addressed IDs. The optional `--revision` rejects an
operation if the comment changed after the agent read it. Prefer using it in
agent workflows. Successful mutations exit zero; missing IDs, stale revisions,
and store failures exit nonzero. The CLI cannot create comments or edit bodies.
Deleting resolved comments is a separate, explicit operation on their IDs.

## Persistence and changed code

Comments live in `devcroft-review/comments.json` under Git's common directory
(normally `.git/devcroft-review/comments.json`). Linked worktrees share that
store. They are local to the repository and are not committed, exported, or
synced through Devcroft's portable data. Removing the repository's Git directory
also removes this feedback.

The identity is remote name + base branch name + head branch name, with separate
full-diff and uncommitted scopes. New commits preserve that identity. Detached
HEAD uses the commit ID in place of the branch name. Renaming a branch creates
a different pair. No branch-pair history or cross-machine portability is needed.

Each anchor retains its source text so relocation can compare complete files,
including unchanged lines outside visible hunks. Insertions/deletions before an
unchanged range move its line numbers. Edited, removed, unavailable, or ambiguous
ranges become **outdated** and keep their original excerpt and feedback.
Restoring the original text can place them again. Repeated lines require unique
surrounding context; the store favors an outdated comment over an uncertain
placement. Exact-content renames detected by the diff follow the new path.
Old-side anchors are checked against the current base snapshot, independently
of changes on the new side.

Writes use a cross-process lock and atomic file replacement. Desktop updates
check revisions, and malformed store files produce errors instead of being
replaced with an empty store. Source snapshots are local anchoring data and may
contain the same sensitive text as the reviewed files.
