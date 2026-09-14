//! Dedicated To Read page: a flat list of reading cards.
//! All writes use Home's stale-write guard.
use super::*;

impl HomeView {
    pub(crate) fn is_reading_page(&self) -> bool {
        self.page == Some("To Read")
    }

    pub(super) fn visible_reading(&self) -> Vec<Item> {
        self.data
            .items
            .iter()
            .filter(|i| i.kind == Kind::Reading && (self.show_completed || !i.completed))
            .cloned()
            .collect()
    }

    pub(super) fn reading_page(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let card_width = reading_card_width(f32::from(window.viewport_size().width));
        let show_completed = self.show_completed;
        let filters = h_flex().gap_2().flex_wrap().items_center().child(
            Checkbox::new("show-completed-reading-page")
                .label("Show completed")
                .checked(show_completed)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.show_completed = *checked;
                    this.navigation_cursor = None;
                    cx.notify();
                })),
        );
        // Two fixed-width cards per row: a lone card on the last row
        // keeps its half width instead of stretching, and widths are
        // floored to whole pixels so fractional rounding can't wrap a
        // card that mathematically fits (notably on Retina 2x).
        let mut rows = h_flex().gap_3().flex_wrap().w_full();
        let items = self.visible_reading();
        if items.is_empty() {
            rows = rows.child(
                div()
                    .w_full()
                    .py_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No links saved yet. Add your first one below."),
            );
        }
        for item in items {
            rows = rows.child(
                div()
                    .flex_none()
                    .w(px(card_width))
                    .child(self.reading_card(&item, cx)),
            );
        }
        v_flex().gap_4()
            .child(filters)
            .child(div().text_sm().text_color(cx.theme().muted_foreground)
                .child("Save links to read later. Mark them read when done."))
            .when_some(self.error.clone(), |view, error| view.child(div().text_color(cx.theme().danger).child(error)))
            .child(rows)
            .child(
                h_flex().w_full().child(
                    Button::new("add-reading-page")
                        .ghost()
                        .label("+ Add link")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.editor(Item::new(Kind::Reading), window, cx);
                        })),
                ),
            )
    }
}

/// Half the page content width for exactly two cards per row, mirroring
/// `recent_card_width`: the page caps at 1440px with 24px gutters and the
/// row uses 12px gaps.
fn reading_card_width(viewport_width: f32) -> f32 {
    let available = (viewport_width.min(1440.) - 48.).max(1.);
    ((available - 12.) / 2.).floor()
}
