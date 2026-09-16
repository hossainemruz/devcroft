# Repository Relationships: implementation plan

Status: implemented and validated on 2026-09-16. See
[implementation validation](repository-relationships-validation.md) for automated
and native desktop results. Review Assistant remains deferred.

## Agreed scope

Build a dedicated page containing a canvas of registered repositories, filtered
by group, including Personal and Work. Repository nodes have descriptions of
their purpose. Users connect nodes and describe the dependency in plain language.
Agents can query repository descriptions and connections in both directions.

Keep definitions high level. Agents discover individual RPCs, fields, execution
paths, and tests from source during their own tasks. This feature does not run
an assistant, inspect source automatically, or require a detailed contract model.

The user's visual reference shows compact rectangular nodes, connection handles,
curved editable edges, and a clear selection state. Use that interaction model
within Devcroft's existing theme.

Cycles are allowed, as confirmed by the user. Use a hierarchical layout wherever
possible and handle cyclic groups explicitly.

## Direction and relationship semantics

Follow the user's example: **provider → consumer**.

- api → backend: backend implements services whose protobuf contracts live in api.
- api → ui and api → cli: these clients use api's shared contracts.
- backend → ui and backend → cli: these clients consume backend's services.

A connection X → Y means Y depends on something provided by X. Descriptions
explain that relationship in natural language. The form labels endpoints
Provider and Consumer and shows a sentence such as "backend depends on api"
to make direction clear.

For a backend query, incoming connections identify dependencies, including api;
outgoing connections identify dependents, including ui and cli. Store each
connection once. Reverse queries derive their results from the same record and
retain its original description.

Allow cycles involving distinct repositories. Reject self-connections and
duplicate ordered pairs. There is one editable description per ordered pair;
the reverse pair is a distinct valid connection. On duplicate creation, offer
to edit the existing connection.

This direction replaces the earlier consumer-to-provider proposal.

## Existing code to reuse

- `src/data/repositories.rs` already stores repository descriptions and group
  names in portable metadata, and joins them with device-local checkout bindings.
  Use the full catalog, including unlinked repositories, not recent projects.
- `src/add_repository.rs` already provides repository creation/editing and has
  description and group fields. Reuse the metadata and creation flow.
- `src/home.rs` has a dedicated Projects page and group filtering; repository
  groups are free-form strings, unlike the Personal/Work enum on dashboard items.
- `src/command_palette.rs`, `src/workspace.rs`, and `src/navigation.rs` own
  global navigation and existing keyboard conventions.
- `src/cli/resources.rs` and `src/commands/` provide headless repository commands.
- `src/data/store_lock.rs` and `src/data/record.rs` provide locking, bounded
  reads, and atomic replacement patterns. Current repository metadata editing
  needs stronger stale-write handling for concurrent graph/Projects/CLI edits.
- The pinned local GPUI implementation provides canvas drawing, cubic paths,
  pointer events, and paint operations. Build a native graph view from these
  primitives; no web canvas runtime is needed.

## Dedicated page

Add a Repository Relationships destination accessible from the global command
palette and a Relationships action on Home/Projects. It is a global page that
does not require an active checkout or start terminals.

The page fills the available window with:

- A compact toolbar: group filter, search, Add relationship, Add repository,
  Fit view, and Auto arrange.
- The graph canvas containing every repository in the selected group, including
  isolated nodes with no connections.
- A details inspector that opens for a selected node or edge and closes when
  dismissed. It does not reserve empty space when nothing is selected.

Use existing repository group metadata. Offer All, Personal, and Work, while
keeping existing custom groups and Ungrouped accessible. Recognize case variants
of Personal/Work for those filter choices without rewriting stored group names.
New assignments are explicit; never infer Personal/Work from remote URLs.

Group filtering changes the view, not stored relationships. Draw an edge when
both endpoints are visible. On a visible node, indicate connections hidden by
the filter, with an action to show their endpoints in All. An agent query is
independent of the last UI filter unless it explicitly supplies a group filter.

