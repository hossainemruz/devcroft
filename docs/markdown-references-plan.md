# Empty states and Markdown references

## Scope

Adopt gpui-kit 0.6.2 `Empty` components for Projects, Resources, and recent
agent sessions. Add native inline repository, artifact, and session references
to the shared Markdown reader. Mermaid remains out of scope.

## Reference contract

- `[Backend](devcroft:repository/backend)` uses a portable repository key.
- `[Design](devcroft:artifact/art-23456789)` uses an artifact ID.
- `[Discussion](devcroft:session/backend/codex/session-id)` uses a repository
  key, provider, and percent-encoded session ID. Local provider-store and
  checkout paths never appear in the URL. Supported session providers are
  OpenCode, Codex, and Claude, matching the resumable catalog.
- Preserve the author's visible label, Markdown source, selection/copy, and
  heading/outline text. Hover cards show locally resolved details. Opening a
  reference is an explicit click or keyboard action; loading Markdown never
  launches a session or fetches remote metadata.
- Resolve hover metadata on a background worker, once per document revision,
  with a bounded number of unique references. Session details use the local
  catalog cache. Navigation validates the destination again; missing,
  ambiguous, and unbound references surface an explanation.
- Resource references open through the existing global Resources browser,
  including archived or paginated targets. Pending edits must not be lost.
  Standalone previews open a workspace via an explicit CLI reference argument.
- Ordinary external links keep their behavior; malformed `devcroft:` links
  stay inside Devcroft and report an error.

## Implementation sequence

1. Implement and test URL parsing, portable session resolution, local hover
   metadata, and the inline plugin in a dedicated module.
2. Wire shared preview events through Resources/Home to workspace navigation;
   add direct artifact selection and standalone-preview routing.
3. Replace relevant hand-built empty messages with `Empty`, keeping loading,
   failure, no-data, and filtered-out states distinct. Reuse existing actions
   for adding repositories, clearing filters, and starting sessions.
4. Document reference syntax and useful future plugin candidates: source-file
   links, GitHub-style callouts, PR/issue previews, and optional math rendering.

## Validation

- Parser tests: encoded IDs, malformed URLs, unsupported providers, ordinary
  links, preserved labels/source, and rejection of local-path identities.
- Resolution/navigation tests: absent or ambiguous sessions, missing data,
  artifact selection outside pagination/filters, and draft preservation.
- Run formatting, Clippy, the full suite, and a build.
- Native desktop smoke: empty states/actions, inline layout/hover/copy,
  repository and artifact navigation, and readable unresolved references.
  Record separately anything that cannot be exercised with a real session.

### Completed verification (2026-09-19)

- Formatting, Clippy with warnings denied, and the locked debug build passed.
- Full suite: 331 unit tests and 6 integration tests passed; 1 test ignored.
  The direct-resource test uses an archived target beyond the current page
  and verifies that navigation preserves an existing edit draft.
- Native Linux/Wayland smoke testing used temporary data roots and a fixture
  repository registered through the desktop. Verified Projects and recent
  activity/session empty states, Add repository, the filtered Resources empty
  state, and Clear filter restoring the resource.
- Verified inline metadata hover cards, standalone-preview activation opening
  the target resource, navigation from Resources to its repository, artifact
  ID copying, and a readable notification for an unavailable session.
- Verified native select-all/copy preserves reference labels and literal code;
  the automated renderer test also checks Markdown-source copying. Preview
  layout was inspected at full width and at 900 and 600 pixels.
- A real historical provider session was not resumed interactively. Session
  identity resolution, unavailable targets, and ambiguous matches are covered
  by automated tests. Mermaid remains disabled.
