//! The Add Repository dialog: pick a local git checkout with the native
//! directory picker, review the harvested metadata, and save a portable
//! record plus device binding.
//!
//! The view is created fresh for every dialog opening (unlike Settings,
//! whose edits survive reopenings): a half-filled add form going stale
//! behind the dialog would be worse than re-harvesting. `Browse` fills only
//! empty fields from the checkout — explicit edits are never overwritten,
//! so clearing a field and re-browsing re-harvests just that field.
//!
//! Why a dialog plus the OS picker instead of the sheet file-browser
//! example from gpui-kit docs: the dialog layer is already rendered by
//! [`Workspace`](crate::workspace::Workspace) while the sheet layer is not,
//! the docs' "file browser" is a layout example (a custom list, not a
//! picker component), and the native picker (`prompt_for_paths`) matches
//! the Electron `showOpenDialog` behavior including permissions and
//! accessibility.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, PathPromptOptions, Render, Styled, Window, div, rgb,
};

use crate::command_palette::{
    PaletteMode, ToggleActionsPalette, ToggleProjectsPalette, palette_mode_for_shortcut,
};
use crate::data::{
    CheckoutInspection, DataRoot, NewRepositoryInput, RepositoryMetadata, create_repository,
    inspect_checkout, link_repository, update_repository_metadata,
};

/// What the dialog does on submit. Create writes a portable record plus a
/// device binding; Link binds an existing portable record to a checkout
/// (recovery for synced-but-unlinked records); Edit overwrites portable
/// metadata in place. Key renames are out of scope (deferred per
/// feature-parity), so Link/Edit carry a fixed key shown as static text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepositoryDialogMode {
    Create,
    Link { key: String },
    Edit { key: String },
}

pub(crate) struct AddRepositoryView {
    focus_handle: FocusHandle,
    mode: RepositoryDialogMode,
    data_root: Option<DataRoot>,
    checkout_path: Option<PathBuf>,
    key: Entity<InputState>,
    display_name: Entity<InputState>,
    owner: Entity<InputState>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    group: Entity<InputState>,
    tags: Entity<InputState>,
    clone_url: Entity<InputState>,
    base_branch: Entity<InputState>,
    revision: String,
    busy: bool,
    error: Option<String>,
}

pub(crate) struct RepositoryAdded;
impl gpui_kit::EventEmitter<RepositoryAdded> for AddRepositoryView {}

