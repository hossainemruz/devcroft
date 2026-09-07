//! Home dashboard and deliberately lightweight destination placeholders.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{Confirm, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AppContext as _, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window, div, point, px,
};
use std::{collections::HashMap, path::PathBuf, time::Duration};

fn item_id(prefix: &str, id: &str) -> SharedString {
    format!("{prefix}-{id}").into()
}

use crate::data::dashboard::{Category, Dashboard, Item, Kind, safe_web_url};
use crate::data::{DataRoot, RecentRepository, SyncStatus, SyncTracker, recent_repositories};
use crate::git_status::{GitStatus, load_git_status};

const PROJECT_GIT_INTERVAL: Duration = Duration::from_secs(5);

/// Cache keys include the checkout path, not just the portable repository key.
/// A reload invalidates in-flight results when bindings or recent projects change.
#[derive(Default)]
struct ProjectGitCache {
    generation: u64,
    statuses: HashMap<PathBuf, GitStatus>,
}

impl ProjectGitCache {
    fn invalidate(&mut self, paths: &[PathBuf]) {
        self.generation = self.generation.wrapping_add(1);
        self.statuses.retain(|path, _| paths.contains(path));
    }

    fn commit(&mut self, generation: u64, statuses: HashMap<PathBuf, GitStatus>) -> bool {
        if generation != self.generation || self.statuses == statuses {
            return false;
        }
        self.statuses = statuses;
        true
    }
}

fn project_branch_label(status: Option<&GitStatus>) -> String {
    let Some(status) = status else {
        return "Loading Git status…".into();
    };
    let Some(branch) = &status.branch else {
        return "Git status unavailable".into();
    };
    if status.detached {
        format!("Detached · {branch}")
    } else {
        format!("⎇ {branch}")
    }
}

fn project_working_tree_label(status: Option<&GitStatus>) -> Option<String> {
    let status = status?;
    status.branch.as_ref()?;
    let mut parts = vec![if status.dirty { "Modified" } else { "Clean" }.to_owned()];
    parts.extend(status.ahead_label());
    parts.extend(status.behind_label());
    Some(parts.join(" · "))
}

fn project_git_label(status: Option<&GitStatus>) -> String {
    let branch = project_branch_label(status);
    match project_working_tree_label(status) {
        Some(detail) => format!("{branch} · {detail}"),
        None => branch,
    }
}

pub(crate) enum HomeEvent {
    OpenRepository { key: String, label: String },
    AddRepository,
}

pub(crate) struct HomeView {
    pub(crate) focus_handle: FocusHandle,
    root: Option<DataRoot>,
    tracker: SyncTracker,
    data: Dashboard,
    error: Option<String>,
    projects: Vec<RecentRepository>,
    page: Option<&'static str>,
    show_completed: bool,
    active: bool,
    project_git: ProjectGitCache,
    git_loading: bool,
    project_focus: HashMap<String, FocusHandle>,
    scroll: ScrollHandle,
}

impl EventEmitter<HomeEvent> for HomeView {}

#[derive(Clone)]
struct DragTodo {
    id: String,
    title: String,
}
impl Render for DragTodo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .child(self.title.clone())
    }
}

