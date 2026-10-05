# Keyboard reference

The app has two keyboard modes. **Normal** is ordinary operation: terminals
and inputs behave as usual. **Navigation** is keyboard-driven navigation in
which nothing can edit: every keystroke is either a navigation command or
consumed before reaching a terminal or input.

Toggle with `Cmd+J` on macOS (`Ctrl+J` on Linux/Windows). The top bar shows a
a keyboard badge with `⌘J` on macOS (`Ctrl+J` elsewhere) right after the command
bar. It becomes an amber `Navigation` badge while the mode owns the keyboard,
without shifting the search field. The key list sits
at the bottom-right of the window. Contextual actions appear only for the
active tab or page and come first. Workspace tab shortcuts use two compact
rows (three tabs, then two), with the active tab highlighted. The final
**Common** section groups shared commands and focus, movement, and close
controls. All sections use the same small text and blue shortcut labels
without keycap outlines. Contextual and common sections use fewer columns in
narrow windows; labels shorten with an ellipsis when needed. Short windows
can scroll the overlay. Very narrow tab rows use abbreviated names
(`Ag`, `Ed`, `TTY`, `Rev`, `Res`) to keep both rows readable.

## Direct shortcuts (normal mode)

- `Cmd+K` on macOS (`Ctrl+K` on Linux/Windows): actions palette
- `Cmd+P` on macOS (`Ctrl+P` on Linux/Windows): projects palette
- `Cmd+Q` on macOS (`Ctrl+Q` on Linux/Windows): quit (works even with a terminal focused or navigation mode open; never sent to the pty)

These three also work inside navigation mode: the palettes exit to normal mode and open, while quit closes navigation and quits. There are no other global direct shortcuts — tab jumps, session creation, and settings live in navigation mode and the palettes. In normal mode **Tab** and **Shift+Tab** belong to the focused component: terminal panes send them to the pty (agent harnesses such as opencode switch agents/models/modes with them), inputs and lists keep them, and nothing moves focus. Form dialogs keep gpui-kit's Tab traversal; the Git changes dialog passes Tab to lazygit instead.

## While navigation mode is open

- Press an action key to run it exactly once and return to normal mode.
- `Tab` and `Shift+Tab` move focus to the next/previous focusable component and keep the mode open so traversal can repeat. This is the only place the app traverses focus; it reaches the controls in the current area (the command bar, dashboard cards, graph nodes and toolbar controls, resource and review panes). `Enter` then keeps focus on the traversed control and returns to normal mode.
- `h` and `l` (or `←`/`→`) move focus between visible panes (left/right) and keep the mode open so movement can repeat. Movement clamps at the outer panes.
- `j` and `k` (or `↓`/`↑`) move within the focused pane and keep the mode open so movement can repeat. Movement clamps at the ends. On the editor file tree, the sessions sidebar, and Home cards the cursor only moves keyboard focus highlighting: `Enter` opens the highlighted file, session, project, or card once and returns to normal mode. Folder toggles keep navigation mode open so you can continue through the tree; “Show more” reveals more files and returns to normal mode. On artifact/resource lists and review files the selection applies live while the mode stays open, in the built-in editor `j`/`k` moves the cursor vertically, and on the review diff `j`/`k` scrolls; there `Enter` only keeps the focused pane and returns to normal mode.
- `Escape` or the toggle again returns to normal mode without running anything. Focus stays where pane movement left it, otherwise where it was. Sidebar, editor tree, and Home cursor highlights clear on exit.
- Unknown keys, modified keys, and held-key repeats are consumed while the
  mode stays open. They never reach a terminal or an input.
- Clicking anywhere, switching location, opening another overlay, or
  deactivating the window returns to normal mode. Dialogs and sheets keep
  their own keyboard handling; the toggle does not open behind them.

## Bindings

| Location | Keys |
| --- | --- |
| Everywhere in navigation mode | `Tab` Next focusable component, `Shift+Tab` Previous focusable component (mode stays open) |
| Home dashboard | `a` Add repository, `r` Browse artifacts, `s` Switch space |
| Space switcher (open) | `↑`/`↓` Highlight a space, `Enter` switch to it, `Escape` close |
| Repository workspace | `a` Agent, `e` Editor, `t` Terminal, `d` Review, `r` Resources, `g` Git changes, `Space` Home |
| Editor tab (Built-in selected) | `f` Find project file, `b` Switch open tab, `w` Search project |
| Agent tab | `n` New agent session |
| Global Artifacts page | `Space` Home, `b` Back to origin, `s` Switch space, plus artifact actions below when available |
| Selected resource, no draft or save running | `m` Edit Markdown, `c` Add comment |
| Resource draft, save not running | `w` Save draft, `q` Cancel draft |
| Multiple visible panes | `h` Focus left pane, `l` Focus right pane |
| Sessions sidebar (Agent tab, sidebar focused) | `j` Next session, `k` Previous session, `Enter` Open highlighted session |
| Home dashboard | `j` Next card, `k` Previous card (sessions, projects, then inbox items in visual order), `Enter` Open or toggle highlighted card |
| Resources sidebar and global Artifacts list | `j` Next resource, `k` Previous resource (preview follows, mode stays open) |
| Review files pane | `j` Next file, `k` Previous file (diff follows, mode stays open) |
| Built-in editor file tree | `j` Next row, `k` Previous row, `Enter` Toggle folder (mode stays open) / open file / show more |
| Built-in editor code area | `j` Move cursor down, `k` Move cursor up (mode stays open; contents stay unchanged) |
| Review diff pane | `j` Scroll down, `k` Scroll up (mode stays open) |

