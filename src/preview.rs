//! File preview (`devcroft preview <path>`).
//!
//! Headless validation (exists, is a file, size-bounded, UTF-8) runs before
//! any GPUI init so bad paths fail fast from scripts and Neovim
//! (`:!devcroft preview %`). [`PreviewView`] then renders the file in a
//! standalone window — no workspace, no terminal panes, no socket — as a
//! stateful scrollable [`TextView`](gpui_kit::component::text::TextView)
//! with a hover-expandable table of contents and scrollspy.
//! Resources embeds this same reader; only the outer spacing and outline
//! controls differ. Both window entry points host it inside the component Root.
//!
//! Markdown is the first renderer; future kinds (images, …) add sibling
//! readers and either extend [`PreviewView`] or add a parallel view,
//! keeping the single `preview` command stable.

mod resource_blocks;
pub(crate) use resource_blocks::{BlockAction, CommentHandler};

use std::path::Path;

use anyhow::{Context as _, Result};
use gpui_kit::base::text::{RangeHighlight, RenderedText};
use gpui_kit::base::{Scrollbar, TextView, TextViewStyle};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::text::{FrontmatterPlugin, MarkdownExtensions, TextViewState};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent, ListOffset,
    MouseButton, Overflow, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    TestSupportExt as _, Window, div, px, relative, rems,
};

/// Shared column width for document content and resource metadata.
pub(crate) const READING_WIDTH: f32 = 820.;

/// Maximum previewed file size: matches the repository file-API limit.
pub(crate) const MAX_MARKDOWN_BYTES: u64 = 4 * 1024 * 1024;

/// Preserve label/value metadata lines without preventing ordinary prose from
/// reflowing. Work on parsed paragraphs so fences and frontmatter stay intact.
/// This is a display copy; the saved Markdown is never rewritten.
fn metadata_line_breaks(source: &str) -> String {
    metadata_display(source).0
}

/// Also retain insertion positions in original source coordinates for comments.
fn metadata_display(source: &str) -> (String, Vec<usize>) {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.frontmatter = true;
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    let Ok(root) = markdown::to_mdast(source, &options) else {
        return (source.to_owned(), Vec::new());
    };
    let mut insertions = Vec::new();
    for node in root.children().into_iter().flatten() {
        let markdown::mdast::Node::Paragraph(paragraph) = node else {
            continue;
        };
        let Some(position) = &paragraph.position else {
            continue;
        };
        let text = &source[position.start.offset..position.end.offset];
        let lines: Vec<_> = text.lines().collect();
        if lines.len() < 2
            || !lines.iter().all(|line| {
                line.strip_prefix("**")
                    .and_then(|line| line.split_once(":**"))
                    .is_some_and(|(label, _)| !label.is_empty())
            })
        {
            continue;
        }
        for (offset, _) in text.match_indices('\n') {
            let end = position.start.offset + offset;
            let end = if source.as_bytes()[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            if !source[..end].ends_with("  ") && !source[..end].ends_with('\\') {
                insertions.push(end);
            }
        }
    }
    let mut result = source.to_owned();
    for &offset in insertions.iter().rev() {
        result.insert_str(offset, "  ");
    }
    (result, insertions)
}

type CodeHighlights = std::collections::HashMap<
    (SharedString, Option<SharedString>, bool),
    Vec<(std::ops::Range<usize>, HighlightStyle)>,
>;

pub(crate) fn highlight_code(
    code: &str,
    language: Option<&str>,
    dark: bool,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    use std::sync::LazyLock;
    use syntect::{
        easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
    };
    static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
    static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);
    let Some(syntax) = language.and_then(|lang| SYNTAXES.find_syntax_by_token(lang)) else {
        return Vec::new();
    };
    let theme = &THEMES.themes[if dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    }];
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in LinesWithEndings::from(code) {
        if let Ok(parts) = highlighter.highlight_line(line, &SYNTAXES) {
            let mut start = offset;
            for (style, text) in parts {
                let color = style.foreground;
                spans.push((
                    start..start + text.len(),
                    HighlightStyle {
                        color: Some(
                            gpui_kit::rgba(u32::from_be_bytes([
                                color.r, color.g, color.b, color.a,
                            ]))
                            .into(),
                        ),
                        ..Default::default()
                    },
                ));
                start += text.len();
            }
        }
        offset += line.len();
    }
    spans
}

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
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
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

/// Upper bound on matches collected per search: a one-character query in a
/// 4 MiB document would otherwise collect millions of ranges. A capped list
/// is marked with a trailing `+` in the find bar.
const MAX_FIND_MATCHES: usize = 10_000;

