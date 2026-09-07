//! The terminal view: one GPUI element per workspace tab.
//!
//! [`TerminalPane`] owns the rendered grid ([`RenderRun`] rows produced by
//! [`TerminalSession`](crate::session::TerminalSession)) and translates
//! keyboard, scroll, and focus events into session input. Terminal-backed
//! tabs share this implementation; the tab itself only selects the label
//! and the startup command.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui_kit::component::ElementExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::v_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, FontStyle, FontWeight,
    HighlightStyle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, StyledText, UnderlineStyle, Window, div, px, rgb,
};

use crate::{
    agent::AgentKind,
    command_palette::{
        GoToAgent, GoToEditor, GoToTerminal, PaletteMode, ToggleActionsPalette,
        ToggleProjectsPalette, is_go_to_agent_shortcut, is_go_to_editor_shortcut,
        is_go_to_terminal_shortcut, palette_mode_for_shortcut,
    },
    fonts::TERMINAL_FONT_FAMILY,
    metrics::{
        INITIAL_COLS, INITIAL_ROWS, MAX_SCROLL_LINES_PER_EVENT, TERMINAL_PADDING,
        WORKSPACE_HEADER_HEIGHT, app_font_size, cell_height, cell_width,
    },
    session::{BlockKind, RenderRun, TerminalSession},
    workspace::WorkspaceTab,
};

pub(crate) struct TerminalPane {
    pub(crate) focus_handle: FocusHandle,
    tab: WorkspaceTab,
    session: Option<TerminalSession>,
    rows: Vec<Vec<RenderRun>>,
    error: Option<SharedString>,
    grid_size: (u16, u16),
    /// Fractional scroll lines carried over from sub-line wheel deltas.
    ///
    /// Trackpads report smooth pixel-precise scrolling at high frequency.
    /// Forwarding every event as a whole line floods the application with
    /// more input than it can redraw, which surfaces as scroll lag. Keeping
    /// the remainder here preserves total finger travel while sending input
    /// at a rate the terminal can keep up with.
    scroll_remainder: f32,
    /// When the latest frame was presented. Redraw storms pace presents
    /// through here (see [`TerminalPane::present_paced`]).
    last_present: Option<Instant>,
    /// Whether a trailing present flush is already scheduled.
    flush_armed: bool,
    /// Drag-selection anchor cell `(column, row)` in grid coordinates.
    selection_anchor: Option<(usize, usize)>,
    /// Drag-selection focus cell `(column, row)`, updated while dragging.
    selection_focus: Option<(usize, usize)>,
    /// Whether the left button is currently held for a selection drag.
    selecting: bool,
    /// Last painted bounds of the outer pane, for mapping window mouse
    /// positions back to grid cells (see `on_prepaint` in [`Render`]).
    pane_bounds: Option<Bounds<Pixels>>,
    /// Sequence counter for copy confirmations, so a stale dismiss timer
    /// cannot remove a newer copy's toast (see `dismiss_copy_feedback`).
    copy_feedback_seq: u64,
}

/// Marker id for the terminal copy confirmation toast. A stable id makes
/// rapid copies replace one another instead of stacking.
struct TerminalCopyFeedback;

