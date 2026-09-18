//! Portable Markdown identities and native inline rendering. Disk/provider work
//! belongs in background resolution; render only reads prepared metadata.
use std::{collections::HashMap, str::FromStr, sync::Arc};

use anyhow::{Context as _, Result, bail, ensure};
use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    hover_card::HoverCard,
    text::{
        InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
        markdown_ast,
    },
    v_flex,
};
use gpui_kit::{
    App, ClickEvent, MouseButton, ParentElement as _, SharedString, Styled as _, StyledText,
    Window, div, px,
};

use crate::{
    agent_sessions::{Catalog, SessionKey, SessionSummary},
    data::{self, DataRoot, artifacts::ArtifactStore},
};

const MAX_REFERENCES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Reference {
    Repository(String),
    Artifact(String),
    Session {
        repository: String,
        provider: String,
        id: String,
    },
}

impl FromStr for Reference {
    type Err = String;

    fn from_str(url: &str) -> std::result::Result<Self, Self::Err> {
        parse_reference(url).map_err(|error| error.to_string())
    }
}

fn parse_reference(url: &str) -> Result<Reference> {
    ensure!(url.len() <= 2048, "Reference is too long");
    let path = url
        .strip_prefix("devcroft:")
        .context("Expected a devcroft: reference")?;
    ensure!(
        !path.contains(['?', '#']),
        "Reference queries and fragments are not supported"
    );
    let parts = path
        .split('/')
        .map(decode_segment)
        .collect::<Result<Vec<_>>>()?;
    match parts.as_slice() {
        [kind, key] if kind == "repository" => {
            Ok(Reference::Repository(data::require_repository_key(key)?))
        }
        [kind, id] if kind == "artifact" => {
            data::artifacts::validate_id(id)?;
            Ok(Reference::Artifact(id.clone()))
        }
        [kind, repository, provider, id] if kind == "session" => {
            data::require_repository_key(repository)?;
            ensure!(
                matches!(provider.as_str(), "opencode" | "codex" | "claude"),
                "Unsupported session provider"
            );
            ensure!(
                id.len() <= 512 && !id.starts_with('-'),
                "Invalid session ID"
            );
            Ok(Reference::Session {
                repository: repository.clone(),
                provider: provider.clone(),
                id: id.clone(),
            })
        }
        _ => bail!(
            "Use devcroft:repository/<key>, devcroft:artifact/<id>, or devcroft:session/<repository>/<provider>/<id>"
        ),
    }
}

fn decode_segment(raw: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut input = raw.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        bytes.push(if byte == b'%' {
            let high = input.next().and_then(|b| (b as char).to_digit(16));
            let low = input.next().and_then(|b| (b as char).to_digit(16));
            ((high.context("Invalid percent encoding")? << 4)
                | low.context("Invalid percent encoding")?) as u8
        } else {
            byte
        });
    }
    let value = String::from_utf8(bytes).context("Reference must be UTF-8")?;
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains(['/', '\\'])
            && !value.chars().any(char::is_control),
        "Invalid reference segment"
    );
    Ok(value)
}

fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

impl Reference {
    pub(crate) fn url(&self) -> String {
        match self {
            Self::Repository(key) => format!("devcroft:repository/{key}"),
            Self::Artifact(id) => format!("devcroft:artifact/{id}"),
            Self::Session {
                repository,
                provider,
                id,
            } => format!(
                "devcroft:session/{repository}/{provider}/{}",
                encode_segment(id)
            ),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Repository(_) => "Repository",
            Self::Artifact(_) => "Resource",
            Self::Session { .. } => "Agent session",
        }
    }

    pub(crate) fn resolve_session(
        &self,
        root: &DataRoot,
        sessions: &[SessionSummary],
    ) -> Result<SessionKey> {
        let Self::Session {
            repository,
            provider,
            id,
        } = self
        else {
            bail!("Expected a session reference")
        };
        let checkout = data::checkout_for(root, repository)
            .context("No checkout linked for this session's repository on this device")?;
        session_key_for(sessions, &checkout, provider, id)
    }
}

