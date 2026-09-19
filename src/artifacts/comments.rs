use super::*;
use crate::preview::BlockAction;

impl ArtifactBrowser {
    pub(super) fn sync_preview_comments(&self, cx: &mut Context<Self>) {
        let (Some(preview), Some(snapshot)) = (&self.preview, &self.selected) else {
            return;
        };
        let view = cx.entity().downgrade();
        let id = snapshot.artifact.id.clone();
        let content = snapshot.artifact.content.clone();
        let handler: crate::preview::CommentHandler =
            std::sync::Arc::new(move |action, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if !this
                        .selected
                        .as_ref()
                        .is_some_and(|s| s.artifact.id == id && s.artifact.content == content)
                    {
                        this.error =
                            Some("The document changed. Select the block again to comment.".into());
                        cx.notify();
                        return;
                    }
                    match action {
                        BlockAction::Comment { block, quote } => {
                            if this.draft.is_some() || this.saving {
                                return;
                            }
                            this.edit(false, None, window, cx);
                            if let Some(draft) = &mut this.draft {
                                draft.block = Some(block);
                                draft.quote = quote;
                            }
                            this.comments_scroll
                                .set_offset(gpui_kit::point(px(0.), px(0.)));
                            cx.notify();
                        }
                        BlockAction::Open(id) => this.open_comment(id, window, cx),
                    }
                });
            });
        preview.update(cx, |preview, cx| {
            preview.set_comments(
                snapshot.artifact.comments.clone(),
                self.active_comment.clone(),
                self.draft.is_some() || self.saving,
                handler,
                cx,
            )
        });
    }

    fn open_comment(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .selected
            .as_ref()
            .and_then(|s| s.artifact.comments.iter().position(|c| c.id == id))
        else {
            return;
        };
        self.show_comments = true;
        self.active_comment = Some(id.clone());
        self.comments_scroll.scroll_to_item(index);
        if self.draft.is_none() {
            self.comments_focus.focus(window, cx);
        }
        if let Some(preview) = &self.preview {
            preview.update(cx, |p, cx| p.reveal_comment(&id, cx));
        }
        cx.notify();
    }

    pub(super) fn render_comments_panel(
        &self,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut panel = v_flex().flex_1().min_h_0().gap_2().p_2().child(
            Button::new("new-artifact-comment")
                .small()
                .label("Add document comment")
                .disabled(self.draft.is_some() || self.saving)
                .on_click(cx.listener(|this, _, window, cx| this.begin_comment(window, cx))),
        );
        if let Some(draft) = self.draft.as_ref().filter(|d| !d.document) {
            let anchor = draft
                .comment
                .as_ref()
                .and_then(|id| {
                    draft
                        .snapshot
                        .artifact
                        .comments
                        .iter()
                        .find(|c| &c.id == id)
                })
                .and_then(|c| c.anchor.as_ref());
            let quote = draft
                .quote
                .clone()
                .or_else(|| anchor.and_then(|a| a.block().quote.clone()));
            let block = draft.block.and_then(|index| {
                crate::data::artifacts::anchors::blocks(&draft.snapshot.artifact.content)
                    .get(index)
                    .cloned()
            });
            let label = if draft.comment.is_some() {
                "Edit comment".to_owned()
            } else if let Some(block) = &block {
                format!("Comment on lines {}–{}", block.start_line, block.end_line)
            } else {
                "Document comment".to_owned()
            };
            panel = panel.child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .child(div().text_sm().font_semibold().child(label))
                    .when_some(quote, |d, quote| {
                        d.child(
                            div()
                                .text_sm()
                                .max_h(px(84.))
                                .overflow_hidden()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("“{quote}”")),
                        )
                    })
                    .child(Textarea::new(&draft.input))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("save-resource-comment")
                                    .primary()
                                    .small()
                                    .label("Save comment")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_draft(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("cancel-resource-comment")
                                    .small()
                                    .label("Cancel")
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_draft(window, cx)
                                    })),
                            ),
                    ),
            );
        }
        let mut cards = v_flex()
            .id("artifact-comments")
            .track_focus(&self.comments_focus)
            .track_scroll(&self.comments_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .focus(|s| s.border_1().border_color(cx.theme().ring));
        if snapshot.artifact.comments.is_empty() {
            cards = cards.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "No comments yet. Right-click a block or use its + button to add feedback.",
                    ),
            );
        }
        for (index, comment) in snapshot.artifact.comments.iter().enumerate() {
            let active = self.active_comment.as_ref() == Some(&comment.id);
            let outdated = comment.anchor.as_ref().is_some_and(|a| a.block().outdated);
            let label = comment
                .anchor
                .as_ref()
                .map(|a| format!("Lines {}–{}", a.block().start_line, a.block().end_line))
                .unwrap_or_else(|| "Document".into());
            let quote = comment.anchor.as_ref().map(|a| {
                a.block()
                    .quote
                    .as_ref()
                    .unwrap_or(&a.block().source)
                    .clone()
            });
            let id = comment.id.clone();
            let menu_view = cx.entity().downgrade();
            let menu_snapshot = snapshot.clone();
            let menu_comment = comment.clone();
            let card = v_flex()
                .id(("artifact-comment", index))
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(if active {
                    cx.theme().ring
                } else {
                    cx.theme().border
                })
                .when(active, |d| d.bg(cx.theme().accent.opacity(0.5)))
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{label}{}{}",
                                    if outdated { " · Outdated" } else { "" },
                                    if comment.resolved { " · Resolved" } else { "" }
                                )),
                        )
                        .child(
                            Button::new(("artifact-comment-options", index))
                                .ghost()
                                .small()
                                .compact()
                                .label("⋯")
                                .accessibility_label("Comment options")
                                .disabled(self.draft.is_some() || self.saving)
                                .dropdown_menu_with_anchor(
                                    Anchor::BottomRight,
                                    move |menu, _, _| {
                                        let view = menu_view.clone();
                                        let id = menu_comment.id.clone();
                                        let menu = menu.item(PopupMenuItem::new("Edit").on_click(
                                            move |_, window, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.edit(false, Some(id.clone()), window, cx)
                                                });
                                            },
                                        ));
                                        let view = menu_view.clone();
                                        let id = menu_comment.id.clone();
                                        let resolved = menu_comment.resolved;
                                        let snapshot = menu_snapshot.clone();
                                        let menu = menu.item(
                                            PopupMenuItem::new(if resolved {
                                                "Reopen"
                                            } else {
                                                "Resolve"
                                            })
                                            .on_click(move |_, _, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.mutate(
                                                        snapshot.clone(),
                                                        Mutation::Comment(CommentChange::Resolve(
                                                            id.clone(),
                                                            !resolved,
                                                        )),
                                                        false,
                                                        cx,
                                                    )
                                                });
                                            }),
                                        );
                                        let view = menu_view.clone();
                                        let id = menu_comment.id.clone();
                                        let snapshot = menu_snapshot.clone();
                                        menu.item(PopupMenuItem::new("Delete").on_click(
                                            move |_, _, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.mutate(
                                                        snapshot.clone(),
                                                        Mutation::Comment(CommentChange::Delete(
                                                            id.clone(),
                                                        )),
                                                        false,
                                                        cx,
                                                    )
                                                });
                                            },
                                        ))
                                    },
                                ),
                        ),
                )
                .child(
                    v_flex()
                        .id(("artifact-comment-content", index))
                        .gap_2()
                        .cursor_pointer()
                        .when_some(quote, |d, quote| {
                            d.child(
                                div()
                                    .text_sm()
                                    .max_h(px(90.))
                                    .overflow_hidden()
                                    .border_l_2()
                                    .border_color(cx.theme().border)
                                    .pl_2()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(quote),
                            )
                        })
                        .child(div().text_sm().child(comment.body.clone()))
                        .when(outdated, |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Original block changed or is unavailable."),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_comment(id.clone(), window, cx)
                        })),
                );
            cards = cards.child(card);
        }
        panel.child(cards)
    }
}