### Repository nodes

Show display name (or key), a short description preview, connection handles,
and a subtle missing/unlinked checkout indication where relevant. Repository
identity is the stable key, not its display name or local directory.

Click to select and inspect. Edit the full description and group through the
inspector using the existing repository record. Preserve drafts on failed or
stale saves. An explicit Open repository action uses existing navigation; a
single node click must not leave the canvas.

Drag a node body to reposition it. Connection handles start connection editing
rather than node movement. Position changes do not change dependency direction.
Adding a repository uses the existing registration flow and brings its node into
the graph; existing repositories appear automatically.

### Relationship creation and editing

Drag from a provider's output handle to a consumer's input handle. Show a
temporary curved connection and highlight the prospective target. Dropping opens
a small editor anchored to the connection/selection with Provider, Consumer,
and a multiline Description. Persist only on Save; Escape/Cancel discards the
draft. Keep a button/form alternative for keyboard users.

Show arrowheads and a compact description label on saved edges. Clicking either
the label or the curve selects the same edge and opens its description in the
inspector. Support description edits, endpoint changes, and deletion there.
Endpoint changes can also use draggable connection handles, with the same
validation and explicit Save/Cancel behavior. Preview edits before persisting.

Use a screen-space hit target wider than the painted line. Selection styling
must remain visible at different zoom levels. Route opposing connections on
different curves so a two-repository cycle remains selectable.

### Canvas behavior and layout

Support pan, zoom about the pointer, Fit view, dragging nodes, and explicit
Auto arrange. Pointer gestures must not conflict with node movement, edge
creation, text selection, or the application's navigation mode. Cancel gestures
on Escape, focus loss, and pointer release outside the canvas.

Use deterministic initial layout: providers above consumers, multiple parents
supported, disconnected repositories arranged visibly. Find strongly connected
groups, arrange the resulting acyclic graph hierarchically, and place members of
each cyclic group together. Every repository remains its own editable node.

Persist node positions locally after gestures finish, with view state per group.
Restore layout when reopening the page. Reuse saved positions; place new nodes
without moving the whole graph. Recompute all positions only for Auto arrange.
Filter switches and reloads must not cause random rearrangement.

Keep keyboard access through focusable nodes, edge entries/labels, and the
inspector's connection list. Use existing navigation conventions; no new global
single-key shortcuts while text inputs are focused.

## Storage

Cross-device sync is a requirement. Store all relationship definitions (IDs,
endpoints, and descriptions) under the selected Devcroft data root's portable
directory and include them in the existing portable Git sync workflow. After
syncing to another device, the same graph and agent queries must be available
even before local checkout paths are linked on that device.

Repository descriptions and groups remain in the existing
`portable/repositories/<key>/repository.json` records. Do not duplicate that
metadata in the relationship store.

Use a single versioned `portable/repository-relationships.json` file containing
a flat list of connections. Stable IDs make an edge editable even when its
endpoints change. The following is an example of stored data:

```json
{
  "schemaVersion": 1,
  "relationships": [
    {
      "id": "rel-api-backend",
      "from": "api",
      "to": "backend",
      "description": "api defines the gRPC contracts implemented by backend"
    },
    {
      "id": "rel-api-ui",
      "from": "api",
      "to": "ui",
      "description": "ui uses the shared protobuf contracts defined in api"
    },
    {
      "id": "rel-api-cli",
      "from": "api",
      "to": "cli",
      "description": "cli uses the shared protobuf contracts defined in api"
    },
    {
      "id": "rel-backend-ui",
      "from": "backend",
      "to": "ui",
      "description": "ui consumes the APIs served by backend"
    },
    {
      "id": "rel-backend-cli",
      "from": "backend",
      "to": "cli",
      "description": "cli consumes the APIs served by backend"
    }
  ]
}
```

This replaces the earlier per-edge-file proposal. A flat document keeps the
small graph easy to inspect and supports one atomic graph update. Read responses
join descriptions and groups from repository metadata and expose a revision
token; reverse edges are never stored as separate copies.