impl TerminalPane {
    /// One pane for `tab`. The Agent tab launches `agent`; every other
    /// tab uses its fixed command (see [`TerminalSession::spawn`]).
    pub(crate) fn new(
        tab: WorkspaceTab,
        cwd: &Path,
        agent: AgentKind,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut pane = Self {
            focus_handle: cx.focus_handle(),
            tab,
            session: None,
            rows: Vec::new(),
            error: None,
            grid_size: (INITIAL_COLS, INITIAL_ROWS),
            scroll_remainder: 0.0,
            last_present: None,
            flush_armed: false,
            selection_anchor: None,
            selection_focus: None,
            selecting: false,
            pane_bounds: None,
            copy_feedback_seq: 0,
        };

        let output = match TerminalSession::spawn(tab, cwd, agent) {
            Ok((session, output)) => {
                pane.session = Some(session);
                Some(output)
            }
            Err(error) => {
                pane.error = Some(format!("Could not start {}: {error:#}", tab.label()).into());
                None
            }
        };

        if let Some(output) = output {
            cx.spawn(async move |this, cx| {
                while let Ok(first) = output.recv().await {
                    if this
                        .update(cx, |pane, cx| {
                            if let Some(session) = pane.session.as_mut() {
                                session.feed(&first);
                                while let Ok(bytes) = output.try_recv() {
                                    session.feed(&bytes);
                                }
                            }
                            pane.present_paced(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        pane
    }

    /// Minimum interval between presents during redraw storms.
    ///
    /// Full-screen redraws arrive much faster than their snapshot+reshape can
    /// land (measured ~13ms per heavy frame on the main thread). Presenting
    /// every batch saturates the main thread, starving input and stuttering
    /// motion — while pacing too coarsely batches several key repeats into
    /// one present, which reads as multi-row jumps in pickers (measured:
    /// 50ms pacing showed ~2.3 rows per present vs one row per present in
    /// the reference terminal at the same repeat rate). Snapshots now
    /// rebuild only dirty rows, roughly halving present cost, so 25ms paces
    /// storms to ~40Hz while tracking a ~30Hz key repeat 1:1. Isolated
    /// presents stay immediate (see [`present_due`]), so typing and single
    /// actions gain no latency.
    const PRESENT_PACE: Duration = Duration::from_millis(25);

    /// Snapshot and repaint unless a present just landed, queuing a trailing
    /// flush instead. Skipped snapshots lose nothing: dirty state accumulates
    /// in the terminal and the next snapshot picks it all up at once.
    fn present_paced(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if !present_due(self.last_present, now) {
            if !self.flush_armed {
                self.flush_armed = true;
                let delay = self
                    .last_present
                    .map(|last| Self::PRESENT_PACE.saturating_sub(now.duration_since(last)))
                    .unwrap_or(Self::PRESENT_PACE);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ = this.update(cx, |pane, cx| {
                        pane.flush_armed = false;
                        pane.present_now(cx);
                    });
                })
                .detach();
            }
            return;
        }
        self.present_now(cx);
    }

    /// Snapshot once and repaint on change, recording the present time.
    fn present_now(&mut self, cx: &mut Context<Self>) {
        let error_before = self.error.clone();
        let mut changed = false;
        if let Some(session) = self.session.as_mut() {
            match session.snapshot() {
                Ok(Some(rows)) => {
                    self.rows = rows;
                    self.error = None;
                    changed = true;
                }
                // The render state reports no changes, so the cached rows are
                // still current.
                Ok(None) => self.error = None,
                Err(error) => self.error = Some(format!("Terminal render error: {error:#}").into()),
            }
        }
        if changed || self.error != error_before {
            self.last_present = Some(Instant::now());
            cx.notify();
        }
    }

    fn resize_for_window(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let cell_width = cell_width();
        let width = (viewport.width.as_f32() - TERMINAL_PADDING * 2.0).max(cell_width);
        let height = (viewport.height.as_f32() - WORKSPACE_HEADER_HEIGHT - TERMINAL_PADDING * 2.0)
            .max(cell_height());
        let cols = (width / cell_width).floor() as u16;
        let rows = (height / cell_height()).floor() as u16;
        let next = (cols.max(1), rows.max(1));
        if next != self.grid_size {
            self.grid_size = next;
            if let Some(session) = self.session.as_mut()
                && let Err(error) = session.resize(next.0, next.1)
            {
                self.error = Some(format!("Terminal resize error: {error:#}").into());
            }
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Typing dismisses a completed selection highlight; the text is
        // already on the clipboard from the drag release.
        if self.selection_anchor.is_some() || self.selection_focus.is_some() {
            self.selection_anchor = None;
            self.selection_focus = None;
            cx.notify();
        }
        // The command-bar toggles must reach the workspace even while a
        // terminal has focus: everything below would otherwise be sent to the
        // pty. Dispatch them as actions (handled by `Workspace`) instead of
        // terminal input. The global `cmd-k`/`ctrl-k` and `cmd-p`/`ctrl-p`
        // bindings cover every other focus site, and dispatching here is
        // idempotent with them — whichever path runs first stops the event.
        // The tab jumps (`cmd-a`/`cmd-e`/`cmd-/`) are platform-only with no
        // `ctrl` fallback, so `ctrl-a`/`ctrl-e`/`ctrl-/` keep reaching the pty.
        if is_go_to_agent_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToAgent), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_go_to_editor_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToEditor), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if is_go_to_terminal_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.alt,
        ) {
            window.dispatch_action(Box::new(GoToTerminal), cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if let Some(mode) = palette_mode_for_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            event.keystroke.modifiers.alt,
        ) {
            match mode {
                PaletteMode::Actions => {
                    window.dispatch_action(Box::new(ToggleActionsPalette), cx);
                }
                PaletteMode::Projects => {
                    window.dispatch_action(Box::new(ToggleProjectsPalette), cx);
                }
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        // Re-snapping an already pinned viewport cannot change the visible
        // rows, so skip the eager grid rebuild on the input path. In
        // full-screen applications such as nvim the viewport is always
        // pinned, which previously doubled the snapshot work per keypress.
        // The re-snap itself paces like any other present.
        let resnap = self.session.as_mut().is_some_and(|session| {
            if session.viewport_active().unwrap_or(true) {
                false
            } else {
                session.scroll_to_bottom();
                true
            }
        });
        if resnap {
            self.present_paced(cx);
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };

        let modifiers = event.keystroke.modifiers;
        if is_paste_shortcut(
            &event.keystroke.key,
            modifiers.platform,
            modifiers.control,
            modifiers.shift,
            modifiers.alt,
        ) {
            match cx.read_from_clipboard() {
                Some(item) => match item.text() {
                    Some(text) => {
                        if let Err(error) = session.paste(&text) {
                            self.error = Some(format!("Paste failed: {error:#}").into());
                            cx.notify();
                        }
                    }
                    // No text on the clipboard (image-only): forward the key
                    // so the hosted application keeps its own handling — e.g.
                    // the agent reads image data directly on Ctrl+Shift+V.
                    // Swallowing it here would break image paste.
                    None if !item.entries().is_empty() => {
                        if let Err(error) = session.send_key(event) {
                            self.error = Some(format!("Keyboard input failed: {error:#}").into());
                            cx.notify();
                        }
                    }
                    None => {
                        self.error = Some("Clipboard has no text to paste".into());
                        cx.notify();
                    }
                },
                None => {
                    self.error = Some("Clipboard has no text to paste".into());
                    cx.notify();
                }
            }
        } else if let Err(error) = session.send_key(event) {
            self.error = Some(format!("Keyboard input failed: {error:#}").into());
            cx.notify();
        }

        window.prevent_default();
        cx.stop_propagation();
    }

    /// Start a drag selection at a window-coordinate mouse position.
    ///
    /// Plain left-drag always selects (mouse clicks are not forwarded to the
    /// terminal application today — see the README — so there is no app
    /// interaction to preserve yet). When click/drag reporting lands, this
    /// should gain a Shift bypass like other terminals. Double-click selects
    /// the word under the cursor, triple-click the whole line; the regular
    /// mouse-up path then copies either one like any drag. Dragging after a
    /// double-click extends character-wise for now — word-wise extension is
    /// future work.
    fn begin_selection(
        &mut self,
        position: Point<Pixels>,
        click_count: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cell) = self.cell_at(position) else {
            return;
        };
        let (anchor, focus) = if click_count >= 3 {
            let (_, row) = cell;
            let width = self
                .rows
                .get(row)
                .map(|row| row.iter().map(|run| run.columns as usize).sum::<usize>())
                .unwrap_or(0);
            if width == 0 {
                (cell, cell)
            } else {
                ((0, row), (width - 1, row))
            }
        } else if click_count == 2 {
            match self
                .rows
                .get(cell.1)
                .and_then(|row| word_bounds_at(row, cell.0))
            {
                Some((start_col, end_col)) => ((start_col, cell.1), (end_col, cell.1)),
                // Past the visible text (padding): fall back to a click.
                None => (cell, cell),
            }
        } else {
            (cell, cell)
        };
        self.selection_anchor = Some(anchor);
        self.selection_focus = Some(focus);
        self.selecting = true;
        cx.notify();
    }

    /// Extend the in-progress drag selection, clamping to the grid.
    fn update_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let Some(cell) = self.cell_at(position) else {
            return;
        };
        if self.selection_focus != Some(cell) {
            self.selection_focus = Some(cell);
            cx.notify();
        }
    }

    /// Finish a drag: copy non-empty selections to the clipboard with a
    /// notification. A plain click (no drag) just clears the highlight.
    /// The highlight stays visible after a copy so the user sees what was
    /// copied; the next click or keypress clears it.
    fn finish_selection(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if let Some(cell) = self.cell_at(position) {
            self.selection_focus = Some(cell);
        }
        let (Some(anchor), Some(focus)) = (self.selection_anchor, self.selection_focus) else {
            cx.notify();
            return;
        };
        let (start, end) = normalize_selection(anchor, focus);
        if start == end {
            self.selection_anchor = None;
            self.selection_focus = None;
            cx.notify();
            return;
        }
        let text = selected_text(&self.rows, start, end);
        if text.is_empty() {
            self.selection_anchor = None;
            self.selection_focus = None;
            cx.notify();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(
            Notification::new()
                .message("copied")
                .id::<TerminalCopyFeedback>(),
            cx,
        );
        self.dismiss_copy_feedback(window, cx);
        cx.notify();
    }

    /// Schedule the copy confirmation to disappear after
    /// [`COPY_FEEDBACK_TTL`]. The toast system only offers a fixed 5s
    /// autohide with no per-notification duration, so we remove our
    /// uniquely tagged toast ourselves. The sequence guard keeps a stale
    /// timer from dismissing a newer copy's toast.
    fn dismiss_copy_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_feedback_seq += 1;
        let seq = self.copy_feedback_seq;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(COPY_FEEDBACK_TTL).await;
            let _ = cx.update(|window, cx| {
                let current = this.update(cx, |pane, _| pane.copy_feedback_seq).ok()?;
                if current == seq {
                    window.remove_notification::<TerminalCopyFeedback>(cx);
                }
                Some(())
            });
        })
        .detach();
    }

    /// Map a window-coordinate position to a grid cell, clamped inside.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let bounds = self.pane_bounds?;
        let rows = self.rows.len();
        if rows == 0 {
            return None;
        }
        let cols = self.grid_size.0.max(1) as usize;
        Some(point_to_cell(position, bounds, rows, cols))
    }

    /// Selected column range `[start, end]` (inclusive) for a grid row, or
    /// `None` when the row is outside the current selection.
    fn selection_for_row(&self, row: usize) -> Option<(usize, usize)> {
        let (anchor, focus) = match (self.selection_anchor, self.selection_focus) {
            (Some(anchor), Some(focus)) => (anchor, focus),
            _ => return None,
        };
        let (start, end) = normalize_selection(anchor, focus);
        let row_columns = self
            .rows
            .get(row)?
            .iter()
            .map(|run| run.columns as usize)
            .sum::<usize>();
        selection_columns_for_row(row, start, end, row_columns)
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.session.as_mut() {
            let error_before = self.error.clone();
            let viewport = window.viewport_size();
            // Terminal applications scale each forwarded press themselves
            // (e.g. nvim `mousescroll`), so undo the platform UI multiplier
            // for the application path (see APP_SCROLL_DIVISOR). The local
            // viewport keeps the multiplied travel to match GUI scroll speed.
            let app = session.app_handles_scroll().unwrap_or(false);
            let travel_px = match event.delta {
                ScrollDelta::Pixels(delta) => {
                    let pixels = delta.y.as_f32();
                    if app {
                        pixels / APP_SCROLL_DIVISOR
                    } else {
                        pixels
                    }
                }
                ScrollDelta::Lines(delta) => {
                    let lines = if app {
                        // Same-value division is exact, so one notch always
                        // yields exactly one line (one press) instead of
                        // accumulating float error across notches.
                        delta.y / APP_SCROLL_DIVISOR
                    } else {
                        delta.y
                    };
                    lines * cell_height()
                }
            };
            let lines = coalesce_scroll_lines(&mut self.scroll_remainder, travel_px);
            if lines == 0 {
                // Not enough finger travel for a whole line yet. The remainder is
                // kept so the gesture stays smooth instead of either dropping the
                // movement or flooding the application with one line per event.
                return;
            }
            let scrolled = session.scroll(
                lines,
                event.position.x.as_f32(),
                event.position.y.as_f32(),
                event.modifiers,
                viewport.width.as_f32(),
                viewport.height.as_f32(),
            );
            match scrolled {
                // Local viewport scrolls repaint through the paced path like
                // everything else; the snapshot work is identical.
                Ok(true) => self.present_paced(cx),
                // The application handles the scroll and redraws through the
                // output task; repainting now would only reshape unchanged
                // rows while its redraw is still in flight.
                Ok(false) => {
                    self.error = None;
                    if self.error != error_before {
                        cx.notify();
                    }
                }
                Err(error) => {
                    self.error = Some(format!("Terminal scroll error: {error:#}").into());
                    if self.error != error_before {
                        cx.notify();
                    }
                }
            }
            cx.stop_propagation();
            window.prevent_default();
        }
    }

    fn render_row(
        row: &[RenderRun],
        index: usize,
        selection: Option<(usize, usize)>,
    ) -> AnyElement {
        let mut text = String::with_capacity(row.iter().map(|run| run.text.len()).sum());
        let mut highlights = Vec::with_capacity(row.len());
        let mut fills = Vec::new();
        let columns = row.iter().map(|run| run.columns as usize).sum::<usize>();
        let mut column = 0_usize;

        for run in row {
            let start = text.len();
            text.push_str(&run.text);
            let end = text.len();
            let (foreground, background) = if run.style.cursor {
                (run.style.background, run.style.foreground)
            } else {
                (run.style.foreground, run.style.background)
            };
            if let Some(kind) = run.block {
                // The run's text is spaces (see `RenderRun::block`); paint the
                // fill as an exact grid-aligned rect over them.
                fills.push((column, run.columns as usize, kind, foreground));
            }
            column += run.columns as usize;
            if start == end {
                continue;
            }
            highlights.push((
                start..end,
                HighlightStyle {
                    color: Some(rgb(foreground).into()),
                    font_weight: run.style.bold.then_some(FontWeight::BOLD),
                    font_style: run.style.italic.then_some(FontStyle::Italic),
                    background_color: Some(rgb(background).into()),
                    underline: run.style.underline.then_some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(rgb(foreground).into()),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            ));
        }

        div()
            .id(("terminal-row", index))
            .relative()
            .w(px(cell_width() * columns as f32))
            .h(px(cell_height()))
            .flex_none()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(app_font_size()))
            .line_height(px(cell_height()))
            .child(StyledText::new(text).with_highlights(highlights))
            .children(
                fills
                    .into_iter()
                    .map(|(start_column, fill_columns, kind, color)| {
                        let (x, y, width, height) =
                            block_fill_bounds(start_column, fill_columns, kind);
                        div()
                            .absolute()
                            .left(px(x))
                            .top(px(y))
                            .w(px(width))
                            .h(px(height))
                            .bg(rgb(color))
                    }),
            )
            .when_some(selection, |this, (start_column, end_column)| {
                // Clamp to the painted row so a drag past the line end still
                // highlights exactly the visible cells.
                let end_column = end_column.min(columns.saturating_sub(1));
                if start_column > end_column || columns == 0 {
                    return this;
                }
                let x = start_column as f32 * cell_width();
                let width = (end_column - start_column + 1) as f32 * cell_width();
                this.child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(0.))
                        .w(px(width))
                        .h(px(cell_height()))
                        .bg(rgb(0x2f81f7))
                        .opacity(0.35),
                )
            })
            .into_any_element()
    }
}