Resource actions appear on the Resources tab and on the global Artifacts page. Drafts survive navigation through the existing save lifecycle, and saving reuses the existing revision checks. Starting an edit focuses the draft input.

## Built-in editor

With navigation mode open, `h` / `l` (or `←` / `→`) move between the visible sidebar and code area. In the file tree, `j` / `k` (or `↓` / `↑`) highlight rows and scroll them into view; `Enter` toggles a folder while keeping navigation mode open and its row highlighted. Opening a file or activating Show more leaves navigation mode. In the code area, `j` / `k` move the cursor vertically using the editor's usual arrow-key behavior, including wrapped lines and preferred columns. Movement keeps the mode open and preserves document contents; `Enter` or `Escape` returns to normal editing. A hidden sidebar is skipped.

On the Editor tab with Built-in selected, `Cmd+J`, then `f` on macOS (`Ctrl+J`, then `f` on Linux/Windows) focuses the checkout file finder. Type a partial path, use `↑`/`↓` to select a result, and press `Enter` to open it. The sidebar search icon also opens the finder. `Cmd+J`, then `b` (`Ctrl+J`, then `b` elsewhere) opens the same finder with only open tabs. Its preview includes unsaved edits; Enter switches tabs while preserving their buffers and cursor positions. `Cmd+J`, then `w` (`Ctrl+J`, then `w` elsewhere) opens live grep in the same popup. On the Resources tab, `w` saves an open resource draft. Results update as you type; Enter opens the selected file at the matching line. The **…** editor actions menu offers Search project, Go to line, Find in file, Replace in file, and Refresh files. `Cmd+S` / `Ctrl+S` saves the active file. **… → Go back / Go forward** follows file and line navigation. The editor's normal selection, clipboard, undo, and redo shortcuts remain available while it has focus.

Git changes opens a near-window-sized terminal dialog running `lazygit` in the current checkout. Enter, plain Escape, and Tab belong to lazygit, not the dialog; press **Shift+Esc** while the terminal is focused to close it (the shortcut is shown in the dialog title). The ✕ button or navigation toggle (`Cmd+J` / `Ctrl+J`) also closes it. Each opening starts a new lazygit session in the current checkout.

## Preview window

The standalone preview window (`devcroft preview <path>`) and the Resources reader share one document reader, so find works identically in both. `Cmd+F` on macOS (`Ctrl+F` on Linux/Windows) opens the find bar over the document; `Enter` steps to the next match and `Shift+Enter` to the previous one (both wrap); `Escape` closes the bar, clears its highlights, and returns focus to the document. Matching is case-insensitive and runs over the rendered text, so it crosses Markdown formatting.

## Panes

Pane movement follows the rendered left-to-right layout and only includes regions on screen. Agent covers the sessions sidebar and the terminal; Review covers files, diff, and comments when shown; Resources and Artifacts cover the list, the document or draft, and the comments and outline rails when shown. The built-in Editor covers its visible sidebar and code area; Neovim and Terminal each have one pane. Home has one pane: `j`/`k` there walk every dashboard card (recent sessions, recent projects, then pull requests, todos, and reading items) instead of switching panes. On the Pull Requests board, `j`/`k` walk cards column by column within the active space and `Enter` edits the selected PR. On the Todos board, `j`/`k` walk cards column by column (Unscoped first, then projects) within the active space and `Enter` toggles the selected todo. On the To Read page, `j`/`k` walk the list in order and `Enter` toggles the selected item read. Card menus also support moving PRs without dragging. The focused pane is named in the list and native panes show a focus ring; the `j`/`k` cursor shows as a ring around the highlighted sidebar row or card.

## Repository Relationships

Open **Repository relationships** from the action palette or Projects.
With navigation mode open, use **Tab / Shift+Tab** to focus toolbar controls,
repository nodes, edge labels, and inspector controls. **Enter** leaves the mode
on the focused control, and a second **Enter** selects the node or edge. The
**Add relationship** form supports provider/consumer choices without dragging.
**Escape** cancels an active gesture or dismisses the editor without saving.
Normal typing remains in the focused input; no single-key graph shortcuts are
installed. Navigation mode offers Home and Back.

Mouse: drag a node body to move it, background to pan, or an output handle to
an input handle to create a connection. Select an edge, then drag its provider
or consumer handle to rewire. Scroll zooms around the pointer. Finish editing
with Save or Cancel; Auto arrange and Fit view are explicit toolbar actions.
The canvas shows the active space's repositories and each space keeps its own
saved layout; switch spaces from the Home titlebar (or `s` in navigation mode).
Connections to repositories in other spaces remain listed in the node
inspector. The icon buttons provide tooltips and accessible names.
