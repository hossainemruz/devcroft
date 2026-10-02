use super::bridge::{Gate, Message, script_json};
use crate::review::authoring;
use crate::review::{
    git::ReviewScope,
    model::ReviewDiff,
    session::{Capture, Finding, Location, Review, ReviewLock, SourceRange, Store, digest, pr},
};
use crate::{
    agent::AgentKind, agent_activity::AgentActivityStore, pane::TerminalPane,
    workspace::WorkspaceTab,
};
use anyhow::{Context as _, Result, ensure};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) fn open(
    cwd: PathBuf,
    diff: ReviewDiff,
    label: String,
    scope: ReviewScope,
    cx: &mut App,
) -> Result<()> {
    open_with_bundle(cwd, diff, label, scope, None, cx)
}
pub(crate) fn open_with_bundle(
    cwd: PathBuf,
    _diff: ReviewDiff,
    label: String,
    scope: ReviewScope,
    bundle: Option<PathBuf>,
    cx: &mut App,
) -> Result<()> {
    // The dedicated window keeps native application palettes out of the
    // child view's rectangle. Its recovery controls are native and remain
    // usable even when authored JavaScript stops responding.
    let store = Store::open(&cwd, &label)?;
    let capture = Capture::stable_local(
        &cwd,
        &scope,
        &format!("Local working-tree snapshot · {label}"),
    )?;
    open_capture(cwd, scope, label, store, capture, bundle, cx)
}
pub(crate) fn open_pr(capture: Capture, bundle: Option<PathBuf>, cx: &mut App) -> Result<()> {
    let identity = pr::Identity::parse(&capture.pr.as_ref().context("Missing PR identity")?.url)?;
    open_capture(
        identity.directory()?,
        ReviewScope::UncommittedChanges,
        identity.url.clone(),
        identity.store()?,
        capture,
        bundle,
        cx,
    )
}
fn open_capture(
    cwd: PathBuf,
    scope: ReviewScope,
    label: String,
    store: Store,
    capture: Capture,
    bundle: Option<PathBuf>,
    cx: &mut App,
) -> Result<()> {
    let loaded = store.load()?;
    let mut review = loaded
        .clone()
        .unwrap_or_else(|| Review::start(capture.clone()));
    let before = serde_json::to_vec(&review)?;
    review.add_capture(capture);
    if loaded.is_none() || before != serde_json::to_vec(&review)? {
        store.save(&mut review)?;
    }
    if let Some(directory) = bundle {
        review.install(crate::review::session::Bundle::load(
            &directory,
            &review.active().capture,
        )?)?;
        store.save(&mut review)?;
    }
    let bounds = cx
        .active_window()
        .and_then(|handle| {
            handle
                .update(cx, |_, window, _| window.window_bounds())
                .ok()
        })
        .unwrap_or_else(|| WindowBounds::centered(size(px(1520.), px(940.)), cx));
    gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some("Devcroft · Guided review".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        move |window, cx| cx.new(|cx| Workspace::new(cwd, scope, label, store, review, window, cx)),
    )?;
    Ok(())
}