/// Grid-aligned pixel bounds `(x, y, width, height)` for a block fill.
///
/// The rect edges land exactly on cell boundaries, so consecutive fills tile
/// seamlessly. Glyph sprites cannot do this: each is antialiased
/// independently, which darkens every shared boundary.
fn block_fill_bounds(start_column: usize, columns: usize, kind: BlockKind) -> (f32, f32, f32, f32) {
    let cell = cell_width();
    let x = start_column as f32 * cell;
    let width = columns as f32 * cell;
    match kind {
        BlockKind::Upper => (x, 0.0, width, cell_height() / 2.0),
        BlockKind::Lower => (x, cell_height() / 2.0, width, cell_height() / 2.0),
        BlockKind::Full => (x, 0.0, width, cell_height()),
    }
}

/// Order two grid cells `(column, row)` row-major so `start <= end`.
fn normalize_selection(
    anchor: (usize, usize),
    focus: (usize, usize),
) -> ((usize, usize), (usize, usize)) {
    let (acol, arow) = anchor;
    let (fcol, frow) = focus;
    if (arow, acol) <= (frow, fcol) {
        (anchor, focus)
    } else {
        (focus, anchor)
    }
}

/// Inclusive column range selected on `row` given normalized `start <= end`
/// cells and the row's width in columns. `None` when the row is outside the
/// selection or the row is empty.
fn selection_columns_for_row(
    row: usize,
    start: (usize, usize),
    end: (usize, usize),
    row_columns: usize,
) -> Option<(usize, usize)> {
    if row_columns == 0 {
        return None;
    }
    let ((scol, srow), (ecol, erow)) = (start, end);
    if row < srow || row > erow {
        return None;
    }
    let last = row_columns.saturating_sub(1);
    if srow == erow {
        Some((scol.min(last), ecol.min(last)))
    } else if row == srow {
        Some((scol.min(last), last))
    } else if row == erow {
        Some((0, ecol.min(last)))
    } else {
        Some((0, last))
    }
}

