//! Full-color harness logos for the Agent surfaces.
//!
//! The SVGs under `assets/icons/` (`claude.svg`, `codex.svg`, `opencode.svg`,
//! `omp.svg`) are embedded with `include_bytes!` and rasterized once
//! through the app's SVG renderer into [`img`] tiles — the same pattern as
//! [`crate::review::icons`]. GPUI's `Svg` element renders monochrome
//! silhouettes, which would flatten these brand marks, so rows use the
//! rasterized tiles instead.
//!
//! Sessions from unknown providers (a removed harness or a hand-edited id)
//! have no logo by definition and fall back to the neutral `Bot` glyph so
//! rows never render an empty slot.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::{Icon, IconName};
use gpui_kit::{AnyElement, App, IntoElement, Styled as _, px};

use crate::agent::AgentKind;

/// Display size of agent logos; tiles rasterize at 3x for retina.
pub(crate) const ICON_PX: f32 = 16.0;
/// Smaller logo for inline use in muted `text_xs` subtitle lines (session
/// sidebar rows, Home cards), where 16px dwarfs the surrounding text.
pub(crate) const ICON_INLINE_PX: f32 = 10.0;
const RASTER_SCALE: f32 = 3.0;

/// Raw bytes for a harness logo, or `None` when the harness has no vendored
/// mark. Callers fall back to the `Bot` glyph.
pub(crate) fn icon_svg(agent: AgentKind) -> Option<&'static [u8]> {
    match agent {
        AgentKind::Opencode => Some(include_bytes!("../assets/icons/opencode.svg")),
        AgentKind::Claude => Some(include_bytes!("../assets/icons/claude.svg")),
        AgentKind::Codex => Some(include_bytes!("../assets/icons/codex.svg")),
        AgentKind::Omp => Some(include_bytes!("../assets/icons/omp.svg")),
    }
}

/// Rasterized full-color logo tiles, keyed by harness.
pub(crate) type AgentIconTiles = HashMap<AgentKind, Arc<gpui_kit::RenderImage>>;

/// Rasterize tiles for every harness in `agents`, skipping keys already
/// cached and harnesses without a vendored logo. Call once when a view is
/// created — each SVG parses and rasterizes once per view lifetime, never
/// per frame. Unparseable SVGs are skipped; rows fall back to the `Bot`
/// glyph.
pub(crate) fn ensure_tiles(
    agents: impl IntoIterator<Item = AgentKind>,
    tiles: &mut AgentIconTiles,
    cx: &App,
) {
    let renderer = cx.svg_renderer();
    for agent in agents {
        if tiles.contains_key(&agent) {
            continue;
        }
        let Some(bytes) = icon_svg(agent) else {
            continue;
        };
        if let Ok(tile) = renderer.render_single_frame(bytes, RASTER_SCALE) {
            tiles.insert(agent, tile);
        }
    }
}

/// Full-color logo for a known harness, or the neutral `Bot` glyph for
/// harnesses without a logo. Always returns an element so rows never branch.
pub(crate) fn agent_icon(agent: AgentKind, tiles: &AgentIconTiles, size_px: f32) -> AnyElement {
    match tiles.get(&agent) {
        Some(tile) => gpui_kit::img(tile.clone())
            .size(px(size_px))
            .rounded_sm()
            .into_any_element(),
        None => Icon::new(IconName::Bot)
            .size(px(size_px))
            .into_any_element(),
    }
}

/// Logo for a session row whose provider may be unknown (a removed harness
/// or a hand-edited id). Known harnesses resolve through `tiles`; anything
/// else gets the `Bot` glyph with the same footprint so rows stay aligned.
pub(crate) fn session_icon(
    agent: Option<AgentKind>,
    tiles: &AgentIconTiles,
    size_px: f32,
) -> AnyElement {
    match agent {
        Some(agent) => agent_icon(agent, tiles, size_px),
        None => Icon::new(IconName::Bot)
            .size(px(size_px))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_logos_are_non_empty_svgs() {
        for agent in AgentKind::ALL {
            let bytes = icon_svg(agent).expect("harness should have a logo");
            assert!(!bytes.is_empty());
            assert!(
                bytes.starts_with(b"<svg"),
                "{agent:?} does not look like an SVG"
            );
        }
    }
}