Store graph positions and viewport preferences outside portable data, under a
device-local layout file. Persist changes after gestures rather than writing
on every pointer move. Portable sync carries the semantic relationships and
repository descriptions; dragging nodes causes no portable diff.

## Shared module and persistence rules

Use one deep module, initially `src/data/relationships.rs`, for loading the joined
graph, validating mutations, and answering queries. UI and CLI cross the same
interface. Keep layout/geometry independent from persistence.

- Missing relationship file means an empty graph over the full repository catalog.
- Reject malformed/unsupported graph data on mutation; never replace it with an
  empty graph. Preserve drafts and show a useful error.
- Validate known endpoints, nonblank description, unique IDs, and unique ordered
  pairs. Permit cycles. Keep bounded file and field sizes.
- Use the portable gate and a graph lock with consistent lock ordering, fresh
  revision checks, and atomic replacement. Perform blocking work off the UI thread.
- Coordinate endpoint validation with repository creation/removal so local
  concurrent mutations cannot silently create invalid references. Avoid nested
  acquisition of the same gate when composing store helpers.
- Add stale-write protection to repository description/group editing. Update
  existing Projects/dialog/CLI write paths to participate in the same locking
  discipline; preserve unrelated metadata when patching a single field.
- If an endpoint was removed externally or by older data, preserve the edge and
  report it as unresolved. Show a missing-repository placeholder where needed
  and allow explicit cleanup. Unlinking a checkout preserves all connections.
- Preserve unknown fields where practical and reject unsupported schema versions.
  Reads should report incomplete catalog data instead of hiding it.
- Refresh on entering the page, after local edits, and after detected external
  changes/sync. Stale results cannot replace newer UI state.

A graph is descriptive knowledge. Missing checkouts do not prevent querying it,
and definitions do not claim that source code still matches every description.

## Agent interface

Extend the current repository CLI. These are proposed commands:

```sh
# Complete graph, including repository descriptions and isolated nodes.
devcroft repository relationships --json

# Direct incoming dependencies and outgoing dependents of backend.
devcroft repository relationships backend --json

# Optional bounded neighborhood or explicit group selection.
devcroft repository relationships backend --depth 2 --json
devcroft repository relationships --group Work --json

# Mutations use the graph revision returned by a read.
devcroft repository relationship create --from api --to backend --description-file relationship.txt --revision REV --json
devcroft repository relationship update EDGE_ID --description-file relationship.txt --revision REV --json
devcroft repository relationship delete EDGE_ID --revision REV --json
```

Endpoint updates use the same update operation with optional from/to arguments.
Provide a direct description flag as a convenience, with file input for multiline
text. Reuse repository get/update for node descriptions and groups.

A backend query returns:

- Its own metadata and descriptions/metadata for the returned related nodes,
  each with a repository metadata revision for description/group edits.
- `dependencies: ["api"]` from incoming api → backend.
- `dependents: ["cli", "ui"]` from outgoing backend → cli/ui.
- The original connection records and descriptions, with direction relative to
  backend available explicitly.
- Format version, graph revision, optional query parameters, checkout availability,
  and diagnostics/truncation.

For depth greater than one, keep dependencies/dependents restricted to direct
neighbors; return expanded nodes/edges separately with distance/path information.
Use stable ordering, cycle detection, visited-node deduplication, and explicit
limits. Never rewrite a description to fabricate a reverse edge. Group filtering
must disclose excluded cross-group connections rather than suggesting they do not
exist. Default agent queries include all groups.

Document the direction convention and query examples in the bundled Devcroft
skill. Queries work without a running desktop and without invoking an LLM.

## Implementation ownership