/// Map a window-coordinate mouse position to a grid cell, clamped inside
/// `[0, cols) x [0, rows)`. `pane_bounds` is the outer pane's painted bounds;
/// the grid starts after its uniform [`TERMINAL_PADDING`] inset.
fn point_to_cell(
    position: Point<Pixels>,
    pane_bounds: Bounds<Pixels>,
    rows: usize,
    cols: usize,
) -> (usize, usize) {
    let rel_x = position.x.as_f32() - pane_bounds.origin.x.as_f32() - TERMINAL_PADDING;
    let rel_y = position.y.as_f32() - pane_bounds.origin.y.as_f32() - TERMINAL_PADDING;
    let col = (rel_x / cell_width()).floor() as isize;
    let row = (rel_y / cell_height()).floor() as isize;
    (
        col.clamp(0, cols.saturating_sub(1) as isize) as usize,
        row.clamp(0, rows.saturating_sub(1) as isize) as usize,
    )
}

/// Extract the selected text for normalized-or-not `anchor`/`focus` cells.
/// Each line is trailing-trimmed (rows are padded to the full grid width
/// with spaces) and trailing blank lines are dropped; middle lines keep
/// their leading indentation. Returns empty when nothing visible is
/// selected.
fn selected_text(rows: &[Vec<RenderRun>], anchor: (usize, usize), focus: (usize, usize)) -> String {
    let (start, end) = normalize_selection(anchor, focus);
    if start == end || rows.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for row_index in start.1..=end.1 {
        let Some(row) = rows.get(row_index) else {
            break;
        };
        let row_columns = row.iter().map(|run| run.columns as usize).sum::<usize>();
        let Some((scol, ecol)) = selection_columns_for_row(row_index, start, end, row_columns)
        else {
            continue;
        };
        lines.push(slice_row_by_columns(row, scol, ecol).trim_end().to_owned());
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Slice one grid row's visible text to inclusive columns `[start_col,
/// end_col]`. Columns are grid cells, so a double-width character is
/// included when any of its cells overlap the range (see
/// [`char_column_width`] and [`row_cells`]).
fn slice_row_by_columns(row: &[RenderRun], start_col: usize, end_col: usize) -> String {
    let mut out = String::new();
    // Whether the previous character was included: zero-width marks attach
    // to it instead of occupying a column of their own.
    let mut previous_included = false;
    for cell in row_cells(row) {
        if cell.width == 0 {
            if previous_included {
                out.push(cell.ch);
            }
            continue;
        }
        let overlaps = cell.start_col <= end_col && cell.start_col + cell.width > start_col;
        if overlaps {
            out.push(cell.ch);
        }
        previous_included = overlaps;
    }
    out
}

/// One visible character with its grid span `[start_col, start_col + width)`.
/// Zero-width marks (`width == 0`) sit at the column of the character they
/// attach to.
struct RowCell {
    ch: char,
    start_col: usize,
    width: usize,
}

/// Flatten a grid row into its visible characters with grid columns.
/// Empty-text runs (wide spacers split by a style change) advance the cursor
/// without contributing characters; the cursor resyncs to each run's grid
/// width so a width-table miss cannot drift later runs out of alignment.
fn row_cells(row: &[RenderRun]) -> Vec<RowCell> {
    let mut cells = Vec::new();
    let mut column = 0_usize;
    for run in row {
        let run_start = column;
        if !run.text.is_empty() {
            for ch in run.text.chars() {
                let width = char_column_width(ch);
                cells.push(RowCell {
                    ch,
                    start_col: column,
                    width,
                });
                column += width;
            }
        }
        // Authoritative grid position wins over the width table.
        column = run_start + run.columns as usize;
    }
    cells
}

/// Character class for double-click word selection: runs of the same class
/// select as a unit, so identifiers, whitespace gaps, and punctuation runs
/// like `->` each select whole. [`WordClass::Word`] deliberately includes
/// path and URL characters, so `src/pane.rs:123`, `@scope/pkg`, `$HOME`,
/// and `https://host/x?y=1` each select in one double-click (see
/// [`word_bounds_at`] for the trailing-punctuation trim that keeps sentence
/// punctuation like `docs.` out of the selection).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Whitespace,
    Punct,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if ch.is_alphanumeric() || is_path_word_char(ch) {
        WordClass::Word
    } else {
        WordClass::Punct
    }
}

/// Path, reference, and URL characters that read as part of a word for
/// double-click selection: file paths and `file:line:col` refs (`. / - :`),
/// home dirs (`~`), scoped packages (`@`), refs and fragments (`#`),
/// env vars (`$`), queries (`% + = ? &`), and `_` (also an identifier char).
fn is_path_word_char(ch: char) -> bool {
    matches!(
        ch,
        '_' | '.' | '/' | '-' | ':' | '~' | '@' | '#' | '$' | '%' | '+' | '=' | '?' | '&'
    )
}

/// Trailing characters trimmed from a word selection: sentence punctuation
/// (`docs.` copies `docs`, `key:` copies `key`). Interior occurrences are
/// kept (`src/pane.rs:123`), and runs made up entirely of these (e.g.
/// `...`, `::`, `:`) are kept whole so trimming can never empty a selection.
fn is_trimmed_word_tail(ch: char) -> bool {
    matches!(ch, '.' | ':' | '?')
}

/// Inclusive column bounds of the double-click word at `col`, or `None`
/// when the column is past the row's visible text (padding). Zero-width
/// marks occupy no columns, so they never split a word and need no special
/// handling here — the base character's span covers them.
fn word_bounds_at(row: &[RenderRun], col: usize) -> Option<(usize, usize)> {
    let words: Vec<(WordClass, char, usize, usize)> = row_cells(row)
        .into_iter()
        .filter(|cell| cell.width > 0)
        .map(|cell| (word_class(cell.ch), cell.ch, cell.start_col, cell.width))
        .collect();
    let index = words
        .iter()
        .position(|(_, _, start, width)| col >= *start && col < start + width)?;
    let class = words[index].0;
    let mut first = index;
    while first > 0 && words[first - 1].0 == class {
        first -= 1;
    }
    let mut last = index;
    while last + 1 < words.len() && words[last + 1].0 == class {
        last += 1;
    }
    // Trim sentence punctuation off the tail (see `is_trimmed_word_tail`),
    // but keep the run whole when it has no other content.
    if let Some(keep) = (first..=last)
        .rev()
        .find(|&i| !is_trimmed_word_tail(words[i].1))
    {
        last = keep;
    }
    Some((words[first].2, words[last].2 + words[last].3 - 1))
}

/// Terminal cell width of a character: 0 for combining marks, 2 for East
/// Asian wide / fullwidth / emoji presentation, 1 otherwise. This mirrors
/// `wcwidth` closely enough for selection slicing; a miss only shifts a
/// wide line's slice by one cell because [`row_cells`] resyncs to each run's
/// grid width.
fn char_column_width(ch: char) -> usize {
    let value = ch as u32;
    // Zero-width combining marks, variation selectors, and joiners attach
    // to the previous character.
    if matches!(
        value,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0x200B..=0x200F
            | 0xE0100..=0xE01EF
    ) {
        return 0;
    }
    if matches!(
        value,
        0x1100..=0x115F
            | 0x231A..=0x231B
            | 0x2329..=0x232A
            | 0x23E9..=0x23EC
            | 0x23F0
            | 0x23F3
            | 0x25FD..=0x25FE
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F3
            | 0x26F5
            | 0x26FA
            | 0x26FD
            | 0x2705
            | 0x270A..=0x270B
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27B0
            | 0x27BF
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE52
            | 0xFE54..=0xFE66
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F000..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resize_for_window(window);
        let focus = self.focus_handle.clone();
        let empty_message = format!("Starting {}…", self.tab.label());
        let entity = cx.entity().clone();

        div()
            .id(("terminal-pane", self.tab as usize))
            .size_full()
            .p(px(TERMINAL_PADDING))
            .overflow_hidden()
            .bg(rgb(0x090a0a))
            .font_family(TERMINAL_FONT_FAMILY)
            .track_focus(&self.focus_handle)
            .on_prepaint(move |bounds, _, cx| {
                // Recorded without notifying: the next mouse event reads it.
                // Notifying here would schedule another paint every frame.
                entity.update(cx, |pane, _| {
                    pane.pane_bounds = Some(bounds);
                });
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    focus.focus(window, cx);
                    this.begin_selection(event.position, event.click_count, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.update_selection(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.finish_selection(event.position, window, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.finish_selection(event.position, window, cx);
                }),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    v_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().text_color(rgb(0xff6b6b)).child(error))
                        .child(div().text_color(rgb(0x777c7c)).child(
                            "Check that the command and your default shell are available on PATH.",
                        )),
                )
            })
            .when(self.error.is_none() && self.rows.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x777c7c))
                        .child(empty_message),
                )
            })
            .when(self.error.is_none(), |this| {
                this.children(self.rows.iter().enumerate().map(|(row_index, row)| {
                    let selection = self.selection_for_row(row_index);
                    Self::render_row(row, row_index, selection)
                }))
            })
    }
}

