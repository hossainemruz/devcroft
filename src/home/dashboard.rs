//! Home dashboard presentation. Data and navigation remain owned by HomeView.
use super::*;
use gpui_kit::Hsla;
use gpui_kit::component::{Icon, IconName};

fn icon_tile(icon: IconName, color: Hsla, cx: &Context<HomeView>) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(32.))
        .rounded_lg()
        .bg(color.opacity(if cx.theme().is_dark() { 0.14 } else { 0.09 }))
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(px(16.)).text_color(color))
}

fn section_title(title: &'static str, count: usize, cx: &Context<HomeView>) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_2()
        .child(div().text_base().font_semibold().child(title))
        .child(
            div()
                .min_w(px(22.))
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .bg(cx.theme().secondary)
                .text_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(count.to_string()),
        )
}

pub(super) fn panel_title(
    title: &'static str,
    count: usize,
    icon: IconName,
    color: Hsla,
    cx: &Context<HomeView>,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(icon_tile(icon, color, cx))
        .child(section_title(title, count, cx))
}

fn empty_state(
    icon: IconName,
    title: &'static str,
    description: &'static str,
    cx: &Context<HomeView>,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_4()
        .p_5()
        .rounded_xl()
        .border_1()
        .border_color(cx.theme().border.opacity(0.7))
        .bg(cx.theme().secondary.opacity(0.22))
        .child(icon_tile(icon, cx.theme().muted_foreground, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_sm().font_medium().child(title))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
}

impl HomeView {
    pub(super) fn dashboard(&self, card_width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .flex_1()
            .gap_6()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .flex_wrap()
                    .gap_4()
                    .pt_2()
                    .pb_2()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_2xl().font_semibold().child("Your workspace"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Recent conversations, active projects, and what’s next.",
                                    ),
                            ),
                    )
                    .child(
                        Button::new("add-project-home")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add project")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeEvent::AddRepository))),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .p_3()
                        .rounded_lg()
                        .bg(cx.theme().danger.opacity(0.08))
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(self.activity_section(card_width, cx))
            .child(self.project_section(card_width, cx))
            .child(
                v_flex()
                    .flex_1()
                    .gap_3()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .flex_wrap()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::Inbox)
                                            .size(px(16.))
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(div().text_base().font_semibold().child("Inbox")),
                            )
                            .child(
                                Checkbox::new("show-completed")
                                    .label("Show completed")
                                    .checked(self.show_completed)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.show_completed = *checked;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_grow(1.)
                            .flex_shrink_0()
                            .items_stretch()
                            .gap_4()
                            .flex_wrap()
                            .child(self.list(Kind::PullRequest, cx))
                            .child(self.list(Kind::Todo, cx))
                            .child(self.list(Kind::Reading, cx)),
                    ),
            )
    }

    fn activity_section(&self, card_width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sessions = h_flex().gap_4().flex_wrap();
        for (index, (session, repository)) in self.sessions.iter().enumerate() {
            let key = session.key.clone();
            let id: SharedString = format!(
                "home-session-{}-{}-{}",
                key.provider,
                key.store.display(),
                key.id
            )
            .into();
            let cursor = self.navigation_cursor == Some(index);
            sessions = sessions.child(
                Button::new(id)
                    .ghost()
                    .w(px(card_width))
                    .h_auto()
                    .min_h(px(136.))
                    .p_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(if cursor {
                        cx.theme().ring
                    } else {
                        cx.theme().border.opacity(0.7)
                    })
                    .bg(cx.theme().secondary.opacity(0.22))
                    .tooltip(session.tooltip())
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .items_start()
                            .gap_3()
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .min_w_0()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .size(px(28.))
                                                    .rounded_md()
                                                    .bg(cx.theme().background)
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(crate::agent_icons::session_icon(
                                                        session.agent(),
                                                        &self.agent_icon_tiles,
                                                        crate::agent_icons::ICON_PX,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(session.provider_label().to_owned()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(session.age()),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .font_semibold()
                                    .child(session.title.clone()),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(Icon::new(IconName::Folder).size(px(13.)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .child(repository.clone()),
                                    )
                                    .child(Icon::new(IconName::ArrowRight).size(px(14.))),
                            ),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(HomeEvent::OpenAgentSession(key.clone()))
                    })),
            );
        }
        if self.sessions.is_empty() {
            sessions = sessions.child(empty_state(
                IconName::Bot,
                if self.sessions_loaded {
                    "Ready when you are"
                } else {
                    "Finding your recent sessions…"
                },
                if self.sessions_loaded {
                    "Start an agent in a project. Your recent conversations will appear here."
                } else {
                    "Your conversations will appear here in a moment."
                },
                cx,
            ));
        }
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(section_title("Recent activity", self.sessions.len(), cx))
                    .child(
                        Button::new("refresh-home-sessions")
                            .ghost()
                            .small()
                            .icon(IconName::RotateCw)
                            .label("Refresh")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(HomeEvent::RefreshSessions)),
                            ),
                    ),
            )
            .child(sessions)
            .children(self.session_errors.iter().map(|error| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(error.clone())
            }))
    }

    fn project_section(&self, card_width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let mut projects = h_flex().gap_4().flex_wrap();
        let now_secs = current_unix_secs();
        for (index, project) in self.projects.iter().enumerate() {
            let key = project.key.clone();
            let label = project.display_name.clone().unwrap_or_else(|| key.clone());
            let open_label = label.clone();
            let git_status = self.project_git.statuses.get(&project.checkout_path);
            let status = project_git_label(git_status);
            let group = project
                .group
                .clone()
                .filter(|value| !value.trim().is_empty());
            let description = project
                .description
                .clone()
                .filter(|value| !value.trim().is_empty());
            let state = project_state_tag(git_status);
            let pending = match git_status {
                None => Some("Loading Git status…"),
                Some(status) if status.branch.is_none() => Some("Git status unavailable"),
                _ => None,
            };
            let branch_pill = project_branch_pill(git_status);
            let sync = project_sync_label(git_status);
            let opened = project_opened_label(project.last_opened_at.as_deref(), now_secs)
                .unwrap_or_else(|| "Not opened yet".to_owned());
            let accessible = format!("Open {label}. {status}. {opened}");
            let cursor = self.navigation_cursor == Some(self.sessions.len().saturating_add(index));
            projects = projects.child(
                // gpui-kit Base Button supplies pointer, Enter/Space, tab
                // traversal and accessibility semantics for the whole card.
                gpui_kit::base::Button::new(item_id("project", &key))
                    .track_focus(&self.project_focus[&key])
                    .accessibility_label(accessible)
                    .cursor_pointer()
                    .flex_col()
                    .items_start()
                    .justify_start()
                    .flex_none()
                    .w(px(card_width))
                    .min_h(px(164.))
                    .gap_2()
                    .p_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(if cursor {
                        cx.theme().ring
                    } else {
                        cx.theme().border.opacity(0.7)
                    })
                    .bg(cx.theme().secondary.opacity(0.22))
                    .hover(|style| {
                        style
                            .bg(cx.theme().secondary.opacity(0.65))
                            .border_color(cx.theme().info.opacity(0.45))
                    })
                    .focus(|style| style.border_color(cx.theme().ring).bg(cx.theme().secondary))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(HomeEvent::OpenRepository {
                            key: key.clone(),
                            label: open_label.clone(),
                        });
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        let columns = recent_columns(f32::from(window.viewport_size().width));
                        if let Some(next) = project_navigation(
                            &event.keystroke.key,
                            index,
                            this.projects.len(),
                            columns,
                        ) {
                            this.project_focus[&this.projects[next].key].focus(window, cx);
                            cx.stop_propagation();
                            window.prevent_default();
                            cx.notify();
                        }
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(icon_tile(IconName::Folder, cx.theme().info, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .font_semibold()
                                    .child(label),
                            )
                            .when_some(group, |this, tag| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .max_w(px(96.))
                                        .truncate()
                                        .px_2()
                                        .rounded_full()
                                        .bg(cx.theme().secondary)
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(tag),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .flex_wrap()
                            .when_some(state, |row, (label, hue)| {
                                row.child(
                                    Tag::color(hue)
                                        .with_size(Size::Small)
                                        .rounded_full()
                                        .flex_none()
                                        .child(label),
                                )
                            })
                            .when_some(pending, |row, text| {
                                row.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(text),
                                )
                            })
                            .when_some(branch_pill, |this, pill| {
                                this.child(
                                    Tag::secondary()
                                        .with_size(Size::Small)
                                        .rounded_full()
                                        .min_w_0()
                                        .max_w_full()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(pill),
                                        ),
                                )
                            })
                            .when_some(sync, |this, counts| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(counts),
                                )
                            }),
                    )
                    .when_some(description, |this, text| {
                        this.child(
                            div()
                                .w_full()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(text),
                        )
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .mt_auto()
                            .pt_3()
                            .border_t_1()
                            .border_color(cx.theme().border.opacity(0.5))
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(opened),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Open →"),
                            ),
                    ),
            );
        }
        if self.projects.is_empty() {
            projects = projects.child(empty_state(
                IconName::FolderOpen,
                "Your next project starts here",
                "Add a repository to keep its code, agents, and reviews together.",
                cx,
            ));
        }
        let dangling = self
            .all_projects
            .iter()
            .filter(|entry| !entry.is_linked())
            .count();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(section_title("Recent projects", self.projects.len(), cx))
                    .child(self.view_all("Projects", cx)),
            )
            .child(projects)
            .when(dangling > 0, |section| {
                section.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{dangling} {} not linked to this device. Open View all to connect {}.",
                            if dangling == 1 {
                                "repository is"
                            } else {
                                "repositories are"
                            },
                            if dangling == 1 { "it" } else { "them" },
                        )),
                )
            })
    }
}