fn session_key_for(
    sessions: &[SessionSummary],
    checkout: &std::path::Path,
    provider: &str,
    id: &str,
) -> Result<SessionKey> {
    let checkout = checkout
        .canonicalize()
        .unwrap_or_else(|_| checkout.to_owned());
    let mut matches = sessions.iter().filter(|session| {
        session.key.provider == provider
            && session.key.id == id
            && session
                .checkout
                .canonicalize()
                .unwrap_or_else(|_| session.checkout.clone())
                == checkout
    });
    let session = matches
        .next()
        .context("Session is unavailable on this device")?;
    ensure!(
        matches.next().is_none(),
        "More than one local store contains this session; open it from the session picker"
    );
    Ok(session.key.clone())
}

#[derive(Clone, Debug)]
pub(crate) struct Details {
    title: String,
    description: String,
}
pub(crate) type ReferenceDetails = HashMap<Reference, Details>;

/// Bounded, read-only metadata resolution. Call on a worker, never from render.
pub(crate) fn load_details(content: &str, root: Option<&DataRoot>) -> ReferenceDetails {
    if !content.contains("devcroft:") {
        return HashMap::new();
    }
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.frontmatter = true;
    let Ok(document) = markdown::to_mdast(content, &options) else {
        return HashMap::new();
    };
    let mut references = Vec::new();
    collect_references(&document, &mut references);
    let sessions = if references
        .iter()
        .any(|r| matches!(r, Reference::Session { .. }))
    {
        let catalog = Catalog::new(root);
        catalog.load_cache();
        catalog.snapshot().sessions
    } else {
        Vec::new()
    };
    references
        .into_iter()
        .map(|reference| {
            let details = root
                .context("Devcroft data is unavailable")
                .and_then(|root| resolve_details(&reference, root, &sessions))
                .unwrap_or_else(|error| Details {
                    title: reference.kind().into(),
                    description: format!("{error:#}"),
                });
            (reference, details)
        })
        .collect()
}

fn collect_references(node: &markdown_ast::Node, references: &mut Vec<Reference>) {
    if references.len() >= MAX_REFERENCES {
        return;
    }
    if let markdown_ast::Node::Link(link) = node
        && let Ok(reference) = link.url.parse::<Reference>()
        && !references.contains(&reference)
    {
        references.push(reference);
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_references(child, references);
        }
    }
}

fn resolve_details(
    reference: &Reference,
    root: &DataRoot,
    sessions: &[SessionSummary],
) -> Result<Details> {
    Ok(match reference {
        Reference::Repository(key) => {
            let metadata = data::get_repository_metadata(root, key)?;
            let availability = match data::checkout_for(root, key) {
                Some(path) if path.is_dir() => "Open this repository",
                Some(_) => "Linked checkout is unavailable on this device",
                None => "No checkout linked on this device",
            };
            Details {
                title: metadata.display_name.unwrap_or_else(|| key.clone()),
                description: format!(
                    "{}\n{availability}",
                    metadata.description.unwrap_or_default()
                ),
            }
        }
        Reference::Artifact(id) => {
            let artifact = ArtifactStore::new(root).get(id)?.artifact;
            Details {
                title: artifact.title,
                description: format!(
                    "{} · {}{}",
                    artifact.kind.label(),
                    artifact.repository.unwrap_or_else(|| "Unassociated".into()),
                    if artifact.archived {
                        " · Archived"
                    } else {
                        ""
                    }
                ),
            }
        }
        Reference::Session { provider, .. } => {
            let key = reference.resolve_session(root, sessions)?;
            let session = sessions
                .iter()
                .find(|s| s.key == key)
                .context("Session is not in the local catalog")?;
            Details {
                title: session.title.clone(),
                description: format!(
                    "{provider} · {}\nSaved local session metadata; availability is checked when opened.",
                    session.absolute_time()
                ),
            }
        }
    })
}

