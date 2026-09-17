//! Shared terminal grid geometry and measured font metrics.
//!
//! The app-wide font size (General settings) is a process-global [`f32`]
//! that every pane reads at render time, so changing it in Settings takes
//! effect without restarting. Cell geometry derives from it: the row height
//! scales with the font's natural line box (see [`cell_height`]), and the
//! cell width scales linearly from the width measured once at
//! [`BASE_FONT_SIZE`] — exact for monospace advances.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, anyhow};

pub(crate) const INITIAL_COLS: u16 = 100;
pub(crate) const INITIAL_ROWS: u16 = 32;
pub(crate) const FALLBACK_CELL_WIDTH: f32 = 8.0;
/// Reference size the cell width is measured at (see [`crate::fonts`]).
/// The live size ([`app_font_size`]) scales from here.
pub(crate) const BASE_FONT_SIZE: f32 = 15.0;
/// Default app-wide font size, matching the original fixed terminal size.
pub(crate) const DEFAULT_APP_FONT_SIZE: f32 = 15.0;
/// Smallest settable app-wide font size.
pub(crate) const MIN_APP_FONT_SIZE: f32 = 10.0;
/// Largest settable app-wide font size.
pub(crate) const MAX_APP_FONT_SIZE: f32 = 24.0;
/// Terminal row height, derived from the live font size.
///
/// The bundled JetBrains Mono reports the same 1320-unit line box (ascender
/// 1020, descender -300, no gap) at 1000 units-per-em across its hhea, typo,
/// and win metrics, i.e. a 1.32 factor. Deriving the height keeps rows matched
/// to the font's natural line box whenever the size changes instead of
/// clipping glyphs or opening seams between rows.
pub(crate) fn cell_height() -> f32 {
    app_font_size() * 1.32
}

/// Font size of the Review diff stream, kept proportionally smaller than the
/// terminal panes as before (13pt at the 15pt default).
pub(crate) fn review_font_size() -> f32 {
    app_font_size() * (13.0 / BASE_FONT_SIZE)
}
pub(crate) const TERMINAL_PADDING: f32 = 12.0;
pub(crate) const WORKSPACE_HEADER_HEIGHT: f32 = 48.0;
/// Maximum scroll lines forwarded to the terminal per wheel event.
///
/// Trackpads emit high-frequency fractional deltas; without a bound a single
/// coarse event could queue a burst of input the application cannot keep up
/// with. The total finger travel is still preserved across events via the
/// accumulated remainder.
pub(crate) const MAX_SCROLL_LINES_PER_EVENT: f32 = 12.0;

/// Live app-wide font size, backing [`app_font_size`]. Stored as bits so the
/// global stays lock-free; writes are rare (Settings stepper) and reads are
/// per-frame.
static APP_FONT_SIZE_BITS: AtomicU32 = AtomicU32::new(DEFAULT_APP_FONT_SIZE.to_bits());

static TERMINAL_CELL_WIDTH: OnceLock<f32> = OnceLock::new();

/// Clamp a candidate size into the settable range. Non-finite input falls
/// back to the default instead of panicking (`f32::clamp` rejects NaN).
pub(crate) fn clamp_app_font_size(size: f32) -> f32 {
    if !size.is_finite() {
        return DEFAULT_APP_FONT_SIZE;
    }
    size.clamp(MIN_APP_FONT_SIZE, MAX_APP_FONT_SIZE)
}

/// Current app-wide font size (General settings). Always within
/// [`MIN_APP_FONT_SIZE`]..=[`MAX_APP_FONT_SIZE`].
pub(crate) fn app_font_size() -> f32 {
    f32::from_bits(APP_FONT_SIZE_BITS.load(Ordering::Relaxed))
}

/// Update the live size. Values are clamped (see [`clamp_app_font_size`]);
/// panes pick it up on their next render.
pub(crate) fn set_app_font_size(size: f32) {
    APP_FONT_SIZE_BITS.store(clamp_app_font_size(size).to_bits(), Ordering::Relaxed);
}

pub(crate) fn cell_width() -> f32 {
    let base = TERMINAL_CELL_WIDTH
        .get()
        .copied()
        .unwrap_or(FALLBACK_CELL_WIDTH);
    base * (app_font_size() / BASE_FONT_SIZE)
}

pub(crate) fn init_cell_width(width: f32) -> Result<()> {
    TERMINAL_CELL_WIDTH
        .set(width)
        .map_err(|_| anyhow!("terminal font metrics were initialized more than once"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_clamp_keeps_usable_bounds() {
        assert_eq!(clamp_app_font_size(15.0), 15.0);
        assert_eq!(clamp_app_font_size(4.0), MIN_APP_FONT_SIZE);
        assert_eq!(clamp_app_font_size(100.0), MAX_APP_FONT_SIZE);
        assert_eq!(clamp_app_font_size(f32::NAN), DEFAULT_APP_FONT_SIZE);
        assert_eq!(clamp_app_font_size(f32::INFINITY), DEFAULT_APP_FONT_SIZE);
    }

    #[test]
    fn cell_geometry_tracks_the_default_size() {
        // No test mutates the global (parallel tests share it), so the live
        // size here is the default and geometry matches the original consts.
        assert_eq!(app_font_size(), DEFAULT_APP_FONT_SIZE);
        assert_eq!(cell_height(), DEFAULT_APP_FONT_SIZE * 1.32);
        assert!(
            (review_font_size() - 13.0).abs() < 0.001,
            "review size should stay ~13pt at default, got {}",
            review_font_size()
        );
    }
}
