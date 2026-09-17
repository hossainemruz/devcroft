//! Clickable `http(s)` links inside Home item descriptions.
//!
//! Todo (and To Read) descriptions are plain text, but they often reference
//! tickets, docs, or PRs. [`split_links`] finds scheme-qualified URLs without
//! any new dependency, and [`description_with_links`] renders them with the
//! theme `Link` style; anything that fails [`safe_web_url`] stays plain text.
use super::*;
use gpui_kit::component::link::Link;

/// Card titles keep their text styling while providing keyboard activation
/// and link semantics. Check the destination at activation like the old Open buttons.
pub(super) fn title_link(
    item: &Item,
    id_prefix: &str,
    title: &str,
    cx: &Context<HomeView>,
) -> gpui_kit::base::Link {
    gpui_kit::base::Link::new(item_id(id_prefix, &item.id))
        .href(item.url.clone())
        .accessibility_label(title.to_owned())
        .flex_1()
        .min_w_0()
        .max_w_full()
        .rounded_sm()
        .font_medium()
        .text_color(cx.theme().foreground)
        .cursor_pointer()
        .hover(|style| style.text_color(cx.theme().link).text_decoration_1())
        .focus(|style| {
            style
                .bg(cx.theme().secondary)
                .text_color(cx.theme().link)
                .text_decoration_1()
        })
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .open_with(|url, _, window, cx| {
            if safe_web_url(url) {
                cx.open_url(url);
            } else {
                window.push_notification("Invalid web URL", cx);
            }
        })
}

/// One run of plain text or a single URL. Pure so splitting stays
/// unit-testable without a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DescriptionSegment {
    Text(String),
    Link(String),
}

/// Split `text` on `http://` and `https://` URLs. A URL runs to the next
/// whitespace; trailing `.,;:!?` and unbalanced closers (`)]}'"`) are left
/// in the text run so sentence punctuation never becomes part of the link.
pub(super) fn split_links(text: &str) -> Vec<DescriptionSegment> {
    let mut segments = Vec::new();
    let mut rest = text.to_owned();
    while let Some(start) = rest
        .find("http://")
        .into_iter()
        .chain(rest.find("https://"))
        .min()
    {
        let before = rest[..start].to_owned();
        if !before.is_empty() {
            segments.push(DescriptionSegment::Text(before));
        }
        let candidate = rest[start..].to_owned();
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .unwrap_or(candidate.len());
        let (mut url, after) = (candidate[..end].to_owned(), candidate[end..].to_owned());
        // Strip sentence punctuation, keeping balanced parens (e.g. wiki
        // links) intact: only closers without a matching opener are trimmed.
        // Trimmed characters are kept as text so nothing is ever swallowed.
        let mut opens = 0;
        let mut closes = 0;
        for c in url.chars() {
            if c == '(' {
                opens += 1;
            } else if c == ')' {
                closes += 1;
            }
        }
        let mut stripped = String::new();
        while let Some(last) = url.chars().last() {
            let strip = matches!(
                last,
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | ']' | '}'
            ) || (last == ')' && closes > opens);
            if !strip {
                break;
            }
            if last == ')' {
                closes -= 1;
            }
            stripped.insert(0, last);
            url.truncate(url.len() - last.len_utf8());
        }
        if url.len() > "https://".len() {
            segments.push(DescriptionSegment::Link(url));
        } else {
            segments.push(DescriptionSegment::Text(url));
        }
        rest = format!("{stripped}{after}");
    }
    if !rest.is_empty() {
        segments.push(DescriptionSegment::Text(rest));
    }
    segments
}

/// Render a description with clickable links. `id_prefix` scopes element ids;
/// pass the item id so links stay unique across cards.
pub(super) fn description_with_links(
    item: &Item,
    id_prefix: &str,
    cx: &mut Context<HomeView>,
) -> impl IntoElement {
    let mut row = h_flex()
        .w_full()
        .min_w_0()
        .max_w_full()
        .overflow_hidden()
        .flex_wrap()
        .whitespace_normal()
        .text_sm()
        .text_color(cx.theme().muted_foreground);
    for (index, segment) in split_links(&item.description).into_iter().enumerate() {
        match segment {
            DescriptionSegment::Text(text) => {
                row = row.child(
                    div()
                        .min_w_0()
                        .max_w_full()
                        .overflow_hidden()
                        .whitespace_normal()
                        .child(text),
                );
            }
            DescriptionSegment::Link(url) if safe_web_url(&url) => {
                row = row.child(
                    Link::new(format!("{id_prefix}-link-{}-{index}", item.id))
                        .href(url.clone())
                        .min_w_0()
                        .max_w_full()
                        .child(
                            div()
                                .max_w_full()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(url),
                        ),
                );
            }
            DescriptionSegment::Link(url) => {
                row = row.child(
                    div()
                        .min_w_0()
                        .max_w_full()
                        .overflow_hidden()
                        .whitespace_normal()
                        .child(url),
                );
            }
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::DescriptionSegment::{Link, Text};
    use super::split_links;

    #[test]
    fn plain_text_has_no_links() {
        assert_eq!(
            split_links("just some words"),
            vec![Text("just some words".to_owned())]
        );
        assert!(split_links("").is_empty());
    }

    #[test]
    fn urls_split_around_text() {
        assert_eq!(
            split_links("see https://example.org/a?q=1 for details"),
            vec![
                Text("see ".to_owned()),
                Link("https://example.org/a?q=1".to_owned()),
                Text(" for details".to_owned()),
            ]
        );
    }

    #[test]
    fn trailing_punctuation_stays_outside_the_link() {
        assert_eq!(
            split_links("open https://example.org/a, then (see https://example.org/b)."),
            vec![
                Text("open ".to_owned()),
                Link("https://example.org/a".to_owned()),
                Text(", then (see ".to_owned()),
                Link("https://example.org/b".to_owned()),
                Text(").".to_owned()),
            ]
        );
    }

    #[test]
    fn balanced_parens_stay_inside_the_link() {
        assert_eq!(
            split_links("https://en.wikipedia.org/wiki/Rust_(language) works"),
            vec![
                Link("https://en.wikipedia.org/wiki/Rust_(language)".to_owned()),
                Text(" works".to_owned()),
            ]
        );
    }

    #[test]
    fn bare_domains_are_not_links() {
        assert_eq!(
            split_links("visit example.org soon"),
            vec![Text("visit example.org soon".to_owned())]
        );
    }
}
