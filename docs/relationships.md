# Repository relationship graph

`api → backend` means **backend depends on api**. Drag from a provider's bottom handle to a consumer's top handle, describe the connection, then Save. Select a node to edit its purpose/space, or an edge/label to edit endpoints, description, or delete it. Selected edge endpoints can also be rewired by dragging their handles; changes remain drafts until Save. Cycles are supported.

Drag the background to pan, scroll to zoom about the pointer, and use **Fit view** or **Auto arrange**. In navigation mode, **Tab / Shift+Tab** move focus to nodes or edge labels, then **Enter** selects the focused one; **Add relationship** provides a keyboard form. **Escape** cancels the active gesture or dismisses the draft. The active space filters the canvas; a node's inspector lists connections across spaces, and its Space selector moves the repository (and its artifacts) to another profile. Each space keeps its own canvas layout. **Auto arrange** puts unconnected repositories in a compact grid. Click **Open repository** to enter a linked checkout explicitly.

```sh
devcroft repository relationships backend --json
devcroft repository relationships backend --depth 2 --json
devcroft repository relationship create --from api --to backend --description 'backend implements api contracts' --revision GRAPH_REV --json
devcroft repository get backend --json
devcroft repository update backend --description 'Service implementation' --revision METADATA_REV --json
```

Read before editing and use the returned revision. Stale saves preserve UI drafts; review the latest values before retrying. See the [agent relationship reference](../assets/skills/devcroft/references/relationships.md) for limits, space filters, unresolved repositories, and the full command set.