pub(crate) type OpenHandler = Arc<dyn Fn(Reference, &mut Window, &mut App) + Send + Sync>;

pub(crate) fn open_in_workspace(reference: Reference, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use std::process::{Command, Stdio};
    let result = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .arg("app")
            .arg("--open-reference")
            .arg(reference.url())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    });
    match result {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => window.push_notification(format!("Could not open Devcroft: {error}"), cx),
    }
}

#[derive(Clone)]
pub(crate) struct ReferencePlugin {
    pub(crate) details: Arc<ReferenceDetails>,
    pub(crate) open: OpenHandler,
}

#[derive(Clone)]
struct InlineReference {
    target: Reference,
    label: String,
}

impl MarkdownPlugin for ReferencePlugin {
    fn name(&self) -> &str {
        "devcroft-reference"
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        context: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Link(link) = node else {
            return None;
        };
        let target = link.url.parse::<Reference>().ok()?;
        let label = plain_text(node);
        let label = if label.is_empty() {
            target.kind().to_owned()
        } else {
            label
        };
        Some(
            MarkdownNode::new(
                "devcroft-reference",
                InlineReference {
                    target: target.clone(),
                    label: label.clone(),
                },
            )
            .text(label.clone())
            .markdown(context.node_source(node).unwrap_or(&link.url).to_owned())
            .accessibility_label(format!("{}: {label}", target.kind())),
        )
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let reference = node.data::<InlineReference>()?;
        let details = self.details.get(&reference.target).cloned().unwrap_or_else(|| Details {
            title: reference.target.kind().into(), description: "Reference details are loading or unavailable. Opening checks the destination again.".into(),
        });
        let mut style = context.text_style().clone();
        style.color = cx.theme().link;
        let text = StyledText::new(reference.label.clone())
            .with_runs(vec![style.to_run(reference.label.len())]);
        let open = self.open.clone();
        let target = reference.target.clone();
        Some(InlineElement::new(
            HoverCard::new("reference-details")
                .trigger(
                    Button::new("reference-open")
                        .link()
                        .h_auto()
                        .p_0()
                        .min_w_0()
                        .text_size(context.font_size())
                        .line_height(context.line_height())
                        .accessibility_label(format!("Open {}: {}", target.kind(), reference.label))
                        .child(text)
                        .on_click(move |_, window, cx| open(target.clone(), window, cx)),
                )
                .content(move |_, _, cx| {
                    v_flex()
                        .max_w(px(340.))
                        .gap_2()
                        .child(div().font_semibold().child(details.title.clone()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(details.description.clone()),
                        )
                }),
        ))
    }
}

fn plain_text(node: &markdown_ast::Node) -> String {
    match node {
        markdown_ast::Node::Text(text) => text.value.clone(),
        markdown_ast::Node::InlineCode(code) => code.value.clone(),
        _ => node
            .children()
            .map(|children| children.iter().map(plain_text).collect())
            .unwrap_or_default(),
    }
}

/// Mirror TextView's normal link activation, intercepting reserved URLs even
/// when malformed so they never escape to an OS protocol handler.
pub(crate) fn open_link(
    url: &SharedString,
    event: &ClickEvent,
    window: &mut Window,
    cx: &mut App,
    open: &OpenHandler,
) {
    let activate = match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    };
    if !activate {
        return;
    }
    if url.to_ascii_lowercase().starts_with("devcroft:") {
        match url.parse() {
            Ok(reference) => open(reference, window, cx),
            Err(error) => {
                use gpui_kit::component::WindowExt as _;
                window.push_notification(format!("Could not open reference: {error}"), cx);
            }
        }
    } else {
        cx.open_url(url);
    }
}

#[cfg(test)]
mod tests;