/// Platform UI scroll multiplier to undo before forwarding scroll input to a
/// terminal application.
///
/// The Linux backend reports one wheel notch as `Lines(±3.0)` (its internal
/// `SCROLL_LINES`) and triples trackpad pixel deltas the same way, matching
/// GUI scrollview conventions. Terminal applications scale each press
/// themselves (nvim `mousescroll` defaults to 3 lines), so forwarding the
/// multiplied travel scrolls ~3x further per rotation than other terminals.
/// Keep in sync with `SCROLL_LINES` in gpui-pre-linux.
const APP_SCROLL_DIVISOR: f32 = 3.0;

/// How long the "copied" confirmation toast stays visible. The toast
/// system's own autohide is a fixed 5s (see `dismiss_copy_feedback`), which
/// is far too long for a single-word acknowledgement.
const COPY_FEEDBACK_TTL: Duration = Duration::from_millis(1500);

/// Whether a keystroke is a terminal paste shortcut. Pure over the keystroke
/// pieces (not `KeyDownEvent`) so the mapping stays unit-testable without a
/// window, matching [`palette_mode_for_shortcut`](crate::command_palette::palette_mode_for_shortcut).
///
/// Matches `Super/Cmd+V`, `Ctrl+Shift+V` (the standard Linux terminal paste),
/// and `Shift+Insert` (classic X11 paste). Plain `Ctrl+V` is deliberately not
/// paste: shells use it for quoted-insert and editors like nvim for
/// visual-block, so it must keep reaching the pty. `Alt` combinations never
/// match, so option-modified typing keeps reaching the terminal.
fn is_paste_shortcut(key: &str, platform: bool, control: bool, shift: bool, alt: bool) -> bool {
    if alt {
        return false;
    }
    if key.eq_ignore_ascii_case("insert") {
        return shift && !control && !platform;
    }
    if !key.eq_ignore_ascii_case("v") {
        return false;
    }
    if platform && !control {
        return true;
    }
    control && shift && !platform
}

