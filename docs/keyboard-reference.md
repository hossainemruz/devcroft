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
- `h` and `l` move focus between visible panes (left/right) and keep the
  mode open so movement can repeat. Movement clamps at the outer panes.
  `Enter` keeps the focused pane and returns to normal mode.
- `Escape` or the toggle again returns to normal mode without running
  anything. Focus stays where pane movement left it, otherwise where it was.
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

Resource actions appear on the Resources tab and on the global Artifacts page. Drafts survive navigation through the existing save lifecycle, and saving reuses the existing revision checks. Starting an edit focuses the draft input.

## Panes

Pane movement follows the rendered left-to-right layout and only includes regions on screen. Agent covers the sessions sidebar and the terminal; Review covers files, diff, and comments when shown; Resources and Artifacts cover the list, the document or draft, and the comments and outline rails when shown. Editor and Terminal each have one pane. Home has one pane. The focused pane is named in the list and native panes show a focus ring.
