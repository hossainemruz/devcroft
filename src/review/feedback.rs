//! Comment interaction and sidebar, kept separate from diff layout.
use super::{
    ReviewState, ReviewView,
    comments::{Anchor, Comment, Side, Store},
    model::FileContent,
    stream::StreamRow,
};
use gpui_kit::{
    component::{
        h_flex,
        input::{Textarea, TextareaState},
        v_flex,
    },
    prelude::FluentBuilder as _,
    *,
};
use std::rc::Rc;

fn anchor_end_row(loaded: &super::LoadedReview, anchor: &Anchor) -> Option<usize> {
    if anchor.outdated {
        return None;
    }
    loaded.rows.iter().rposition(|row| {
        let StreamRow::Line { file, hunk, line } = *row else {
            return false;
        };
        let file = &loaded.diff.files[file];
        let FileContent::Text { hunks, .. } = &file.content else {
            return false;
        };
        let line = &hunks[hunk].lines[line];
        file.path == anchor.path
            && match anchor.side {
                Side::Old => line.old_no,
                Side::New => line.new_no,
            } == Some(anchor.end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{
        LoadedReview,
        model::{ChangedFile, FileStatus, ReviewDiff, diff_text},
        stream::flatten,
    };

    #[::core::prelude::v1::test]
    fn editor_follows_range_end_on_each_side_and_rejects_missing_anchors() {
        let (hunks, truncated) = diff_text("first\nold\nlast\n", "first\nnew\nextra\nlast\n");
        let diff = ReviewDiff {
            files: vec![ChangedFile {
                path: "file.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                additions: 2,
                deletions: 1,
                content: FileContent::Text { hunks, truncated },
            }],
            base_commit: "base".into(),
            head_commit: "head".into(),
            base_ref: None,
            head_branch: None,
        };
        let (rows, file_row_start) = flatten(&diff);
        let loaded = LoadedReview {
            diff,
            rows,
            file_row_start,
            syntax: Default::default(),
        };
        let mut anchor = Anchor {
            path: "file.rs".into(),
            side: Side::New,
            start: 2,
            end: 3,
            outdated: false,
            source: String::new(),
        };
        let ix = anchor_end_row(&loaded, &anchor).unwrap();
        let StreamRow::Line { file, hunk, line } = loaded.rows[ix] else {
            panic!("expected a code row");
        };
        let FileContent::Text { hunks, .. } = &loaded.diff.files[file].content else {
            panic!("expected text");
        };
        assert_eq!(hunks[hunk].lines[line].text, "extra");
        anchor.side = Side::Old;
        anchor.end = 2;
        assert!(anchor_end_row(&loaded, &anchor).unwrap() < ix);
        anchor.outdated = true;
        assert_eq!(anchor_end_row(&loaded, &anchor), None);
        assert_eq!(
            thread_anchor_row(&loaded, &anchor),
            loaded.file_row_start[0]
        );
        anchor.outdated = false;
        anchor.path = "missing.rs".into();
        assert_eq!(anchor_end_row(&loaded, &anchor), None);
        assert_eq!(thread_anchor_row(&loaded, &anchor), loaded.rows.len());
    }
}

fn thread_anchor_row(loaded: &super::LoadedReview, anchor: &Anchor) -> usize {
    anchor_end_row(loaded, anchor).unwrap_or_else(|| {
        loaded
            .diff
            .files
            .iter()
            .position(|f| f.path == anchor.path)
            .map(|i| loaded.file_row_start[i])
            .unwrap_or(loaded.rows.len())
    })
}

#[derive(Default)]
pub(super) struct Feedback {
    store: Option<Store>,
    comments: Vec<Rc<Comment>>,
    selection: Option<Anchor>,
    input: Option<Entity<TextareaState>>,
    editing: Option<Rc<Comment>>,
    active: Option<String>,
    error: Option<String>,
    filter: usize,
    busy: bool,
    scroll: ScrollHandle,
}

impl ReviewView {
    pub(super) fn comment_count(&self) -> usize {
        self.feedback.comments.len()
    }

    pub(super) fn comment_revisions(&self) -> Vec<(String, u64)> {
        self.feedback
            .comments
            .iter()
            .map(|c| (c.id.clone(), c.revision))
            .collect()
    }

    fn thread_row(&self, c: &Comment) -> Option<usize> {
        let ReviewState::Loaded(loaded) = &self.state else {
            return None;
        };
        Some(thread_anchor_row(loaded, &c.anchor))
    }

    pub(super) fn inline_thread_groups(
        &self,
    ) -> std::collections::HashMap<usize, Vec<Rc<Comment>>> {
        let mut groups: std::collections::HashMap<usize, Vec<Rc<Comment>>> = Default::default();
        for comment in &self.feedback.comments {
            if let Some(row) = self.thread_row(comment) {
                groups.entry(row).or_default().push(comment.clone());
            }
        }
        groups
    }

    pub(super) fn render_inline_threads(
        &self,
        comments: &[Rc<Comment>],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .gap_2()
            .children(comments.iter().map(|c| self.render_thread(c.clone(), cx)))
            .into_any_element()
    }

    pub(super) fn is_new_comment(&self) -> bool {
        self.feedback.editing.is_none()
    }

    pub(super) fn inline_editor_row(&self) -> Option<usize> {
        self.feedback.input.as_ref()?;
        if let Some(c) = &self.feedback.editing {
            return self.thread_row(c);
        }
        let anchor = self
            .feedback
            .selection
            .as_ref()
            .or_else(|| self.feedback.editing.as_ref().map(|c| &c.anchor))?;
        let ReviewState::Loaded(loaded) = &self.state else {
            return None;
        };
        anchor_end_row(loaded, anchor)
    }

    pub(super) fn render_comment_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(input) = &self.feedback.input else {
            return div().into_any_element();
        };
        let label = self
            .feedback
            .editing
            .as_ref()
            .map(|c| format!("Edit {}", c.id))
            .or_else(|| {
                self.feedback
                    .selection
                    .as_ref()
                    .map(|a| format!("Comment on lines {}–{}", a.start, a.end))
            })
            .unwrap_or_default();
        let error_message = self.feedback.error.clone().unwrap_or_default();
        v_flex()
            .h(px(210.))
            .flex_none()
            .w_full()
            .min_w_0()
            .p_2()
            .gap_2()
            .border_1()
            .border_color(rgb(0x292b2b))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().text_sm().child(label))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(Textarea::new(input).h_full()),
            )
            .when(!error_message.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xfbbf24))
                        .child(error_message.clone()),
                )
            })
            .child(
                h_flex()
                    .gap_3()
                    .text_sm()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x244434))
                            .cursor_pointer()
                            .child(if self.feedback.busy {
                                "Saving…"
                            } else {
                                "Save comment"
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.save_comment(cx)),
                            ),
                    )
                    .child(div().cursor_pointer().child("Cancel").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            if this.feedback.busy {
                                return;
                            }
                            this.feedback.input = None;
                            this.feedback.editing = None;
                            this.feedback.selection = None;
                            cx.notify();
                        }),
                    )),
            )
            .into_any_element()
    }

    pub(super) fn load_comments(&mut self, result: anyhow::Result<(Store, Vec<Comment>)>) {
        match result {
            Ok((store, comments)) => {
                if self
                    .feedback
                    .store
                    .as_ref()
                    .is_none_or(|s| s.pair != store.pair || s.scope != store.scope)
                {
                    self.feedback = Feedback::default();
                }
                self.feedback.store = Some(store);
                self.feedback.comments = comments.into_iter().map(Rc::new).collect();
                self.feedback.error = None;
            }
            Err(e) => {
                self.feedback.error = Some(format!("{e:#}"));
                self.feedback.store = None;
            }
        }
    }

    pub(super) fn select_line(
        &mut self,
        row: StreamRow,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.feedback.busy {
            return;
        }
        let ReviewState::Loaded(loaded) = &self.state else {
            return;
        };
        let StreamRow::Line { file, hunk, line } = row else {
            return;
        };
        let file = &loaded.diff.files[file];
        let FileContent::Text { hunks, .. } = &file.content else {
            return;
        };
        let line = &hunks[hunk].lines[line];
        let (mut side, mut number) = match line.new_no {
            Some(n) => (Side::New, n),
            None => (Side::Old, line.old_no.unwrap()),
        };
        if extend && let Some(previous) = &self.feedback.selection {
            let selected_number = match previous.side {
                Side::Old => line.old_no,
                Side::New => line.new_no,
            };
            if previous.path != file.path || selected_number.is_none() {
                self.feedback.error =
                    Some("Select a range within one file and one side of the diff.".into());
                cx.notify();
                return;
            }
            side = previous.side;
            number = selected_number.unwrap();
        }
        let result = super::git::anchor_source(&self.cwd, &loaded.diff, &file.path, side);
        match result {
            Ok(Some(source)) => {
                let mut anchor = Anchor {
                    path: file.path.clone(),
                    side,
                    start: number,
                    end: number,
                    source,
                    outdated: false,
                };
                if extend
                    && let Some(previous) = &self.feedback.selection
                    && previous.path == anchor.path
                    && previous.side == anchor.side
                {
                    anchor.start = previous.start.min(number);
                    anchor.end = previous.start.max(number);
                }
                // Reject a stale displayed line after external worktree changes.
                let source_lines: Vec<_> = anchor.source.lines().collect();
                if hunks.iter().flat_map(|h| &h.lines).any(|line| {
                    let n = match side {
                        Side::Old => line.old_no,
                        Side::New => line.new_no,
                    };
                    n.is_some_and(|n| {
                        n >= anchor.start
                            && n <= anchor.end
                            && source_lines
                                .get(n as usize - 1)
                                .map(|s| s.trim_end_matches('\r'))
                                != Some(line.text.as_str())
                    })
                }) {
                    self.feedback.error =
                        Some("Code changed. Refresh the diff before commenting.".into());
                } else {
                    self.feedback.selection = Some(anchor);
                    if self.feedback.editing.take().is_some() {
                        self.feedback.input = None;
                    }
                    self.feedback.error = None;
                    if self.feedback.input.is_none() {
                        self.feedback.input = Some(cx.new(|cx| {
                            TextareaState::new(window, cx)
                                .auto_grow(3, 3)
                                .placeholder("Write review feedback…")
                        }));
                    }
                    if let Some(input) = &self.feedback.input {
                        input.read(cx).focus_handle(cx).focus(window, cx);
                    }
                }
            }
            Ok(None) => {
                self.feedback.error =
                    Some("This file is no longer available. Refresh the diff.".into())
            }
            Err(e) => self.feedback.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    pub(super) fn line_feedback(&self, row: StreamRow) -> (bool, usize) {
        let ReviewState::Loaded(loaded) = &self.state else {
            return (false, 0);
        };
        let StreamRow::Line { file, hunk, line } = row else {
            return (false, 0);
        };
        let file = &loaded.diff.files[file];
        let FileContent::Text { hunks, .. } = &file.content else {
            return (false, 0);
        };
        let line = &hunks[hunk].lines[line];
        let matches = |a: &Anchor| {
            a.path == file.path
                && !a.outdated
                && match a.side {
                    Side::Old => line.old_no,
                    Side::New => line.new_no,
                }
                .is_some_and(|n| n >= a.start && n <= a.end)
        };
        (
            self.feedback.selection.as_ref().is_some_and(matches)
                || self
                    .feedback
                    .comments
                    .iter()
                    .any(|c| Some(&c.id) == self.feedback.active.as_ref() && matches(&c.anchor)),
            self.feedback
                .comments
                .iter()
                .filter(|c| matches(&c.anchor))
                .count(),
        )
    }

    fn save_comment(&mut self, cx: &mut Context<Self>) {
        if self.feedback.busy {
            return;
        }
        let Some(store) = &self.feedback.store else {
            return;
        };
        let Some(input) = &self.feedback.input else {
            return;
        };
        let body = input.read(cx).value().to_string();
        let store = store.clone();
        let editing = self
            .feedback
            .editing
            .as_ref()
            .map(|c| (c.id.clone(), c.revision));
        let anchor = self.feedback.selection.clone();
        self.write_comment(
            store,
            true,
            move |store| {
                if let Some((id, revision)) = editing {
                    store.change(&id, Some(revision), Some(body), None, false)?;
                    Ok(Some(id))
                } else if let Some(a) = anchor {
                    let id = store.create(a, body)?;
                    Ok(Some(id))
                } else {
                    anyhow::bail!("Select a line first")
                }
            },
            cx,
        );
    }

    fn comment_action(&mut self, c: &Comment, delete: bool, cx: &mut Context<Self>) {
        if self.feedback.busy {
            return;
        }
        let Some(store) = &self.feedback.store else {
            return;
        };
        let clear = self.feedback.editing.as_ref().is_some_and(|e| e.id == c.id);
        let c = c.clone();
        self.write_comment(
            store.clone(),
            clear,
            move |store| {
                store.change(&c.id, Some(c.revision), None, Some(!c.resolved), delete)?;
                Ok(None)
            },
            cx,
        );
    }

    fn write_comment(
        &mut self,
        store: Store,
        clear_editor: bool,
        action: impl FnOnce(&Store) -> anyhow::Result<Option<String>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.feedback.busy = true;
        if let Some(input) = &self.feedback.input {
            input.update(cx, |input, cx| input.set_disabled(true, cx));
        }
        let generation = self.generation;
        let cwd = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    store.ensure_checkout(&cwd)?;
                    let activated = action(&store)?;
                    let comments = store.list()?;
                    anyhow::Ok((activated, comments))
                })
                .await;
            let _ =
                this.update(cx, |this, cx| {
                    this.feedback.busy = false;
                    if let Some(input) = &this.feedback.input {
                        input.update(cx, |input, cx| input.set_disabled(false, cx));
                    }
                    if this.generation != generation {
                        this.reload(cx);
                        return;
                    }
                    match result {
                        Ok((activated, comments)) => {
                            this.feedback.comments = comments.into_iter().map(Rc::new).collect();
                            this.feedback.error = None;
                            if clear_editor {
                                this.feedback.input = None;
                                this.feedback.editing = None;
                                this.feedback.selection = None;
                            }
                            // A fresh save stays highlighted through `active`;
                            // the selection is gone by now, so without this the
                            // blue range highlight would vanish on save.
                            if let Some(id) = activated {
                                this.feedback.active = Some(id);
                            } else if this.feedback.active.as_ref().is_some_and(|id| {
                                !this.feedback.comments.iter().any(|c| &c.id == id)
                            }) {
                                this.feedback.active = None;
                            }
                        }
                        Err(e) => this.feedback.error = Some(format!("{e:#}")),
                    }
                    cx.notify();
                });
        })
        .detach();
        cx.notify();
    }

    fn visible_comments(&self) -> Vec<Rc<Comment>> {
        self.feedback
            .comments
            .iter()
            .filter(|c| match self.feedback.filter {
                1 => !c.resolved,
                2 => c.resolved,
                3 => c.anchor.outdated,
                _ => true,
            })
            .cloned()
            .collect()
    }

    fn jump_comment(&mut self, c: &Comment, window: &mut Window, cx: &mut Context<Self>) {
        self.feedback.active = Some(c.id.clone());
        if let Some(i) = self
            .visible_comments()
            .iter()
            .position(|item| item.id == c.id)
        {
            self.feedback.scroll.scroll_to_item(i);
        }
        if let Some(row) = self.thread_row(c) {
            if let ReviewState::Loaded(loaded) = &self.state
                && let Some(stream_row) = loaded.rows.get(row)
            {
                let path = &loaded.diff.files[stream_row.file()].path;
                self.last_scrolled = Some(path.clone());
                let id: SharedString = super::tree::file_item_id(path).into();
                self.tree_state.update(cx, |state, cx| {
                    state.set_selected_index(state.index_of(&id), cx)
                });
            }
            // Center the inline thread when possible: leave ~half a viewport
            // of context above the anchor row so the comment box lands near
            // the middle instead of pinned to the top. Near the start/end
            // of the file the list clamps, which is the "when possible".
            let above_px = (window.viewport_size().height * 0.5 - px(120.)).max(px(0.));
            let above_rows = (above_px / px(super::stream::ROW_H)) as usize;
            self.list_handle.scroll_to(ListOffset {
                item_ix: row.saturating_sub(above_rows),
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    fn render_thread(&self, c: Rc<Comment>, cx: &mut Context<Self>) -> AnyElement {
        let jump = c.clone();
        let edit = c.clone();
        let resolve = c.clone();
        let delete = c.clone();
        let excerpt = c
            .anchor
            .source
            .lines()
            .skip(c.anchor.start as usize - 1)
            .take((c.anchor.end - c.anchor.start + 1).min(8) as usize)
            .collect::<Vec<_>>()
            .join("\n");
        v_flex()
            .flex_none()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.jump_comment(&jump, window, cx);
                }),
            )
            .gap_2()
            .p_2()
            .border_1()
            .border_color(rgb(if self.feedback.active.as_ref() == Some(&c.id) {
                0x7dd3fc
            } else {
                0x292b2b
            }))
            .child(div().cursor_pointer().text_xs().child(format!(
                "{} · {} · {:?} {}–{}{}{}",
                c.id,
                c.anchor.path,
                c.anchor.side,
                c.anchor.start,
                c.anchor.end,
                if c.resolved {
                    " · resolved"
                } else {
                    " · open"
                },
                if c.anchor.outdated {
                    " · outdated"
                } else {
                    ""
                }
            )))
            .when(c.anchor.outdated, |d| {
                d.child(div().text_xs().text_color(rgb(0x858989)).child(excerpt))
            })
            .child(c.body.clone())
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .child(div().cursor_pointer().child("Edit").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if this.feedback.busy {
                                return;
                            }
                            let body = edit.body.clone();
                            this.feedback.input = Some(cx.new(|cx| {
                                let mut input = TextareaState::new(window, cx).auto_grow(3, 3);
                                input.set_value(body, window, cx);
                                input
                            }));
                            this.feedback.editing = Some(edit.clone());
                            this.feedback.selection = None;
                            if let Some(input) = &this.feedback.input {
                                input.read(cx).focus_handle(cx).focus(window, cx);
                            }
                            cx.notify();
                        }),
                    ))
                    .child(
                        div()
                            .cursor_pointer()
                            .child(if c.resolved { "Reopen" } else { "Resolve" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.comment_action(&resolve, false, cx)
                                }),
                            ),
                    )
                    .child(div().cursor_pointer().child("Delete").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.comment_action(&delete, true, cx)
                        }),
                    )),
            )
            .when(
                self.feedback
                    .editing
                    .as_ref()
                    .is_some_and(|editing| editing.id == c.id),
                |d| d.child(self.render_comment_editor(cx)),
            )
            .into_any_element()
    }

    pub(super) fn render_comments(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = v_flex()
            .w(px(340.))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(rgb(0x292b2b))
            .p_3()
            .gap_2()
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex().gap_2().children(
                    ["All", "Open", "Resolved", "Outdated"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, label)| {
                            div()
                                .text_xs()
                                .cursor_pointer()
                                .when(self.feedback.filter == i, |d| d.text_color(rgb(0x7dd3fc)))
                                .child(label)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.feedback.filter = i;
                                        cx.notify();
                                    }),
                                )
                        }),
                ),
            );
        if let Some(error) = &self.feedback.error {
            panel = panel.child(div().text_color(rgb(0xf87171)).child(error.clone()));
        }
        if self.feedback.busy {
            panel = panel.child("Saving…");
        }
        let cards = self.visible_comments().into_iter().map(|c| {
            let jump = c.clone();
            v_flex()
                .flex_none()
                .gap_2()
                .p_2()
                .border_1()
                .border_color(rgb(if self.feedback.active.as_ref() == Some(&c.id) {
                    0x7dd3fc
                } else {
                    0x292b2b
                }))
                .when(c.anchor.outdated, |d| d.bg(rgb(0x292511)))
                .when(!c.anchor.outdated && c.resolved, |d| d.bg(rgb(0x10271b)))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.jump_comment(&jump, window, cx);
                    }),
                )
                .child(div().text_xs().text_color(rgb(0x858989)).child(format!(
                    "{} · {}–{}",
                    c.anchor.path, c.anchor.start, c.anchor.end
                )))
                .child(c.body.clone())
        });
        panel
            .child(
                div()
                    .id("review-comments")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.feedback.scroll)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(cards),
            )
            .into_any_element()
    }
}
