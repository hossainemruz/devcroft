//! Shared PR cards and the dedicated board; all writes use Home's stale-write guard.
use super::*;
use crate::pull_requests::Ci;

#[derive(Clone)]
struct DragPr(Item, String);

impl Render for DragPr {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .child(self.1.clone())
    }
}

impl HomeView {
    pub(crate) fn is_pull_requests_page(&self) -> bool {
        self.page == Some("Pull Requests")
    }

    pub(super) fn visible_pull_requests(&self, category: Category) -> Vec<Item> {
        self.data
            .pull_requests(category, Some(self.active_space.as_str()))
    }

    pub(super) fn refresh_pull_requests(&mut self, force: bool, cx: &mut Context<Self>) {
        let urls: Vec<_> = self
            .data
            .items
            .iter()
            .filter(|item| item.kind == Kind::PullRequest)
            .map(|item| item.url.clone())
            .collect();
        // Always prune deleted/edited URLs, even if an older batch is running.
        self.pr_status.entries.retain(|url, _| urls.contains(url));
        if !self.active || (self.page.is_some() && !self.is_pull_requests_page()) || self.pr_loading
        {
            return;
        }
        let pending = self.pr_status.begin(&urls, force);
        if pending.is_empty() {
            return;
        }
        self.pr_loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            for url in pending {
                let fetch_url = url.clone();
                let result = cx
                    .background_spawn(async move { crate::pull_requests::fetch(&fetch_url) })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.pr_status.finish(&url, result);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.pr_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn pr_refresh_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("refresh-pr-status")
            .ghost()
            .label(if self.pr_loading {
                "Refreshing…"
            } else {
                "Refresh status"
            })
            .disabled(self.pr_loading)
            .on_click(cx.listener(|this, _, _, cx| this.refresh_pull_requests(true, cx)))
    }

    fn move_pr(
        &mut self,
        original: &Item,
        category: Category,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        self.change(window, cx, |data| data.move_pr(original, category));
    }

    pub(super) fn pr_card(
        &self,
        item: &Item,
        draggable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entry = self.pr_status.entries.get(&item.url);
        let status = entry.and_then(|entry| entry.status.as_ref());
        let title = status
            .map(|status| status.title.clone())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| item.title.clone());
        let mut badges = h_flex().gap_2().flex_wrap();
        if let Some(status) = status {
            let (state, color) = match status.state.as_str() {
                "MERGED" => ("Merged", ColorName::Purple),
                "CLOSED" => ("Closed", ColorName::Red),
                "OPEN" if status.is_draft => ("Draft", ColorName::Gray),
                "OPEN" => ("Open", ColorName::Green),
                _ => ("State unknown", ColorName::Gray),
            };
            let approval_color = match status.approval() {
                "Approved" => ColorName::Green,
                "Changes requested" => ColorName::Red,
                "Approval required" => ColorName::Amber,
                _ => ColorName::Gray,
            };
            let (ci, ci_color) = match status.ci() {
                Ci::Passed => ("CI passed", ColorName::Green),
                Ci::Failed => ("CI failed", ColorName::Red),
                Ci::Pending => ("CI pending", ColorName::Amber),
                Ci::None => ("No CI checks", ColorName::Gray),
                Ci::Unknown => ("CI unknown", ColorName::Gray),
            };
            for (label, color) in [
                (state, color),
                (status.approval(), approval_color),
                (ci, ci_color),
            ] {
                badges = badges.child(Tag::color(color).with_size(Size::Small).child(label));
            }
        }
        if entry.is_some_and(|entry| entry.error.is_some()) {
            // Scannable error indicator: red when there is no data at all,
            // amber caution when showing retained stale data.
            let (label, color) = if status.is_some() {
                ("Stale", ColorName::Amber)
            } else {
                ("Fetch failed", ColorName::Red)
            };
            badges = badges.child(Tag::color(color).with_size(Size::Small).child(label));
        }
        let delete_id = item.id.clone();
        let updated = if let Some(at) = entry.and_then(|entry| entry.updated_at) {
            Some(format!(
                "Updated {}{}",
                relative_duration_label(current_unix_secs().saturating_sub(at)),
                if entry.is_some_and(|entry| entry.fetching) {
                    " · refreshing…"
                } else {
                    ""
                }
            ))
        } else if entry.is_none_or(|entry| entry.fetching) {
            Some("Fetching status…".to_owned())
        } else {
            None
        };
        let menu_item = item.clone();
        let menu_title = title.clone();
        let entity = cx.entity();
        let edit_item = item.clone();
        let mut card = v_flex()
            .id(item_id("pr", &item.id))
            .w_full()
            .min_w_0()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if self.is_cursor_item(&item.id) {
                cx.theme().ring
            } else {
                cx.theme().border
            })
            .bg(cx.theme().background)
            .when(draggable, |card| {
                card.on_drag(DragPr(item.clone(), title.clone()), |drag, _, _, cx| {
                    cx.new(|_| drag.clone())
                })
            })
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_2()
                    .child(
                        title_link(item, "pr-title", &title, cx).child(
                            div()
                                .w_full()
                                .overflow_hidden()
                                .whitespace_normal()
                                .child(title.clone()),
                        ),
                    )
                    .child(
                        Button::new(item_id("pr-menu", &item.id))
                            .ghost()
                            .label("⋯")
                            .accessibility_label(format!("Options for {menu_title}"))
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                                let home = entity.clone();
                                let edit = edit_item.clone();
                                menu = menu.item(PopupMenuItem::new("Edit").on_click(
                                    move |_, window, cx| {
                                        home.update(cx, |this, cx| {
                                            this.editor(edit.clone(), window, cx);
                                        });
                                    },
                                ));
                                for category in Category::ALL {
                                    if category == menu_item.category {
                                        continue;
                                    }
                                    let item = menu_item.clone();
                                    let home = entity.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(format!("Move to {}", category.label()))
                                            .on_click(move |_, window, cx| {
                                                home.update(cx, |this, cx| {
                                                    this.move_pr(&item, category, window, cx)
                                                })
                                            }),
                                    );
                                }
                                let home = entity.clone();
                                let id = delete_id.clone();
                                menu.separator().item(
                                    PopupMenuItem::new("Remove from tracking").on_click(
                                        move |_, window, cx| {
                                            home.update(cx, |this, cx| {
                                                this.change(window, cx, |data| {
                                                    data.items.retain(|item| item.id != id);
                                                    Ok(())
                                                });
                                            });
                                        },
                                    ),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            item.url
                                .trim_start_matches("https://github.com/")
                                .replace("/pull/", " #"),
                        ),
                    )
                    .when_some(updated, |row, updated| {
                        row.child(div().flex_none().max_w(px(170.)).truncate().child(updated))
                    }),
            )
            .child(badges);
        if let Some(error) = entry.and_then(|entry| entry.error.as_ref()) {
            card = card.child(div().text_xs().text_color(cx.theme().danger).child(format!(
                "{}: {error}",
                if status.is_some() {
                    "Status stale"
                } else {
                    "Status unavailable"
                }
            )));
        }
        card
    }

    pub(super) fn pull_requests_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filters = h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Space"),
            )
            .child(
                Tag::secondary()
                    .with_size(Size::Small)
                    .rounded_full()
                    .child(self.active_space.clone()),
            );
        let mut board = h_flex().items_stretch().gap_4().flex_wrap();
        for (index, category) in Category::ALL.into_iter().enumerate() {
            let items = self.visible_pull_requests(category);
            let mut column = v_flex()
                .id(("pr-column", index))
                .flex_1()
                .min_w(px(290.))
                .min_h(px(380.))
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .on_drop(cx.listener(move |this, drag: &DragPr, window, cx| {
                    this.move_pr(&drag.0, category, window, cx)
                }))
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .child(div().font_semibold().child(category.label()))
                        .child(Tag::secondary().child(items.len().to_string())),
                );
            if items.is_empty() {
                column = column.child(
                    div()
                        .py_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No pull requests"),
                );
            }
            for item in items {
                column = column.child(self.pr_card(&item, true, cx));
            }
            let space = self.active_space.clone();
            column = column.child(
                Button::new(("add-pr-column", index))
                    .ghost()
                    .mt_auto()
                    .label("+ Add PR")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let mut item = Item::new(Kind::PullRequest);
                        item.category = category;
                        item.space = space.clone();
                        this.editor(item, window, cx);
                    })),
            );
            board = board.child(column);
        }
        v_flex().gap_4()
            .child(h_flex().justify_between().gap_3().flex_wrap().child(filters).child(self.pr_refresh_button(cx)))
            .child(div().text_sm().text_color(cx.theme().muted_foreground)
                .child("Drag PRs between columns or use the card menu. Status refreshes every minute."))
            .when_some(self.error.clone(), |view, error| view.child(div().text_color(cx.theme().danger).child(error)))
            .child(board)
    }
}
