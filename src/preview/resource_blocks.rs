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
    Comment {
        block: usize,
        quote: Option<String>,
        selection: Option<std::ops::Range<usize>>,
    },
    Open(String),
}
pub(crate) type CommentHandler = Arc<dyn Fn(BlockAction, &mut Window, &mut App) + Send + Sync>;

pub(super) struct ResourceBlocks {
    pub source: SharedString,
    pub blocks: Vec<MarkdownBlock>,
    display_ranges: Arc<Vec<std::ops::Range<usize>>>,
    states: Vec<Option<Entity<TextViewState>>>,
    definitions: String,
    selection_maps: Vec<Option<Arc<SelectionMap>>>,
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
            selection_maps: vec![None; blocks.len()],
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
            let anchor = anchor.location();
            if !anchor.outdated
                && let Some(index) = resources
                    .blocks
                    .iter()
                    .position(|b| b.range.start <= anchor.start && anchor.start < b.range.end)
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
            .code_block_actions(|block, _, _| {
                let code = block.code();
                Button::new("copy")
                    .ghost()
                    .small()
                    .compact()
                    .label("Copy")
                    .tooltip("Copy code block")
                    .accessibility_label("Copy code block")
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                        window.push_notification("Copied code block", cx);
                    })
            })
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
        let selection_map = resources.selection_maps[index]
            .get_or_insert_with(|| {
                let (display, insertions) =
                    metadata_display(&resources.source[block.range.clone()]);
                Arc::new(SelectionMap {
                    block: block.range.clone(),
                    display_len: display.len(),
                    insertions,
                })
            })
            .clone();
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
                    let a = a.location();
                    !a.outdated && a.start < block.range.end && a.end > block.range.start
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
        // Comments tint the block background only; the colored left border
        // read as decorative chrome rather than document state. One wash
        // strength keeps every commented block equally scannable; only the
        // tint color distinguishes open, resolved, and currently-selected.
        let mut block = h_flex()
            .id(("commentable-block", index))
            .items_start()
            .w_full()
            .gap_1()
            .rounded_md()
            .when(count > 0, |d| d.bg(tint.opacity(0.18)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    // The old 2px left border plus 4px padding kept text at a
                    // 6px inset; keep that now that the border is gone.
                    .pl_1p5()
                    .child(self.text_view(&state, style, cx).selectable(true)),
            );
        // A fixed gutter keeps text wrapping unchanged when comments are saved.
        // Empty "+" affordances stay dim so long documents scan cleanly;
        // the ghost hover surface still raises them to full accent on hover.
        // Note: Button reserves `hover()`/`opacity()` for its variant
        // styling, so dim here via normal-state text color only.
        let empty = count == 0;
        let gutter_color = if empty {
            cx.theme().muted_foreground.opacity(0.35)
        } else {
            cx.theme().secondary_foreground
        };
        block = block.child(
            div().w(px(30.)).flex_none().child(
                Button::new(("block-comments", index))
                    .ghost()
                    .small()
                    .compact()
                    .text_color(gutter_color)
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
                        let selection_map = selection_map.clone();
                        move |_, window, cx| {
                            if let Some(handler) = &handler {
                                let action = if let Some(id) = &id {
                                    BlockAction::Open(id.clone())
                                } else {
                                    BlockAction::Comment {
                                        block: index,
                                        quote: selected_quote(&state, cx),
                                        selection: selected_range(&state, &selection_map, cx),
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
                let selection = selected_range(&state, &selection_map, cx);
                let handler = handler.clone();
                menu.item(
                    PopupMenuItem::new(if selection.is_some() {
                        "Comment on selected text"
                    } else {
                        "Comment on this block"
                    })
                    .disabled(disabled)
                    .on_click(move |_, window, cx| {
                        if let Some(handler) = &handler {
                            handler(
                                BlockAction::Comment {
                                    block: index,
                                    quote: quote.clone(),
                                    selection: selection.clone(),
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
/// Map the inner TextView source back to the artifact body. Appended reference
/// definitions are invisible support content, never part of a comment selection.
struct SelectionMap {
    block: std::ops::Range<usize>,
    display_len: usize,
    insertions: Vec<usize>,
}
impl SelectionMap {
    fn original_range(&self, range: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
        let endpoint = |offset: usize| {
            let offset = offset.min(self.display_len);
            let mut removed = 0;
            for &insertion in &self.insertions {
                let start = insertion + removed;
                if offset <= start {
                    break;
                }
                removed += (offset - start).min(2);
                if offset < start + 2 {
                    break;
                }
            }
            self.block.start + offset - removed
        };
        let range = endpoint(range.start)..endpoint(range.end);
        (range.start < range.end && range.end <= self.block.end).then_some(range)
    }
}
fn selected_range(
    state: &Entity<TextViewState>,
    map: &SelectionMap,
    cx: &App,
) -> Option<std::ops::Range<usize>> {
    selected_quote(state, cx)?;
    map.original_range(state.read(cx).selected_source_range()?)
}

fn selected_quote(state: &Entity<TextViewState>, cx: &App) -> Option<String> {
    let text = state.read(cx).selected_text();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    #[test]
    fn selection_offsets_exclude_display_breaks_and_reference_definitions() {
        let source = "**Name:** λ\r\n**Status:** ready\r\n**Owner:** me";
        let (display, insertions) = metadata_display(source);
        let map = SelectionMap {
            block: 12..12 + source.len(),
            display_len: display.len(),
            insertions,
        };
        let start = display.find("ready").unwrap();
        let original = source.find("ready").unwrap() + 12;
        assert_eq!(
            map.original_range(start..start + 5),
            Some(original..original + 5)
        );
        assert_eq!(
            map.original_range(0..display.len() + 100),
            Some(12..12 + source.len())
        );
        assert_eq!(
            map.original_range(display.len() + 2..display.len() + 20),
            None
        );
        let owner = display.rfind("me").unwrap();
        assert_eq!(
            map.original_range(owner..owner + 2),
            Some(12 + source.rfind("me").unwrap()..14 + source.rfind("me").unwrap())
        );
    }

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
                    if let BlockAction::Comment {
                        block,
                        quote,
                        selection,
                    } = action
                    {
                        events.lock().push((block, quote, selection));
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
        assert_eq!(captured[0].2, Some(9..40));
        assert_eq!(
            captured[0].1.as_deref().unwrap().trim(),
            "Words with bold and code."
        );
    }
}