impl HomeView {
    pub(crate) fn new(
        root: Option<DataRoot>,
        tracker: SyncTracker,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            root,
            tracker,
            data: Dashboard::default(),
            error: None,
            projects: Vec::new(),
            page: None,
            show_completed: false,
            active: true,
            project_git: ProjectGitCache::default(),
            git_loading: false,
            project_focus: HashMap::new(),
            scroll: ScrollHandle::new(),
        };
        view.reload(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROJECT_GIT_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.refresh_project_git(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        view
    }

    pub(crate) fn activate(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.page = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.reload(cx);
    }

    pub(crate) fn deactivate(&mut self) {
        self.active = false;
    }

    fn refresh_project_git(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.page.is_some() || self.git_loading || self.projects.is_empty() {
            return;
        }
        self.git_loading = true;
        let generation = self.project_git.generation;
        let paths: Vec<_> = self
            .projects
            .iter()
            .map(|project| project.checkout_path.clone())
            .collect();
        cx.spawn(async move |this, cx| {
            let statuses = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| {
                            let status = load_git_status(&path);
                            (path, status)
                        })
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_loading = false;
                if this.project_git.commit(generation, statuses) {
                    cx.notify();
                }
                if generation != this.project_git.generation {
                    this.refresh_project_git(cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = &self.root {
            self.projects = recent_repositories(root, 4);
            match Dashboard::load(root) {
                Ok(data) => {
                    self.data = data;
                    self.error = None;
                }
                Err(error) => {
                    self.data = Dashboard::default();
                    self.error = Some(format!("Could not load Home: {error:#}"));
                }
            }
        } else {
            self.error = Some("Portable data is unavailable".into());
        }
        self.project_focus
            .retain(|key, _| self.projects.iter().any(|project| &project.key == key));
        for project in &self.projects {
            self.project_focus
                .entry(project.key.clone())
                .or_insert_with(|| cx.focus_handle());
        }
        self.project_git.invalidate(
            &self
                .projects
                .iter()
                .map(|project| project.checkout_path.clone())
                .collect::<Vec<_>>(),
        );
        self.refresh_project_git(cx);
        cx.notify();
    }

    fn change(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut Dashboard) -> anyhow::Result<()>,
    ) -> bool {
        let result = (|| {
            anyhow::ensure!(
                self.tracker.status() != SyncStatus::Syncing,
                "Wait for portable sync to finish before editing Home"
            );
            anyhow::ensure!(
                self.error.is_none(),
                "Reload Home successfully before editing"
            );
            let root = self
                .root
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Portable data is unavailable"))?;
            let mut next = self.data.clone();
            edit(&mut next)?;
            next.save(root, &self.data)?;
            self.data = next;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                cx.notify();
                true
            }
            Err(error) => {
                window.push_notification(format!("Could not save: {error:#}"), cx);
                false
            }
        }
    }

    fn editor(&self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx).placeholder(placeholder);
                    state.set_value(value.to_owned(), window, cx);
                    state
                })
            };
        let title = input(&item.title, "Title", window, cx);
        let description = input(&item.description, "Description (optional)", window, cx);
        let label = input(&item.label, "Label (optional)", window, cx);
        let url = input(&item.url, "https://…", window, cx);
        let home = cx.entity().downgrade();
        let category = std::rc::Rc::new(std::cell::Cell::new(item.category));
        let original = self.data.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let mut form = v_flex().gap_3().child("Title").child(Input::new(&title));
            if item.kind == Kind::Todo {
                form = form.child("Description").child(Input::new(&description)).child("Label").child(Input::new(&label));
            } else {
                form = form.child("URL").child(Input::new(&url));
            }
            if item.kind == Kind::PullRequest {
                form = form.child("Category").child(h_flex().gap_2().flex_wrap().children(Category::ALL.into_iter().enumerate().map(|(index, choice)| {
                    let category = category.clone();
                    Button::new(("category", index)).label(if category.get() == choice { format!("✓ {}", choice.label()) } else { choice.label().to_owned() })
                        .on_click(move |_, _, cx| { category.set(choice); cx.refresh_windows(); })
                }))).child("GitHub status fetching is not implemented yet.");
            }
            let (title, description, label, url, home, category, item, original) = (title.clone(), description.clone(), label.clone(), url.clone(), home.clone(), category.clone(), item.clone(), original.clone());
            dialog.title("Edit Home item").w(px(560.)).child(form)
                .footer(DialogFooter::new()
                    .child(Button::new("cancel-home-item").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("save-home-item").primary().label("Save").on_click(|_, window, cx| window.dispatch_action(Box::new(Confirm { secondary: false }), cx))))
                .on_ok(move |_, window, cx| {
                let mut item = item.clone();
                item.title = title.read(cx).value().to_string();
                item.description = description.read(cx).value().to_string();
                item.label = label.read(cx).value().to_string();
                item.url = url.read(cx).value().to_string();
                item.category = category.get();
                home.update(cx, |this, cx| this.change(window, cx, |data| {
                    anyhow::ensure!(*data == original, "Home changed while this form was open. Close it and reopen the item.");
                    data.upsert(item)
                })).unwrap_or(false)
            })
        });
    }

    fn heading(
        &self,
        title: &'static str,
        destination: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .gap_2()
            .child(div().text_lg().font_semibold().child(title))
            .child(
                Button::new(item_id("view-all", title))
                    .ghost()
                    .label("View all →")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = Some(destination);
                        this.scroll.set_offset(point(px(0.), px(0.)));
                        cx.notify();
                    })),
            )
    }

    fn todo_card(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = v_flex()
            .id(item_id("home-item", &item.id))
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border);
        if !item.completed {
            let drag = DragTodo {
                id: item.id.clone(),
                title: item.title.clone(),
            };
            row = row.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()));
            let target = item.id.clone();
            row = row.on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                this.change(window, cx, |data| {
                    data.move_todo(&drag.id, &target);
                    Ok(())
                });
            }));
        }
        let toggle_id = item.id.clone();
        let title = item.title.clone();
        let home = cx.entity().downgrade();
        let edit = item.clone();
        let delete_id = item.id.clone();
        let (move_up, move_down) = if item.completed {
            (None, None)
        } else {
            let pending: Vec<_> = self
                .data
                .items
                .iter()
                .filter(|i| i.kind == Kind::Todo && !i.completed)
                .collect();
            let index = pending.iter().position(|i| i.id == item.id).unwrap_or(0);
            (
                index
                    .checked_sub(1)
                    .and_then(|i| pending.get(i))
                    .map(|i| i.id.clone()),
                pending.get(index + 1).map(|i| i.id.clone()),
            )
        };
        let source_up = item.id.clone();
        let source_down = item.id.clone();
        row = row.child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    Checkbox::new(item_id("complete", &item.id))
                        .checked(item.completed)
                        .accessibility_label(title.clone())
                        .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                            this.change(window, cx, |data| {
                                if let Some(i) = data.items.iter_mut().find(|i| i.id == toggle_id) {
                                    i.completed = *checked;
                                }
                                Ok(())
                            });
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_medium()
                        .child(title.clone()),
                )
                .child(
                    Button::new(item_id("options", &item.id))
                        .ghost()
                        .label("⋯")
                        .accessibility_label(format!("Options for {title}"))
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            menu.item({
                                let home = home.clone();
                                let edit = edit.clone();
                                PopupMenuItem::new("Edit").on_click(move |_, window, cx| {
                                    let _ = home.update(cx, |this, cx| {
                                        this.editor(edit.clone(), window, cx);
                                    });
                                })
                            })
                            .item({
                                let home = home.clone();
                                let delete_id = delete_id.clone();
                                PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                                    let _ = home.update(cx, |this, cx| {
                                        this.change(window, cx, |data| {
                                            data.items.retain(|i| i.id != delete_id);
                                            Ok(())
                                        });
                                    });
                                })
                            })
                            .separator()
                            .item({
                                let home = home.clone();
                                let source = source_up.clone();
                                let target = move_up.clone();
                                PopupMenuItem::new("Move up")
                                    .disabled(target.is_none())
                                    .on_click(move |_, window, cx| {
                                        if let Some(target) = target.clone() {
                                            let _ = home.update(cx, |this, cx| {
                                                this.change(window, cx, |data| {
                                                    data.move_todo(&source, &target);
                                                    Ok(())
                                                });
                                            });
                                        }
                                    })
                            })
                            .item({
                                let home = home.clone();
                                let source = source_down.clone();
                                let target = move_down.clone();
                                PopupMenuItem::new("Move down")
                                    .disabled(target.is_none())
                                    .on_click(move |_, window, cx| {
                                        if let Some(target) = target.clone() {
                                            let _ = home.update(cx, |this, cx| {
                                                this.change(window, cx, |data| {
                                                    data.move_todo(&source, &target);
                                                    Ok(())
                                                });
                                            });
                                        }
                                    })
                            })
                        }),
                ),
        );
        if !item.description.is_empty() {
            row = row.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(item.description.clone()),
            );
        }
        if !item.label.is_empty() {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("#{}", item.label)),
            );
        }
        row
    }

    fn list(&self, kind: Kind, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, destination) = match kind {
            Kind::Todo => ("Todos", "Todos"),
            Kind::PullRequest => ("Pull Requests", "Pull Requests"),
            Kind::Reading => ("To Read", "To Read"),
        };
        let items: Vec<_> = self
            .data
            .items
            .iter()
            .filter(|i| i.kind == kind && (self.show_completed || !i.completed))
            .cloned()
            .collect();
        let mut panel = v_flex()
            .flex_1()
            .min_w(px(290.))
            .min_h(px(380.))
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(self.heading(title, destination, cx));
        if kind == Kind::Todo {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Next items to do · drag a card to reorder"),
            );
        }
        if kind == Kind::PullRequest {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Manually tracked · status not fetched"),
            );
        }
        if items.is_empty() {
            panel = panel.child(
                div()
                    .py_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("Nothing here yet. Add your first item below."),
            );
        }
        for category in Category::ALL {
            if kind == Kind::PullRequest && items.iter().any(|item| item.category == category) {
                panel = panel.child(div().text_sm().font_semibold().child(category.label()));
            }
            for item in items
                .iter()
                .filter(|i| kind != Kind::PullRequest || i.category == category)
            {
                if kind == Kind::Todo {
                    panel = panel.child(self.todo_card(item, cx));
                    continue;
                }
                let id = item.id.clone();
                let mut row = v_flex()
                    .id(item_id("home-item", &id))
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border);
                let mut top = h_flex().gap_2();
                if kind != Kind::PullRequest {
                    top = top.child(
                        Checkbox::new(item_id("complete", &id))
                            .checked(item.completed)
                            .label(if kind == Kind::Todo { "Done" } else { "Read" })
                            .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                                this.change(window, cx, |data| {
                                    if let Some(i) = data.items.iter_mut().find(|i| i.id == id) {
                                        i.completed = *checked;
                                    }
                                    Ok(())
                                });
                            })),
                    );
                }
                if kind == Kind::Todo && !item.completed {
                    let drag = DragTodo {
                        id: item.id.clone(),
                        title: item.title.clone(),
                    };
                    top = top.child(
                        div()
                            .id(item_id("drag", &item.id))
                            .cursor_pointer()
                            .child("⋮⋮")
                            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone())),
                    );
                    let target = item.id.clone();
                    row = row.on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                        this.change(window, cx, |data| {
                            data.move_todo(&drag.id, &target);
                            Ok(())
                        });
                    }));
                }
                row = row
                    .child(top)
                    .child(div().font_medium().child(item.title.clone()));
                if !item.description.is_empty() {
                    row = row.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(item.description.clone()),
                    );
                }
                if !item.label.is_empty() {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("#{}", item.label)),
                    );
                }
                let edit = item.clone();
                let delete_id = item.id.clone();
                let mut actions = h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(item_id("edit", &item.id))
                            .ghost()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editor(edit.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(item_id("delete", &item.id))
                            .ghost()
                            .label("Delete")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.change(window, cx, |data| {
                                    data.items.retain(|i| i.id != delete_id);
                                    Ok(())
                                });
                            })),
                    );
                if kind == Kind::Todo && !item.completed {
                    let pending: Vec<_> = self
                        .data
                        .items
                        .iter()
                        .filter(|i| i.kind == Kind::Todo && !i.completed)
                        .collect();
                    let index = pending.iter().position(|i| i.id == item.id).unwrap_or(0);
                    for (name, target) in [
                        ("Move up", index.checked_sub(1).and_then(|i| pending.get(i))),
                        ("Move down", pending.get(index + 1)),
                    ] {
                        let source = item.id.clone();
                        let target = target.map(|i| i.id.clone());
                        actions = actions.child(
                            Button::new(item_id(name, &source))
                                .ghost()
                                .label(name)
                                .disabled(target.is_none())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(target) = &target {
                                        this.change(window, cx, |data| {
                                            data.move_todo(&source, target);
                                            Ok(())
                                        });
                                    }
                                })),
                        );
                    }
                }
                if kind != Kind::Todo {
                    let url = item.url.clone();
                    actions = actions.child(
                        Button::new(item_id("open", &item.id))
                            .ghost()
                            .label("Open ↗")
                            .on_click(move |_, window, cx| {
                                if safe_web_url(&url) {
                                    cx.open_url(&url);
                                } else {
                                    window.push_notification("Invalid web URL", cx);
                                }
                            }),
                    );
                }
                panel = panel.child(row.child(actions));
            }
            if kind != Kind::PullRequest {
                break;
            }
        }
        panel.child(h_flex().mt_auto().pt_3().justify_end().child(
            Button::new(item_id("add", title)).label("+ Add").on_click(
                cx.listener(move |this, _, window, cx| this.editor(Item::new(kind), window, cx)),
            ),
        ))
    }
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card_width = recent_card_width(f32::from(window.viewport_size().width));
        let mut body = v_flex().w_full().max_w(px(1440.)).mx_auto().gap_6().p_6();
        if let Some(page) = self.page {
            body = body
                .child(
                    Button::new("back-home")
                        .self_start()
                        .ghost()
                        .label("← Home")
                        .on_click(cx.listener(|this, _, _, cx| this.activate(cx))),
                )
                .child(div().text_2xl().font_semibold().child(page))
                .child("Coming soon — this dedicated page is a placeholder.")
                .child(
                    "Use Home to manage your current items. More views and workflows will follow.",
                );
        } else {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Tab to navigate · Enter/Space to activate · Arrow keys between projects · Page Up/Down to scroll"),
            );
            if let Some(error) = &self.error {
                body = body.child(div().text_color(cx.theme().danger).child(error.clone()));
            }
            body = body.child(self.heading("Recent Projects", "Projects", cx));
            let mut projects = h_flex().gap_4().flex_wrap();
            for (index, project) in self.projects.iter().enumerate() {
                let key = project.key.clone();
                let label = project.display_name.clone().unwrap_or_else(|| key.clone());
                let open_label = label.clone();
                let git_status = self.project_git.statuses.get(&project.checkout_path);
                let status = project_git_label(git_status);
                let branch = project_branch_label(git_status);
                let working_tree = project_working_tree_label(git_status);
                projects = projects.child(
                    // gpui-kit Base Button supplies pointer, Enter/Space, tab
                    // traversal and accessibility semantics for the whole card.
                    gpui_kit::base::Button::new(item_id("project", &key))
                        .track_focus(&self.project_focus[&key])
                        .accessibility_label(format!("Open {label}. {status}"))
                        .cursor_pointer()
                        .flex_col()
                        .items_start()
                        .justify_start()
                        .flex_none()
                        .w(px(card_width))
                        .min_h(px(160.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .hover(|style| style.bg(cx.theme().secondary))
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
                            div()
                                .w_full()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .font_semibold()
                                .child(label),
                        )
                        .child(
                            div()
                                .w_full()
                                .text_sm()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(branch),
                        )
                        .when_some(working_tree, |this, detail| {
                            this.child(
                                div()
                                    .w_full()
                                    .text_sm()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(detail),
                            )
                        })
                        .child(
                            div()
                                .mt_auto()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Open workspace →"),
                        ),
                );
            }
            if self.projects.is_empty() {
                projects = projects.child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("No recent projects yet."),
                );
            }
            body = body.child(projects).child(
                Button::new("add-project-home")
                    .self_start()
                    .ghost()
                    .label("+ Add project")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HomeEvent::AddRepository))),
            );
            body = body.child(self.heading("Recent Tasks", "Tasks", cx)).child(
                h_flex().gap_4().flex_wrap().children((0..4).map(|index| {
                    v_flex()
                        .flex_none()
                        .w(px(card_width))
                        .min_h(px(140.))
                        .p_4()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(format!("Task placeholder {}", index + 1))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Task planning is coming later"),
                        )
                })),
            );
            body = body.child(
                Checkbox::new("show-completed")
                    .label("Show completed todos and read links")
                    .checked(self.show_completed)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.show_completed = *checked;
                        cx.notify();
                    })),
            );
            body = body.child(
                h_flex()
                    .items_stretch()
                    .gap_4()
                    .flex_wrap()
                    .child(self.list(Kind::PullRequest, cx))
                    .child(self.list(Kind::Todo, cx))
                    .child(self.list(Kind::Reading, cx)),
            );
        }
        div()
            .id("home")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let height = f32::from(this.scroll.bounds().size.height) * 0.85;
                let delta = match event.keystroke.key.as_str() {
                    "pageup" => height,
                    "pagedown" => -height,
                    _ => return,
                };
                let mut offset = this.scroll.offset();
                offset.y =
                    px((f32::from(offset.y) + delta)
                        .clamp(-f32::from(this.scroll.max_offset().y), 0.));
                this.scroll.set_offset(offset);
                window.prevent_default();
                cx.stop_propagation();
                cx.notify();
            }))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .vertical_scrollbar(&self.scroll)
            .child(body)
    }
}

