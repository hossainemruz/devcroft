use super::{
    author,
    bridge::{Gate, Message, script_json},
};
use crate::review::{
    git::ReviewScope,
    model::ReviewDiff,
    session::{Capture, Finding, Investigation, Location, Review, SourceRange, Store, digest, pr},
};
use anyhow::{Context as _, Result, ensure};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Window, WindowOptions, div, px,
};
use raw_window_handle::HasWindowHandle as _;
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
    gpui_kit::open_window(
        WindowOptions {
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
    job: Option<author::Job>,
    log: String,
    ready: bool,
    preview: Option<pr::Preview>,
    publishing: bool,
    refreshing: bool,
    author_error: Option<String>,
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
        let author_error = author::preflight(&cwd).err().map(|e| format!("{e:#}"));
        let mut this = Self {
            cwd,
            scope,
            label,
            store,
            review,
            webview: None,
            gate: Gate::new(),
            error: None,
            job: None,
            log: String::new(),
            ready: false,
            preview: None,
            publishing: false,
            refreshing: false,
            author_error,
            source_recovery: false,
        };
        this.create(window, cx);
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
            cx.spawn(async move |this, cx| {
                while let Ok(body) = receiver.recv().await {
                    if this.update(cx, |view, cx| view.receive(&body, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            let native = wry::WebViewBuilder::new()
                .with_html(html)
                .with_incognito(true)
                .with_navigation_handler(|url| url == "about:blank" || url == "about:srcdoc")
                .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
                .with_download_started_handler(|_, _| false)
                .with_ipc_handler(move |request| {
                    if request.body().len() <= 64 * 1024
                        && let Ok(message) = serde_json::from_str::<Message>(request.body())
                        && message.token == capability
                    {
                        let _ = sender.try_send(request.into_body());
                    }
                })
                .build_as_child(&window.window_handle()?)?;
            Ok(cx.new(|cx| gpui_wry::WebView::new(native, window, cx)))
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
        let provider = crate::review::assistant::generation_options().provider;
        json!({"version":self.review.version,"capture":{"id":r.capture.id,"label":r.capture.label,"base":r.capture.base,"head":r.capture.head,"branch":r.capture.branch,"pr":r.capture.pr,"files":files,"evidence":r.capture.evidence},"bundle":r.bundle,"bundleHash":r.bundle.as_ref().and_then(|b|serde_json::to_vec(b).ok()).map(digest),"examined":r.examined,"guideHistory":r.guide_history.iter().filter_map(|g|serde_json::to_vec(g).ok().map(|bytes|json!({"hash":digest(bytes),"title":g.bundle.manifest.title,"examined":g.examined.len()}))).collect::<Vec<_>>(),"page":if self.source_recovery {"changes"} else {r.location.page.as_str()},"chapter":r.location.chapter,"evidence":r.location.evidence,"findings":self.review.findings,"investigations":self.review.investigations,"drafts":self.review.drafts,"history":self.review.revisions.iter().map(|r|json!({"id":r.capture.id,"head":r.capture.head,"examined":r.examined.len()})).collect::<Vec<_>>(),"busy":self.job.is_some(),"capturing":self.refreshing,"publishing":self.publishing,"preview":self.preview,"submissions":self.review.submissions,"provider":provider.label().to_lowercase(),"providers":[{"id":"codex","label":"Codex · confined authoring","eligible":self.author_error.is_none(),"reason":self.author_error},{"id":"claude","label":"Claude · adapter not yet confined","eligible":false},{"id":"opencode","label":"OpenCode · adapter not yet confined","eligible":false},{"id":"omp","label":"Omp · adapter not yet confined","eligible":false}],"sharing":"Generation and questions send the captured source excerpts and your instructions to the selected agent provider. The live checkout and related repositories are excluded."})
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
    fn receive(&mut self, body: &str, cx: &mut Context<Self>) {
        let message = match self.gate.accept(body) {
            Ok(m) => m,
            Err(_) => return,
        };
        let seq = message.seq;
        let include_state = message.op != "source" && message.op != "copy";
        match self.handle(message, cx) {
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
        self.review = next;
        self.preview = None;
        Ok(())
    }
    fn handle(&mut self, m: Message, cx: &mut Context<Self>) -> Result<Value> {
        if m.op == "ready" {
            self.ready = true;
            if let Some(view) = &self.webview
                && let Err(error) =
                    view.update(cx, |view, _| super::accessibility::attach(view.raw()))
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
            self.review = latest;
            return Ok(Value::Null);
        }
        if m.op == "source" {
            let id = text(&m.data, "evidence", 100)?;
            let capture = &self.review.active().capture;
            return Ok(json!({"source":capture.source(capture.evidence(id)?)?}));
        }
        if m.op == "stop" {
            self.cancel_job("Request stopped by you")?;
            return Ok(Value::Null);
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
                ensure!(
                    self.job.is_none(),
                    "Finish or stop the agent request before previewing a review"
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
                ensure!(
                    self.job.is_none(),
                    "Finish or stop the agent request before publication"
                );
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
                self.cancel_job("Changed viewed revision")?;
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
                self.cancel_job("Changed viewed guide")?;
                next = self.review.clone();
                next.select_guide(text(&m.data, "hash", 100)?)?;
                self.commit(next)?;
            }
            "repair" => {
                ensure!(self.job.is_none(), "An agent request is already running");
                ensure!(
                    text(&m.data, "provider", 30)? == "codex",
                    "This provider has no confined authoring adapter yet"
                );
                let chapter = text(&m.data, "chapter", 80)?.to_owned();
                let problem = text_allow_empty(&m.data, "problem", 2000)?.to_owned();
                self.run(author::Task::Repair { chapter, problem }, cx)?;
            }
            "generate" => {
                ensure!(self.job.is_none(), "An agent request is already running");
                ensure!(
                    text(&m.data, "provider", 30)? == "codex",
                    "This provider has no confined authoring adapter yet"
                );
                let priorities = text_allow_empty(&m.data, "priorities", 12000)?;
                self.run(
                    author::Task::Guide {
                        priorities: priorities.into(),
                    },
                    cx,
                )?;
            }
            "ask" => {
                ensure!(self.job.is_none(), "An agent request is already running");
                ensure!(
                    text(&m.data, "provider", 30)? == "codex",
                    "This provider has no confined authoring adapter yet"
                );
                let (chapter, evidence) = self.validate_anchor(&m.data)?;
                let question = text(&m.data, "question", 12000)?.trim().to_owned();
                ensure!(
                    !question.is_empty() && next.investigations.len() < 1000,
                    "Empty question or investigation limit reached"
                );
                let id = digest(rand::random::<[u8; 32]>());
                let key = format!(
                    "{}:question:{}:{}",
                    next.active().capture.id,
                    chapter.as_deref().unwrap_or("source"),
                    evidence.as_deref().unwrap_or("none")
                );
                next.drafts.remove(&key);
                next.investigations.push(Investigation {
                    id: id.clone(),
                    capture: next.active().capture.id.clone(),
                    chapter: chapter.clone(),
                    evidence: evidence.clone(),
                    question: question.clone(),
                    answer: None,
                    error: None,
                });
                self.commit(next)?;
                if let Err(e) = self.run(
                    author::Task::Question {
                        id: id.clone(),
                        question,
                        chapter,
                        evidence,
                    },
                    cx,
                ) {
                    let mut next = self.review.clone();
                    next.investigations
                        .iter_mut()
                        .find(|q| q.id == id)
                        .unwrap()
                        .error = Some(format!("{e:#}"));
                    self.commit(next)?;
                    return Err(e);
                }
            }
            _ => anyhow::bail!("Unsupported review operation"),
        }
        Ok(Value::Null)
    }
    fn run(&mut self, task: author::Task, cx: &mut Context<Self>) -> Result<()> {
        ensure!(
            !self.refreshing,
            "Wait for capture to finish before requesting an explanation"
        );
        let capture = self.review.active().capture.clone();
        let id = capture.id.clone();
        let (job, receiver) = author::start(
            &self.cwd,
            capture,
            self.review.active().bundle.clone(),
            task,
            crate::review::assistant::generation_options(),
        )?;
        let operation = job.id.clone();
        self.job = Some(job);
        self.log.clear();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                let finished = matches!(event, author::Event::Finished(_));
                if this.update(cx, |view, cx| {
                    if view.review.active().capture.id != id || view.job.as_ref().is_none_or(|job|job.id != operation) { return; }
                    match event {
                        author::Event::Progress(line) => {
                            if view.log.len()+line.len()<64*1024 {view.log.push_str(&line);view.log.push('\n');}
                            view.send(json!({"log":view.log,"status":"Working on the captured revision…"}),cx);
                        }
                        author::Event::Finished(result) => {
                            let question = view.job.take().and_then(|job|job.question.clone());
                            let mut next=view.review.clone();
                            let result=match result {
                                Ok(output) => (|| -> Result<()> {
                                    match output { author::Output::Guide(bundle)=>next.install(bundle)?,author::Output::Answer{id,answer}=>if let Some(q)=next.investigations.iter_mut().find(|q|q.id==id){q.answer=Some(answer);q.error=None;} }
                                    view.commit(next)
                                })(),
                                Err(error) => {
                                    if let Some(id)=question && let Some(q)=next.investigations.iter_mut().find(|q|q.id==id) {q.error=Some(format!("{error:#}"));if let Err(save_error)=view.commit(next) { view.error=Some(format!("Could not save the failed question: {save_error:#}")); }}
                                    Err(error)
                                }
                            };
                            match result {Ok(())=>view.send(json!({"state":view.snapshot(),"status":"Saved locally."}),cx),Err(e)=>view.send(json!({"state":view.snapshot(),"error":format!("Agent request failed: {e:#}")}),cx)}
                            cx.notify();
                        }
                    }
                }).is_err() || finished {break;}
            }
        }).detach();
        Ok(())
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
                    Ok(review) => { view.review = review; view.send(json!({"state":view.snapshot(),"status":"GitHub review status saved. Inspect the publication record below."}), cx); },
                    Err(e) => { if let Ok(Some(review)) = view.store.load() {view.review = review;} view.send(json!({"state":view.snapshot(),"error":format!("Publication status needs attention: {e:#}")}), cx); }
                }
                cx.notify();
            });
        }).detach();
    }
    fn cancel_job(&mut self, reason: &str) -> Result<()> {
        if let Some(job) = self.job.take()
            && let Some(id) = &job.question
        {
            let mut next = self.review.clone();
            if let Some(q) = next.investigations.iter_mut().find(|q| &q.id == id) {
                q.error = Some(reason.into());
                self.commit(next)?;
            }
        }
        Ok(())
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
        if let Err(e) = self.cancel_job("New capture requested") {
            self.error = Some(format!("{e:#}"));
            return;
        }
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
                    view.commit(next)
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
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.cancel_job("Review window closed before the answer was saved");
    }
}
impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let webview = self.webview.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(px(38.))
                    .gap_3()
                    .px_3()
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
                            .label("Recover source view")
                            .on_click(cx.listener(|this, _, window, cx| { this.source_recovery = true; this.create(window, cx); })),
                    )
                    .child(Button::new("review-reload").ghost().label("Reload saved state").on_click(cx.listener(|this,_,_,cx| {
                        match this.store.load() {Ok(Some(mut latest))=>{
                            let id=&this.review.active().capture.id;
                            if let Some(index)=latest.revisions.iter().position(|r|&r.capture.id==id){latest.current=index;}
                            this.review=latest;this.send(json!({"state":this.snapshot(),"status":"Loaded saved review. Unsaved composer text is retained; retry saving."}),cx);
                        },Ok(None)=>{},Err(e)=>this.error=Some(format!("{e:#}"))}cx.notify();
                    })))
                    .child(
                        Button::new("review-close")
                            .ghost()
                            .label("Close review")
                            .on_click(cx.listener(|this, _, window, _| {
                                let _ = this.cancel_job("Review window closed");
                                this.webview.take();
                                window.remove_window();
                            })),
                    ),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(div().p_4().child(error))
            })
            .when_some(webview, |d, webview| {
                d.child(div().flex_1().min_h_0().child(webview))
            })
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
