use super::*;
use gpui_kit::WeakEntity;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};

/// One stop for navigation-mode `j`/`k` in the Agent sessions sidebar, in
/// display order (open sessions newest-first, then recent catalog rows).
pub(super) enum SessionNavTarget {
    Open(u64),
    Recent(SessionKey),
}

/// Shared presentation for live sessions and rows from the recent catalog.
struct SessionSidebarRow {
    target: SessionNavTarget,
    title: String,
    provider: String,
    age: Option<String>,
    tooltip: String,
    icon: AnyElement,
    status: Option<ActivityState>,
    open_id: Option<u64>,
    selected: bool,
    cursor: bool,
}

fn session_section(title: &'static str, count: usize, cx: &App) -> impl IntoElement {
    h_flex()
        .items_center()
        .justify_between()
        .px_2()
        .pt_2()
        .pb_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(div().font_semibold().child(title))
        .child(
            div()
                .px_2()
                .rounded_full()
                .bg(cx.theme().secondary)
                .child(count.to_string()),
        )
}

/// Keyboard-navigable agent chooser behind `New session…`.
/// Backed by the kit's `List`, so up/down move the selection, Enter confirms,
/// Esc closes, and the global default (Settings > Agent) starts selected
/// with a `Default` badge. Only enabled harnesses are offered; the list never
/// ends empty — an empty enable set falls back to all harnesses.
/// `ListDelegate` callbacks cannot borrow the workspace, so the delegate
/// holds a weak handle instead.
struct AgentPicker {
    agents: Vec<AgentKind>,
    default: AgentKind,
    selected: usize,
    workspace: WeakEntity<Workspace>,
    icons: Rc<crate::agent_icons::AgentIconTiles>,
}

impl ListDelegate for AgentPicker {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.agents.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let agent = self.agents.get(ix.row).copied()?;
        let icon = crate::agent_icons::agent_icon(agent, &self.icons, crate::agent_icons::ICON_PX);
        Some(
            ListItem::new(("new-session-agent", ix.row)).child(
                v_flex()
                    .w_full()
                    .items_start()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(icon)
                            .child(div().text_sm().font_semibold().child(agent.label()))
                            .when(agent == self.default, |row| {
                                row.child(
                                    div().text_xs().text_color(rgb(0x737878)).child("Default"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x737878))
                            .child(agent.description()),
                    ),
            ),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        if let Some(ix) = ix {
            self.selected = ix.row;
        }
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let agent = self
            .agents
            .get(self.selected)
            .copied()
            .or_else(|| self.agents.first().copied())
            .unwrap_or(AgentKind::DEFAULT);
        let workspace = self.workspace.clone();
        window.close_dialog(cx);
        workspace
            .update(cx, |this, cx| this.start_agent_session(agent, window, cx))
            .ok();
    }
}

impl Workspace {
    pub(super) fn new_review(cwd: &Path, cx: &mut Context<Self>) -> Entity<ReviewView> {
        let review = cx.new(|cx| ReviewView::new(cwd, cx));
        cx.subscribe(
            &review,
            |this, _, request: &crate::review::ReviewAgentRequested, cx| {
                if !request.assistant.read(cx).accepts_agent(request.request_id) {
                    return;
                }
                let pane =
                    this.create_agent_session(request.agent, &request.cwd, "Review tutorial", cx);
                request.assistant.update(cx, |assistant, cx| {
                    assistant.attach_agent(pane, request.request_id, cx);
                });
                this.refresh_sessions(cx);
            },
        )
        .detach();
        review
    }

