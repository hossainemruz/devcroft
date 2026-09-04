//! The terminal view: one GPUI element per workspace tab.
//!
//! [`TerminalPane`] owns the rendered grid ([`RenderRun`] rows produced by
//! [`TerminalSession`](crate::session::TerminalSession)) and translates
//! keyboard, scroll, and focus events into session input. Terminal-backed
//! tabs share this implementation; the tab itself only selects the label
//! and the startup command.

use std::path::Path;

use gpui_kit::component::v_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, FocusHandle, Focusable, FontStyle, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollWheelEvent,
    SharedString, Styled, StyledText, UnderlineStyle, Window, div, px, rgb,
};

use crate::{
    fonts::TERMINAL_FONT_FAMILY,
    metrics::{
        CELL_HEIGHT, CHROME_HEIGHT, INITIAL_COLS, INITIAL_ROWS, MAX_SCROLL_LINES_PER_EVENT,
        TERMINAL_FONT_SIZE, TERMINAL_PADDING, cell_width,
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
                            pane.process_output(first, &output);
                            cx.notify();
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

    fn process_output(&mut self, first: Vec<u8>, output: &async_channel::Receiver<Vec<u8>>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        session.feed(&first);
        while let Ok(bytes) = output.try_recv() {
            session.feed(&bytes);
        }
        match session.snapshot() {
            Ok(Some(rows)) => {
                self.rows = rows;
                self.error = None;
            }
            // The render state reports no changes, so the cached rows are
            // still current.
            Ok(None) => self.error = None,
            Err(error) => self.error = Some(format!("Terminal render error: {error:#}").into()),
        }
    }

    fn resize_for_window(&mut self, window: &Window) {
        let viewport = window.viewport_size();
        let cell_width = cell_width();
        let width = (viewport.width.as_f32() - TERMINAL_PADDING * 2.0).max(cell_width);
        let height =
            (viewport.height.as_f32() - CHROME_HEIGHT - TERMINAL_PADDING * 2.0).max(CELL_HEIGHT);
        let cols = (width / cell_width).floor() as u16;
        let rows = (height / CELL_HEIGHT).floor() as u16;
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
        let Some(session) = self.session.as_mut() else {
            return;
        };

        // Re-snapping an already pinned viewport cannot change the visible
        // rows, so skip the eager grid rebuild on the input path. In
        // full-screen applications such as nvim the viewport is always
        // pinned, which previously doubled the snapshot work per keypress.
        if !session.viewport_active().unwrap_or(true) {
            session.scroll_to_bottom();
            match session.snapshot() {
                Ok(Some(rows)) => {
                    self.rows = rows;
                    self.error = None;
                }
                Ok(None) => self.error = None,
                Err(error) => self.error = Some(format!("Terminal render error: {error:#}").into()),
            }
            cx.notify();
        }

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
        let delta = event.delta.pixel_delta(px(CELL_HEIGHT)).y.as_f32();
        let lines = coalesce_scroll_lines(&mut self.scroll_remainder, delta);
        if lines == 0 {
            // Not enough finger travel for a whole line yet. The remainder is
            // kept so the gesture stays smooth instead of either dropping the
            // movement or flooding the application with one line per event.
            return;
        }
        if let Some(session) = self.session.as_mut() {
            let viewport = window.viewport_size();
            match session.scroll(
                lines,
                event.position.x.as_f32(),
                event.position.y.as_f32(),
                event.modifiers,
                viewport.width.as_f32(),
                viewport.height.as_f32(),
            ) {
                Ok(true) => match session.snapshot() {
                    Ok(Some(rows)) => {
                        self.rows = rows;
                        self.error = None;
                    }
                    Ok(None) => self.error = None,
                    Err(error) => {
                        self.error = Some(format!("Terminal scroll error: {error:#}").into())
                    }
                },
                Ok(false) => self.error = None,
                Err(error) => self.error = Some(format!("Terminal scroll error: {error:#}").into()),
            }
            cx.notify();
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
            .h(px(CELL_HEIGHT))
            .flex_none()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(TERMINAL_FONT_SIZE))
            .line_height(px(CELL_HEIGHT))
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
        BlockKind::Upper => (x, 0.0, width, CELL_HEIGHT / 2.0),
        BlockKind::Lower => (x, CELL_HEIGHT / 2.0, width, CELL_HEIGHT / 2.0),
        BlockKind::Full => (x, 0.0, width, CELL_HEIGHT),
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

/// Converts a vertical wheel delta in pixels into whole scroll lines,
/// carrying sub-line movement in `remainder` for the next event.
///
/// A single trackpad gesture produces many fractional deltas. Truncating each
/// event independently either drops short gestures or, with a minimum of one
/// line per event, sends far more input than the finger traveled. Coalescing
/// keeps scrolling smooth without outpacing the application's redraw rate.
fn coalesce_scroll_lines(remainder: &mut f32, delta_pixels_y: f32) -> isize {
    let accumulated = (-(delta_pixels_y / CELL_HEIGHT) + *remainder)
        .clamp(-MAX_SCROLL_LINES_PER_EVENT, MAX_SCROLL_LINES_PER_EVENT);
    let lines = accumulated.trunc() as isize;
    *remainder = accumulated - lines as f32;
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_fill_bounds_cover_exact_cell_halves() {
        let cell = cell_width();
        assert_eq!(
            block_fill_bounds(3, 2, BlockKind::Upper),
            (3.0 * cell, 0.0, 2.0 * cell, CELL_HEIGHT / 2.0)
        );
        assert_eq!(
            block_fill_bounds(0, 1, BlockKind::Lower),
            (0.0, CELL_HEIGHT / 2.0, cell, CELL_HEIGHT / 2.0)
        );
        assert_eq!(
            block_fill_bounds(5, 4, BlockKind::Full),
            (5.0 * cell, 0.0, 4.0 * cell, CELL_HEIGHT)
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
        let half_line = CELL_HEIGHT / 2.0;
        // Half-line deltas accumulate instead of being dropped or amplified.
        assert_eq!(coalesce_scroll_lines(&mut remainder, half_line), 0);
        assert!((remainder - -0.5).abs() < f32::EPSILON);
        assert_eq!(coalesce_scroll_lines(&mut remainder, half_line), -1);
        assert!(remainder.abs() < f32::EPSILON);
    }

    #[test]
    fn scroll_coalescing_handles_both_directions() {
        let mut remainder = 0.0;
        let half_line = CELL_HEIGHT / 2.0;
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
