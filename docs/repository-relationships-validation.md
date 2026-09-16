# Repository Relationships validation

Implemented and verified on 2026-09-16 against
[the design](repository-relationships-design.md).

## Delivered

- Dedicated native graph page from Home, Projects, and the action palette, with
  repository purpose/group editing and explicit checkout navigation.
- Provider → consumer connections with stable IDs, descriptions, drag creation,
  endpoint rewiring, deletion, keyboard selection, and unsaved previews.
- Per-group filters, hidden-connection discovery, search, pan/zoom, Fit view,
  deterministic SCC layout, and device-local position/viewport persistence.
- Shared store and headless CLI, with bounded neighborhood queries, reverse
  directions, metadata revisions, diagnostics, and unresolved placeholders.
- Portable definitions and metadata, coordinated repository/graph locking,
  atomic writes, stale-save recovery, and preservation of unknown fields.
- Bundled agent skill reference and README/keyboard instructions.

## Automated checks

`MISE_LOG_LEVEL=error mise run check` passed formatting, compilation, and
Clippy across all targets with warnings denied.

`MISE_LOG_LEVEL=error mise run test` passed: 319 unit tests and 6 integration
tests; 1 existing test remains ignored. The suite ran outside the filesystem
sandbox because an existing OpenCode status test binds a localhost socket.

Graph-specific coverage includes the supplied topology, direct dependencies and
dependents, cyclic traversal, group exclusions, stale/concurrent writers,
duplicate/self-edge rejection, malformed data, unresolved endpoints, unknown
field preservation, metadata revisions, rewiring, and deletion. CLI integration
runs without display environment variables and includes multiline descriptions.

A real local bare Git remote connects two temporary data roots through the
existing portable sync implementation. The test verifies queries without
checkout bindings, purpose edits, endpoint/description updates, and deletions
in both directions. Local layout files do not transfer.

Geometry/layout tests cover SCC ordering, preservation of saved positions,
coordinate conversion, pointer-anchored zoom, screen-space curve hit targets,
and routing long connections around intermediate nodes. GPUI interaction tests
cover Escape, focus loss, outside release, text input, and inspector occlusion.

## Native desktop smoke test

Ran the debug build on Linux/Wayland with
`DEVCROFT_DATA_DIR=/tmp/devcroft-relationships-smoke`. The fixture contained api,
backend, ui, cli, and an isolated repository, all initially unlinked. The
temporary app instance was closed after verification.

- Used actual handle drags and the description editor to save api → backend,
  api → ui/cli, and backend → ui/cli. The headless backend query returned
  `dependencies: ["api"]` and `dependents: ["cli", "ui"]`, with five records.
- Edited backend's purpose in the inspector and verified the same value in
  Projects and CLI output.
- Added ui → backend to form a cycle, selected the reverse connection, dragged
  its consumer handle to the isolated repository, and saved. Its ID remained
  unchanged and the other five records were preserved.
- Typed a UI draft, updated that same edge through the CLI, and attempted Save.
  The UI rejected the stale revision, retained the draft, and displayed the
  current stored text. Reviewing the latest revision and retrying saved the draft.
- Deleted only that temporary edge. A subsequent canceled connection draft
  left the original five records unchanged.
- Dragged node bodies, panned, zoomed about the pointer, fitted the view, and
  auto-arranged. Restarting the app restored the positions, viewport, metadata,
  and relationships. Long connections visibly bypass intermediate nodes.
- Switched to Work and followed hidden connections back to All. The separate
  group layouts and all five unlinked repositories remained available.
- Selected a connection using Tab and Enter and opened its editor. Escape
  dismissed the editor while a textarea was focused.
- Verified Home/Projects entry and Back navigation. Graph interactions did not
  open coding-agent terminals or modify the user's normal Devcroft data.

## Scope and limits

Queries return at most 1000 nodes and depth 8; the store allows 5000 edges and
16 KiB descriptions in a bounded 4 MiB document. Truncation and incomplete data
are explicit in query responses. Cross-process locks coordinate Devcroft's
writers and portable sync; external editors do not participate in those locks.

Review Assistant, source discovery, and automated contract verification remain
outside this feature. Native verification was performed on Linux/Wayland;
other desktop platforms were not exercised in this run.
