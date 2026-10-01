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
| `askAbout(claimId)` | Offer a discussion that the reviewer opens in the native Agent pane. |
| `proposeFinding(evidenceId)` | Open the trusted finding composer. |

`data-evidence="evidenceId"` is a declarative source link. IDs must come from the
capture and chapter contract. These requests cannot save findings, mark work
examined, execute code or submit a review. Manual source inspection pins the
evidence pane until the reviewer chooses to follow the story again.

Generation and follow-up conversation live in the native **Guide agent** pane.
Choose any harness enabled in Settings (OpenCode, Claude Code, Codex or OMP).
The configured default is selected. This is an ordinary interactive agent:
it keeps its usual configuration, authentication, permissions, tools, skills,
hooks, integrations and related-repository access. The working checkout and
captured revision are visible; captured source remains the review authority.
For a PR, the agent uses its linked checkout or a managed working checkout at
the captured head without switching the user's branch.

**Generate guide** prepares a persistent authoring directory and starts the
native agent with a short prompt pointing to `instructions.md`. Inspect the
full instructions with **Copy brief**. That directory contains `capture.json`,
reviewer instructions in `request.md`, the selected validated guide in
`viewed-guide.json`, runtime documentation, `commit.py`, and
an editable `manifest.json` plus chapter HTML files. Existing guides seed the
workspace. Continue chatting in the same pane to investigate or refine the guide.
Contextual questions and visual-repair requests open native context; **Copy
context** lets you paste it into the ongoing conversation. Chapter requests
never automatically launch or send an agent prompt.

After a complete update the agent runs `python3 /path/to/guide/commit.py`.
The helper atomically writes `ready.json` with the capture identity and SHA-256
hashes of the manifest and every chapter. Rust reads the committed bytes once,
checks all hashes, registered citations, paths and size limits, and installs a
valid update automatically. **Apply updates** retries manually. Incomplete,
invalid or stale updates preserve the last saved guide and show native status.
Earlier guides and their explicit decisions remain in history. Selecting history
pauses automatic application and updates `viewed-guide.json` without replacing
unfinished candidate edits. A per-capture lock allows one authoring session at
a time; an agent keeps its original capture identity.
Close that session and start another when you want to author a different capture.
Saved files provide continuation context when replacing an agent; conversation
history stays in the provider's usual session store and regular session catalog.

The manifest requires `runtime: 1`, the exact `capture`, `title`, `summary`, and
`chapters`. Each chapter has `id`, `title`, `summary`, `document`, `evidence_ids`,
`claims` (`id`, `text`, `evidence_ids`) and `questions`. Use unique IDs, relative
chapter paths and registered captured evidence. HTML runs only in the isolated
canvas; native generation scope does not grant the frame native capabilities.

Runtime script or resource failures switch to the readable alternative. The
native **Recover source view** button destroys and recreates the webview in
source mode, without mounting a potentially hung chapter. Saved findings,
drafts and guide history remain available.

Native hosting is currently enabled on macOS. Generation uses the same installed
interactive CLI as the regular Agent tab, without a separate version allowlist.
Windows and Linux hosting require their own native integration checks before
enablement.

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