/// Whether a present is due given the last one. Pure helper so the storm
/// cadence is unit-testable without a window: the first present is always
/// due, then at most one per [`TerminalPane::PRESENT_PACE`].
fn present_due(last_present: Option<Instant>, now: Instant) -> bool {
    last_present.is_none_or(|last| now.duration_since(last) >= TerminalPane::PRESENT_PACE)
}

/// Converts a vertical wheel delta in pixels into whole scroll lines,
/// carrying sub-line movement in `remainder` for the next event.
///
/// A single trackpad gesture produces many fractional deltas. Truncating each
/// event independently either drops short gestures or, with a minimum of one
/// line per event, sends far more input than the finger traveled. Coalescing
/// keeps scrolling smooth without outpacing the application's redraw rate.
fn coalesce_scroll_lines(remainder: &mut f32, delta_pixels_y: f32) -> isize {
    let accumulated = (-(delta_pixels_y / cell_height()) + *remainder)
        .clamp(-MAX_SCROLL_LINES_PER_EVENT, MAX_SCROLL_LINES_PER_EVENT);
    let lines = accumulated.trunc() as isize;
    *remainder = accumulated - lines as f32;
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CellStyle;

    #[test]
    fn app_scroll_scales_one_notch_to_one_line() {
        // The backend reports one wheel notch as Lines(±3.0); undoing the UI
        // factor leaves exactly one line, i.e. one press per notch like other
        // terminals. Each step below is exact in f32 (same-value division,
        // scaling by one, same-value division), so no travel leaks into the
        // remainder across notches regardless of font size.
        let mut remainder = 0.0;
        let notch_px = (3.0 / APP_SCROLL_DIVISOR) * cell_height();
        assert_eq!(coalesce_scroll_lines(&mut remainder, notch_px), -1);
        assert!(remainder.abs() < f32::EPSILON);
        assert_eq!(coalesce_scroll_lines(&mut remainder, -notch_px), 1);
        assert!(remainder.abs() < f32::EPSILON);
    }

    #[test]
    fn storm_pacing_defers_presents_within_interval() {
        use std::time::Duration;

        let now = Instant::now();
        // The first present is always due: typing and single actions gain no
        // latency from pacing.
        assert!(present_due(None, now));
        // Back-to-back redraws wait out the interval; the trailing flush
        // presents the latest frame instead.
        assert!(!present_due(Some(now), now));
        assert!(!present_due(
            Some(now),
            now + TerminalPane::PRESENT_PACE - Duration::from_millis(1)
        ));
        assert!(present_due(Some(now), now + TerminalPane::PRESENT_PACE));
        assert!(present_due(Some(now), now + Duration::from_secs(1)));
    }

    #[test]
    fn paste_shortcut_matches_platform_ctrl_shift_and_insert() {
        // Super/Cmd+V, shift-lenient like the other letter shortcuts.
        assert!(is_paste_shortcut("v", true, false, false, false));
        assert!(is_paste_shortcut("V", true, false, true, false));
        // Ctrl+Shift+V: the standard Linux terminal paste.
        assert!(is_paste_shortcut("v", false, true, true, false));
        assert!(is_paste_shortcut("V", false, true, true, false));
        // Shift+Insert: the classic X11 paste.
        assert!(is_paste_shortcut("insert", false, false, true, false));
        assert!(is_paste_shortcut("Insert", false, false, true, false));
        // Plain Ctrl+V must keep reaching the pty (shell quoted-insert,
        // nvim visual-block), as must bare keys and alt combinations.
        assert!(!is_paste_shortcut("v", false, true, false, false));
        assert!(!is_paste_shortcut("v", false, false, false, false));
        assert!(!is_paste_shortcut("v", true, false, false, true));
        assert!(!is_paste_shortcut("v", false, true, true, true));
        assert!(!is_paste_shortcut("insert", false, false, false, false));
        assert!(!is_paste_shortcut("insert", false, true, true, false));
        assert!(!is_paste_shortcut("insert", true, false, true, false));
        assert!(!is_paste_shortcut("c", true, false, false, false));
        assert!(!is_paste_shortcut("Enter", true, false, false, false));
    }

    #[test]
    fn block_fill_bounds_cover_exact_cell_halves() {
        let cell = cell_width();
        assert_eq!(
            block_fill_bounds(3, 2, BlockKind::Upper),
            (3.0 * cell, 0.0, 2.0 * cell, cell_height() / 2.0)
        );
        assert_eq!(
            block_fill_bounds(0, 1, BlockKind::Lower),
            (0.0, cell_height() / 2.0, cell, cell_height() / 2.0)
        );
        assert_eq!(
            block_fill_bounds(5, 4, BlockKind::Full),
            (5.0 * cell, 0.0, 4.0 * cell, cell_height())
        );
    }

    #[test]
    fn adjacent_block_fills_abut_without_gaps() {
        // Consecutive fills must tile seamlessly: the end edge of one is
        // exactly the start edge of the next, so no background shows through.
        let (x0, _, width0, _) = block_fill_bounds(0, 3, BlockKind::Full);
        let (x1, _, _, _) = block_fill_bounds(3, 2, BlockKind::Full);
        assert_eq!(x0 + width0, x1);
    }

    #[test]
    fn scroll_coalescing_preserves_slow_gesture_travel() {
        let mut remainder = 0.0;
        let half_line = cell_height() / 2.0;
        // Half-line deltas accumulate instead of being dropped or amplified.
        assert_eq!(coalesce_scroll_lines(&mut remainder, half_line), 0);
        assert!((remainder - -0.5).abs() < f32::EPSILON);
        assert_eq!(coalesce_scroll_lines(&mut remainder, half_line), -1);
        assert!(remainder.abs() < f32::EPSILON);
    }

    #[test]
    fn scroll_coalescing_handles_both_directions() {
        let mut remainder = 0.0;
        let half_line = cell_height() / 2.0;
        assert_eq!(coalesce_scroll_lines(&mut remainder, -half_line), 0);
        assert_eq!(coalesce_scroll_lines(&mut remainder, -half_line), 1);
        assert!(remainder.abs() < f32::EPSILON);
    }

    #[test]
    fn scroll_coalescing_bounds_a_single_event() {
        let mut remainder = 0.0;
        assert_eq!(coalesce_scroll_lines(&mut remainder, 10_000.0), -12);
        assert_eq!(coalesce_scroll_lines(&mut remainder, -10_000.0), 12);
    }

    fn test_style() -> CellStyle {
        CellStyle {
            foreground: 0xE7E7E7,
            background: 0x090A0A,
            bold: false,
            italic: false,
            underline: false,
            cursor: false,
        }
    }

    fn test_row(text: &str, columns: u16) -> Vec<RenderRun> {
        vec![RenderRun {
            text: text.into(),
            columns,
            style: test_style(),
            block: None,
        }]
    }

    #[test]
    fn selection_normalizes_row_major() {
        assert_eq!(normalize_selection((2, 0), (5, 0)), ((2, 0), (5, 0)));
        // Dragging backwards swaps to row-major order.
        assert_eq!(normalize_selection((5, 0), (2, 0)), ((2, 0), (5, 0)));
        assert_eq!(normalize_selection((7, 3), (1, 1)), ((1, 1), (7, 3)));
        assert_eq!(normalize_selection((4, 2), (4, 2)), ((4, 2), (4, 2)));
    }

    #[test]
    fn selection_row_ranges_cover_first_middle_and_last() {
        // Single row: exactly the dragged columns.
        assert_eq!(
            selection_columns_for_row(1, (2, 1), (5, 1), 10),
            Some((2, 5))
        );
        // First row runs to the line end, middle rows are full, last row
        // starts at column zero.
        assert_eq!(
            selection_columns_for_row(0, (3, 0), (2, 2), 10),
            Some((3, 9))
        );
        assert_eq!(
            selection_columns_for_row(1, (3, 0), (2, 2), 10),
            Some((0, 9))
        );
        assert_eq!(
            selection_columns_for_row(2, (3, 0), (2, 2), 10),
            Some((0, 2))
        );
        // Rows outside the selection and empty rows select nothing.
        assert_eq!(selection_columns_for_row(3, (3, 0), (2, 2), 10), None);
        assert_eq!(selection_columns_for_row(0, (0, 1), (2, 1), 10), None);
        assert_eq!(selection_columns_for_row(0, (0, 0), (2, 0), 0), None);
        // A drag past the line end clamps to the visible cells.
        assert_eq!(
            selection_columns_for_row(0, (2, 0), (99, 0), 8),
            Some((2, 7))
        );
    }

    #[test]
    fn selection_slicing_reads_ascii_runs() {
        let row = test_row("hello world", 11);
        assert_eq!(slice_row_by_columns(&row, 0, 4), "hello");
        assert_eq!(slice_row_by_columns(&row, 6, 10), "world");
        // A drag across a run boundary still reads contiguous text.
        let row = vec![
            RenderRun {
                text: "hel".into(),
                columns: 3,
                style: test_style(),
                block: None,
            },
            RenderRun {
                text: "lo".into(),
                columns: 2,
                style: test_style(),
                block: None,
            },
        ];
        assert_eq!(slice_row_by_columns(&row, 1, 3), "ell");
    }

    #[test]
    fn selection_slicing_includes_overlapped_wide_chars() {
        // `あ` occupies two grid cells but is one character: touching either
        // cell copies it, and the following ASCII stays aligned.
        let row = test_row("あa", 3);
        assert_eq!(slice_row_by_columns(&row, 0, 0), "あ");
        assert_eq!(slice_row_by_columns(&row, 1, 1), "あ");
        assert_eq!(slice_row_by_columns(&row, 2, 2), "a");
        assert_eq!(slice_row_by_columns(&row, 0, 2), "あa");
        assert_eq!(char_column_width('あ'), 2);
        assert_eq!(char_column_width('a'), 1);
        assert_eq!(char_column_width('\u{301}'), 0);
    }

    #[test]
    fn selection_text_trims_padding_and_blank_tail() {
        let rows = vec![
            test_row("hello   ", 8),
            test_row("  indented", 10),
            test_row("        ", 8),
        ];
        // Trailing padding is not copied, indentation is kept, and the
        // blank tail line is dropped instead of adding a trailing newline.
        assert_eq!(selected_text(&rows, (0, 0), (7, 2)), "hello\n  indented");
        // A single click (no drag) copies nothing.
        assert_eq!(selected_text(&rows, (2, 0), (2, 0)), "");
        // A whitespace-only drag copies nothing.
        assert_eq!(selected_text(&rows, (0, 2), (7, 2)), "");
    }

    #[test]
    fn selection_maps_window_points_to_grid_cells() {
        let bounds = Bounds {
            origin: Point {
                x: px(100.),
                y: px(200.),
            },
            ..Default::default()
        };
        let grid = |x: f32, y: f32| Point {
            x: px(100. + TERMINAL_PADDING + x),
            y: px(200. + TERMINAL_PADDING + y),
        };
        // The grid origin maps to the first cell.
        assert_eq!(point_to_cell(grid(0., 0.), bounds, 24, 80), (0, 0));
        // Mid-cell positions floor to their cell.
        assert_eq!(
            point_to_cell(
                grid(cell_width() * 2.5, cell_height() * 1.5),
                bounds,
                24,
                80
            ),
            (2, 1)
        );
        // Positions outside clamp to the grid instead of underflowing.
        assert_eq!(point_to_cell(grid(-50., -50.), bounds, 24, 80), (0, 0));
        assert_eq!(
            point_to_cell(grid(100_000., 100_000.), bounds, 24, 80),
            (79, 23)
        );
    }

    #[test]
    fn double_click_selects_word_runs() {
        let row = test_row("hello world", 11);
        assert_eq!(word_bounds_at(&row, 1), Some((0, 4)));
        assert_eq!(word_bounds_at(&row, 4), Some((0, 4)));
        assert_eq!(word_bounds_at(&row, 6), Some((6, 10)));
        // Whitespace selects the gap itself.
        assert_eq!(word_bounds_at(&row, 5), Some((5, 5)));
        // Past the visible text (padding) selects nothing.
        assert_eq!(word_bounds_at(&row, 11), None);
        assert_eq!(word_bounds_at(&test_row("", 0), 0), None);
    }

    #[test]
    fn double_click_groups_runs_by_class() {
        // `-` is a path word char now, so `foo-` groups while a lone `>`
        // still selects as its own punctuation run.
        let row = test_row("foo->bar", 8);
        assert_eq!(word_bounds_at(&row, 1), Some((0, 3)));
        assert_eq!(word_bounds_at(&row, 3), Some((0, 3)));
        assert_eq!(word_bounds_at(&row, 4), Some((4, 4)));
        assert_eq!(word_bounds_at(&row, 5), Some((5, 7)));
    }

    #[test]
    fn double_click_selects_paths_whole() {
        let row = test_row("see src/pane.rs:123 ok", 22);
        // Clicking anywhere in the ref selects all of it.
        assert_eq!(word_bounds_at(&row, 4), Some((4, 18)));
        assert_eq!(word_bounds_at(&row, 12), Some((4, 18)));
        assert_eq!(word_bounds_at(&row, 18), Some((4, 18)));
        // Neighbouring words are unaffected.
        assert_eq!(word_bounds_at(&row, 0), Some((0, 2)));
        assert_eq!(word_bounds_at(&row, 20), Some((20, 21)));
    }

    #[test]
    fn double_click_trims_sentence_punctuation() {
        // Trailing `.`/`:`/`?` read as sentence punctuation, not content.
        assert_eq!(word_bounds_at(&test_row("docs.", 5), 1), Some((0, 3)));
        assert_eq!(word_bounds_at(&test_row("key:", 4), 1), Some((0, 2)));
        assert_eq!(word_bounds_at(&test_row("Really?", 7), 1), Some((0, 5)));
        assert_eq!(word_bounds_at(&test_row("bar...", 6), 5), Some((0, 2)));
        // Interior occurrences are kept.
        assert_eq!(word_bounds_at(&test_row("a.b:c", 5), 2), Some((0, 4)));
        // Runs made only of trimmables are kept whole, never emptied.
        assert_eq!(word_bounds_at(&test_row("...", 3), 1), Some((0, 2)));
        assert_eq!(word_bounds_at(&test_row("::", 2), 0), Some((0, 1)));
        assert_eq!(word_bounds_at(&test_row("x : y", 5), 2), Some((2, 2)));
    }

    #[test]
    fn double_click_covers_wide_chars_by_cell() {
        // `あ` is alphanumeric, so `あa` is one word run spanning cols 0-2.
        let row = test_row("あa", 3);
        assert_eq!(word_bounds_at(&row, 0), Some((0, 2)));
        assert_eq!(word_bounds_at(&row, 1), Some((0, 2)));
        // Either cell of `あ` still resolves into the run: with trailing
        // punctuation the word stops after the wide char's second cell.
        let row = test_row("あ!", 3);
        assert_eq!(word_bounds_at(&row, 0), Some((0, 1)));
        assert_eq!(word_bounds_at(&row, 1), Some((0, 1)));
        assert_eq!(word_bounds_at(&row, 2), Some((2, 2)));
    }

    #[test]
    fn full_line_selection_copies_trimmed_text() {
        let rows = vec![test_row("  indented   ", 13), test_row("next", 4)];
        assert_eq!(selected_text(&rows, (0, 0), (12, 0)), "  indented");
    }
}