/// Use the same grid for projects and tasks, regardless of how many projects
/// exist. The dashboard has 24px gutters and 16px gaps; spare columns stay empty.
fn recent_card_width(viewport_width: f32) -> f32 {
    let available = (viewport_width.min(1440.) - 48.).max(1.);
    let columns = recent_columns(viewport_width) as f32;
    (available - (columns - 1.) * 16.) / columns
}

fn recent_columns(viewport_width: f32) -> usize {
    let available = (viewport_width.min(1440.) - 48.).max(1.);
    ((available + 16.) / (280. + 16.)).floor().clamp(1., 4.) as usize
}

fn project_navigation(key: &str, index: usize, count: usize, columns: usize) -> Option<usize> {
    if count == 0 || index >= count || columns == 0 {
        return None;
    }
    Some(match key {
        "left" => index.saturating_sub(1),
        "right" => (index + 1).min(count - 1),
        "up" => index.checked_sub(columns).unwrap_or(index),
        "down" => {
            if index + columns < count {
                index + columns
            } else {
                index
            }
        }
        "home" => 0,
        "end" => count - 1,
        _ => return None,
    })
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn project_keys_follow_the_responsive_grid_without_escaping_it() {
        assert_eq!(project_navigation("right", 0, 4, 4), Some(1));
        assert_eq!(project_navigation("left", 0, 4, 4), Some(0));
        assert_eq!(project_navigation("right", 3, 4, 4), Some(3));
        assert_eq!(project_navigation("down", 0, 4, 2), Some(2));
        assert_eq!(project_navigation("up", 3, 4, 2), Some(1));
        assert_eq!(project_navigation("down", 1, 3, 2), Some(1));
        assert_eq!(project_navigation("down", 0, 4, 1), Some(1));
        assert_eq!(project_navigation("end", 0, 4, 2), Some(3));
        assert_eq!(project_navigation("home", 3, 4, 2), Some(0));
        for key in ["tab", "enter", "space", "escape", "pageup"] {
            assert_eq!(project_navigation(key, 0, 4, 2), None);
        }
        assert_eq!(project_navigation("right", 0, 0, 4), None);
    }

    #[test]
    fn stale_git_batches_cannot_repopulate_removed_checkouts() {
        let mut cache = ProjectGitCache::default();
        let first = PathBuf::from("first");
        let second = PathBuf::from("second");
        cache.invalidate(std::slice::from_ref(&first));
        let old_generation = cache.generation;
        let status = GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        };
        assert!(cache.commit(
            old_generation,
            HashMap::from([(first.clone(), status.clone())])
        ));
        cache.invalidate(std::slice::from_ref(&second));
        assert!(cache.statuses.is_empty());
        assert!(!cache.commit(old_generation, HashMap::from([(first, status.clone())])));
        assert!(cache.statuses.is_empty());
        assert!(cache.commit(cache.generation, HashMap::from([(second, status)])));
    }

    #[test]
    fn project_status_distinguishes_loading_missing_dirty_and_detached() {
        assert_eq!(project_git_label(None), "Loading Git status…");
        assert_eq!(
            project_git_label(Some(&GitStatus::default())),
            "Git status unavailable"
        );
        let mut status = GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        };
        assert_eq!(project_git_label(Some(&status)), "⎇ main · Clean");
        status.dirty = true;
        status.has_upstream = true;
        status.ahead = 2;
        status.behind = 1;
        assert_eq!(
            project_git_label(Some(&status)),
            "⎇ main · Modified · ↑2 · ↓1"
        );
        status.detached = true;
        status.branch = Some("abcdef1".into());
        assert!(project_git_label(Some(&status)).starts_with("Detached · abcdef1"));
    }

    #[test]
    fn wide_screens_do_not_stretch_recent_cards() {
        assert_eq!(recent_card_width(1440.), 336.);
        assert_eq!(recent_card_width(2560.), 336.);
        assert_eq!(recent_card_width(3840.), 336.);
    }

    #[test]
    fn recent_grid_fits_smaller_windows() {
        for (viewport, columns) in [(1200., 3.), (900., 2.), (600., 1.), (320., 1.)] {
            let occupied = recent_card_width(viewport) * columns + (columns - 1.) * 16.;
            assert!((occupied - (viewport - 48.)).abs() < 0.01);
        }
    }
}