    fn create_agent_session(
        &mut self,
        agent: AgentKind,
        cwd: &Path,
        title: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalPane> {
        let pane = cx
            .new(|cx| TerminalPane::new(WorkspaceTab::Agent, cwd, agent, &self.agent_activity, cx));
        if !pane.read(cx).launch_failed()
            && let Some(id) = pane.read(cx).launch_id()
        {
            self.open_sessions.insert(
                id,
                OpenAgentSession {
                    pane: pane.clone(),
                    checkout: cwd.to_owned(),
                    agent,
                    key: None,
                    title: title.into(),
                    created_at: crate::relative_time::current_unix_secs(),
                },
            );
        }
        pane
    }

    pub(super) fn remember_agent_pane(&mut self, cx: &App) {
        let Some(pane) = self.tabs[WorkspaceTab::Agent as usize].clone() else {
            return;
        };
        let Some(id) = pane.read(cx).launch_id() else {
            return;
        };
        self.open_sessions
            .entry(id)
            .or_insert_with(|| OpenAgentSession {
                pane,
                checkout: self.working_directory.clone(),
                agent: self.session_agent,
                key: None,
                title: "New session".into(),
                created_at: crate::relative_time::current_unix_secs(),
            });
    }

    pub(super) fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        if self.session_refreshing {
            return;
        }
        self.session_refreshing = true;
        let catalog = self.session_catalog.clone();
        let root = self.data_root.clone();
        cx.spawn(async move |this, cx| {
            let projects = cx
                .background_spawn(async move {
                    let mut projects = root
                        .as_ref()
                        .map(|r| recent_repositories(r, usize::MAX))
                        .unwrap_or_default();
                    for project in &mut projects {
                        project.checkout_path = checkout_identity(&project.checkout_path);
                    }
                    catalog.refresh();
                    projects
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.session_refreshing = false;
                this.session_projects = projects;
                this.publish_sessions(cx);
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn publish_sessions(&mut self, cx: &mut Context<Self>) {
        self.session_snapshot = self.session_catalog.snapshot();
        self.remember_agent_pane(cx);
        for open in self.open_sessions.values_mut() {
            if let Some(native) = open.pane.read(cx).native_session() {
                // The provider signal, not recency or title, establishes identity.
                if let Some(session) = self.session_snapshot.sessions.iter().find(|s| {
                    s.key.provider == open.agent.id()
                        && s.key.id == native
                        && s.checkout == open.checkout
                }) {
                    open.key = Some(session.key.clone());
                    open.title = session.title.clone();
                }
            }
        }
        // Fallback for harnesses without an identity signal (Codex has no
        // observer) or missed provider events: when native is still unknown,
        // adopt the single unclaimed catalog session for the same
        // provider+checkout updated after the pane existed. Requires exactly
        // one candidate for the pane and exactly one claimant globally, so
        // concurrent new sessions never cross-adopt and old history is never
        // mistaken for the new pane.
        let claimed: HashSet<SessionKey> = self
            .open_sessions
            .values()
            .filter_map(|open| open.key.clone())
            .collect();
        let mut candidates_by_open: HashMap<u64, Vec<SessionKey>> = HashMap::new();
        for (&id, open) in &self.open_sessions {
            if open.key.is_some() || open.pane.read(cx).native_session().is_some() {
                continue;
            }
            let threshold = open.created_at.saturating_sub(60);
            let candidates: Vec<SessionKey> = self
                .session_snapshot
                .sessions
                .iter()
                .filter(|session| {
                    session.key.provider == open.agent.id()
                        && session.checkout == open.checkout
                        && session.updated > 0
                        && session.updated >= threshold
                        && !claimed.contains(&session.key)
                })
                .map(|session| session.key.clone())
                .collect();
            candidates_by_open.insert(id, candidates);
        }
        let mut usage: HashMap<SessionKey, usize> = HashMap::new();
        for candidates in candidates_by_open.values() {
            for key in candidates {
                *usage.entry(key.clone()).or_default() += 1;
            }
        }
        for (&id, candidates) in &candidates_by_open {
            if candidates.len() == 1
                && usage.get(&candidates[0]) == Some(&1)
                && let Some(session) = self
                    .session_snapshot
                    .sessions
                    .iter()
                    .find(|session| session.key == candidates[0])
                && let Some(open) = self.open_sessions.get_mut(&id)
            {
                open.key = Some(session.key.clone());
                open.title = session.title.clone();
            }
        }
        let mut cards = Vec::new();
        for session in &self.session_snapshot.sessions {
            let Some(label) = self.session_repository_label(&session.checkout) else {
                continue;
            };
            // Registered repositories filter by the active space; an
            // unregistered checkout (the current or a retained ad-hoc one)
            // has no space to filter on and stays visible — the user opened
            // it explicitly.
            if self
                .session_space(&session.checkout)
                .is_some_and(|space| !space_eq(&space, &self.active_space))
            {
                continue;
            }
            cards.push((session.clone(), label));
            if cards.len() == HOME_LIMIT {
                break;
            }
        }
        self.home.update(cx, |view, cx| {
            view.set_sessions(
                cards,
                self.session_snapshot.errors.clone(),
                self.session_snapshot.loaded,
                cx,
            )
        });
        cx.notify();
    }

    /// Space of the registered repository whose checkout is `checkout`, when
    /// one exists. Unregistered checkouts return `None`.
    pub(super) fn session_space(&self, checkout: &Path) -> Option<String> {
        self.session_projects
            .iter()
            .chain(self.recent_repositories.iter())
            .find(|project| project.checkout_path == checkout)
            .map(|project| project.space.clone())
    }

    /// Display label for one session checkout: a registered repository's
    /// label when bound, otherwise the ad-hoc checkout's directory name while
    /// it is current or retained. The palette cache is consulted alongside
    /// the catalog's project list so session rows can label rows before the first
    /// catalog refresh republishes `session_projects`.
    pub(super) fn session_repository_label(&self, checkout: &Path) -> Option<String> {
        self.session_projects
            .iter()
            .chain(self.recent_repositories.iter())
            .find(|project| project.checkout_path == checkout)
            .map(|project| project.label().to_owned())
            .or_else(|| {
                (self.working_directory == checkout
                    || self.inactive_repositories.contains_key(checkout))
                .then(|| {
                    checkout
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            })
    }

    /// Whether a session list is currently on screen: Home's Recent
    /// Activity or the Agent sidebar. Gates the
    /// periodic catalog refresh and the relative-time repaint so they run
    /// exactly while their labels are visible.
    pub(super) fn sessions_visible(&self) -> bool {
        self.home_visible || self.active_tab == WorkspaceTab::Agent
    }

    pub(super) fn open_agent_session(
        &mut self,
        key: SessionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Location changes (including async completions racing an open HUD)
        // clear stale navigation state.
        self.close_navigation(cx);
        self.remember_agent_pane(cx);
        if let Some((&id, _)) = self
            .open_sessions
            .iter()
            .find(|(_, s)| s.key.as_ref() == Some(&key))
        {
            self.activate_open_session(id, window, cx);
            return;
        }
        self.session_navigation = self.session_navigation.wrapping_add(1);
        let generation = self.session_navigation;
        let root = self.data_root.clone();
        let catalog = self.session_catalog.clone();
        let mut open_checkouts: HashSet<PathBuf> =
            self.inactive_repositories.keys().cloned().collect();
        open_checkouts.insert(self.working_directory.clone());
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let session = catalog.prepare_open(&key)?;
                    let project = root.as_ref().and_then(|root| {
                        recent_repositories(root, usize::MAX)
                            .into_iter()
                            .find(|p| p.checkout_path == session.checkout)
                    });
                    anyhow::ensure!(
                        project.is_some() || open_checkouts.contains(&session.checkout),
                        "This checkout is no longer registered or open"
                    );
                    if let (Some(root), Some(project)) = (&root, &project) {
                        record_repository_open(root, &project.key)?;
                    }
                    Ok::<_, anyhow::Error>((session, project.map(|p| p.key)))
                })
                .await;
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if this.session_navigation != generation {
                        return;
                    }
                    match outcome {
                        Ok((session, repository)) => {
                            this.launch_recent_session(session, repository, window, cx)
                        }
                        Err(error) => {
                            window.push_notification(format!("Could not open session: {error}"), cx)
                        }
                    }
                })
            });
        })
        .detach();
    }