impl AddRepositoryView {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        data_root: Option<DataRoot>,
    ) -> Self {
        let mut input = |placeholder: &str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            focus_handle: cx.focus_handle(),
            mode: RepositoryDialogMode::Create,
            data_root,
            checkout_path: None,
            key: input("e.g. hossainemruz-devcroft", cx),
            display_name: input("e.g. Devcroft", cx),
            owner: input("e.g. hossainemruz", cx),
            name: input("e.g. devcroft", cx),
            description: input("What is this repository for?", cx),
            group: input("e.g. personal", cx),
            tags: input("Comma-separated, e.g. rust, desktop", cx),
            clone_url: input("e.g. git@github.com:hossainemruz/devcroft.git", cx),
            base_branch: input("e.g. main", cx),
            revision: String::new(),
            busy: false,
            error: None,
        }
    }

    /// Link mode: bind an existing portable record to a checkout. Only the
    /// checkout browser matters; the key is fixed and rendered as static
    /// text.
    pub(crate) fn for_link(
        window: &mut Window,
        cx: &mut Context<Self>,
        data_root: Option<DataRoot>,
        key: String,
    ) -> Self {
        let mut view = Self::new(window, cx, data_root);
        view.mode = RepositoryDialogMode::Link { key };
        view
    }

    /// Edit mode: overwrite portable metadata in place. Fields prefill from
    /// the stored record, so a cleared field means "clear it"; the key is
    /// fixed (renames deferred) and rendered as static text.
    pub(crate) fn for_edit(
        window: &mut Window,
        cx: &mut Context<Self>,
        data_root: Option<DataRoot>,
        key: String,
        metadata: &RepositoryMetadata,
    ) -> Self {
        let mut view = Self::new(window, cx, data_root);
        view.revision = metadata.revision.clone();
        let mut prefill = |state: &Entity<InputState>, value: &str| {
            if !value.is_empty() {
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        };
        prefill(
            &view.display_name,
            metadata.display_name.as_deref().unwrap_or(""),
        );
        prefill(&view.owner, metadata.owner.as_deref().unwrap_or(""));
        prefill(&view.name, metadata.name.as_deref().unwrap_or(""));
        prefill(
            &view.description,
            metadata.description.as_deref().unwrap_or(""),
        );
        prefill(&view.group, metadata.group.as_deref().unwrap_or(""));
        prefill(
            &view.tags,
            &metadata.tags.clone().unwrap_or_default().join(", "),
        );
        prefill(&view.clone_url, metadata.clone_url.as_deref().unwrap_or(""));
        prefill(
            &view.base_branch,
            metadata.base_branch.as_deref().unwrap_or(""),
        );
        view.mode = RepositoryDialogMode::Edit { key };
        view
    }

    /// Pure form snapshot for validation: whitespace-only counts as absent.
    /// Unit-testable without a window. Eight arguments because the form has
    /// eight metadata fields mapping 1:1 onto [`NewRepositoryInput`].
    #[allow(clippy::too_many_arguments)]
    fn snapshot(
        display_name: &str,
        owner: &str,
        name: &str,
        description: &str,
        group: &str,
        tags: &str,
        clone_url: &str,
        base_branch: &str,
    ) -> NewRepositoryInput {
        let present = |value: &str| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        NewRepositoryInput {
            display_name: present(display_name),
            owner: present(owner),
            name: present(name),
            description: present(description),
            group: present(group),
            tags: tags.split(',').map(str::to_owned).collect(),
            clone_url: present(clone_url),
            base_branch: present(base_branch),
        }
    }

    fn values(&self, cx: &App) -> (String, NewRepositoryInput) {
        let text = |state: &Entity<InputState>| state.read(cx).value().to_string();
        let key = text(&self.key).trim().to_owned();
        let input = Self::snapshot(
            &text(&self.display_name),
            &text(&self.owner),
            &text(&self.name),
            &text(&self.description),
            &text(&self.group),
            &text(&self.tags),
            &text(&self.clone_url),
            &text(&self.base_branch),
        );
        (key, input)
    }

    /// Open the native directory picker, then harvest the checkout on a
    /// background thread and fill empty fields with the mouse still free —
    /// the future is polled on the main thread but git runs off it.
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.error = None;
        cx.notify();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select a local Git checkout".into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let selected: Option<PathBuf> = match receiver.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                Ok(Ok(None)) => return, // Cancelled: leave the form untouched.
                Ok(Err(error)) => {
                    let _ = cx.update(|_, cx| {
                        view.update(cx, |this, cx| {
                            this.error = Some(format!("could not open the file picker: {error:#}"));
                            cx.notify();
                        })
                        .ok();
                    });
                    return;
                }
                Err(_) => return, // Picker went away with the window.
            };
            let Some(path) = selected else {
                return;
            };
            let inspected = cx
                .background_spawn(async move {
                    let inspection = inspect_checkout(&path);
                    (path, inspection)
                })
                .await;
            let _ = cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.apply_inspection(inspected, window, cx);
                })
                .ok();
            });
        })
        .detach();
    }

    /// Record the picked directory and fill empty fields from the harvest.
    /// A failed harvest keeps the path visible and surfaces why, so the user
    /// sees which directory was rejected.
    fn apply_inspection(
        &mut self,
        inspected: (PathBuf, anyhow::Result<CheckoutInspection>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window = &mut *window;
        let (path, inspection) = inspected;
        self.checkout_path = Some(path);
        match inspection {
            Ok(inspection) => {
                self.error = None;
                self.fill_if_empty(&self.key, &inspection.suggested_key, window, cx);
                self.fill_if_empty(
                    &self.owner,
                    inspection.owner.as_deref().unwrap_or(""),
                    window,
                    cx,
                );
                self.fill_if_empty(&self.name, &inspection.name, window, cx);
                self.fill_if_empty(
                    &self.clone_url,
                    inspection.clone_url.as_deref().unwrap_or(""),
                    window,
                    cx,
                );
                self.fill_if_empty(&self.base_branch, &inspection.base_branch, window, cx);
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    /// `set_value` needs a window, so harvested fills go through here
    /// rather than the async completion directly.
    fn fill_if_empty(
        &self,
        state: &Entity<InputState>,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if value.is_empty() {
            return;
        }
        let empty = state.read(cx).value().trim().is_empty();
        if empty {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }

    /// Validate, then persist off the main thread. Create writes the record
    /// plus binding, Link binds an existing record, Edit overwrites metadata.
    /// The dialog stays open with the error on failure; success notifies
    /// and closes.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(root) = self.data_root.clone() else {
            self.error = Some("portable data is unavailable".to_owned());
            cx.notify();
            return;
        };
        let mode = self.mode.clone();
        // Edit needs no checkout; Create/Link require one.
        let checkout = if mode == RepositoryDialogMode::Create
            || matches!(mode, RepositoryDialogMode::Link { .. })
        {
            match self.checkout_path.clone() {
                Some(checkout) => Some(checkout),
                None => {
                    self.error = Some("select a checkout directory first".to_owned());
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let (key_text, input) = self.values(cx);
        // Resolve the fixed key for Link/Edit; validate the typed key for
        // Create.
        let key = match &mode {
            RepositoryDialogMode::Create => {
                if key_text.is_empty() {
                    self.error = Some("enter a repository key".to_owned());
                    cx.notify();
                    return;
                }
                key_text
            }
            RepositoryDialogMode::Link { key } | RepositoryDialogMode::Edit { key } => key.clone(),
        };
        let verb = match &mode {
            RepositoryDialogMode::Create => "added",
            RepositoryDialogMode::Link { .. } => "linked",
            RepositoryDialogMode::Edit { .. } => "saved",
        }
        .to_owned();
        let revision = self.revision.clone();
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let outcome = cx
                .background_spawn(async move {
                    match mode {
                        RepositoryDialogMode::Create => {
                            let checkout = checkout.expect("create requires a checkout");
                            create_repository(&root, &key, &checkout, &input)
                                .map(|created| created.key)
                        }
                        RepositoryDialogMode::Link { .. } => {
                            let checkout = checkout.expect("link requires a checkout");
                            link_repository(&root, &key, &checkout).map(|linked| linked.key)
                        }
                        RepositoryDialogMode::Edit { .. } => {
                            update_repository_metadata(&root, &key, &input, &revision)
                                .map(|_| key.clone())
                        }
                    }
                })
                .await;
            let _ = cx.update(|window, cx| match outcome {
                Ok(key) => {
                    let _ = view.update(cx, |_, cx| cx.emit(RepositoryAdded));
                    window.push_notification(format!("Repository \"{key}\" {verb}"), cx);
                    window.close_dialog(cx);
                }
                Err(error) => {
                    view.update(cx, |this, cx| {
                        this.busy = false;
                        this.error = Some(format!("{error:#}"));
                        cx.notify();
                    })
                    .ok();
                }
            });
        })
        .detach();
    }

    /// Mirror Settings/terminal panes: the palette toggles keep working
    /// while the dialog has focus; tab jumps and session creation live in
    /// navigation mode and the palettes now. Everything else bubbles
    /// normally.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mode) = palette_mode_for_shortcut(
            &event.keystroke.key,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
            event.keystroke.modifiers.alt,
        ) {
            match mode {
                PaletteMode::Actions => {
                    window.dispatch_action(Box::new(ToggleActionsPalette), cx);
                }
                PaletteMode::Projects => {
                    window.dispatch_action(Box::new(ToggleProjectsPalette), cx);
                }
            }
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn field(&self, label: &str, state: &Entity<InputState>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .child(label.to_owned()),
            )
            .child(Input::new(state))
    }

    fn render_checkout(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let path_text = self
            .checkout_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No checkout selected".to_owned());
        let picked = self.checkout_path.is_some();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .child("Checkout directory (required)"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x292b2b))
                            .bg(rgb(0x0e0f0f))
                            .text_sm()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if picked { rgb(0xe7e7e7) } else { rgb(0x555a5a) })
                            .child(path_text),
                    )
                    .child(
                        Button::new("add-repository-browse")
                            .label("Browse…")
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.browse(window, cx)).ok();
                            }),
                    ),
            )
    }

    fn render_fixed_key(&self, key: &str) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x858989))
                    .child("Repository key"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xe7e7e7))
                    .child(key.to_owned()),
            )
    }

    fn render_metadata_fields(&self) -> impl IntoElement {
        v_flex()
            .gap_3()
            .w_full()
            .child(self.field("Display name", &self.display_name))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.field("Owner", &self.owner)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.field("Name", &self.name)),
                    ),
            )
            .child(self.field("Description", &self.description))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.field("Group", &self.group)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.field("Tags (comma-separated)", &self.tags)),
                    ),
            )
            .child(self.field("Clone URL", &self.clone_url))
            .child(self.field("Base branch", &self.base_branch))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let confirm = view.clone();
        let (busy_label, idle_label) = match self.mode {
            RepositoryDialogMode::Create => ("Adding…", "Add repository"),
            RepositoryDialogMode::Link { .. } => ("Linking…", "Link checkout"),
            RepositoryDialogMode::Edit { .. } => ("Saving…", "Save changes"),
        };
        h_flex()
            .gap_2()
            .justify_end()
            .pt_4()
            .child(
                Button::new("add-repository-cancel")
                    .label("Cancel")
                    .ghost()
                    .on_click(|_, window, cx| {
                        window.close_dialog(cx);
                    }),
            )
            .child(
                Button::new("add-repository-submit")
                    .label(if self.busy { busy_label } else { idle_label })
                    .primary()
                    .on_click(move |_, window, cx| {
                        confirm.update(cx, |this, cx| this.submit(window, cx)).ok();
                    }),
            )
    }
}

