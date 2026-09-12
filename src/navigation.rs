//! Context policy for the two-step workspace navigator.
//!
//! This module intentionally has no GPUI dependency. Rendering and dispatch
//! turn the current workspace into a [`Context`], while the policy here keeps
//! the HUD rows and key resolution in lockstep and easy to exercise in tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Context {
    Home,
    Workspace,
    Artifacts,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResourceState {
    pub selected: bool,
    pub drafting: bool,
    pub saving: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Command {
    AddRepository,
    BrowseArtifacts,
    Agent,
    Editor,
    Terminal,
    Review,
    Resources,
    Home,
    Back,
    NewSession,
    EditMarkdown,
    AddComment,
    SaveDraft,
    CancelDraft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub key: char,
    pub label: &'static str,
    pub group: &'static str,
    pub command: Command,
}

const HOME: [Row; 2] = [
    Row {
        key: 'a',
        label: "Add repository",
        group: "Home",
        command: Command::AddRepository,
    },
    Row {
        key: 'r',
        label: "Browse artifacts",
        group: "Home",
        command: Command::BrowseArtifacts,
    },
];

const WORKSPACE: [Row; 7] = [
    Row {
        key: 'a',
        label: "Agent",
        group: "Navigate",
        command: Command::Agent,
    },
    Row {
        key: 'e',
        label: "Editor",
        group: "Navigate",
        command: Command::Editor,
    },
    Row {
        key: 't',
        label: "Terminal",
        group: "Navigate",
        command: Command::Terminal,
    },
    Row {
        key: 'd',
        label: "Review",
        group: "Navigate",
        command: Command::Review,
    },
    Row {
        key: 'r',
        label: "Resources",
        group: "Navigate",
        command: Command::Resources,
    },
    Row {
        key: 'g',
        label: "Home",
        group: "Navigate",
        command: Command::Home,
    },
    Row {
        key: 'n',
        label: "New agent session",
        group: "Workspace",
        command: Command::NewSession,
    },
];

const ARTIFACTS: [Row; 2] = [
    Row {
        key: 'g',
        label: "Home",
        group: "Navigate",
        command: Command::Home,
    },
    Row {
        key: 'b',
        label: "Back",
        group: "Navigate",
        command: Command::Back,
    },
];

const RESOURCE_EDIT: [Row; 2] = [
    Row {
        key: 'm',
        label: "Edit Markdown",
        group: "Resource",
        command: Command::EditMarkdown,
    },
    Row {
        key: 'c',
        label: "Add comment",
        group: "Resource",
        command: Command::AddComment,
    },
];

const RESOURCE_DRAFT: [Row; 2] = [
    Row {
        key: 'w',
        label: "Save draft",
        group: "Resource",
        command: Command::SaveDraft,
    },
    Row {
        key: 'q',
        label: "Cancel draft",
        group: "Resource",
        command: Command::CancelDraft,
    },
];

pub(crate) fn rows(context: Context, resource: ResourceState) -> Vec<Row> {
    let mut result = match context {
        Context::Home => HOME.to_vec(),
        Context::Workspace => WORKSPACE.to_vec(),
        Context::Artifacts => ARTIFACTS.to_vec(),
    };
    if resource.selected && !resource.saving {
        result.extend(if resource.drafting {
            RESOURCE_DRAFT
        } else {
            RESOURCE_EDIT
        });
    }
    debug_assert!(unique_keys(&result));
    result
}

pub(crate) fn resolve(context: Context, resource: ResourceState, key: char) -> Option<Command> {
    rows(context, resource)
        .into_iter()
        .find(|row| row.key == key.to_ascii_lowercase())
        .map(|row| row.command)
}

pub(crate) fn unique_keys(rows: &[Row]) -> bool {
    let mut keys = std::collections::HashSet::new();
    rows.iter().all(|row| keys.insert(row.key))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    Trigger,
    Escape,
    Enter,
    Left,
    Right,
    Key(char),
    Modified,
    /// Raw held-key events are normalized by the platform before the
    /// pre-keymap interceptor sees them; retain this policy case for callers
    /// that do have raw events and for regression coverage.
    #[allow(dead_code)]
    Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Open,
    Close,
    FocusLeft,
    FocusRight,
    AcceptFocus,
    Execute(Command),
    Consume,
    Ignore,
}

pub(crate) fn decide(
    open: bool,
    input: Input,
    context: Context,
    resource: ResourceState,
) -> Decision {
    if !open {
        return if input == Input::Trigger {
            Decision::Open
        } else {
            Decision::Ignore
        };
    }
    match input {
        Input::Trigger | Input::Escape => Decision::Close,
        Input::Enter => Decision::AcceptFocus,
        Input::Left => Decision::FocusLeft,
        Input::Right => Decision::FocusRight,
        Input::Key(key) => {
            resolve(context, resource, key).map_or(Decision::Consume, Decision::Execute)
        }
        Input::Modified | Input::Repeat => Decision::Consume,
    }
}

pub(crate) fn move_index(index: usize, count: usize, right: bool) -> usize {
    if count == 0 {
        return 0;
    }
    let index = index.min(count - 1);
    if right {
        (index + 1).min(count - 1)
    } else {
        index.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_offer_only_their_actions() {
        assert_eq!(
            resolve(Context::Home, ResourceState::default(), 'a'),
            Some(Command::AddRepository)
        );
        assert_eq!(resolve(Context::Home, ResourceState::default(), 'e'), None);
        assert_eq!(
            resolve(Context::Workspace, ResourceState::default(), 'e'),
            Some(Command::Editor)
        );
        assert_eq!(
            resolve(Context::Artifacts, ResourceState::default(), 'b'),
            Some(Command::Back)
        );
    }

    #[test]
    fn shared_palette_keys_are_gone_everywhere() {
        // The Common section was removed: palettes stay reachable through
        // their direct shortcuts, so p/s/o must offer nothing in any context.
        for context in [Context::Home, Context::Workspace, Context::Artifacts] {
            for key in ['p', 's', 'o'] {
                assert_eq!(resolve(context, ResourceState::default(), key), None);
            }
            assert!(
                rows(context, ResourceState::default())
                    .iter()
                    .all(|row| row.group != "Common")
            );
        }
    }

    #[test]
    fn every_available_context_has_unique_keys() {
        for context in [Context::Home, Context::Workspace, Context::Artifacts] {
            for resource in [
                ResourceState::default(),
                ResourceState {
                    selected: true,
                    drafting: false,
                    saving: false,
                },
                ResourceState {
                    selected: true,
                    drafting: true,
                    saving: false,
                },
                ResourceState {
                    selected: true,
                    drafting: true,
                    saving: true,
                },
            ] {
                let available = rows(context, resource);
                assert!(unique_keys(&available));
                for row in available {
                    assert_eq!(resolve(context, resource, row.key), Some(row.command));
                }
            }
        }
    }

    #[test]
    fn resource_rows_follow_draft_and_save_state() {
        let selected = ResourceState {
            selected: true,
            ..Default::default()
        };
        assert_eq!(
            resolve(Context::Workspace, selected, 'm'),
            Some(Command::EditMarkdown)
        );
        assert_eq!(resolve(Context::Workspace, selected, 'w'), None);
        let draft = ResourceState {
            drafting: true,
            ..selected
        };
        assert_eq!(
            resolve(Context::Workspace, draft, 'w'),
            Some(Command::SaveDraft)
        );
        assert_eq!(resolve(Context::Workspace, draft, 'm'), None);
        let saving = ResourceState {
            saving: true,
            ..draft
        };
        assert_eq!(resolve(Context::Workspace, saving, 'w'), None);
    }

    #[test]
    fn navigation_inputs_are_consumed_while_open() {
        assert_eq!(
            decide(
                false,
                Input::Key('j'),
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Ignore
        );
        assert_eq!(
            decide(
                true,
                Input::Escape,
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Close
        );
        assert_eq!(
            decide(
                true,
                Input::Trigger,
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Close
        );
        assert_eq!(
            decide(
                true,
                Input::Enter,
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::AcceptFocus
        );
        assert_eq!(
            decide(
                true,
                Input::Key('x'),
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Consume
        );
        assert_eq!(
            decide(
                true,
                Input::Modified,
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Consume
        );
        assert_eq!(
            decide(
                true,
                Input::Repeat,
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Consume
        );
        assert_eq!(
            decide(
                true,
                Input::Key('e'),
                Context::Workspace,
                ResourceState::default()
            ),
            Decision::Execute(Command::Editor)
        );
    }

    #[test]
    fn pane_motion_clamps() {
        assert_eq!(move_index(0, 3, false), 0);
        assert_eq!(move_index(1, 3, true), 2);
        assert_eq!(move_index(2, 3, true), 2);
        assert_eq!(move_index(0, 0, true), 0);
        assert_eq!(move_index(3, 2, false), 0);
    }

    gpui_kit::actions!(navigation_test, [LeakingAction]);

    struct InterceptorHarness {
        focus: gpui_kit::FocusHandle,
        action_count: usize,
        navigation_open: bool,
        interceptor: Option<gpui_kit::Subscription>,
    }

    impl gpui_kit::Render for InterceptorHarness {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{InteractiveElement as _, Styled as _};
            gpui_kit::div()
                .size_full()
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &LeakingAction, _, _| {
                    this.action_count += 1;
                }))
        }
    }

    #[gpui_kit::test]
    fn interceptor_stops_bound_actions_before_dispatch(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::KeyBinding;

        let (view, cx) = cx.add_window_view(|_, cx| InterceptorHarness {
            focus: cx.focus_handle(),
            action_count: 0,
            navigation_open: true,
            interceptor: None,
        });
        cx.update(|window, app| {
            view.update(app, |harness, cx| {
                harness.focus.focus(window, cx);
                let weak = cx.entity().downgrade();
                harness.interceptor = Some(cx.intercept_keystrokes(move |event, _, cx| {
                    let _ = weak.update(cx, |harness, cx| {
                        if harness.navigation_open && event.keystroke.key == "a" {
                            cx.stop_propagation();
                        }
                    });
                }));
            });
        });
        cx.update(|_, app| app.bind_keys([KeyBinding::new("a", LeakingAction, None)]));
        cx.simulate_keystrokes("a");
        view.update(cx, |harness, _| assert_eq!(harness.action_count, 0));
    }
}
