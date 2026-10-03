# Visual tutorials

Generate one self-contained HTML page that explains a change. The tutorial helps the
user understand the mechanism; code verification stays in the Review tab and its line
comments. The page is an interpretation, not a record: Devcroft does not validate its
claims, and the user reviews them.

## When to generate one

- The user asks to explain a change, branch, commit range, PR diff, or an architecture
  decision before or alongside review.
- The user wants a walkthrough of how something works, with diagrams instead of prose.
- Do not use tutorials to record findings, decisions, or review outcomes. Use the
  Review tab or a review artifact for those.

## Deliverable and iteration

- Write one complete HTML file at a stable scratch path (for example
  `~/.agent/diagrams/<name>.html`) so revisions stay comparable.
- Create the artifact:

  ```sh
  devcroft artifact create --repository KEY --title "…" --kind tutorial --content-file /absolute/tutorial.html --json
  ```

- Update it with the current revision from `artifact get` or `artifact list`:

  ```sh
  devcroft artifact update ART_ID --revision TOKEN --content-file /absolute/tutorial.html --json
  ```

- Regenerate the whole file on feedback. Do not hand-patch fragments.
- Keep the artifact open for follow-up: the user iterates by asking for changes, and
  you regenerate and update the same record.
- Discover the repository key with `devcroft repository list --json`. Attach the
  originating session with `--sessions-file` when its identity is known (see
  [resources](resources.md)); otherwise omit it.
- Do not modify repository files, run mutations, or publish anything unless the user
  asks.

## Offline sandbox contract (required)

Devcroft renders tutorials in a locked-down frame, and **Open in browser** shows
the same content with the reader presentation applied. The page must work with no network and no
host capabilities:

- Inline all CSS, JavaScript, and SVG in the one file.
- No external URLs: no CDN fonts or libraries, stylesheets, scripts, images, or
  iframes. Use `data:` images and prefer inline SVG.
- No `fetch`, XHR, WebSocket, or other network calls.
- No storage assumptions (`localStorage`/`sessionStorage` fail in the sandbox's
  opaque origin). Guard `history` calls in `try/catch`.
- Use system font stacks or embedded fonts. If a visual-explainer skill emits a CDN
  font link, replace it with a self-contained stack before saving.
- No forms, popups, downloads, or top-level navigation.
- The file must also open correctly from `file://`.
- Keep it within 4 MiB of UTF-8 and keep embedded raster images small.

## Devcroft presentation (required)

Start from `templates/tutorial.html`, including when composing with another visual
skill. Tutorials belong to the Resources reader; keep its chrome quiet so the
figures carry the explanation.

- Use the template's neutral palette: dark background `#0a0a0a`, surface `#171717`,
  border `#262626`, text `#fafafa`, muted text `#a3a3a3`; light background `#ffffff`,
  surface `#fafafa`, border `#e5e5e5`, text `#171717`, muted text `#737373`.
- Keep `--bg`, `--surface`, `--border`, `--text`, `--muted`, and `--font` as the
  page tokens. The embedded reader maps these to the active Devcroft theme. Use
  accent colors for diagram meaning and interaction, not a tinted page canvas.
- Keep a `.shell` with direct `main` and `nav.toc` children. On desktop, put the
  contents rail on the **right**, 310px wide, with a subtle left border and compact
  system-font links under “On this page”. On phones it may stack above the content.
- Use one `h1` (30px), takeaway `h2` headings (23px), and optional `h3` headings
  (20px), with 16px body text in an approximately 820px reading column. Keep the
  title compact; avoid oversized hero typography, decorative grids, and extra
  title bars. Diagrams and interactive figures can still be expressive.
- Give sections stable IDs. The reader derives its outline from `h1`–`h3`, uses
  authored `.toc` link labels where available, and highlights the current section.
  Navigation and scroll tracking stay inside the sandbox; never add host messaging.
- Devcroft supplies the artifact metadata and options menu above the right rail.
  Do not reproduce those controls, add a comments panel, or add a second sidebar.

## Quality bar

- The first viewport states the main idea as a picture plus one sentence.
- One claim per figure; figures outnumber prose paragraphs. Captions state the claim
  in one sentence, and headings state takeaways rather than topics.
- Arrows are labeled with verbs (`writes`, `retries`, `polls 30s`); encode state with
  shape and position, never color alone.
- Support light and dark color schemes and respect `prefers-reduced-motion`: motion
  shows change or flow, starts on interaction, and the final state shows without it.
- Body text is at least 16px, labels at least 12px, text contrast at least 4.5:1.
- No horizontal overflow at 1280px or 390px wide; use rem units and a left-aligned
  reading column.
- Label simulations and example values as illustrative.

## Evidence discipline

The app does not validate tutorial claims; the reviewer does. Ground every
substantive claim in the captured change:

- Cite repository-relative `path:line` references in captions or callouts.
- Distinguish what you observed in the source from what you infer, and say which is
  which.
- Never claim a test ran or passed unless you ran it; name the command when you did.
- Label illustrative examples and simulations.
- State uncertainty and unknowns instead of smoothing them over; drop claims you
  cannot ground or list them as open questions.

## Composition with other skills

If a visual-explainer skill is installed, use it for layout and visual craft, then
adapt its output to the Devcroft presentation above and this offline contract, with
evidence citations included. This reference is authoritative for the Devcroft integration. Without one,
start from the bundled `templates/tutorial.html` and the quality bar above.

## Viewing

Resources lists the tutorial with a Tutorial badge. On macOS it renders in a
sandboxed embedded view; **Open in browser** opens the cached copy in the default
browser on every platform. Tutorials are view-only: there is no Markdown editor,
or comment UI, and comment commands are rejected. A right-hand “On this page”
outline navigates the tutorial. Open in browser, Copy HTML, Copy ID, Archive, and
Delete live in the artifact options menu above the rail.