| Location | Responsibility |
| --- | --- |
| `src/data/relationships.rs` | Semantic graph store, validation, revision checks, queries |
| `src/data/repositories.rs` and locking helpers | Joined metadata, safe description/group updates, endpoint lifecycle |
| `src/repository_graph/mod.rs` | Page state, selection, inspector, commands, asynchronous loading/saving |
| `src/repository_graph/canvas.rs` | Drawing, coordinate transforms, hit testing, pointer gestures |
| `src/repository_graph/layout.rs` | Deterministic hierarchical/cyclic layout and local positions |
| `src/home.rs`, `src/workspace.rs`, palette/navigation | Global page entry, full-height rendering, back navigation, focus integration |
| `src/cli/resources.rs`, `src/commands/relationships.rs` | CLI parsing and thin shared-module handlers |
| `assets/skills/devcroft/`, README and docs | Agent discovery, usage, direction semantics, validation instructions |

Names can be adjusted during implementation. Start with a focused entity for the
graph page and integrate it into existing global-page routing. Avoid spreading
canvas state into Home's card rendering or refactoring unrelated navigation.
Allow the graph to fill the page instead of inheriting the dashboard's scrolling
card layout.

## Delivery steps

1. **Store and query contract.** Implement normalized connections, metadata joins,
   reverse queries, cyclic traversal, atomic writes, and CLI operations. Verify
   the supplied api/backend/ui/cli example exactly.
2. **Dedicated page and repository nodes.** Add navigation, group filters, all
   registered nodes, descriptions, inspector editing, and local view persistence.
3. **Complete canvas interaction.** Add curved directed edges, create/edit/rewire/
   delete, drag, pan/zoom, hit testing, Fit view, and deterministic layout with
   cycles. The canvas is required for the first complete feature.
4. **Integration and documentation.** Wire refresh and concurrent editing,
   describe CLI use in the bundled skill, and update keyboard/help documentation.
5. **Validation and desktop smoke test.** Run automated checks and exercise the
   interaction end to end against isolated data before reporting completion.

Review Assistant remains deferred. No investigation engine, provider runner,
source indexing, automatic relationship discovery, or review-specific context
selection is included in this implementation.

## Acceptance checks

- The canvas shows all registered repositories in All, including isolated and
  unlinked repositories. Personal/Work/custom/Ungrouped filters preserve data.
- Edit a node's purpose and see the same description in Projects and CLI output.
- Draw api → backend and backend → ui/cli, add descriptions, and reopen the page.
  The direction, text, positions, and stored connections remain correct.
- Query backend: api is a dependency; ui and cli are dependents. Descriptions are
  preserved and no reverse connection is duplicated in storage.
- Create descriptions and connections on device A, sync through the portable
  remote, and load them on device B with different or absent checkout bindings.
  Both the canvas and CLI show the same nodes, descriptions, directions, and
  reverse-query results. Relationship edits and deletions sync back correctly.
- Add ui → backend: a cycle is accepted, both curves remain selectable, and
  layout/query operations terminate. Duplicate pairs and self-connections fail.
- Editing/rewiring/deleting one selected connection changes only that record.
  An abandoned connection gesture creates no stored relationship.
- Filtered-out connections are discoverable; missing checkouts remain represented.
- Pan/zoom/drag preserve accurate handle placement and hit testing. Node dragging
  does not change connection endpoints or cause portable sync changes.
- Concurrent CLI and UI changes reject stale saves without losing editor drafts;
  malformed files are not overwritten. External endpoint removal is surfaced.
- Keyboard users can select nodes/connections and edit them without dragging.
- No action starts or changes an existing coding-agent terminal.

Automated coverage should test the store/CLI through their shared interface,
reverse-query semantics, cycles, group exclusions, concurrent edits, and recovery.
Pure layout/geometry tests should exercise coordinate round trips, zoom anchoring,
cycle layout, and hit targets at different scales. GPUI interaction tests should
cover cancellation and input focus where practical.

Run `mise run check` and `mise run test` after implementation. Perform a real
desktop smoke test for creation, editing, rewiring, filtering, zoom/pan, reopening,
and the supplied four-repository topology. Compilation and screenshot inspection
alone do not establish that drag interactions work.