/// All non-overlapping matches of `needle` in `haystack`, ASCII-case-
/// insensitive, as byte ranges of `haystack`. Matching walks characters, so
/// every range covers whole characters; non-ASCII comparisons are exact
/// (`char::eq_ignore_ascii_case`). Returns whether the list was capped at
/// [`MAX_FIND_MATCHES`].
fn find_matches(haystack: &str, needle: &str) -> (Vec<std::ops::Range<usize>>, bool) {
    if needle.is_empty() {
        return (Vec::new(), false);
    }
    let needle: Vec<char> = needle.chars().collect();
    let first = needle[0];
    let mut matches = Vec::new();
    let mut pos = 0;
    while pos < haystack.len() {
        let candidate = haystack[pos..]
            .chars()
            .next()
            .expect("pos is a character boundary");
        if candidate.eq_ignore_ascii_case(&first) {
            let mut end = pos;
            let mut matched = true;
            for expected in &needle {
                match haystack[end..].chars().next() {
                    Some(actual) if actual.eq_ignore_ascii_case(expected) => {
                        end += actual.len_utf8();
                    }
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            if matched {
                if matches.len() == MAX_FIND_MATCHES {
                    return (matches, true);
                }
                matches.push(pos..end);
                // Standard find semantics: matches never overlap.
                pos = end;
                continue;
            }
        }
        pos += candidate.len_utf8();
    }
    (matches, false)
}

/// Status text for the find bar: `None` for an empty query, `"No results"`
/// when nothing matched, and `"{position}/{total}{+}"` otherwise.
fn find_status_label(query: &str, total: usize, index: usize, truncated: bool) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    if total == 0 {
        return Some("No results".to_owned());
    }
    Some(format!(
        "{}/{}{}",
        index + 1,
        total,
        if truncated { "+" } else { "" }
    ))
}

/// Live in-document find session: the query input, the matches computed
/// against the current rendered text, and the active match. Created on the
/// first `Cmd/Ctrl+F` and dropped when the bar closes.
struct FindState {
    input: Entity<InputState>,
    /// Owns the subscription to `input`'s events.
    _subscription: Subscription,
    /// The rendered text the matches were computed against. Comparing it with
    /// the current revision tells a landing parse or host content swap apart
    /// from the notifications the highlights themselves produce.
    searched: Option<RenderedText>,
    query: String,
    matches: Vec<std::ops::Range<usize>>,
    /// Index into `matches`; the active match is revealed and painted with
    /// the stronger highlight.
    index: usize,
    /// Whether `matches` was capped at [`MAX_FIND_MATCHES`].
    truncated: bool,
}

/// Standalone preview window content: a centered document with an outline
/// overlay. Focused on mount (see `run_preview`) so keyboard
/// scrolling works without a click first.
pub(crate) struct PreviewView {
    content: SharedString,
    resources: Option<resource_blocks::ResourceBlocks>,
    reference_root: Option<crate::data::DataRoot>,
    reference_generation: u64,
    reference_details: std::sync::Arc<crate::markdown_references::ReferenceDetails>,
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
    editor_preview: bool,
    code_highlights: std::sync::Arc<parking_lot::Mutex<CodeHighlights>>,
    /// Open find session; `None` while the bar is closed.
    find: Option<FindState>,
}

/// Scrollspy selection, emitted when the visible section changes.
pub(crate) struct TocActive(pub usize);
impl EventEmitter<TocActive> for PreviewView {}

#[derive(Clone)]
pub(crate) struct OpenReference(pub crate::markdown_references::Reference);
impl EventEmitter<OpenReference> for PreviewView {}

impl PreviewView {
    fn reference_handler(&self, cx: &Context<Self>) -> crate::markdown_references::OpenHandler {
        let view = cx.entity().downgrade();
        std::sync::Arc::new(move |reference, window, cx| {
            let _ = view.update(cx, |view, cx| {
                if view.embedded {
                    cx.emit(OpenReference(reference));
                } else {
                    crate::markdown_references::open_in_workspace(reference, window, cx);
                }
            });
        })
    }

    pub(crate) fn new(content: SharedString, cx: &mut Context<Self>) -> Self {
        let content = metadata_line_breaks(&content);
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
        // Find results follow the rendered text: a landing parse or a host
        // content swap re-runs the search. Setting highlights notifies this
        // same state, so the observer compares rendered-text revisions to
        // ignore its own notifications.
        cx.observe(&state, |this, state, cx| {
            this.sync_find_with_text(state, cx)
        })
        .detach();
        Self {
            content: content.into(),
            resources: None,
            reference_root: None,
            reference_generation: 0,
            reference_details: Default::default(),
            state,
            toc,
            blocks,
            active: 0,
            focus_handle: cx.focus_handle(),
            toc_hovered: false,
            toc_focus: cx.focus_handle().tab_stop(true),
            embedded: false,
            editor_preview: false,
            code_highlights: Default::default(),
            find: None,
        }
    }

    /// Embedded readers share Tab traversal with their surrounding controls.
    pub(crate) fn embedded(content: SharedString, cx: &mut Context<Self>) -> Self {
        let mut view = Self::new(content.clone(), cx);
        view.resources = Some(resource_blocks::ResourceBlocks::new(content, &view.content));
        view.embedded = true;
        view
    }

    /// Editor files use the reader without Resources' comment affordances.
    pub(crate) fn for_editor(content: SharedString, cx: &mut Context<Self>) -> Self {
        let mut view = Self::new(content, cx);
        view.embedded = true;
        view.editor_preview = true;
        view
    }

    pub(crate) fn set_reference_root(
        &mut self,
        root: Option<crate::data::DataRoot>,
        cx: &mut Context<Self>,
    ) {
        self.reference_root = root;
        self.reference_generation = self.reference_generation.wrapping_add(1);
        let generation = self.reference_generation;
        let content = self.content.clone();
        let root = self.reference_root.clone();
        cx.spawn(async move |this, cx| {
            let details = cx
                .background_spawn(async move {
                    crate::markdown_references::load_details(&content, root.as_ref())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.reference_generation == generation {
                    this.reference_details = std::sync::Arc::new(details);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Keep keyboard focus and the nearest outline position on live revisions.
    /// Unchanged Markdown never calls this, so ordinary polling preserves exact scroll.
    pub(crate) fn set_content(&mut self, content: SharedString, cx: &mut Context<Self>) {
        let heading = self.toc.get(self.active).map(|entry| entry.title.clone());
        let mut next = if self.editor_preview {
            Self::for_editor(content, cx)
        } else if self.embedded {
            Self::embedded(content, cx)
        } else {
            Self::new(content, cx)
        };
        next.focus_handle = self.focus_handle.clone();
        next.toc_focus = self.toc_focus.clone();
        next.embedded = self.embedded;
        next.reference_generation = self.reference_generation;
        next.reference_root = self.reference_root.clone();
        // An open find session survives the swap; its matches are recomputed
        // against the new text below (and again when its parse lands).
        next.find = self.find.take();
        if let Some(index) =
            heading.and_then(|title| next.toc.iter().position(|entry| entry.title == title))
        {
            next.on_toc_click(index, cx);
        }
        *self = next;
        if self.find.is_some() {
            self.refresh_find(cx);
        }
        self.set_reference_root(self.reference_root.clone(), cx);
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
    pub(crate) fn move_navigation(&mut self, down: bool, cx: &mut Context<Self>) {
        self.state
            .read(cx)
            .list_state()
            .scroll_by(px(if down { 40. } else { -40. }));
        cx.notify();
    }

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

    /// Open the find bar, or refocus its query input when it is already open.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            find.input.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in document"));
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.refresh_find(cx),
            InputEvent::PressEnter { shift, .. } => {
                this.step_find(if *shift { -1 } else { 1 }, cx);
            }
            _ => {}
        });
        self.find = Some(FindState {
            input,
            _subscription: subscription,
            searched: None,
            query: String::new(),
            matches: Vec::new(),
            index: 0,
            truncated: false,
        });
        self.refresh_find(cx);
        self.find
            .as_ref()
            .expect("find was just created")
            .input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    /// Dismiss the find bar, clear its highlights, and return focus to the
    /// document so keyboard scrolling resumes.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_none() {
            return;
        }
        self.state
            .update(cx, |state, cx| state.clear_range_highlights(cx));
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Re-search the input's current value against the rendered text, then
    /// repaint the highlights and reveal the first match. Runs on every
    /// keystroke and after a host content swap.
    fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let text = self.state.read(cx).rendered_text();
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let query = find.input.read(cx).value().to_string();
        // Not every `Change` moves text (an enter/IME normalization can
        // re-emit one): only a different query restarts the active match.
        let query_changed = query != find.query;
        find.query = query;
        find.searched = Some(text.clone());
        let (matches, truncated) = find_matches(text.as_str(), &find.query);
        find.matches = matches;
        find.truncated = truncated;
        if query_changed || find.index >= find.matches.len() {
            find.index = 0;
        }
        self.apply_find_highlights(cx);
    }

    /// Re-run the search when the rendered text changed under an open find
    /// bar (a landing parse or a host content swap). The revision comparison
    /// keeps the highlights' own notifications from starting another search.
    fn sync_find_with_text(&mut self, state: Entity<TextViewState>, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let text = state.read(cx).rendered_text();
        if find.searched.as_ref() == Some(&text) {
            return;
        }
        find.searched = Some(text.clone());
        let (matches, truncated) = find_matches(text.as_str(), &find.query);
        find.matches = matches;
        find.truncated = truncated;
        if find.index >= find.matches.len() {
            find.index = 0;
        }
        self.apply_find_highlights(cx);
    }

    /// Move to the next (`delta = 1`) or previous (`delta = -1`) match,
    /// wrapping at either end.
    fn step_find(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as isize;
        find.index = (find.index as isize + delta).rem_euclid(len) as usize;
        self.apply_find_highlights(cx);
    }

    /// Paint every match and reveal the active one. The rendered text is
    /// unchanged, so the state's notify does not re-enter the search.
    fn apply_find_highlights(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let inactive = cx.theme().warning.opacity(0.28);
        let active = cx.theme().warning.opacity(0.6);
        let highlights = find
            .matches
            .iter()
            .enumerate()
            .map(|(index, range)| {
                RangeHighlight::new(
                    range.clone(),
                    if index == find.index {
                        active
                    } else {
                        inactive
                    },
                )
            })
            .collect::<Vec<_>>();
        let reveal = find.matches.get(find.index).cloned();
        self.state.update(cx, |state, cx| {
            if highlights.is_empty() {
                state.clear_range_highlights(cx);
            } else {
                // Ranges come from the current rendered text; a rejected set
                // (e.g. the text moved under us) stays unhighlighted until
                // the next sync.
                let _ = state.set_range_highlights(highlights, cx);
            }
            if let Some(range) = reveal {
                let _ = state.reveal_range(range, cx);
            }
        });
        cx.notify();
    }

    /// Floating find widget over the reading column: query input, match
    /// counter, previous/next and close. Rendered only while find is open.
    fn render_find_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(find) = self.find.as_ref() else {
            return div().into_any_element();
        };
        let status = find_status_label(&find.query, find.matches.len(), find.index, find.truncated);
        h_flex()
            .id("preview-find")
            .absolute()
            .top(px(4.))
            .right_0()
            .max_w_full()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_lg()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_find(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .child(
                Input::new(&find.input)
                    .small()
                    .appearance(false)
                    .w(px(190.)),
            )
            .when_some(status, |this, status| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                )
            })
            .child(
                Button::new("find-prev")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .tooltip("Previous match")
                    .accessibility_label("Previous match")
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(-1, cx))),
            )
            .child(
                Button::new("find-next")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip("Next match")
                    .accessibility_label("Next match")
                    .on_click(cx.listener(|this, _, _, cx| this.step_find(1, cx))),
            )
            .child(
                Button::new("find-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close find")
                    .accessibility_label("Close find")
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            )
            .into_any_element()
    }

    /// The outline overlays the document rather than changing its wrapping.
    /// Focus expands it too; arrow keys navigate and Escape returns to reading.
    fn render_editor_outline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("editor-preview-outline")
            .test_support()
            .debug_selector(|| "editor-preview-outline".into())
            .track_focus(&self.toc_focus)
            .w(px(240.))
            .max_w(relative(0.3))
            .flex_shrink_0()
            .h_full()
            .min_h_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(div().px_3().py_2().text_sm().child("On this page"))
            .child(
                v_flex()
                    .id("editor-preview-outline-entries")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_4()
                    .gap_1()
                    .when(self.toc.is_empty(), |outline| {
                        outline.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("No headings on this page"),
                        )
                    })
                    .children(self.toc.iter().enumerate().map(|(index, entry)| {
                        div()
                            .id(("editor-preview-heading", index))
                            .test_support()
                            .debug_selector(move || format!("editor-preview-heading-{index}"))
                            .flex_none()
                            .w_full()
                            .py(px(6.))
                            .pl(px(8. + f32::from(entry.level.saturating_sub(1)) * 12.))
                            .pr(px(8.))
                            .rounded_md()
                            .cursor_pointer()
                            .text_ellipsis()
                            .text_sm()
                            .when(index == self.active, |row| {
                                row.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .when(index != self.active, |row| {
                                row.text_color(cx.theme().muted_foreground)
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| this.on_toc_click(index, cx)),
                            )
                            .child(entry.title.clone())
                    })),
            )
    }

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
        // Embedded readers sit below the host view's own compact header,
        // so they keep only a small top margin. Bottom keeps 32px to
        // match the scrollbar insets below.
        let reader_top = if self.embedded { px(8.) } else { px(32.) };
        let list_state = self.state.read(cx).list_state().clone();
        let mut table = StyleRefinement::default();
        table.overflow.x = Some(Overflow::Scroll);
        let mut code_block = div()
            .px_4()
            .py_3()
            .rounded_md()
            .bg(cx.theme().accent.opacity(0.6))
            .text_size(px(14.))
            .line_height(px(23.))
            .whitespace_nowrap()
            .style()
            .clone();
        code_block.overflow.x = Some(Overflow::Scroll);
        // Scrollable code blocks are opt-in: with `overflow.y = Scroll`
        // plus a max height, long blocks cap their height, get their own
        // scrollbar, and keep wheel events inside the block until the code
        // reaches its edge (gpui-kit v0.7.1).
        code_block.overflow.y = Some(Overflow::Scroll);
        code_block = code_block.max_h(px(320.));
        let table_head = div()
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .bg(cx.theme().accent.opacity(0.5))
            .style()
            .clone();
        let table_cell = div().px_3().py_2().style().clone();
        let body = cx.theme().foreground.opacity(0.85);
        let style = TextViewStyle::from_theme(&gpui_kit::base::Theme::global(cx))
            .with_foreground(body)
            .with_paragraph_gap(rems(1.))
            // Level-aware refinements layer over the renderer's built-in
            // heading weight and spacing, so only the sizes are overridden.
            .with_heading(|level| {
                StyleRefinement::default().text_size(px(match level {
                    1 => 30.,
                    2 => 23.,
                    3 => 20.,
                    4 => 18.,
                    _ => 16.,
                }))
            })
            // HighlightStyle cannot change a span's font, padding, or corner
            // radius. Keep the source untouched and use a quiet, theme-aware
            // highlight until the renderer exposes real inline-code styling.
            .with_inline_code(HighlightStyle {
                background_color: Some(cx.theme().foreground.opacity(0.10)),
                ..Default::default()
            })
            .with_code_block(code_block)
            .with_table(table)
            .with_table_head(table_head)
            .with_table_cell(table_cell);
        // Both hosts use a bounded reading column. Embedded width must follow
        // the pane rather than the window (Resources can have side rails).
        let find_bar = self.find.is_some().then(|| self.render_find_bar(cx));
        let reader = h_flex()
            .id("preview-reader")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                let find_focused = this
                    .find
                    .as_ref()
                    .is_some_and(|find| find.input.read(cx).focus_handle(cx).is_focused(window));
                if event.keystroke.key == "f"
                    && !modifiers.shift
                    && !modifiers.alt
                    && crate::command_palette::is_primary_modifier(
                        modifiers.platform,
                        modifiers.control,
                    )
                    && !find_focused
                {
                    this.open_find(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape" && this.find.is_some() {
                    this.close_find(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if !this.embedded
                    && this.find.is_none()
                    && event.keystroke.key == "tab"
                    && !this.toc.is_empty()
                {
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
            .when(!self.embedded, |this| this.px(px(40.)))
            .bg(cx.theme().background)
            .text_color(body)
            .font_family(crate::fonts::MARKDOWN_FONT_FAMILY)
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(READING_WIDTH))
                    .min_w_0()
                    .h_full()
                    .pt(reader_top)
                    .pb(px(32.))
                    .text_size(px(16.))
                    // Keep 26px body lines, while allowing larger headings to
                    // inherit proportionate spacing when they wrap.
                    .line_height(relative(26. / 16.))
                    .child(
                        // TextView always draws a 16px scrollbar in scrollable
                        // mode. Clip that gutter (including hitboxes) and bind
                        // an edge-docked scrollbar to the same list, so the
                        // thumb sits at the pane edge instead of mid-content.
                        // Keeping the virtualized list preserves TOC offsets,
                        // selection, and keyboard scrolling.
                        div().relative().size_full().overflow_hidden().child(
                            self.text_view(&self.state, style.clone(), cx)
                                .when(self.resources.is_some(), |view| {
                                    view.plugin(self.resource_plugin(style, cx))
                                })
                                .scrollable(true)
                                // Both hosts stretch 16px past the clip box to
                                // hide the built-in scrollbar. w_auto is
                                // required: the scrollable TextView defaults to
                                // width 100%, which would otherwise win over
                                // the inset and leave its scrollbar visible.
                                .absolute()
                                .left_0()
                                .top_0()
                                .bottom_0()
                                .right(px(-16.))
                                .w_auto()
                                .pr(px(16.)),
                        ),
                    )
                    .when_some(find_bar, |this, find_bar| this.child(find_bar)),
            )
            // Both hosts dock the thumb to the pane edge. Match the column's
            // insets so its viewport height and the scrollbar stay in sync.
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top(reader_top)
                    .bottom(px(32.))
                    .w(px(16.))
                    .child(
                        Scrollbar::vertical(&list_state)
                            .id("preview-scrollbar")
                            .viewport_from_layout(),
                    ),
            )
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
            });
        if self.editor_preview {
            h_flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .items_stretch()
                .child(div().flex_1().min_w_0().h_full().child(reader))
                .child(self.render_editor_outline(cx))
                .into_any_element()
        } else {
            reader.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn editor_preview_stays_comment_free_after_content_updates(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let preview = cx.new(|cx| PreviewView::for_editor("# First\n\nText\n".into(), cx));
        preview.update(cx, |view, cx| {
            assert!(view.resources.is_none());
            assert!(view.editor_preview);
            view.set_content("# Updated\n\n## Section\n".into(), cx);
            assert!(view.resources.is_none());
            assert!(view.editor_preview);
            assert_eq!(view.toc_snapshot().0.len(), 2);
            assert_eq!(view.toc_snapshot().0[0].title, "Updated");
        });
        let resources = cx.new(|cx| PreviewView::embedded("# Resource\n".into(), cx));
        resources.update(cx, |view, cx| {
            assert!(view.resources.is_some());
            view.set_content("# Updated resource\n".into(), cx);
            assert!(view.resources.is_some());
            assert!(!view.editor_preview);
        });
    }

    #[test]
    fn both_hosts_reserve_space_for_wrapped_headings() {
        use gpui_kit::component::Root;
        use gpui_kit::{Bounds, Pixels, TestApp, WindowBounds, WindowOptions, point, size};

        const CONTENT: &str = "# Release checklist: verify document rendering and comment layout before shipping\n\nStatus: revised design proposal.\n";

        let mut app = TestApp::new();
        app.update(gpui_kit::init);
        for column_width in [320., 820.] {
            let mut layouts = Vec::new();
            for embedded in [false, true] {
                let mut reader = None;
                // Give both hosts the same content width; standalone adds
                // 40px side margins while Resources supplies its own spacing.
                let width = column_width + if embedded { 0. } else { 80. };
                let mut window = app.open_window_with_options(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.), px(0.)),
                            size(px(width), px(700.)),
                        ))),
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| {
                            if embedded {
                                PreviewView::embedded(CONTENT.into(), cx)
                            } else {
                                PreviewView::new(CONTENT.into(), cx)
                            }
                        });
                        reader = Some(view.clone());
                        Root::new(view, window, cx)
                    },
                );
                window.draw();
                app.run_until_parked();
                window.draw();
                let (heading, paragraph) = window.read(|_, cx| {
                    let view = reader.as_ref().unwrap().read(cx);
                    let list = view.state.read(cx).list_state();
                    (
                        list.bounds_for_item(0).expect("heading must render"),
                        list.bounds_for_item(1).expect("paragraph must render"),
                    )
                });
                assert!(
                    heading.size.height > px(60.),
                    "wrapped 30px heading needs more than two 26px body lines: {heading:?}"
                );
                assert!(paragraph.top() >= heading.bottom());
                layouts.push((heading.size, paragraph.size));
            }
            let close = |a: Pixels, b: Pixels| (a - b).abs() < px(0.1);
            assert!(close(layouts[0].0.width, layouts[1].0.width));
            // Resources reserves a comment gutter inside the same column.
            // Its narrower text may wrap onto more lines, but never reserve
            // less height or overlap the following block.
            assert!(layouts[1].0.height >= layouts[0].0.height);
            assert!(layouts[1].1.height >= layouts[0].1.height);
        }
    }

    #[test]
    fn metadata_breaks_preserve_prose_fences_and_outline() {
        let source = "# Title\n\n**Status:** proposed\n**Repository:** example\n**Date:** today\n\nHard wrapped\nprose with **bold** text.\n\n```md\n**A:** one\n**B:** two\n```\n\n## Next\n";
        let rendered = metadata_line_breaks(source);
        assert!(
            rendered.contains("**Status:** proposed  \n**Repository:** example  \n**Date:** today")
        );
        assert!(rendered.contains("Hard wrapped\nprose with **bold** text."));
        assert!(rendered.contains("```md\n**A:** one\n**B:** two\n```"));
        assert_eq!(extract_toc(source), extract_toc(&rendered));
        assert_eq!(metadata_line_breaks(&rendered), rendered);
    }

    #[test]
    fn metadata_breaks_handle_crlf_and_existing_hard_breaks() {
        let source = "**Status:** proposed\r\n**Date:** today";
        assert_eq!(
            metadata_line_breaks(source),
            "**Status:** proposed  \r\n**Date:** today"
        );
        let source = "**Status:** proposed\\\n**Date:** today";
        assert_eq!(metadata_line_breaks(source), source);
    }

    #[test]
    fn fenced_code_has_language_and_theme_aware_colors() {
        for (language, code) in [
            ("go", "// café\ntype Example struct { Name string }\n"),
            ("sql", "SELECT id FROM snapshots WHERE id = 1;\n"),
            ("python", "def foo():\n    return 42\n"),
            ("py", "def foo():\n    return 42\n"),
            ("rust", "fn main() {}\n"),
            ("rs", "fn main() {}\n"),
        ] {
            let dark = highlight_code(code, Some(language), true);
            let light = highlight_code(code, Some(language), false);
            assert!(!dark.is_empty(), "{language} should have syntax styles");
            assert!(
                dark.windows(2)
                    .any(|spans| spans[0].1.color != spans[1].1.color)
            );
            assert_ne!(dark, light);
            assert_eq!(dark.first().unwrap().0.start, 0);
            assert_eq!(dark.last().unwrap().0.end, code.len());
            for (range, _) in dark {
                assert!(code.get(range).is_some());
            }
        }
        assert!(highlight_code("plain", None, true).is_empty());
        assert!(highlight_code("plain", Some("unknown-language"), true).is_empty());
    }

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

    #[test]
    fn find_matches_are_ascii_case_insensitive_and_non_overlapping() {
        let haystack = "Alpha beta ALPHA gamma alpha.";
        let (matches, truncated) = find_matches(haystack, "alpha");
        assert_eq!(matches.len(), 3);
        assert!(!truncated);
        for range in &matches {
            assert!(
                haystack.get(range.clone()).is_some(),
                "whole-character range"
            );
            assert_eq!(&haystack[range.clone()].to_ascii_lowercase(), "alpha");
        }

        // Empty or missing needles never match; matches never overlap.
        assert!(find_matches(haystack, "").0.is_empty());
        assert!(find_matches(haystack, "missing").0.is_empty());
        assert_eq!(find_matches("aaaa", "aa").0, vec![0..2, 2..4]);

        // Case folding is ASCII-only: accented letters compare exactly.
        let haystack = "Café CAFÉ café";
        let (matches, _) = find_matches(haystack, "café");
        assert_eq!(matches.len(), 2);
        assert_eq!(&haystack[matches[0].clone()], "Café");
    }

    #[test]
    fn find_matches_cap_the_result_count() {
        let haystack = "a".repeat(MAX_FIND_MATCHES + 5);
        let (matches, truncated) = find_matches(&haystack, "a");
        assert_eq!(matches.len(), MAX_FIND_MATCHES);
        assert!(truncated);
    }

    #[test]
    fn find_status_labels_report_progress_and_truncation() {
        assert_eq!(find_status_label("", 0, 0, false), None);
        assert_eq!(
            find_status_label("x", 0, 0, false).as_deref(),
            Some("No results")
        );
        assert_eq!(find_status_label("x", 3, 0, false).as_deref(), Some("1/3"));
        assert_eq!(find_status_label("x", 3, 2, false).as_deref(), Some("3/3"));
        assert_eq!(
            find_status_label("x", MAX_FIND_MATCHES, 0, true).as_deref(),
            Some("1/10000+")
        );
    }

    /// End-to-end find session in a real window: open with the OS-primary
    /// chord, type a query, step through the matches, and close with Escape.
    #[test]
    fn find_in_document_searches_navigates_and_closes() {
        use gpui_kit::component::Root;
        use gpui_kit::{Bounds, TestApp, WindowBounds, WindowOptions, point, size};

        const CONTENT: &str =
            "# Alpha\n\nfirst alpha paragraph\n\n## Beta\n\nsecond ALPHA paragraph\n";

        let mut app = TestApp::new();
        app.update(gpui_kit::init);
        let mut reader = None;
        let mut window = app.open_window_with_options(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(900.), px(700.)),
                ))),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| PreviewView::new(CONTENT.into(), cx));
                reader = Some(view.clone());
                Root::new(view, window, cx)
            },
        );
        window.draw();
        app.run_until_parked();
        window.draw();

        let reader = reader.unwrap();
        window.update(|_, window, cx| {
            reader.read(cx).focus_handle(cx).focus(window, cx);
        });

        const OPEN: &str = if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        };
        window.simulate_keystroke(OPEN);
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().map(|f| f.query.clone())),
            Some(String::new()),
            "the shortcut opens the find bar with an empty query"
        );

        window.simulate_input("alpha");
        window.draw();
        let (total, index, truncated) = window.read(|_, cx| {
            let find = reader.read(cx).find.as_ref().unwrap();
            (find.matches.len(), find.index, find.truncated)
        });
        assert_eq!(
            (total, index),
            (3, 0),
            "all three alpha matches, first active"
        );
        assert!(!truncated);

        let input_focused = window.update(|_, window, cx| {
            window.focused(cx)
                == Some(
                    reader
                        .read(cx)
                        .find
                        .as_ref()
                        .unwrap()
                        .input
                        .read(cx)
                        .focus_handle(cx),
                )
        });
        assert!(input_focused, "the find input owns focus after typing");

        window.simulate_keystroke("enter");
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().index),
            1,
            "Enter steps to the next match"
        );
        window.simulate_keystroke("shift-enter");
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().index),
            0,
            "Shift+Enter steps back"
        );
        window.simulate_keystroke("shift-enter");
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().index),
            2,
            "stepping back from the first match wraps to the last"
        );

        window.simulate_keystroke("escape");
        window.draw();
        assert!(
            window.read(|_, cx| reader.read(cx).find.is_none()),
            "Escape closes the find bar"
        );
        let focused = window.update(|_, window, cx| window.focused(cx));
        assert_eq!(
            focused,
            Some(window.read(|_, cx| reader.read(cx).focus_handle(cx))),
            "closing returns focus to the document"
        );
    }

    /// Stepping to a match below the fold scrolls the document to it
    /// (`reveal_range`), not just highlights it.
    #[test]
    fn find_reveals_a_match_below_the_viewport() {
        use gpui_kit::component::Root;
        use gpui_kit::{Bounds, TestApp, WindowBounds, WindowOptions, point, size};

        // Enough filler between the hits that the second one starts well
        // below a 700px viewport.
        let mut content = String::from("# Top\n\nneedle one\n\n");
        for index in 0..40 {
            content.push_str(&format!("filler paragraph {index}\n\n"));
        }
        content.push_str("needle two\n");

        let mut app = TestApp::new();
        app.update(gpui_kit::init);
        let mut reader = None;
        let mut window = app.open_window_with_options(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(900.), px(700.)),
                ))),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| PreviewView::new(content.clone().into(), cx));
                reader = Some(view.clone());
                Root::new(view, window, cx)
            },
        );
        window.draw();
        app.run_until_parked();
        window.draw();

        let reader = reader.unwrap();
        window.update(|_, window, cx| {
            reader.read(cx).focus_handle(cx).focus(window, cx);
        });
        window.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        });
        window.draw();
        window.simulate_input("needle");
        window.draw();
        let scroll_top = |window: &mut gpui_kit::TestAppWindow<Root>| {
            window.read(|_, cx| {
                reader
                    .read(cx)
                    .state
                    .read(cx)
                    .list_state()
                    .logical_scroll_top()
                    .item_ix
            })
        };
        let before = scroll_top(&mut window);
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().matches.len()),
            2
        );

        window.simulate_keystroke("enter");
        window.draw();
        app.run_until_parked();
        window.draw();
        let after = scroll_top(&mut window);
        assert!(
            after > before,
            "stepping to the off-screen match scrolls to it: {before} -> {after}"
        );
    }

    /// An open find session re-searches when the host replaces the content
    /// (`set_content`), like the Resources reader does on live revisions.
    #[test]
    fn find_follows_a_content_replacement() {
        use gpui_kit::component::Root;
        use gpui_kit::{Bounds, TestApp, WindowBounds, WindowOptions, point, size};

        let mut app = TestApp::new();
        app.update(gpui_kit::init);
        let mut reader = None;
        let mut window = app.open_window_with_options(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(900.), px(700.)),
                ))),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| PreviewView::new("Hello there.\n".into(), cx));
                reader = Some(view.clone());
                Root::new(view, window, cx)
            },
        );
        window.draw();
        app.run_until_parked();
        window.draw();

        let reader = reader.unwrap();
        window.update(|_, window, cx| {
            reader.read(cx).focus_handle(cx).focus(window, cx);
        });
        window.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        });
        window.draw();
        window.simulate_input("beta");
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().matches.len()),
            0,
            "the initial content has no beta"
        );

        window.update(|_, _, cx| {
            reader.update(cx, |view, cx| {
                view.set_content("# Beta\n\nbeta again\n".into(), cx);
            });
        });
        app.run_until_parked();
        window.draw();
        assert_eq!(
            window.read(|_, cx| reader.read(cx).find.as_ref().unwrap().matches.len()),
            2,
            "the open session re-searches the replaced content"
        );
    }
}
