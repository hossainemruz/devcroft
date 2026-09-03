//! Shared terminal grid geometry and measured font metrics.

use std::sync::OnceLock;

use anyhow::{Result, anyhow};

pub(crate) const INITIAL_COLS: u16 = 100;
pub(crate) const INITIAL_ROWS: u16 = 32;
pub(crate) const FALLBACK_CELL_WIDTH: f32 = 8.0;
pub(crate) const TERMINAL_FONT_SIZE: f32 = 13.0;
pub(crate) const CELL_HEIGHT: f32 = 18.0;
pub(crate) const TERMINAL_PADDING: f32 = 12.0;
pub(crate) const CHROME_HEIGHT: f32 = 90.0;
pub(crate) const CONTENT_PADDING: f32 = 8.0;
pub(crate) const CONTENT_BORDER: f32 = 1.0;
pub(crate) const WORKSPACE_HEADER_HEIGHT: f32 = 58.0;
/// Maximum scroll lines forwarded to the terminal per wheel event.
///
/// Trackpads emit high-frequency fractional deltas; without a bound a single
/// coarse event could queue a burst of input the application cannot keep up
/// with. The total finger travel is still preserved across events via the
/// accumulated remainder.
pub(crate) const MAX_SCROLL_LINES_PER_EVENT: f32 = 12.0;

static TERMINAL_CELL_WIDTH: OnceLock<f32> = OnceLock::new();

pub(crate) fn cell_width() -> f32 {
    TERMINAL_CELL_WIDTH
        .get()
        .copied()
        .unwrap_or(FALLBACK_CELL_WIDTH)
}

pub(crate) fn init_cell_width(width: f32) -> Result<()> {
    TERMINAL_CELL_WIDTH
        .set(width)
        .map_err(|_| anyhow!("terminal font metrics were initialized more than once"))
}
