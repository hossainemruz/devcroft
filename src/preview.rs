//! File preview (`devcroft preview <path>`).
//!
//! Headless validation (exists, is a file, size-bounded, UTF-8) runs before
//! any GPUI init so bad paths fail fast from scripts and Neovim
//! (`:!devcroft preview %`). [`PreviewView`] then renders the file in a
//! standalone window — no workspace, no terminal panes, no socket — as a
//! stateful scrollable [`TextView`](gpui_kit::component::text::TextView)
//! with a hover-expandable table of contents and scrollspy.
//!
//! Markdown is the first renderer; future kinds (images, …) add sibling
//! readers and either extend [`PreviewView`] or add a parallel view,
//! keeping the single `preview` command stable.

use std::path::Path;

use anyhow::{Context as _, Result};
use gpui_kit::base::Scrollbar;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::text::{
    FrontmatterPlugin, MarkdownExtensions, TextView, TextViewState, TextViewStyle,
};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    InteractiveElement as _, IntoElement, KeyDownEvent, ListOffset, MouseButton, Overflow,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, StyleRefinement,
    Styled as _, Window, div, px, rems,
};

/// Maximum previewed file size: matches the 4 MiB repository file-API limit
/// referenced in `docs/cli-plan.md`'s open questions.
pub(crate) const MAX_MARKDOWN_BYTES: u64 = 4 * 1024 * 1024;

/// Read and validate a preview target. Returns `(title, content)` where the
/// title is the file name (falling back to the full path when it has none).
pub(crate) fn read_markdown_file(path: &Path) -> Result<(String, String)> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("not a file: {}", path.display());
    }
    if metadata.len() > MAX_MARKDOWN_BYTES {
        anyhow::bail!(
            "{} is {} bytes, over the {}-byte preview limit",
            path.display(),
            metadata.len(),
            MAX_MARKDOWN_BYTES
        );
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} as UTF-8", path.display()))?;
    let title = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| path.as_os_str().to_str().unwrap_or("Markdown preview"))
        .to_owned();
    Ok((title, content))
}

/// One table-of-contents entry: a top-level heading and the document block
/// index it renders at. The renderer maps each top-level syntax node to
/// exactly one block in order, and each block to exactly one virtualized
/// list item — so the node's position in the top-level list *is* the scroll
/// item index. Nested headings (blockquotes, list items, code fences) are
/// correctly excluded by only walking the top level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TocEntry {
    /// Heading depth 1–6.
    pub(crate) level: u8,
    /// Plain-text title with inline markup stripped.
    pub(crate) title: String,
    /// Index of the heading's block in the document (== list item index).
    pub(crate) item_ix: usize,
}

/// Collect the table of contents from `source`, parsed with the same parser
/// and options (GFM with frontmatter) the renderer uses so heading positions
/// can never drift from the rendered blocks. Runs once per window on up to
/// 4 MiB — a brief one-time cost, never per-frame.
///
/// Returns the entries plus the number of top-level blocks, which is the
/// virtualized list's item count (`render_root` resets the list to exactly
/// `blocks.len()`). The count is captured here because reading it back
/// inside the scroll handler would re-enter the list's borrow while it is
/// held for scroll processing and panic (`RefCell already mutably
/// borrowed`).
fn extract_toc(source: &str) -> (Vec<TocEntry>, usize) {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.frontmatter = true;
    let Ok(root) = markdown::to_mdast(source, &options) else {
        return (Vec::new(), 0);
    };
    let Some(children) = root.children() else {
        return (Vec::new(), 0);
    };
    let entries = children
        .iter()
        .enumerate()
        .filter_map(|(item_ix, node)| {
            let markdown::mdast::Node::Heading(heading) = node else {
                return None;
            };
            let title = heading_plain_text(node);
            if title.is_empty() {
                return None;
            }
            Some(TocEntry {
                level: heading.depth,
                title,
                item_ix,
            })
        })
        .collect();
    (entries, children.len())
}