struct Workspace {
    cwd: PathBuf,
    scope: ReviewScope,
    label: String,
    store: Store,
    review: Review,
    webview: Option<Entity<gpui_wry::WebView>>,
    gate: Gate,
    error: Option<String>,
    agent: Option<Entity<TerminalPane>>,
    authoring_lock: Option<ReviewLock>,
    agent_kind: AgentKind,
    enabled_agents: Vec<AgentKind>,
    activity: AgentActivityStore,
    authoring: Option<authoring::Workspace>,
    focus: Entity<TextareaState>,
    show_agent: bool,
    show_details: bool,
    show_recovery: bool,
    preparing: bool,
    applying: bool,
    auto_apply: bool,
    author_epoch: u64,
    author_status: String,
    context_capture: Option<String>,
    ready: bool,
    preview: Option<pr::Preview>,
    publishing: bool,
    refreshing: bool,
    source_recovery: bool,
}
impl Workspace {
    fn new(
        cwd: PathBuf,
        scope: ReviewScope,
        label: String,
        store: Store,
        review: Review,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = crate::data::resolve_data_root()
            .ok()
            .and_then(|root| crate::data::DeviceStore::new(&root).load().ok())
            .unwrap_or_default();
        let enabled_agents = settings.enabled_agents_or_default();
        let agent_kind = settings.default_agent_or_default();
        let agent_kind = if enabled_agents.contains(&agent_kind) {
            agent_kind
        } else {
            enabled_agents[0]
        };
        let (activity, updates) = AgentActivityStore::new();
        cx.spawn(async move |this, cx| {
            while updates.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let focus = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("What should this guide help you understand? Leave blank for a complete walkthrough.")
        });
        let show_agent = review.active().bundle.is_none();
        let mut this = Self {
            cwd,
            scope,
            label,
            store,
            review,
            webview: None,
            gate: Gate::new(),
            error: None,
            agent: None,
            authoring_lock: None,
            agent_kind,
            enabled_agents,
            activity,
            authoring: None,
            focus,
            show_agent,
            show_details: false,
            show_recovery: false,
            preparing: false,
            applying: false,
            auto_apply: false,
            author_epoch: 0,
            author_status: "Ready when you are. Your guide will appear here after the agent completes its first revision.".into(),
            context_capture: None,
            ready: false,
            preview: None,
            publishing: false,
            refreshing: false,
            source_recovery: false,
        };
        this.create(window, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        if view.auto_apply {
                            view.apply_guide(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.webview.take();
        self.gate = Gate::new();
        self.ready = false;
        let result = (|| -> Result<_> {
            let sdk = script_json(&include_str!("../../../assets/review-runtime/sdk.js"))?;
            let script = include_str!("../../../ui/review/shell.js")
                .replace("__CAPABILITY__", &script_json(&self.gate.token())?)
                .replace("__CHAPTER_SDK__", &sdk);
            let html = include_str!("../../../ui/review/shell.html")
                .replace("__STYLE__", include_str!("../../../ui/review/shell.css"))
                .replace("__SCRIPT__", &script);
            let capability = self.gate.token().to_owned();
            let (sender, receiver) = async_channel::bounded::<String>(64);
            cx.spawn_in(window, async move |this, cx| {
                while let Ok(body) = receiver.recv().await {
                    if cx
                        .update(|window, cx| {
                            this.update(cx, |view, cx| view.receive(&body, window, cx))
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            crate::webview::create(window, cx, move |builder| {
                builder.with_html(html).with_ipc_handler(move |request| {
                    if request.body().len() <= 64 * 1024
                        && let Ok(message) = serde_json::from_str::<Message>(request.body())
                        && message.token == capability
                    {
                        let _ = sender.try_send(request.into_body());
                    }
                })
            })
        })();
        match result {
            Ok(webview) => {
                self.webview = Some(webview);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not create review webview: {e:#}")),
        }
        cx.notify();
    }
    fn snapshot(&self) -> Value {
        let r = self.review.active();
        let files=r.capture.files.iter().map(|f|json!({"path":f.path,"old_path":f.old_path,"status":f.status,"additions":f.additions,"deletions":f.deletions,"lines":f.lines,"unavailable":f.unavailable,"truncated":f.truncated})).collect::<Vec<_>>();
        json!({"version":self.review.version,"capture":{"id":r.capture.id,"label":r.capture.label,"base":r.capture.base,"head":r.capture.head,"branch":r.capture.branch,"pr":r.capture.pr,"files":files,"evidence":r.capture.evidence},"bundle":r.bundle,"bundleHash":r.bundle.as_ref().and_then(|b|serde_json::to_vec(b).ok()).map(digest),"examined":r.examined,"guideHistory":r.guide_history.iter().filter_map(|g|serde_json::to_vec(g).ok().map(|bytes|json!({"hash":digest(bytes),"title":g.bundle.manifest.title,"examined":g.examined.len()}))).collect::<Vec<_>>(),"page":if self.source_recovery {"changes"} else {r.location.page.as_str()},"chapter":r.location.chapter,"evidence":r.location.evidence,"findings":self.review.findings,"investigations":self.review.investigations,"drafts":self.review.drafts,"history":self.review.revisions.iter().map(|r|json!({"id":r.capture.id,"head":r.capture.head,"examined":r.examined.len()})).collect::<Vec<_>>(),"busy":false,"capturing":self.refreshing,"publishing":self.publishing,"preview":self.preview,"submissions":self.review.submissions})
    }
    fn send(&self, value: Value, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            match script_json(&value) {
                Ok(json) => {
                    let result = webview.update(cx, |view, _| {
                        view.raw()
                            .evaluate_script(&format!("window.__dcReceive({json});"))
                    });
                    if let Err(e) = result {
                        eprintln!("Review bridge delivery failed: {e}");
                    }
                }
                Err(e) => eprintln!("Review bridge serialization failed: {e}"),
            }
        }
    }
    fn receive(&mut self, body: &str, window: &mut Window, cx: &mut Context<Self>) {
        let message = match self.gate.accept(body) {
            Ok(m) => m,
            Err(_) => return,
        };
        let seq = message.seq;
        let include_state = message.op != "source" && message.op != "copy";
        match self.handle(message, window, cx) {
            Ok(result) => {
                let mut response = json!({"seq":seq,"result":result});
                if include_state {
                    response["state"] = self.snapshot();
                }
                self.send(response, cx);
            }
            Err(error) => self.send(json!({"seq":seq,"error":format!("{error:#}")}), cx),
        }
    }
    fn validate_anchor(&self, data: &Value) -> Result<(Option<String>, Option<String>)> {
        let chapter = optional(data, "chapter")?;
        let evidence = optional(data, "evidence")?;
        if let Some(id) = &evidence {
            self.review.active().capture.evidence(id)?;
        }
        if let Some(id) = &chapter {
            let (c, _) = self
                .review
                .active()
                .bundle
                .as_ref()
                .and_then(|b| b.chapter(id))
                .context("Unknown behavior")?;
            if let Some(e) = &evidence {
                ensure!(
                    c.evidence_ids.contains(e),
                    "Evidence is outside this behavior"
                );
            }
        }
        Ok((chapter, evidence))
    }
    fn commit(&mut self, mut next: Review) -> Result<()> {
        self.store.save(&mut next)?;
        self.preview = None;
        self.replace_review(next)
    }
    fn replace_review(&mut self, next: Review) -> Result<()> {
        let changed = self.review.active().capture.id != next.active().capture.id
            || self.review.active().bundle != next.active().bundle;
        self.review = next;
        if changed
            && self.authoring_lock.is_some()
            && let Some(workspace) = &self.authoring
            && workspace.capture == self.review.active().capture.id
            && let Err(error) = workspace.update_viewed_guide(
                &self.review.active().capture.id,
                self.review.active().bundle.as_ref(),
            )
        {
            self.auto_apply = false;
            anyhow::bail!(
                "Review saved, but agent context could not be synchronized. Close and restart the agent before further edits: {error:#}"
            );
        }
        Ok(())
    }
    fn handle(&mut self, m: Message, window: &mut Window, cx: &mut Context<Self>) -> Result<Value> {
        if m.op == "ready" {
            self.ready = true;
            if let Some(view) = &self.webview
                && let Err(error) = view.update(cx, |view, _| {
                    crate::webview::accessibility::attach(view.raw())
                })
            {
                self.error = Some(format!(
                    "Review accessibility setup needs attention: {error:#}"
                ));
            }
            return Ok(Value::Null);
        }
        ensure!(self.ready, "Review document has not initialized");
        ensure!(
            m.capture.as_deref() == Some(&self.review.active().capture.id),
            "Captured revision changed; reopen this review"
        );
        ensure!(
            !self.publishing || ["source", "copy"].contains(&m.op.as_str()),
            "Publication is running; wait for its saved status"
        );
        if m.op == "reload" {
            let active = self.review.active().capture.id.clone();
            let mut latest = self.store.load()?.context("Saved review unavailable")?;
            latest.current = latest
                .revisions
                .iter()
                .position(|r| r.capture.id == active)
                .context("Captured revision no longer exists")?;
            self.replace_review(latest)?;
            return Ok(Value::Null);
        }
        if m.op == "source" {
            let id = text(&m.data, "evidence", 100)?;
            let capture = &self.review.active().capture;
            return Ok(json!({"source":capture.source(capture.evidence(id)?)?}));
        }
        if m.op == "copy" {
            let mut summary = format!(
                "Local review · {}\nCaptured revision {}\n\n",
                self.review.active().capture.label,
                self.review.active().capture.id
            );
            for f in &self.review.findings {
                summary.push_str(&format!(
                    "- [{}] {}{}\n",
                    if f.resolved { "Resolved" } else { "Open" },
                    f.body,
                    if f.capture == self.review.active().capture.id {
                        ""
                    } else {
                        " (earlier revision)"
                    }
                ));
            }
            for q in &self.review.investigations {
                summary.push_str(&format!(
                    "\nQuestion: {}\n{}\n",
                    q.question,
                    q.answer.as_deref().unwrap_or("Unanswered")
                ));
            }
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(summary));
            return Ok(Value::Null);
        }
        ensure!(
            m.version == Some(self.review.version),
            "Review changed; retry after loading the latest state"
        );
        let mut next = self.review.clone();
        match m.op.as_str() {
            "preview" => {
                ensure!(
                    !self.refreshing,
                    "Wait for capture to finish before previewing"
                );
                self.preview = Some(pr::preview(
                    &self.review,
                    text(&m.data, "event", 30)?,
                    text_allow_empty(&m.data, "body", 12000)?,
                )?);
                return Ok(serde_json::to_value(&self.preview)?);
            }
            "publish" | "reconcile" | "discard" => {
                ensure!(
                    !self.refreshing,
                    "Wait for capture to finish before publication"
                );
                let hash = text(&m.data, "hash", 100)?.to_owned();
                let saved = self
                    .review
                    .submissions
                    .iter()
                    .find(|s| s.preview.hash == hash);
                let create = if let Some(saved) = saved {
                    ensure!(
                        !matches!(saved.state.as_str(), "submitted" | "rejected" | "discarded"),
                        "This publication is complete or was rejected; inspect a fresh preview"
                    );
                    false
                } else {
                    ensure!(m.op == "publish", "No saved publication to reconcile");
                    ensure!(
                        !next.submissions.iter().any(|s| !matches!(
                            s.state.as_str(),
                            "submitted" | "rejected" | "discarded"
                        )),
                        "A previous publication is unresolved. Inspect and reconcile its saved preview before creating another review"
                    );
                    let preview = self
                        .preview
                        .as_ref()
                        .context("Create a fresh preview before publishing")?;
                    ensure!(
                        preview.hash == hash
                            && preview.version == self.review.version
                            && preview.capture == self.review.active().capture.id,
                        "The preview is stale; inspect a new preview"
                    );
                    ensure!(
                        next.submissions.len() < 100,
                        "Publication history limit reached"
                    );
                    next.submissions.push(pr::Submission {
                        preview: preview.clone(),
                        remote_id: None,
                        remote_author: None,
                        event_sent: false,
                        remote_url: None,
                        state: "creating".into(),
                        error: None,
                    });
                    // This intent is durable before the first external mutation.
                    self.commit(next)?;
                    true
                };
                self.publication(hash, create, m.op == "publish", m.op == "discard", cx);
            }
            "revision" => {
                self.auto_apply = false;
                self.author_epoch = self.author_epoch.wrapping_add(1);
                next = self.review.clone();
                let id = text(&m.data, "id", 100)?;
                next.current = next
                    .revisions
                    .iter()
                    .position(|r| r.capture.id == id)
                    .context("Unknown captured revision")?;
                self.commit(next)?;
            }
            "location" => {
                self.source_recovery = false;
                let page = text(&m.data, "page", 20)?;
                ensure!(
                    ["overview", "walkthrough", "changes", "findings"].contains(&page),
                    "Invalid review page"
                );
                let (chapter, evidence) = self.validate_anchor(&m.data)?;
                next.active_mut().location = Location {
                    page: page.into(),
                    chapter,
                    evidence,
                };
                self.commit(next)?;
            }
            "draft" => {
                let key = text(&m.data, "key", 400)?;
                ensure!(
                    key.starts_with(&format!("{}:", next.active().capture.id)),
                    "Draft is outside this capture"
                );
                let body = text_allow_empty(&m.data, "body", 12000)?;
                ensure!(
                    next.drafts.len() < 256 || next.drafts.contains_key(key),
                    "Too many drafts"
                );
                if body.is_empty() {
                    next.drafts.remove(key);
                } else {
                    next.drafts.insert(key.into(), body.into());
                }
                self.commit(next)?;
            }
            "finding" => {
                let (chapter, evidence) = self.validate_anchor(&m.data)?;
                let body = text(&m.data, "body", 12000)?.trim();
                ensure!(
                    !body.is_empty() && next.findings.len() < 1000,
                    "Empty finding or finding limit reached"
                );
                let id = digest(rand::random::<[u8; 32]>());
                let range = match m.data.get("range") {
                    None | Some(Value::Null) => None,
                    Some(value) => {
                        let range: SourceRange = serde_json::from_value(value.clone())?;
                        let e = next.active().capture.evidence(
                            evidence
                                .as_deref()
                                .context("Range requires source evidence")?,
                        )?;
                        ensure!(
                            range.start >= e.start
                                && range.start <= range.end
                                && range.end <= e.end,
                            "Finding range is outside captured evidence"
                        );
                        Some(range)
                    }
                };
                next.findings.push(Finding {
                    id,
                    capture: next.active().capture.id.clone(),
                    chapter,
                    evidence,
                    body: body.into(),
                    resolved: false,
                    range,
                });
                if let Some(key) = m.data.get("key").and_then(Value::as_str) {
                    ensure!(
                        key.starts_with(&format!("{}:", next.active().capture.id)),
                        "Draft belongs to another capture"
                    );
                    next.drafts.remove(key);
                }
                self.commit(next)?;
            }
            "resolve" => {
                let id = text(&m.data, "id", 100)?;
                let finding = next
                    .findings
                    .iter_mut()
                    .find(|f| f.id == id)
                    .context("Finding no longer exists")?;
                finding.resolved = m
                    .data
                    .get("resolved")
                    .and_then(Value::as_bool)
                    .context("Missing resolution state")?;
                self.commit(next)?;
            }
            "examined" => {
                let id = text(&m.data, "chapter", 100)?;
                ensure!(
                    next.active()
                        .bundle
                        .as_ref()
                        .and_then(|b| b.chapter(id))
                        .is_some(),
                    "Unknown behavior"
                );
                let examined = m
                    .data
                    .get("examined")
                    .and_then(Value::as_bool)
                    .context("Missing decision")?;
                let decisions = &mut next.active_mut().examined;
                decisions.retain(|c| c != id);
                if examined {
                    decisions.push(id.into());
                }
                self.commit(next)?;
            }
            "guide" => {
                self.auto_apply = false;
                self.author_epoch = self.author_epoch.wrapping_add(1);
                next = self.review.clone();
                next.select_guide(text(&m.data, "hash", 100)?)?;
                self.commit(next)?;
            }
            "ask" | "repair" => {
                let (chapter, evidence) = self.validate_anchor(&m.data)?;
                let capture = &self.review.active().capture;
                let request = if m.op == "repair" {
                    format!(
                        "Repair the visual for chapter {}. {}",
                        chapter.as_deref().context("Repair needs a chapter")?,
                        text_allow_empty(&m.data, "problem", 2000)?
                    )
                } else {
                    text_allow_empty(&m.data, "question", 12000)?.to_owned()
                };
                let context = format!(
                    "{}\n\nCaptured revision: {}\nChapter: {}\nEvidence: {}",
                    request,
                    capture.id,
                    chapter.as_deref().unwrap_or("source review"),
                    evidence.as_deref().unwrap_or("none selected")
                );
                self.focus
                    .update(cx, |input, cx| input.set_value(context, window, cx));
                self.context_capture = Some(capture.id.clone());
                self.show_agent = true;
                if let Some(webview) = &self.webview {
                    let _ = webview.read(cx).raw().focus_parent();
                }
                self.focus.focus_handle(cx).focus(window, cx);
                self.author_status = "Context opened in the native pane. Generate with this context, or copy it into your ongoing agent conversation.".into();
                cx.notify();
            }
            _ => anyhow::bail!("Unsupported review operation"),
        }
        Ok(Value::Null)
    }
    fn publication(
        &mut self,
        hash: String,
        create: bool,
        submit: bool,
        discard: bool,
        cx: &mut Context<Self>,
    ) {
        self.publishing = true;
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move {
                if discard { pr::discard_publication(&store, &hash) }
                else { pr::complete_publication(&store, &hash, create, submit) }
            }).await;
            let _ = this.update(cx, |view, cx| {
                view.publishing = false;
                match result {
                    Ok(review) => { if let Err(error) = view.replace_review(review) {view.author_status = format!("{error:#}");} view.send(json!({"state":view.snapshot(),"status":"GitHub review status saved. Inspect the publication record below."}), cx); },
                    Err(e) => { if let Ok(Some(review)) = view.store.load() {let _ = view.replace_review(review);} view.send(json!({"state":view.snapshot(),"error":format!("Publication status needs attention: {e:#}")}), cx); }
                }
                cx.notify();
            });
        }).detach();
    }
    fn start_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preparing || self.refreshing || self.agent.is_some() {
            return;
        }
        let settings = crate::data::resolve_data_root()
            .ok()
            .and_then(|root| crate::data::DeviceStore::new(&root).load().ok())
            .unwrap_or_default();
        self.enabled_agents = settings.enabled_agents_or_default();
        if !self.enabled_agents.contains(&self.agent_kind) {
            self.author_status =
                "This agent was disabled in Settings. Select an enabled agent.".into();
            cx.notify();
            return;
        }
        if self
            .context_capture
            .as_ref()
            .is_some_and(|id| id != &self.review.active().capture.id)
        {
            self.author_status = "This context belongs to an earlier capture. Reopen it or clear the context before generating.".into();
            cx.notify();
            return;
        }
        let store = self.store.clone();
        let capture = self.review.active().capture.clone();
        let id = capture.id.clone();
        let bundle = self.review.active().bundle.clone();
        let cwd = self.cwd.clone();
        let focus = self.focus.read(cx).value().to_string();
        self.preparing = true;
        self.author_status = "Preparing the guide workspace and ordinary agent session…".into();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx.background_spawn(async move {
                let lock = store.authoring_lock(&capture)?;
                authoring::Workspace::prepare(&store, &capture, bundle.as_ref(), &cwd, &focus).map(|workspace| (workspace, lock))
            }).await;
            let _ = cx.update(|window, cx| this.update(cx, |view, cx| {
                view.preparing = false;
                if view.review.active().capture.id != id { view.author_status = "Viewed capture changed. Generate again for the selected revision.".into(); cx.notify(); return; }
                match prepared {
                    Ok((workspace, lock)) => {
                        let prompt = workspace.prompt();
                        view.author_epoch = view.author_epoch.wrapping_add(1);
                        let pane = cx.new(|cx| TerminalPane::with_prompt(WorkspaceTab::Agent, &workspace.checkout, view.agent_kind, &view.activity, None, Some(&prompt), cx));
                        pane.focus_handle(cx).focus(window, cx);
                        view.authoring = Some(workspace);
                        view.authoring_lock = Some(lock);
                        view.auto_apply = true;
                        view.agent = Some(pane);
                        view.show_agent = true;
                        view.author_status = "Chat with your agent below. Complete guide commits appear automatically; tools and approvals use your usual agent settings.".into();
                    }
                    Err(error) => view.author_status = format!("Could not prepare guide authoring: {error:#}"),
                }
                cx.notify();
            }));
        }).detach();
    }
    fn apply_guide(&mut self, cx: &mut Context<Self>) {
        if self.applying || self.publishing || self.refreshing {
            return;
        }
        let Some(workspace) = self.authoring.clone() else {
            return;
        };
        let capture = self.review.active().capture.id.clone();
        if workspace.capture != capture {
            self.auto_apply = false;
            self.author_epoch = self.author_epoch.wrapping_add(1);
            return;
        }
        self.applying = true;
        let id = capture.clone();
        let epoch = self.author_epoch;
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { workspace.load(&capture) }).await;
            let _ = this.update(cx, |view, cx| {
                view.applying = false;
                if view.author_epoch != epoch || view.review.active().capture.id != id || view.authoring.as_ref().is_none_or(|a| a.capture != id) { return; }
                let result = result.and_then(|bundle| {
                    let Some(bundle) = bundle else { return Ok(false); };
                    let hash = digest(serde_json::to_vec(&bundle)?);
                    if view.review.active().bundle.as_ref().map(serde_json::to_vec).transpose()?.map(digest).as_ref() == Some(&hash) { return Ok(false); }
                    let mut next = view.review.clone(); next.install(bundle)?; view.commit(next)?; Ok(true)
                });
                match result {
                    Ok(true) => {view.author_status = "Guide update validated and saved. Continue chatting to refine it.".into(); view.send(json!({"state":view.snapshot(),"status":"Guide updated from your native agent session."}), cx);}
                    Ok(false) => {}
                    Err(error) => view.author_status = format!("Guide update needs attention; a saved guide remains available. {error:#}"),
                }
                cx.notify();
            });
        }).detach();
    }
    fn close_agent(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.agent.take() {
            pane.update(cx, |pane, cx| pane.close(cx));
        }
        self.authoring_lock.take();
        self.auto_apply = false;
        self.author_epoch = self.author_epoch.wrapping_add(1);
        self.author_status = "Agent session closed. Guide files and saved revisions remain; start another agent to continue.".into();
        cx.notify();
    }
    fn show_reader(&mut self, cx: &mut Context<Self>) {
        self.show_agent = false;
        self.source_recovery = false;
        // The explicit Read guide action should open the explanation, even
        // when generation began from a source-only capture.
        if self.review.active().bundle.is_some()
            && matches!(
                self.review.active().location.page.as_str(),
                "changes" | "findings"
            )
        {
            let mut next = self.review.clone();
            next.active_mut().location.page = "overview".into();
            if let Err(error) = self.commit(next) {
                self.error = Some(format!("Could not open guide: {error:#}"));
            }
        }
        self.send(json!({"state":self.snapshot()}), cx);
        cx.notify();
    }
    fn author_panel(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let mut providers = h_flex().gap_1();
        for kind in &self.enabled_agents {
            let kind = *kind;
            providers = providers.child(
                Button::new(format!("review-agent-{}", kind.id()))
                    .ghost()
                    .label(format!(
                        "{}{}",
                        if kind == self.agent_kind { "✓ " } else { "" },
                        kind.label()
                    ))
                    .disabled(self.agent.is_some() || self.preparing)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent_kind = kind;
                        cx.notify();
                    })),
            );
        }
        let scope = if let Some(a) = &self.authoring {
            format!(
                "Working checkout: {}\nCheckout HEAD: {} · captured HEAD: {}",
                a.checkout.display(),
                a.checkout_head
                    .as_deref()
                    .map(|head| &head[..12])
                    .unwrap_or("unavailable"),
                &self.review.active().capture.head
                    [..12.min(self.review.active().capture.head.len())]
            )
        } else if self.review.active().capture.pr.is_some() {
            "Uses the verified linked checkout, or a managed checkout at the captured PR head."
                .to_owned()
        } else {
            format!("Working checkout: {}", self.cwd.display())
        };
        let session = self.authoring.as_ref().map(|a| {
            format!(
                "Session capture {} · viewed {}",
                &a.capture[..10.min(a.capture.len())],
                &self.review.active().capture.id[..10]
            )
        });
        let activity = self
            .agent
            .as_ref()
            .and_then(|p| p.read(cx).launch_id())
            .and_then(|id| self.activity.snapshot().for_launch(id).cloned());
        let start = h_flex().flex_wrap().gap_2()
            .child(Button::new("review-generate-native").primary().label(if self.preparing {"Preparing…"} else if self.agent.is_some() {"Agent session open"} else if self.review.active().bundle.is_some() {"Continue with agent"} else {"Generate guide"})
                .disabled(self.preparing || self.refreshing || self.agent.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.start_agent(window, cx))))
            .child(Button::new("review-copy-context").ghost().label("Copy context").on_click(cx.listener(|this, _, _, cx| {
                let context = this.focus.read(cx).value().to_string();
                let mut prompt = format!("{}\n\nViewed capture: {}", context, this.review.active().capture.id);
                if let Some(a) = &this.authoring {
                    if a.capture == this.review.active().capture.id {prompt.push_str(&format!("\n{}", a.prompt()));}
                    else {prompt.push_str("\nThe open agent belongs to an earlier capture. Close it and start another agent for this revision before editing the guide.");}
                }
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(prompt));
                this.author_status = "Context copied. Paste it into your agent conversation to discuss or update the guide.".into(); cx.notify();
            })));
        let tools = h_flex()
            .flex_wrap()
            .gap_1()
            .child(
                Button::new("review-apply-guide")
                    .ghost()
                    .label("Apply updates")
                    .disabled(
                        self.applying
                            || self
                                .authoring
                                .as_ref()
                                .is_none_or(|a| a.capture != self.review.active().capture.id),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.apply_guide(cx))),
            )
            .child(
                Button::new("review-copy-brief")
                    .ghost()
                    .label("Copy brief")
                    .disabled(self.authoring.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(a) = &this.authoring {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                a.instructions.clone(),
                            ));
                            this.author_status = "Full authoring instructions copied.".into();
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("review-clear-context")
                    .ghost()
                    .label("Clear")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        this.context_capture = None;
                        cx.notify();
                    })),
            )
            .when(self.agent.is_some(), |d| {
                d.child(
                    Button::new("review-stop-agent")
                        .ghost()
                        .label("Close agent")
                        .on_click(cx.listener(|this, _, _, cx| this.close_agent(cx))),
                )
            });
        let capture = &self.review.active().capture;
        let additions: u64 = capture
            .files
            .iter()
            .map(|file| u64::from(file.additions))
            .sum();
        let deletions: u64 = capture
            .files
            .iter()
            .map(|file| u64::from(file.deletions))
            .sum();
        let mut presets = h_flex().flex_wrap().gap_2();
        for (id, label, prompt) in [
            (
                "architecture",
                "Architecture",
                "Explain the architecture and how data flows through this change.",
            ),
            (
                "risks",
                "Risks & edge cases",
                "Focus on correctness risks, edge cases, and failure handling.",
            ),
            (
                "tests",
                "Test coverage",
                "Explain the test coverage, missing cases, and how to verify this change.",
            ),
        ] {
            presets = presets.child(Button::new(id).ghost().label(label).on_click(cx.listener(
                move |this, _, window, cx| {
                    let current = this.focus.read(cx).value().to_string();
                    let value = if current.trim().is_empty() {
                        prompt.to_owned()
                    } else {
                        format!("{current}\n{prompt}")
                    };
                    this.focus
                        .update(cx, |input, cx| input.set_value(value, window, cx));
                    cx.notify();
                },
            )));
        }
        let header = div()
            .id("guide-setup")
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(720.))
            .when(self.agent.is_some() && !compact, |d| d.w(px(440.)).flex_shrink_0())
            .when(self.agent.is_some() && compact, |d| d.max_w_full().h(px(240.)).flex_shrink_0())
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child("GUIDED REVIEW"))
            .child(div().text_2xl().font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(if self.agent.is_some() { "Build your understanding" } else if self.review.active().bundle.is_some() { "Refine your guide" } else { "Turn changes into a walkthrough" }))
            .child(div().text_color(cx.theme().muted_foreground)
                .child("Explore the reasoning behind a change, follow its source, and capture the questions that matter."))
            .child(div().p_4().rounded_lg().bg(cx.theme().muted)
                .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(capture.label.clone()))
                .child(div().text_sm().text_color(cx.theme().muted_foreground)
                    .child(format!("{} files  ·  +{additions}  −{deletions}  ·  captured {}", capture.files.len(), &capture.head[..12.min(capture.head.len())]))))
            .child(div().text_sm().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("1  Choose your agent"))
            .child(providers)
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child("Uses your existing agent settings, tools, and permissions."))
            .child(div().text_sm().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("2  Set a focus · optional"))
            .child(Textarea::new(&self.focus).h(px(112.)))
            .child(presets)
            .child(start)
            .child(div().p_3().rounded_lg().bg(cx.theme().muted).text_sm().child(self.author_status.clone()))
            .when_some(activity, |d, a| d.child(div().text_sm().child(format!("{} · {}", self.agent_kind.label(), a.detail.as_deref().unwrap_or(a.state.label())))))
            .when(self.review.active().bundle.is_some(), |d| d.child(
                Button::new("read-saved-guide").primary().label("Read guide →")
                    .on_click(cx.listener(|this, _, _, cx| { this.show_reader(cx); }))))
            .child(Button::new("author-details").ghost().label(if self.show_details { "Hide session details" } else { "Session details & tools" })
                .on_click(cx.listener(|this, _, _, cx| { this.show_details = !this.show_details; cx.notify(); })))
            .when(self.show_details, |d| d
                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(scope))
                .when_some(session, |d, s| d.child(div().text_sm().child(s)))
                .child(tools));
        h_flex()
            .size_full()
            .min_h_0()
            .items_stretch()
            .justify_center()
            .when(compact, |d| d.flex_col())
            .child(header)
            .when_some(self.agent.clone(), |d, pane| {
                d.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .when(!compact, |d| d.h_full())
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .child(pane),
                )
            })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        if self.publishing {
            self.error = Some("Wait for publication to finish before refreshing".into());
            cx.notify();
            return;
        }
        self.author_epoch = self.author_epoch.wrapping_add(1);
        let cwd = self.cwd.clone();
        let scope = self.scope.clone();
        let label = self.label.clone();
        let original = self.review.active().capture.id.clone();
        let url = self
            .review
            .active()
            .capture
            .pr
            .as_ref()
            .map(|p| p.url.clone());
        self.refreshing = true;
        self.send(
            json!({"state":self.snapshot(),"status":"Capturing a coherent source revision…"}),
            cx,
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move {
                if let Some(url) = url {
                    pr::acquire(&url)
                } else {
                    Capture::stable_local(&cwd, &scope, &format!("Local working-tree snapshot · {label}"))
                }
            }).await;
            let _ = this.update(cx, |view, cx| {
                view.refreshing = false;
                if view.review.active().capture.id != original {
                    view.send(json!({"state":view.snapshot(),"status":"Capture selection changed while refreshing. Capture again when ready."}), cx);
                    cx.notify();
                    return;
                }
                let result = result.and_then(|capture| {
                    let mut next = view.review.clone();
                    next.add_capture(capture);
                    view.commit(next)?;
                    if view.review.active().capture.id != original {
                        view.auto_apply = false;
                        view.author_status = "New capture selected. The open agent remains on its original capture; close it and start another to update this revision.".into();
                    }
                    Ok(())
                });
                match result {
                    Ok(()) => view.send(json!({"state":view.snapshot(),"status":"Captured source refreshed. Previous decisions remain in revision history."}), cx),
                    Err(error) => view.send(json!({"state":view.snapshot(),"error":format!("Capture failed: {error:#}")}), cx),
                }
                cx.notify();
            });
        }).detach();
    }
}
impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // AccessKit can install its native subclass after the first ready
        // event. Keep the web area's accessibility tree joined after native
        // input/terminal updates as well as at document initialization.
        if self.ready
            && let Some(webview) = &self.webview
        {
            let _ = crate::webview::accessibility::attach(webview.read(cx).raw());
        }
        // Native child views do not follow GPUI layout visibility automatically.
        // Hide explicitly, retaining the DOM and terminal session across mode switches.
        if let Some(webview) = &self.webview {
            let visible = !self.show_agent;
            webview.update(cx, |view, cx| {
                if visible != view.visible() {
                    if visible {
                        view.show();
                    } else {
                        view.hide();
                    }
                    cx.notify();
                }
            });
        }
        let webview = self.webview.clone().filter(|_| !self.show_agent);
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .min_h(px(46.)).flex_shrink_0().flex_wrap().border_b_1().border_color(cx.theme().border)
                    .gap_3()
                    .px_3()
                    .child(div().font_weight(gpui_kit::FontWeight::SEMIBOLD).child("Guided review"))
                    .child(div().flex_1())
                    .child(Button::new("review-more").ghost().label(if self.show_recovery { "Less" } else { "More" })
                        .on_click(cx.listener(|this, _, _, cx| { this.show_recovery = !this.show_recovery; cx.notify(); })))
                    .child(Button::new("review-agent-toggle").ghost().label(if self.show_agent { if self.review.active().bundle.is_some() { "Read guide" } else { "Inspect source" } } else { "Edit guide" }).on_click(cx.listener(|this, _, _, cx| {if this.show_agent { this.show_reader(cx); } else { this.show_agent = true; cx.notify(); }})))
                    .child(
                        Button::new("review-close")
                            .ghost()
                            .label("Close review")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_agent(cx);
                                this.webview.take();
                                window.remove_window();
                            })),
                    ),
            )
            .when(self.show_recovery, |d| d.child(h_flex().flex_wrap().px_3().py_2().gap_2()
                    .child(
                        Button::new("review-refresh")
                            .ghost()
                            .label(if self.refreshing {"Capturing revision…"} else {"Capture new revision"})
                            .disabled(self.refreshing)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("review-recover")
                            .ghost()
                            .label("Recover view")
                            .on_click(cx.listener(|this, _, window, cx| { this.source_recovery = true; this.show_agent = false; this.create(window, cx); })),
                    )
                    .child(Button::new("review-reload").ghost().label("Reload saved state").on_click(cx.listener(|this,_,_,cx| {
                        match this.store.load() {Ok(Some(mut latest))=>{
                            let id=&this.review.active().capture.id;
                            if let Some(index)=latest.revisions.iter().position(|r|&r.capture.id==id){latest.current=index;}
                            if let Err(error) = this.replace_review(latest) {this.author_status = format!("{error:#}");} this.send(json!({"state":this.snapshot(),"status":"Loaded saved review. Unsaved composer text is retained; retry saving."}),cx);
                        },Ok(None)=>{},Err(e)=>this.error=Some(format!("{e:#}"))}cx.notify();
                    })))
            ))
            .when_some(self.error.clone(), |d, error| {
                d.child(div().p_4().child(error))
            })
            .child(h_flex().flex_1().min_h_0().items_stretch()
                .when(self.show_agent, |d| d.child(self.author_panel(window.viewport_size().width < px(900.), cx)))
                .when_some(webview, |d, webview| d.child(div().flex_1().min_w_0().min_h_0().child(webview))))
    }
}
fn text<'a>(data: &'a Value, key: &str, limit: usize) -> Result<&'a str> {
    let value = text_allow_empty(data, key, limit)?;
    ensure!(!value.is_empty(), "Missing {key}");
    Ok(value)
}
fn text_allow_empty<'a>(data: &'a Value, key: &str, limit: usize) -> Result<&'a str> {
    let value = data
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing {key}"))?;
    ensure!(value.len() <= limit, "{key} exceeds its limit");
    Ok(value)
}
fn optional(data: &Value, key: &str) -> Result<Option<String>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= 100 => Ok(Some(s.clone())),
        _ => anyhow::bail!("Invalid {key}"),
    }
}