    fn launch_recent_session(
        &mut self,
        session: SessionSummary,
        repository: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((&id, _)) = self
            .open_sessions
            .iter()
            .find(|(_, s)| s.key.as_ref() == Some(&session.key))
        {
            self.activate_open_session(id, window, cx);
            return;
        }
        let Some(agent) = session.agent() else {
            return;
        };
        let pane = cx.new(|cx| {
            TerminalPane::with_session(
                WorkspaceTab::Agent,
                &session.cwd,
                agent,
                &self.agent_activity,
                Some(&session),
                cx,
            )
        });
        if pane.read(cx).launch_failed() {
            window.push_notification(
                "Could not launch the session terminal. Your previous session is still open.",
                cx,
            );
            return;
        }
        let Some(id) = pane.read(cx).launch_id() else {
            return;
        };
        self.open_sessions.insert(
            id,
            OpenAgentSession {
                pane,
                checkout: session.checkout.clone(),
                agent,
                key: Some(session.key),
                title: session.title,
                created_at: crate::relative_time::current_unix_secs(),
            },
        );
        self.activate_open_session(id, window, cx);
        self.current_repository = repository;
        self.refresh_sessions(cx);
    }

    pub(super) fn focus_attention_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.agent_activity.snapshot();
        let id = self
            .open_sessions
            .iter()
            .filter(|(_, session)| session.checkout == self.working_directory)
            .filter(|(id, _)| {
                snapshot
                    .for_launch(**id)
                    .is_some_and(|a| a.state == ActivityState::NeedsAttention)
            })
            .map(|(id, _)| *id)
            .min();
        if let Some(id) = id {
            self.activate_open_session(id, window, cx);
        }
    }

    fn activate_open_session(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.close_navigation(cx);
        self.remember_agent_pane(cx);
        let Some(open) = self.open_sessions.get(&id) else {
            return;
        };
        let (pane, checkout, agent) = (open.pane.clone(), open.checkout.clone(), open.agent);
        self.restore_repository_tabs(&checkout, cx);
        self.tabs[WorkspaceTab::Agent as usize] = Some(pane);
        self.session_agent = agent;
        self.active_tab = WorkspaceTab::Agent;
        self.current_repository = self
            .session_projects
            .iter()
            .find(|p| p.checkout_path == checkout)
            .map(|p| p.key.clone());
        self.project_name = checkout
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
            .into();
        self.resources.update(cx, |view, cx| {
            view.set_scope(
                crate::artifacts::Scope::Repository(self.current_repository.clone()),
                cx,
            )
        });
        self.enter_repository(window, cx);
        self.refresh_git_status(cx);
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Ask which harness a new session should use before spawning anything.
    /// Starting a session launches a process, so an explicit pick beats a
    /// mislaunch. The global default (Settings > Agent) starts selected;
    /// this choice applies to this session only, exactly like selecting a
    /// historical session. The picker is a real `List`, so arrow keys move
    /// the highlight and Enter confirms.
    pub(super) fn prompt_new_agent_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Opening a dialog clears stale navigation state; the dialog trap
        // owns the keyboard from here.
        self.close_navigation(cx);
        let workspace = cx.entity().downgrade();
        let project = self.project_name.clone();
        let icons = self.agent_icon_tiles.clone();
        let mut agents = self.enabled_agents.clone();
        if agents.is_empty() {
            agents = AgentKind::ALL.to_vec();
        }
        let default = if agents.contains(&self.default_agent) {
            self.default_agent
        } else {
            agents.first().copied().unwrap_or(AgentKind::DEFAULT)
        };
        let selected = agents
            .iter()
            .position(|agent| *agent == default)
            .unwrap_or(0);
        let list_state = cx.new(|cx| {
            ListState::new(
                AgentPicker {
                    agents,
                    default,
                    selected,
                    workspace,
                    icons,
                },
                window,
                cx,
            )
        });
        list_state.update(cx, |state, cx| {
            state.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });
        let dialog_list = list_state.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("New session")
                .w(px(460.))
                .child(
                    div()
                        .pb_3()
                        .text_sm()
                        .text_color(rgb(0x858989))
                        .child(format!("Choose the agent for a new session in {project}.")),
                )
                .child(div().w_full().h(px(288.)).child(List::new(&dialog_list)))
                .footer(
                    DialogFooter::new()
                        .child(
                            div()
                                .mr_auto()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("↑↓ Select · Enter Start"),
                        )
                        .child(
                            Button::new("cancel-new-session")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
        });
        // Opening the dialog focuses its own trap; hand focus to the list so
        // up/down and Enter work immediately without a click or Tab.
        list_state.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Launch a new session with the chosen harness in the current checkout.
    fn start_agent_session(
        &mut self,
        agent: AgentKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_agent_pane(cx);
        // An explicit New wins over an in-flight auto-resume: bump the
        // navigation so a stale open completion cannot replace this pane,
        // and mark the checkout started either way.
        self.session_navigation += 1;
        self.agent_autostart.insert(self.working_directory.clone());
        let pane =
            self.create_agent_session(agent, &self.working_directory.clone(), "New session", cx);
        if pane.read(cx).launch_failed() {
            window.push_notification("Could not create a session terminal", cx);
            return;
        }
        self.tabs[WorkspaceTab::Agent as usize] = Some(pane);
        self.session_agent = agent;
        // A new session can start from Home or any tab: reveal the Agent tab
        // and enter the checkout so the new pane is visible. The Agent tab is
        // already set, so `enter_repository` cannot auto-resume a second
        // session on the way in.
        self.active_tab = WorkspaceTab::Agent;
        self.enter_repository(window, cx);
        self.focus_active_pane(window, cx);
        self.refresh_sessions(cx);
        cx.notify();
    }

    fn close_agent_session(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let busy = self
            .agent_activity
            .snapshot()
            .for_launch(id)
            .is_some_and(|a| {
                matches!(
                    a.state,
                    ActivityState::Working
                        | ActivityState::NeedsAttention
                        | ActivityState::Starting
                        | ActivityState::Unavailable
                )
            });
        if busy {
            let workspace = cx.entity().downgrade();
            window.open_dialog(cx,move |dialog,_,_| {
                let workspace=workspace.clone();
                dialog.title("Close agent session?").child("This stops its terminal process. The saved conversation stays in the agent's history.")
                    .footer(DialogFooter::new()
                        .child(Button::new("cancel-close-session").label("Cancel").on_click(|_,window,cx|window.close_dialog(cx)))
                        .child(Button::new("confirm-close-session").primary().label("Close session").on_click(|_,window,cx|window.dispatch_action(Box::new(Confirm{secondary:false}),cx))))
                    .on_ok(move |_,window,cx| {let _=workspace.update(cx,|this,cx|this.remove_agent_session(id,window,cx));true})
            });
        } else {
            self.remove_agent_session(id, window, cx);
        }
    }
    fn remove_agent_session(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.open_sessions.remove(&id) {
            // Other views can retain the pane, so dropping the sidebar row is
            // insufficient to stop a terminal shared with a review assistant.
            session.pane.update(cx, |pane, cx| pane.close(cx));
        }
        let agent_index = WorkspaceTab::Agent as usize;
        let active = self.tabs[agent_index]
            .as_ref()
            .and_then(|p| p.read(cx).launch_id())
            == Some(id);
        if active {
            self.tabs[agent_index] = None;
        }
        for repository in self.inactive_repositories.values_mut() {
            if repository.tabs[agent_index]
                .as_ref()
                .and_then(|p| p.read(cx).launch_id())
                == Some(id)
            {
                repository.tabs[agent_index] = None;
            }
        }
        if active
            && let Some((&next, _)) = self
                .open_sessions
                .iter()
                .find(|(_, s)| s.checkout == self.working_directory)
        {
            self.activate_open_session(next, window, cx);
        }
        self.sync_activity_visibility(window, cx);
        cx.notify();
    }

    /// Display order for the sessions sidebar cursor, matching
    /// `render_agent_sessions`: open sessions for this checkout (newest
    /// first, excluding rows already shown as recent), then recent catalog
    /// sessions.
    pub(super) fn session_nav_order(&self) -> Vec<SessionNavTarget> {
        let recent = self
            .session_snapshot
            .recent(&self.working_directory, self.session_limit);
        let mut open: Vec<u64> = self
            .open_sessions
            .iter()
            .filter(|(_, s)| {
                s.checkout == self.working_directory
                    && !recent.iter().any(|r| s.key.as_ref() == Some(&r.key))
            })
            .map(|(&id, _)| id)
            .collect();
        open.sort_by_key(|id| std::cmp::Reverse(*id));
        let mut order = Vec::new();
        for id in open {
            order.push(SessionNavTarget::Open(id));
        }
        for session in &recent {
            order.push(SessionNavTarget::Recent(session.key.clone()));
        }
        order
    }

    /// Start the sidebar cursor at the active session (or the first row) when
    /// navigation mode opens on the Sessions pane.
    pub(super) fn init_session_cursor(
        &mut self,
        active: Option<u64>,
        active_key: Option<SessionKey>,
    ) {
        let order = self.session_nav_order();
        if order.is_empty() {
            self.session_cursor = None;
            return;
        }
        let position = active
            .and_then(|id| {
                order
                    .iter()
                    .position(|t| matches!(t, SessionNavTarget::Open(open) if *open == id))
            })
            .or_else(|| {
                active_key.as_ref().and_then(|key| {
                    order.iter().position(|t| match t {
                        SessionNavTarget::Open(id) => {
                            self.open_sessions.get(id).and_then(|s| s.key.as_ref()) == Some(key)
                        }
                        SessionNavTarget::Recent(recent) => recent == key,
                    })
                })
            })
            .unwrap_or(0);
        self.session_cursor = Some(position.min(order.len() - 1));
    }

    /// Move the sidebar cursor one step, clamping at the ends like pane
    /// movement. Keeps navigation mode open for repeats. A first press with
    /// no cursor lands at the nearest end instead of skipping it.
    pub(super) fn move_session_cursor(&mut self, down: bool) {
        let order = self.session_nav_order();
        if order.is_empty() {
            self.session_cursor = None;
            return;
        }
        let Some(current) = self.session_cursor else {
            self.session_cursor = Some(if down { 0 } else { order.len() - 1 });
            return;
        };
        let current = current.min(order.len() - 1);
        self.session_cursor = Some(crate::navigation::move_index(current, order.len(), down));
    }

    /// Open the cursor session for navigation-mode `Enter`. Returns true when
    /// something ran so the caller exits navigation mode.
    pub(super) fn activate_session_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let order = self.session_nav_order();
        let Some(cursor) = self.session_cursor else {
            return false;
        };
        match order.get(cursor) {
            Some(SessionNavTarget::Open(id)) => {
                let id = *id;
                if self.open_sessions.contains_key(&id) {
                    self.activate_open_session(id, window, cx);
                    return true;
                }
                false
            }
            Some(SessionNavTarget::Recent(key)) => {
                let key = key.clone();
                self.open_agent_session(key, window, cx);
                true
            }
            None => false,
        }
    }

    fn render_session_row(&self, row: SessionSidebarRow, cx: &mut Context<Self>) -> AnyElement {
        let accent = cx.theme().info;
        let muted = cx.theme().muted_foreground;
        let id = match &row.target {
            SessionNavTarget::Open(id) => format!("open-agent-{id}"),
            SessionNavTarget::Recent(key) => format!(
                "recent-agent-{}-{}-{}",
                key.provider,
                key.store.display(),
                key.id
            ),
        };
        let close_id = match &row.target {
            SessionNavTarget::Open(id) => format!("close-agent-{id}"),
            SessionNavTarget::Recent(_) => {
                format!("close-recent-{}", row.open_id.unwrap_or_default())
            }
        };
        let mut metadata = h_flex()
            .items_center()
            .gap_2()
            .w_full()
            .min_w_0()
            .text_xs()
            .text_color(muted)
            .child(div().min_w_0().truncate().child(row.provider));
        if let Some(status) = row.status {
            let color = match status {
                ActivityState::Starting | ActivityState::Working => accent,
                ActivityState::NeedsAttention => cx.theme().warning,
                ActivityState::Finished => cx.theme().success,
                _ => muted,
            };
            metadata = metadata.child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .items_center()
                    .text_color(color)
                    .child(div().flex_none().size(px(5.)).rounded_full().bg(color))
                    .child(div().truncate().child(status.label())),
            );
        }
        metadata = metadata.child(div().flex_1());
        if let Some(age) = row.age {
            metadata = metadata.child(div().flex_none().max_w(px(72.)).truncate().child(age));
        }
        let title = row.title;
        let mut card = h_flex()
            .relative()
            .items_center()
            .rounded_lg()
            .border_1()
            .border_color(if row.cursor {
                cx.theme().ring
            } else {
                gpui_kit::transparent_black()
            })
            .when(row.selected, |card| card.bg(accent.opacity(0.10)))
            .hover(|card| {
                card.bg(if row.selected {
                    accent.opacity(0.15)
                } else {
                    cx.theme().secondary.opacity(0.6)
                })
            })
            .child(
                Button::new(SharedString::from(id))
                    .ghost()
                    .small()
                    .flex_1()
                    .min_w_0()
                    .h_auto()
                    .px_2()
                    .py_2()
                    .tooltip(row.tooltip)
                    .accessibility_label(title.clone())
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(26.))
                                    .mt(px(2.))
                                    .rounded_md()
                                    .bg(cx.theme().secondary)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(row.icon),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_start()
                                    .gap_1()
                                    .child(
                                        div()
                                            .w_full()
                                            .text_sm()
                                            .truncate()
                                            .when(row.selected, |title| title.font_semibold())
                                            .child(title.clone()),
                                    )
                                    .child(metadata),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| match &row.target {
                        SessionNavTarget::Open(id) => this.activate_open_session(*id, window, cx),
                        SessionNavTarget::Recent(key) => {
                            this.open_agent_session(key.clone(), window, cx)
                        }
                    })),
            );
        if row.selected {
            card = card.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(10.))
                    .bottom(px(10.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(accent),
            );
        }
        if let Some(id) = row.open_id {
            card = card.child(
                Button::new(SharedString::from(close_id))
                    .ghost()
                    .xsmall()
                    .flex_none()
                    .mr_1()
                    .icon(IconName::Close)
                    .text_color(muted)
                    .tooltip("Close session")
                    .accessibility_label(format!("Close session: {title}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_agent_session(id, window, cx)
                    })),
            );
        }
        card.into_any_element()
    }

    pub(super) fn render_agent_sessions(&self, cx: &mut Context<Self>) -> AnyElement {
        let agent_index = WorkspaceTab::Agent as usize;
        let active = self.tabs[agent_index]
            .as_ref()
            .and_then(|p| p.read(cx).launch_id());
        let selected = active
            .and_then(|id| self.open_sessions.get(&id))
            .and_then(|s| s.key.as_ref());
        let activity = self.agent_activity.snapshot();
        let mut sidebar = v_flex()
            .track_focus(&self.agent_sidebar_focus)
            .focus(|style| style.border_1().border_color(cx.theme().ring))
            .h_full()
            .min_h_0()
            .flex_none()
            .w(px(280.))
            .border_r_1()
            .border_color(cx.theme().border)
            .p_2()
            .gap_2();
        let recent = self
            .session_snapshot
            .recent(&self.working_directory, self.session_limit);
        let mut list = v_flex()
            .id("agent-session-list")
            .flex_1()
            .min_h_0()
            .gap_1()
            .overflow_y_scrollbar();
        // Open sessions outside the recent limit remain reachable, including new unsaved sessions.
        let mut open: Vec<_> = self
            .open_sessions
            .iter()
            .filter(|(_, s)| {
                s.checkout == self.working_directory
                    && !recent.iter().any(|r| s.key.as_ref() == Some(&r.key))
            })
            .collect();
        open.sort_by_key(|(id, _)| std::cmp::Reverse(**id));
        if !open.is_empty() {
            list = list.child(session_section("Open sessions", open.len(), cx));
        }
        let open_count = open.len();
        for (open_pos, (&id, session)) in open.into_iter().enumerate() {
            let status = activity
                .for_launch(id)
                .map(|a| a.state)
                .unwrap_or(ActivityState::Unavailable);
            list = list.child(self.render_session_row(
                SessionSidebarRow {
                    target: SessionNavTarget::Open(id),
                    title: session.title.clone(),
                    provider: session.agent.label().into(),
                    age: None,
                    tooltip: format!(
                        "{}\n{} · {}",
                        session.title,
                        session.agent.label(),
                        status.label()
                    ),
                    icon: crate::agent_icons::agent_icon(
                        session.agent,
                        &self.agent_icon_tiles,
                        crate::agent_icons::ICON_PX,
                    ),
                    status: Some(status),
                    open_id: Some(id),
                    selected: active == Some(id),
                    cursor: self.navigation_open
                        && self.navigation_pane == 0
                        && self.session_cursor == Some(open_pos),
                },
                cx,
            ));
        }
        list = list.child(session_section("Recent sessions", recent.len(), cx));
        for (recent_pos, session) in recent.iter().enumerate() {
            let key = session.key.clone();
            let open = self
                .open_sessions
                .iter()
                .find(|(_, s)| s.key.as_ref() == Some(&key))
                .map(|(&id, _)| id);
            let status = open.and_then(|id| activity.for_launch(id)).map(|a| a.state);
            let mut tooltip = session.tooltip();
            if let Some(status) = status {
                tooltip.push_str(&format!("\n{}", status.label()));
            }
            list = list.child(self.render_session_row(
                SessionSidebarRow {
                    target: SessionNavTarget::Recent(key.clone()),
                    title: session.title.clone(),
                    provider: session.provider_label().into(),
                    age: Some(session.age()),
                    tooltip,
                    icon: crate::agent_icons::session_icon(
                        session.agent(),
                        &self.agent_icon_tiles,
                        crate::agent_icons::ICON_PX,
                    ),
                    status,
                    open_id: open,
                    selected: selected == Some(&key),
                    cursor: self.navigation_open
                        && self.navigation_pane == 0
                        && self.session_cursor == Some(open_count.saturating_add(recent_pos)),
                },
                cx,
            ));
        }
        if recent.is_empty() {
            list = if !self.session_snapshot.loaded {
                list.child(div().p_3().text_sm().child("Loading sessions…"))
            } else if self.session_snapshot.errors.is_empty() {
                list.child(
                    crate::empty_state::empty_state(
                        IconName::SquareTerminal,
                        "No recent sessions yet",
                        "Use New session below to start working with an agent.",
                    )
                    .p_3()
                    .flex_none(),
                )
            } else {
                list
            };
        }
        for error in &self.session_snapshot.errors {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(error.clone()),
            );
        }
        sidebar = sidebar.child(list);
        sidebar = sidebar.child(
            div()
                .border_t_1()
                .border_color(cx.theme().border)
                .pt_2()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("new-agent-session")
                                .small()
                                .flex_1()
                                .icon(IconName::Plus)
                                .label("New session")
                                .tooltip("Choose an agent and start a session")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.prompt_new_agent_session(window, cx)
                                })),
                        )
                        .child(
                            Button::new("refresh-agent-sessions")
                                .small()
                                .ghost()
                                .icon(IconName::RotateCw)
                                .tooltip("Refresh sessions")
                                .accessibility_label("Refresh sessions")
                                .loading(self.session_refreshing)
                                .on_click(cx.listener(|this, _, _, cx| this.refresh_sessions(cx))),
                        ),
                ),
        );
        let terminal = self.tabs[agent_index]
            .clone()
            .map(|p| p.into_any_element())
            .unwrap_or_else(|| {
                div()
                    .p_4()
                    .child("Select a session or start a new one.")
                    .into_any_element()
            });
        h_flex()
            .items_stretch()
            .size_full()
            .min_h_0()
            .child(sidebar)
            .child(div().flex_1().min_w_0().h_full().child(terminal))
            .into_any_element()
    }
}
