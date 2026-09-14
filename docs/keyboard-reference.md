# Keyboard reference

The app has two keyboard modes. **Normal** is ordinary operation: terminals
and inputs behave as usual. **Navigation** is keyboard-driven navigation in
which nothing can edit: every keystroke is either a navigation command or
consumed before reaching a terminal or input.

Toggle with `Cmd+M` on macOS (`Super+M` on Linux). The top bar shows a
`NAVIGATION` pill right after the command bar while navigation
mode owns the keyboard, and a dim `⌘M` hint otherwise. The key list sits
at the bottom-right of the window.

## Direct shortcuts (normal mode)

- `Cmd+K` / `Ctrl+K`: actions palette
- `Cmd+P` / `Ctrl+P`: projects palette

These two also work inside navigation mode: they exit to normal mode and
open their palette. There are no other direct shortcuts — tab jumps, session
creation, and settings live in navigation mode and the palettes.

## While navigation mode is open

- Press an action key to run it exactly once and return to normal mode.
- `h` and `l` (or `←`/`→`) move focus between visible panes (left/right) and keep the mode open so movement can repeat. Movement clamps at the outer panes.
- `j` and `k` (or `↓`/`↑`) move within the focused pane and keep the mode open so movement can repeat. Movement clamps at the ends. On the sessions sidebar and on Home cards the cursor only moves keyboard focus highlighting: `Enter` opens the highlighted session, project, or card once and returns to normal mode. On artifact/resource lists and review files the selection applies live while the mode stays open, and on the review diff `j`/`k` scrolls; there `Enter` only keeps the focused pane and returns to normal mode.
- `Escape` or the toggle again returns to normal mode without running anything. Focus stays where pane movement left it, otherwise where it was. Sidebar and Home cursor highlights clear on exit.
- Unknown keys, modified keys, and held-key repeats are consumed while the
  mode stays open. They never reach a terminal or an input.
- Clicking anywhere, switching location, opening another overlay, or
  deactivating the window returns to normal mode. Dialogs and sheets keep
  their own keyboard handling; the toggle does not open behind them.

## Bindings

| Location | Keys |
| --- | --- |
| Home dashboard | `a` Add repository, `r` Browse artifacts |
| Repository workspace | `a` Agent, `e` Editor, `t` Terminal, `d` Review, `r` Resources, `g` Home, `n` New agent session |
| Global Artifacts page | `g` Home, `b` Back to origin, plus artifact actions below when available |
| Selected resource, no draft or save running | `m` Edit Markdown, `c` Add comment |
| Resource draft, save not running | `w` Save draft, `q` Cancel draft |
| Multiple visible panes | `h` Focus left pane, `l` Focus right pane |
| Sessions sidebar (Agent tab, sidebar focused) | `j` Next session, `k` Previous session, `Enter` Open highlighted session |
| Home dashboard | `j` Next card, `k` Previous card (sessions, projects, then inbox items in visual order), `Enter` Open or toggle highlighted card |
| Resources sidebar and global Artifacts list | `j` Next resource, `k` Previous resource (preview follows, mode stays open) |
| Review files pane | `j` Next file, `k` Previous file (diff follows, mode stays open) |
| Review diff pane | `j` Scroll down, `k` Scroll up (mode stays open) |

Resource actions appear on the Resources tab and on the global Artifacts page. Drafts survive navigation through the existing save lifecycle, and saving reuses the existing revision checks. Starting an edit focuses the draft input.

## Panes

Pane movement follows the rendered left-to-right layout and only includes regions on screen. Agent covers the sessions sidebar and the terminal; Review covers files, diff, and comments when shown; Resources and Artifacts cover the list, the document or draft, and the comments and outline rails when shown. Editor and Terminal each have one pane. Home has one pane: `j`/`k` there walk every dashboard card (recent sessions, recent projects, then pull requests, todos, and reading items) instead of switching panes. On the Pull Requests board, `j`/`k` walk cards column by column within the selected All/Personal/Work filter and `Enter` edits the selected PR. On the Todos board, `j`/`k` walk cards column by column (Unscoped first, then projects) within the selected group filter and `Enter` toggles the selected todo. Card menus also support moving PRs without dragging. The focused pane is named in the list and native panes show a focus ring; the `j`/`k` cursor shows as a ring around the highlighted sidebar row or card.
