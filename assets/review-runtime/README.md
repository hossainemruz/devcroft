# Guided review authoring

The application owns the review workspace, captured source, findings, decisions,
questions and GitHub publication. A chapter owns its explanation canvas.

Author an HTML **body fragment** with inline CSS, SVG and optional JavaScript.
No framework or build step is required. External resources, network requests,
forms, nested frames, popups and local file access are unavailable. Inline data
images are supported. The fragment runs in an opaque sandbox and receives no
native capability token.

Use these theme tokens: `--review-bg`, `--review-ink`, `--review-muted`,
`--review-accent` and `--review-soft`. Fit narrow widths, label controls, respect
`prefers-reduced-motion`, and provide a complete readable summary. Animations
should start on deliberate interaction and offer pause or step controls.
Label simulations as illustrative. Captured test source is not an execution
result.

The bundled `Devcroft` SDK supports four requests:

| Method | Result |
| --- | --- |
| `showEvidence(evidenceId)` | Reveal registered captured source. |
| `focusClaim(claimId)` | Select the claim's registered evidence. |
| `askAbout(claimId)` | Open the trusted question composer. |
| `proposeFinding(evidenceId)` | Open the trusted finding composer. |

`data-evidence="evidenceId"` is a declarative source link. IDs must come from the
capture and chapter contract. These requests cannot save findings, mark work
examined, execute code or submit a review. Manual source inspection pins the
evidence pane until the reviewer chooses to follow the story again.

The Codex adapter requests a tool-free structured response containing chapter
HTML, summaries, claims, questions and evidence IDs. Rust validates capture
identity, citations and size before installation. A chapter repair replaces only
that chapter's HTML, keeping other chapters and the semantic contract intact.
Earlier complete guides and their explicit decisions remain in guide history.

Runtime script or resource failures switch to the readable alternative. The
native **Recover source view** button destroys and recreates the webview in
source mode, without mounting a potentially hung chapter. Saved findings,
drafts and guide history remain available.

Native hosting is currently enabled on macOS. Confined generation is verified
against installed Codex 0.159.2 and 0.159.3 with existing CLI authentication; other providers
and unverified versions remain disabled. Windows and Linux hosting require their
own native integration checks before enablement.

Local comparison:

```sh
devcroft review --checkout /path/to/repo --uncommitted open
```

GitHub PR (captured independently of the checkout):

```sh
devcroft review --pr https://github.com/owner/repo/pull/123 open
```

Saved PR reviews reopen offline by default; add `--refresh` to acquire new
objects. GitHub publishing requires installed `gh` authentication and a separate
explicit, exact-payload preview and publication action.
