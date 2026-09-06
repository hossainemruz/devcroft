//! The terminal view: one GPUI element per workspace tab.
//!
//! [`TerminalPane`] owns the rendered grid ([`RenderRun`] rows produced by
//! [`TerminalSession`](crate::session::TerminalSession)) and translates
//! keyboard, scroll, and focus events into session input. Terminal-backed
//! tabs share this implementation; the tab itself only selects the label
//! and the startup command.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui_kit::component::v_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, FocusHandle, Focusable, FontStyle, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, StyledText, UnderlineStyle, Window, div, px, rgb,
};

use crate::{
    command_palette::ToggleCommandPalette,
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
}

impl TerminalPane {
    pub(crate) fn new(tab: WorkspaceTab, cwd: &Path, cx: &mut Context<Self>) -> Self {
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
        };

        let output = match TerminalSession::spawn(tab, cwd) {
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
        // The command bar toggle must reach the workspace even while a
        // terminal has focus: everything below would otherwise be sent to the
        // pty. Dispatch it as an action (handled by `Workspace`) instead of
        // terminal input. The global `cmd-k`/`ctrl-k` binding covers every
        // other focus site, and dispatching here is idempotent with it —
        // whichever path runs first stops the event.
        if event.keystroke.key.eq_ignore_ascii_case("k")
            && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
            && !event.keystroke.modifiers.alt
        {
            window.dispatch_action(Box::new(ToggleCommandPalette), cx);
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
        if modifiers.platform && event.keystroke.key.eq_ignore_ascii_case("v") {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                && let Err(error) = session.paste(&text)
            {
                self.error = Some(format!("Paste failed: {error:#}").into());
                cx.notify();
            }
        } else if let Err(error) = session.send_key(event) {
            self.error = Some(format!("Keyboard input failed: {error:#}").into());
            cx.notify();
        }

        window.prevent_default();
        cx.stop_propagation();
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

    fn render_row(row: &[RenderRun], index: usize) -> AnyElement {
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

        div()
            .id(("terminal-pane", self.tab as usize))
            .size_full()
            .p(px(TERMINAL_PADDING))
            .overflow_hidden()
            .bg(rgb(0x090a0a))
            .font_family(TERMINAL_FONT_FAMILY)
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(move |_, _, window, cx| focus.focus(window, cx)),
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
                this.children(
                    self.rows
                        .iter()
                        .enumerate()
                        .map(|(row_index, row)| Self::render_row(row, row_index)),
                )
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
}