/// Plain text of a heading node for TOC display: concatenates text and code
/// leaves, recursing through emphasis/links/etc., collapsing whitespace
/// (setext headings can span lines).
fn heading_plain_text(node: &markdown::mdast::Node) -> String {
    let mut title = String::new();
    append_plain_text(node, &mut title);
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn append_plain_text(node: &markdown::mdast::Node, title: &mut String) {
    use markdown::mdast::Node as Md;
    match node {
        Md::Text(text) => title.push_str(&text.value),
        Md::InlineCode(code) => title.push_str(&code.value),
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    append_plain_text(child, title);
                }
            }
        }
    }
}

/// Scrollspy rule (pure, unit-tested): the active entry is the last heading
/// at or above the first visible item — the section containing the viewport
/// top. Pinned to the last entry when the viewport reaches the document end
/// with a nonzero top: the tail belongs to the final section even after its
/// heading scrolled above the viewport.
fn active_heading(toc: &[TocEntry], top: usize, bottom: usize, total: usize) -> usize {
    if toc.is_empty() {
        return 0;
    }
    if top > 0 && bottom >= total {
        return toc.len() - 1;
    }
    let mut active = 0;
    for (index, entry) in toc.iter().enumerate() {
        if entry.item_ix <= top {
            active = index;
        } else {
            break;
        }
    }
    active
}

/// Standalone preview window content: a centered document with an outline
/// overlay. Focused on mount (see `run_preview`) so keyboard
/// scrolling works without a click first.
pub(crate) struct PreviewView {
    state: Entity<TextViewState>,
    toc: Vec<TocEntry>,
    /// Top-level block count from our own parse (== the list's item count).
    /// Stored — never read back from the list inside the scroll handler
    /// (see `extract_toc`).
    blocks: usize,
    active: usize,
    focus_handle: FocusHandle,
    toc_hovered: bool,
    toc_focus: FocusHandle,
    embedded: bool,
}

/// Scrollspy selection, emitted when the visible section changes.
pub(crate) struct TocActive(pub usize);
impl EventEmitter<TocActive> for PreviewView {}

impl PreviewView {
    pub(crate) fn new(content: SharedString, cx: &mut Context<Self>) -> Self {
        let (toc, blocks) = extract_toc(&content);
        let state = cx.new(|cx| TextViewState::markdown(&content, cx));
        // Scrollspy: the virtualized list reports its visible item range on
        // every scroll; mapping the top back onto the TOC keeps the sidebar
        // highlight in lockstep (nothing inside the renderer uses this
        // handler, so installing ours is safe). The handler must not touch
        // the list's own state — it runs while the list holds its borrow —
        // so the spy works purely from the event range plus data captured
        // at construction.
        let spy = cx.entity().downgrade();
        state.update(cx, |state, _| {
            state
                .list_state()
                .set_scroll_handler(move |event, _window, cx| {
                    spy.update(cx, |view, cx| {
                        view.on_scroll(event.visible_range.start, event.visible_range.end, cx);
                    })
                    .ok();
                });
        });
        Self {
            state,
            toc,
            blocks,
            active: 0,
            focus_handle: cx.focus_handle(),
            toc_hovered: false,
            toc_focus: cx.focus_handle().tab_stop(true),
            embedded: false,
        }
    }

    /// Embedded readers share Tab traversal with their surrounding controls.
    pub(crate) fn embedded(content: SharedString, cx: &mut Context<Self>) -> Self {
        let mut view = Self::new(content, cx);
        view.embedded = true;
        view
    }

    /// Keep keyboard focus and the nearest outline position on live revisions.
    /// Unchanged Markdown never calls this, so ordinary polling preserves exact scroll.
    pub(crate) fn set_content(&mut self, content: SharedString, cx: &mut Context<Self>) {
        let heading = self.toc.get(self.active).map(|entry| entry.title.clone());
        let mut next = Self::new(content, cx);
        next.focus_handle = self.focus_handle.clone();
        next.toc_focus = self.toc_focus.clone();
        next.embedded = self.embedded;
        if let Some(index) =
            heading.and_then(|title| next.toc.iter().position(|entry| entry.title == title))
        {
            next.on_toc_click(index, cx);
        }
        *self = next;
        cx.notify();
    }

    fn on_scroll(&mut self, top: usize, bottom: usize, cx: &mut Context<Self>) {
        let active = active_heading(&self.toc, top, bottom, self.blocks);
        if active != self.active {
            self.active = active;
            // Hosts that render their own outline (Resources) track this to
            // highlight the visible section. Standalone has no subscribers.
            cx.emit(TocActive(active));
            cx.notify();
        }
    }

