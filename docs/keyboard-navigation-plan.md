# Context-aware keyboard navigation plan

## Modes

The application has two keyboard modes:

1. **Normal:** ordinary operation. Text, terminals, and inputs behave as
   usual. The only direct shortcuts are the two command palettes:
   `Cmd+K` actions (`Ctrl+K` fallback) and `Cmd+P` projects (`Ctrl+P`
   fallback). No tab jumps, no session creation, no
   settings shortcut — those live in navigation mode and the palettes.
2. **Navigation:** keyboard-driven navigation. No editing happens here:
   every keystroke is either a navigation command or consumed before it
   can reach a terminal or an input.

Toggle between the modes with `Cmd+M` on macOS (`Super+M` on Linux).
`Ctrl+M` is intentionally untouched (terminals read it as carriage
return). The top bar shows a `NAVIGATION` pill right after the command
bar while navigation mode owns the keyboard, and a dim `⌘M` hint otherwise.
The HUD sits at the bottom-right of the window.

## Intended behavior

Press the toggle to reveal a compact HUD of keys available at the current
location. This is additive to the command palettes.

- An action key executes its displayed action exactly once and returns to
  normal mode.
- `h` and `l` move focus between visible application panes, left and right,
  and keep navigation mode open so movement can repeat. Clamp at the outer
  panes. `Enter` finishes movement, returns to normal mode, and leaves the
  chosen pane focused.
- `Escape` or the toggle again exits to normal mode without performing an
  action. Keep the original focus unless pane movement deliberately
  changed it.
- Unknown keys are consumed while navigation mode stays open; do not send
  them to a terminal or insert them into an input. Modified and repeated
  keystrokes must not accidentally execute actions. The two palette
  shortcuts stay live inside navigation mode: they exit to normal mode
  and open their palette. No timeout is needed for this iteration.
- Pointer interaction dismisses navigation mode before ordinary interaction.
  Window deactivation, a location change, or opening another overlay clears
  stale navigation state. Dialogs and settings sheets retain their own input;
  the navigation toggle should not activate behind them.

## Context and bindings

Build HUD rows and key resolution from the same typed command registry. Each
entry has a unique key, label, scope/group, and action. Resolve availability
from current state, including resource selection, drafts, and pending saves.

| Context | Keys and actions |
| --- | --- |
| Home dashboard | `a` Add repository, `r` Browse artifacts |
| Repository workspace | `a` Agent, `e` Editor, `t` Terminal, `d` Review, `r` Resources, `g` Home, `n` New agent session |
| Global artifacts page | `g` Home, `b` Back to origin, plus available artifact actions |
| Selected resource, no draft/save in progress | `m` Edit Markdown, `c` Add comment |
| Resource draft, save not in progress | `w` Save draft, `q` Cancel draft |
| Multiple visible app panes | `h` Focus left pane, `l` Focus right pane |

Resource commands extend the repository bindings on Resources and the page
bindings on global Artifacts. Never offer a command that silently navigates
into a repository from Home. Preserve unsaved drafts through navigation using
the existing lifecycle. Revalidate availability when executing, rather than
trusting an old HUD snapshot. Saving and editing must use existing artifact
operations and concurrency checks. Focus the textarea when starting an edit.

## Pane navigation

Use actual focus handles and the rendered left-to-right layout, not tab changes
or simulated terminal input. Cover Agent (sessions sidebar, terminal), Review
(visible tree/diff/comment regions), and Resources/Artifacts (list, document
or draft, visible comments/outline rail). Only include rendered regions. Editor
and Terminal each have one application pane; internal Neovim splits remain
owned by Neovim. Home should not invent panes that do not exist.

Show the focused pane's name in the HUD and give focused native panes a visible
focus indication. Sidebar and content targets must be useful after leaving
mode: focus an existing control/input where suitable, or support normal keyboard
traversal within the focusable region. Derive the starting pane from actual
focus, including descendants, so mouse focus and keyboard focus stay aligned.

## Implementation structure

1. Add `src/navigation.rs` for typed context, commands, row construction, input
   classification, and pane movement helpers. Keep policy independently testable.
2. Integrate mode state and rendering in `Workspace`. Use a window-scoped
   keystroke interceptor before GPUI keymap dispatch: review of the installed
   implementation established that element capture handlers run after bound
   actions and cannot isolate Enter/Escape or existing shortcuts. Keep its
   subscription tied to the workspace lifetime. Track held keys so repeating
   a trigger or action cannot toggle twice or leak into a newly focused terminal.
   Observe key-up at the element level (`.on_key_up` on the workspace root):
   `window.on_key_event` panics outside the paint phase, and view render runs
   in layout. As redundant re-arms, clear the held set on an
   all-modifiers-released event while the mode is closed, and mark claims on
   any modifier movement so a re-pressed trigger is honored as fresh: a lost
   key-up must never wedge the toggle, and the trigger's own flicker guard
   cannot trip on either because its repeats always carry modifiers.
3. Add minimal APIs on child views for pane focus and available local commands.
   Reuse existing navigation, palettes, settings, session, and resource methods.
   Keep context policy and HUD rendering separate from action execution.
4. Render a compact overlay at the bottom-right above workspace content and
   below dialogs/sheets, with a context title, grouped key/label rows, current
   pane, and exit hints. Show the mode pill in the top bar right after the
   command bar. Ensure readable contrast and fit/scroll behavior in small
   windows. HUD rows may also invoke the same commands by click if this does
   not complicate focus.
5. Test scope composition, unique keys, resource availability, modifier and
   repeat handling, consumed unknown keys, cancellation, pane clamping, and
   terminal-safe dispatch. Add integration/event tests where the existing GPUI
   test infrastructure permits; use a documented manual matrix otherwise.

## Review and validation

The builder owns implementation and its focused tests. The parent reviews the
diff and input/focus lifecycle, then sends concrete findings back to the builder
for correction. Run formatting, compilation, unit/integration tests, and Clippy
with the repository's mise toolchain. Record any environmental limitations
instead of claiming unperformed GUI verification.

Manual desktop matrix: Home, each repository tab, global Artifacts, empty and
selected Resources, Markdown/comment drafts, save in progress, command palette,
settings sheet, and dialog. Check activation from terminals and native inputs;
`Cmd+M e`; repeated `h/l` then `Enter`; Escape/retrigger/unknown key; palette
shortcut from inside navigation mode; pointer dismissal; focus loss; switching
repositories; ordinary terminal `m`, `h`, `l`, `Ctrl+M` in normal mode; and
that no direct tab-jump shortcut remains.

After implementation and review, update README keyboard guidance and Resources
documentation to describe the final behavior and link a dedicated keyboard
reference. Keep documentation consistent with bindings actually implemented.