impl Focusable for AddRepositoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AddRepositoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match &self.mode {
            RepositoryDialogMode::Create => {
                "Browse fills empty fields from the checkout — your edits are never overwritten."
            }
            RepositoryDialogMode::Link { .. } => {
                "Pick the local checkout to bind. Portable metadata stays untouched."
            }
            RepositoryDialogMode::Edit { .. } => {
                "Clearing a field removes it from the portable record. The key cannot be renamed."
            }
        };
        let mut body = v_flex().gap_3().w_full();
        match &self.mode {
            RepositoryDialogMode::Create => {
                body = body
                    .child(self.render_checkout(cx))
                    .child(self.field("Repository key (required, lowercase)", &self.key))
                    .child(self.render_metadata_fields());
            }
            RepositoryDialogMode::Link { key } => {
                body = body
                    .child(self.render_fixed_key(key))
                    .child(self.render_checkout(cx));
            }
            RepositoryDialogMode::Edit { key } => {
                body = body
                    .child(self.render_fixed_key(key))
                    .child(self.render_metadata_fields());
            }
        }
        body = body.when_some(self.error.clone(), |this, error| {
            this.child(div().text_sm().text_color(rgb(0xf87171)).child(error))
        });
        v_flex()
            .size_full()
            .px_6()
            .py_5()
            .gap_3()
            .bg(rgb(0x090a0a))
            .text_color(rgb(0xe7e7e7))
            .child(div().text_xs().text_color(rgb(0x737878)).child(hint))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(body),
            )
            .child(self.render_footer(cx))
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

#[cfg(test)]
mod tests {
    use super::AddRepositoryView;

    #[test]
    fn snapshot_trims_and_splits_tags() {
        let input = AddRepositoryView::snapshot(
            "  Display  ",
            "",
            "name",
            "   ",
            "personal",
            "rust, desktop ,,rust",
            "  https://example.com/o/r.git ",
            "",
        );
        assert_eq!(input.display_name.as_deref(), Some("Display"));
        assert!(input.owner.is_none());
        assert_eq!(input.name.as_deref(), Some("name"));
        assert!(input.description.is_none());
        assert_eq!(input.group.as_deref(), Some("personal"));
        assert_eq!(
            input.clone_url.as_deref(),
            Some("https://example.com/o/r.git")
        );
        assert!(input.base_branch.is_none());
        // Raw split here; `clean_tags` dedups at create time.
        assert_eq!(input.tags, vec!["rust", " desktop ", "", "rust"]);
    }
}