    /// Current outline entries plus the scrollspy-selected index, for hosts
    /// that lay out their own outline panel beside the document.
    pub(crate) fn toc_snapshot(&self) -> (Vec<TocEntry>, usize) {
        (self.toc.clone(), self.active)
    }

    /// Jump to a TOC entry, pinning its heading to the viewport top so the
    /// spy rule reselects it. (Minimal-reveal would leave the target
    /// mid-viewport under an earlier section's tail, fighting the
    /// highlight.)
    pub(crate) fn on_toc_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.toc.get(index) else {
            return;
        };
        let item_ix = entry.item_ix;
        self.active = index;
        self.state.update(cx, |state, _| {
            state.list_state().scroll_to(ListOffset {
                item_ix,
                offset_in_item: px(0.),
            });
        });
        cx.notify();
    }

    /// The outline overlays the document rather than changing its wrapping.
    /// Focus expands it too; arrow keys navigate and Escape returns to reading.
    fn render_toc(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let expanded = self.toc_hovered || self.toc_focus.is_focused(window);
        v_flex()
            .id("preview-toc")
            .track_focus(&self.toc_focus)
            .flex_none()
            .w(px(if expanded { 280. } else { 40. }))
            .max_w_full()
            .h_auto()
            .max_h((window.viewport_size().height - px(96.)).max(px(40.)))
            .py_3()
            .px_2()
            .rounded_lg()
            .when(expanded, |this| {
                this.bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
            })
            .on_hover(cx.listener(|this, hovered, _, cx| {
                this.toc_hovered = *hovered;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let index = match event.keystroke.key.as_str() {
                    "up" => this.active.saturating_sub(1),
                    "down" => (this.active + 1).min(this.toc.len() - 1),
                    "home" => 0,
                    "end" => this.toc.len() - 1,
                    "escape" => {
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    _ => return,
                };
                this.on_toc_click(index, cx);
                window.prevent_default();
                cx.stop_propagation();
            }))
            .overflow_y_scroll()
            .when(expanded, |this| {
                this.child(
                    div()
                        .px_2()
                        .pb_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("On this page"),
                )
            })
            .children(self.toc.iter().enumerate().map(|(index, entry)| {
                let is_active = index == active;
                // h1 flush; each deeper level indented one step.
                let indent = px(8. + f32::from(entry.level.saturating_sub(1)) * 12.);
                div()
                    .id(("toc-row", index))
                    .flex_none()
                    .w_full()
                    .py(px(if expanded { 6. } else { 4. }))
                    .pl(if expanded { indent } else { px(0.) })
                    .pr(px(if expanded { 8. } else { 0. }))
                    .rounded_md()
                    .cursor_pointer()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .when(is_active && expanded, |this| {
                        this.bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    })
                    .when(!is_active, |this| {
                        this.text_color(cx.theme().muted_foreground)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.on_toc_click(index, cx);
                        }),
                    )
                    .when(expanded, |this| this.child(entry.title.clone()))
                    .when(!expanded, |this| {
                        this.items_end().flex().justify_end().child(
                            div()
                                .flex_none()
                                .w(px((26. - f32::from(entry.level - 1) * 4.).max(8.)))
                                .h(px(2.))
                                .rounded_full()
                                .bg(if is_active {
                                    cx.theme().foreground
                                } else {
                                    cx.theme().muted_foreground.opacity(0.45)
                                }),
                        )
                    })
            }))
    }
}

