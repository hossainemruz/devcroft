//! Resource-only block interaction using public Markdown plugin hooks. Each
//! block retains the normal TextView renderer, including references and code.
use super::*;
use crate::data::artifacts::{
    Comment,
    anchors::{self, MarkdownBlock},
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::text::{MarkdownNode, MarkdownParseContext, MarkdownPlugin, markdown_ast};
use gpui_kit::component::{Disableable as _, Sizable as _};
use std::sync::Arc;

pub(crate) enum BlockAction {
    Comment { block: usize, quote: Option<String> },
    Open(String),
}
pub(crate) type CommentHandler = Arc<dyn Fn(BlockAction, &mut Window, &mut App) + Send + Sync>;

pub(super) struct ResourceBlocks {
    pub source: SharedString,
    pub blocks: Vec<MarkdownBlock>,
    display_ranges: Arc<Vec<std::ops::Range<usize>>>,
    states: Vec<Option<Entity<TextViewState>>>,
    definitions: String,
    pub comments: Vec<Comment>,
    pub active: Option<String>,
    pub drafting: bool,
    pub handler: Option<CommentHandler>,
}
impl ResourceBlocks {
    pub fn new(source: SharedString, display: &str) -> Self {
        let blocks = anchors::blocks(&source);
        let definitions = blocks
            .iter()
            .filter(|block| block.definition)
            .map(|block| &source[block.range.clone()])
            .collect::<Vec<_>>()
            .join("\n\n");
        Self {
            states: vec![None; blocks.len()],
            source,
            blocks,
            definitions,
            display_ranges: Arc::new(
                anchors::blocks(display)
                    .into_iter()
                    .map(|block| block.range)
                    .collect(),
            ),
            comments: Vec::new(),
            active: None,
            drafting: false,
            handler: None,
        }
    }
}

struct BlockPlugin {
    ranges: Arc<Vec<std::ops::Range<usize>>>,
    view: gpui_kit::WeakEntity<PreviewView>,
    style: TextViewStyle,
}
impl MarkdownPlugin for BlockPlugin {
    fn name(&self) -> &str {
        "devcroft-resource-block"
    }
    fn is_block(&self) -> bool {
        true
    }
    fn parse(
        &self,
        node: &markdown_ast::Node,
        context: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        if matches!(node, markdown_ast::Node::Definition(_)) {
            return None;
        }
        let pos = node.position()?;
        let range = context.offset() + pos.start.offset..context.offset() + pos.end.offset;
        let index = self.ranges.iter().position(|r| *r == range)?;
        Some(
            MarkdownNode::new(self.name(), index)
                .text(heading_plain_text(node))
                .markdown(context.node_source(node).unwrap_or_default().to_owned()),
        )
    }
    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let index = *node.data::<usize>().expect("resource block index");
        self.view
            .update(cx, |view, cx| {
                view.render_resource_block(index, self.style.clone(), window, cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl PreviewView {
    pub(crate) fn set_comments(
        &mut self,
        comments: Vec<Comment>,
        active: Option<String>,
        drafting: bool,
        handler: CommentHandler,
        cx: &mut Context<Self>,
    ) {
        if let Some(resources) = &mut self.resources {
            let changed = resources.comments != comments
                || resources.active != active
                || resources.drafting != drafting;
            resources.comments = comments;
            resources.active = active;
            resources.drafting = drafting;
            resources.handler = Some(handler);
            if changed {
                cx.notify();
            }
        }
    }
    pub(crate) fn reveal_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(resources) = &mut self.resources else {
            return;
        };
        resources.active = Some(id.to_owned());
        if let Some(anchor) = resources
            .comments
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.anchor.as_ref())
        {
            let anchor = anchor.block();
            if !anchor.outdated
                && let Some(index) = resources
                    .blocks
                    .iter()
                    .position(|b| b.range.start == anchor.start && b.range.end == anchor.end)
            {
                self.state.read(cx).list_state().scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
            }
        }
        cx.notify();
    }
    pub(super) fn resource_plugin(
        &self,
        style: TextViewStyle,
        cx: &Context<Self>,
    ) -> impl MarkdownPlugin {
        BlockPlugin {
            ranges: self.resources.as_ref().unwrap().display_ranges.clone(),
            view: cx.entity().downgrade(),
            style,
        }
    }
    /// The shared stock renderer is also used for each resource block. No
    /// block plugin is installed recursively on these inner text views.
    pub(super) fn text_view(
        &self,
        state: &Entity<TextViewState>,
        style: TextViewStyle,
        cx: &Context<Self>,
    ) -> TextView {
        TextView::new(state)
            .markdown_extensions(MarkdownExtensions::default().frontmatter())
            .plugin(FrontmatterPlugin)
            .plugin(crate::markdown_references::ReferencePlugin {
                details: self.reference_details.clone(),
                open: self.reference_handler(cx),
            })
            .on_link_click({
                let open = self.reference_handler(cx);
                move |url, event, window, cx| {
                    crate::markdown_references::open_link(url, event, window, cx, &open)
                }
            })
            .style(style)
            .code_block_highlighter({
                let dark = cx.theme().is_dark();
                let cache = self.code_highlights.clone();
                move |block| {
                    let code = block.code();
                    let language = block.lang();
                    cache
                        .lock()
                        .entry((code.clone(), language.clone(), dark))
                        .or_insert_with(|| highlight_code(&code, language.as_deref(), dark))
                        .clone()
                }
            })
    }
    fn render_resource_block(
        &mut self,
        index: usize,
        style: TextViewStyle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let Some(resources) = &mut self.resources else {
            return div().into_any_element();
        };
        let Some(block) = resources.blocks.get(index) else {
            return div().into_any_element();
        };
        let state = resources.states[index]
            .get_or_insert_with(|| {
                let source = metadata_line_breaks(&resources.source[block.range.clone()]);
                // Definitions outside a block must still resolve when it is rendered alone.
                let source = if resources.definitions.is_empty() {
                    source
                } else {
                    format!("{source}\n\n{}", resources.definitions)
                };
                cx.new(|cx| TextViewState::markdown(&source, cx))
            })
            .clone();
        let comments = resources
            .comments
            .iter()
            .filter(|c| {
                c.anchor.as_ref().is_some_and(|a| {
                    let a = a.block();
                    !a.outdated && a.start == block.range.start && a.end == block.range.end
                })
            })
            .collect::<Vec<_>>();
        let active = comments
            .iter()
            .any(|c| Some(&c.id) == resources.active.as_ref());
        let id = comments
            .iter()
            .find(|c| Some(&c.id) == resources.active.as_ref())
            .or_else(|| comments.first())
            .map(|c| c.id.clone());
        let count = comments.len();
        let resolved = !comments.is_empty() && comments.iter().all(|c| c.resolved);
        let handler = resources.handler.clone();
        let disabled = resources.drafting;
        let tint = if active {
            cx.theme().ring
        } else if resolved {
            cx.theme().success
        } else {
            gpui_kit::rgb(0xb8860b).into()
        };
        let mut block = h_flex()
            .id(("commentable-block", index))
            .items_start()
            .w_full()
            .gap_1()
            .rounded_md()
            .border_l_2()
            .border_color(if count > 0 {
                tint
            } else {
                gpui_kit::transparent_black()
            })
            .when(count > 0, |d| {
                d.bg(tint.opacity(if active { 0.12 } else { 0.05 }))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_1()
                    .child(self.text_view(&state, style, cx).selectable(true)),
            );
        // A fixed gutter keeps text wrapping unchanged when comments are saved.
        block = block.child(
            div().w(px(30.)).flex_none().child(
                Button::new(("block-comments", index))
                    .ghost()
                    .small()
                    .compact()
                    .label(if count == 0 {
                        "+".to_owned()
                    } else {
                        count.to_string()
                    })
                    .tooltip(if count == 0 {
                        "Comment on this block"
                    } else {
                        "Open block comments"
                    })
                    .accessibility_label(if count == 0 {
                        "Comment on this block"
                    } else {
                        "Open block comments"
                    })
                    .disabled(disabled && count == 0)
                    .on_click({
                        let handler = handler.clone();
                        let state = state.clone();
                        move |_, window, cx| {
                            if let Some(handler) = &handler {
                                let action = if let Some(id) = &id {
                                    BlockAction::Open(id.clone())
                                } else {
                                    BlockAction::Comment {
                                        block: index,
                                        quote: selected_quote(&state, cx),
                                    }
                                };
                                handler(action, window, cx);
                            }
                        }
                    }),
            ),
        );
        block
            .context_menu(move |menu, _, cx| {
                // Capture before moving focus to the menu/editor; never copy through the clipboard.
                let quote = selected_quote(&state, cx);
                let handler = handler.clone();
                menu.item(
                    PopupMenuItem::new("Comment on this block")
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            if let Some(handler) = &handler {
                                handler(
                                    BlockAction::Comment {
                                        block: index,
                                        quote: quote.clone(),
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }),
                )
            })
            .into_any_element()
    }
}
fn selected_quote(state: &Entity<TextViewState>, cx: &App) -> Option<String> {
    let text = state.read(cx).selected_text();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    #[gpui_kit::test]
    fn right_click_captures_quote_before_editor_focus(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let view = cx.new(|cx| {
            PreviewView::embedded(
                "# Title\n\nWords with **bold** and `code`.\n\nOther paragraph.".into(),
                cx,
            )
        });
        let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events = captured.clone();
        view.update(cx, |view, cx| {
            view.set_comments(
                Vec::new(),
                None,
                false,
                Arc::new(move |action, _, _| {
                    if let BlockAction::Comment { block, quote } = action {
                        events.lock().push((block, quote));
                    }
                }),
                cx,
            )
        });
        let reader = view.clone();
        let (_, cx) = cx
            .add_window_view(move |window, cx| gpui_kit::component::Root::new(reader, window, cx));
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update(|window, cx| window.render_frame(cx));
        }
        view.update(cx, |view, cx| {
            let state = view.resources.as_ref().unwrap().states[1]
                .as_ref()
                .unwrap()
                .clone();
            state.update(cx, |state, cx| state.select_all(cx));
        });
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.right_click(("block-comments", 1usize), cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            assert!(window.find("popup-menu").visible());
            window.press("down", cx);
            window.press("enter", cx);
        });
        cx.run_until_parked();
        let captured = captured.lock();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, 1);
        assert_eq!(
            captured[0].1.as_deref().unwrap().trim(),
            "Words with bold and code."
        );
    }
}