impl Focusable for PreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reading_width = (window.viewport_size().width - px(80.)).clamp(px(1.), px(960.));
        // Embedded readers sit below the host view's own header, so they skip
        // the standalone window's generous top margin. Bottom keeps 32px to
        // match the scrollbar insets below.
        let reader_top = if self.embedded { px(8.) } else { px(32.) };
        let list_state = self.state.read(cx).list_state().clone();
        let mut table = StyleRefinement::default();
        table.overflow.x = Some(Overflow::Scroll);
        let code_block = div()
            .px_4()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().accent.opacity(0.6))
            .text_size(px(14.))
            .line_height(px(23.))
            .style()
            .clone();
        let table_head = div()
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .bg(cx.theme().accent.opacity(0.5))
            .style()
            .clone();
        let table_cell = div().px_3().py_2().style().clone();
        let style = TextViewStyle::default()
            .paragraph_gap(rems(1.25))
            .heading_font_size(|level, _| {
                px(match level {
                    1 => 34.,
                    2 => 26.,
                    3 => 21.,
                    _ => 18.,
                })
            })
            // HighlightStyle cannot change a span's font, padding, or corner
            // radius. Keep the source untouched and use a quiet, theme-aware
            // highlight until the renderer exposes real inline-code styling.
            .inline_code(HighlightStyle {
                background_color: Some(cx.theme().foreground.opacity(0.075)),
                ..Default::default()
            })
            .code_block(code_block)
            .table(table)
            .table_head(table_head)
            .table_cell(table_cell);
        // Embedded readers (Resources) let the document take the full width:
        // the host lays out its own outline rail beside this view. The
        // standalone window keeps its centered column, clipped gutter with
        // edge scrollbar, and hover outline.
        h_flex()
            .id("preview-reader")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !this.embedded && event.keystroke.key == "tab" && !this.toc.is_empty() {
                    if this.toc_focus.is_focused(window) {
                        this.focus_handle.focus(window, cx);
                    } else {
                        this.toc_focus.focus(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .relative()
            .size_full()
            .justify_center()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .w(reading_width)
                    // flex_1's zero basis overrides the fixed width above,
                    // so embedded documents fill the host-provided width
                    // instead of centering a 960px column.
                    .when(self.embedded, |this| this.flex_1())
                    .min_w_0()
                    .h_full()
                    .pt(reader_top)
                    .pb(px(32.))
                    .text_size(px(17.))
                    .line_height(px(29.))
                    .child(
                        // TextView currently always draws a 16px scrollbar in
                        // scrollable mode. Clip that gutter (including hitboxes)
                        // and bind the window-edge scrollbar to the same list.
                        // Keeping the virtualized list preserves TOC offsets,
                        // selection, and keyboard scrolling.
                        div().size_full().overflow_hidden().child(
                            TextView::new(&self.state)
                                .markdown_extensions(MarkdownExtensions::default().frontmatter())
                                .plugin(FrontmatterPlugin)
                                .style(style)
                                .scrollable(true)
                                .w(reading_width + px(16.))
                                // Fluid embedded columns cannot precompute the
                                // +16 clip, so they use the TextView's own
                                // scrollbar (the pr_16 below keeps text clear
                                // of it) while standalone keeps edge docking.
                                .when(self.embedded, |this| this.w_full())
                                .pr(px(16.)),
                        ),
                    ),
            )
            .when(!self.embedded, |this| {
                this.child(
                    div()
                        .absolute()
                        .right_0()
                        .top(px(32.))
                        .bottom(px(32.))
                        .w(px(16.))
                        .child(
                            Scrollbar::vertical(&list_state)
                                .id("preview-window-scrollbar")
                                .viewport_from_layout(),
                        ),
                )
            })
            .when(!self.toc.is_empty() && !self.embedded, |this| {
                this.child(
                    v_flex()
                        .absolute()
                        .right(px(24.))
                        .top_0()
                        .h_full()
                        .max_w_full()
                        .py(px(48.))
                        .justify_center()
                        .child(self.render_toc(window, cx)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_file_with_file_name_as_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Hello\n\nSome **text**.\n").unwrap();

        let (title, content) = read_markdown_file(&path).unwrap();
        assert_eq!(title, "notes.md");
        assert!(content.contains("# Hello"));
    }

    #[test]
    fn missing_path_errors_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.md");
        let error = read_markdown_file(&missing).unwrap_err();
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "error should name the path, got: {error:#}"
        );
    }

    #[test]
    fn directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_markdown_file(dir.path()).unwrap_err();
        assert!(
            error.to_string().contains("not a file"),
            "expected not-a-file, got: {error:#}"
        );
    }

    #[test]
    fn oversized_file_is_rejected_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.md");
        let content = vec![b'a'; MAX_MARKDOWN_BYTES as usize + 1];
        std::fs::write(&path, content).unwrap();

        let error = read_markdown_file(&path).unwrap_err();
        assert!(
            error.to_string().contains("over the"),
            "expected size-limit error, got: {error:#}"
        );
    }

    #[test]
    fn non_utf8_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.md");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x28]).unwrap();

        let error = read_markdown_file(&path).unwrap_err();
        assert!(
            error.to_string().contains("UTF-8"),
            "expected UTF-8 error, got: {error:#}"
        );
    }

    #[test]
    fn toc_counts_frontmatter_as_one_block_without_metadata_headings() {
        for metadata in ["title: Example", "config:\n  theme: dark"] {
            let source = format!("---\n{metadata}\n---\n\n# Title\n\nBody\n\n## Section\n");
            let (toc, blocks) = extract_toc(&source);
            assert_eq!(blocks, 4);
            assert_eq!(toc.len(), 2);
            assert_eq!(toc[0].title, "Title");
            assert_eq!(toc[0].item_ix, 1);
            assert_eq!(toc[1].item_ix, 3);
        }
    }

    #[test]
    fn toc_collects_top_level_headings_with_item_indices() {
        let (toc, blocks) =
            extract_toc("# Title\n\nSome text.\n\n## Section *one*\n\n### `code` head\n");
        assert_eq!(blocks, 4);
        assert_eq!(toc.len(), 3);
        assert_eq!(
            toc[0],
            TocEntry {
                level: 1,
                title: "Title".to_owned(),
                item_ix: 0,
            }
        );
        assert_eq!(
            toc[1],
            TocEntry {
                level: 2,
                title: "Section one".to_owned(),
                item_ix: 2,
            }
        );
        assert_eq!(
            toc[2],
            TocEntry {
                level: 3,
                title: "code head".to_owned(),
                item_ix: 3,
            }
        );
    }

    #[test]
    fn toc_includes_setext_headings() {
        let (toc, blocks) = extract_toc("Title\n=====\n\nSubtitle\n--------\n");
        assert_eq!(blocks, 2);
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].level, 1);
        assert_eq!(toc[0].item_ix, 0);
        assert_eq!(toc[1].level, 2);
        assert_eq!(toc[1].item_ix, 1);
    }

    #[test]
    fn toc_skips_nested_and_non_heading_content() {
        // Quoted, listed, and fenced `#` lines are nested blocks, and the
        // link definition is its own non-heading node: none are top-level
        // headings, so the item indices must not shift.
        let source =
            "# Top\n\n> # Quoted\n\n- item\n\n```\n# fenced\n```\n\n[ref]: /url\n\n## End\n";
        let (toc, blocks) = extract_toc(source);
        assert_eq!(blocks, 6);
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].title, "Top");
        assert_eq!(toc[0].item_ix, 0);
        assert_eq!(toc[1].title, "End");
        assert_eq!(toc[1].item_ix, 5);
    }

    #[test]
    fn toc_is_empty_without_headings() {
        let (toc, blocks) = extract_toc("Just *text*.\n\n- a\n- b\n");
        assert!(toc.is_empty());
        assert_eq!(blocks, 2);
        let (toc, blocks) = extract_toc("");
        assert!(toc.is_empty());
        assert_eq!(blocks, 0);
    }

    fn sample_toc() -> Vec<TocEntry> {
        [(1, 0), (2, 10), (2, 20)]
            .into_iter()
            .map(|(level, item_ix)| TocEntry {
                level,
                title: String::new(),
                item_ix,
            })
            .collect()
    }

    #[test]
    fn spy_selects_section_containing_viewport_top() {
        let toc = sample_toc();
        assert_eq!(active_heading(&toc, 0, 15, 30), 0);
        assert_eq!(active_heading(&toc, 10, 22, 30), 1);
        assert_eq!(active_heading(&toc, 15, 28, 30), 1);
    }

    #[test]
    fn spy_pins_to_last_entry_at_document_end() {
        let toc = sample_toc();
        assert_eq!(active_heading(&toc, 25, 30, 30), 2);
    }

    #[test]
    fn spy_stays_on_first_entry_when_everything_fits() {
        let toc = sample_toc();
        assert_eq!(active_heading(&toc, 0, 30, 30), 0);
    }

    #[test]
    fn spy_is_zero_without_headings() {
        assert_eq!(active_heading(&[], 5, 10, 20), 0);
    }
}
