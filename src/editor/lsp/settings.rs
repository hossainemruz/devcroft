//! Shared settings surface, embedded in Settings and the editor status dialog.
use super::{
    catalog::ServerId,
    install::{self, Action},
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px,
    rgb,
};
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _};
use std::{collections::HashMap, path::PathBuf, sync::atomic::Ordering, time::Duration};

struct Row {
    executable: Entity<InputState>,
    runtime: Entity<InputState>,
    root: Entity<InputState>,
    settings: Entity<InputState>,
    expanded: bool,
}
pub(crate) struct LanguageServers {
    data: Option<PathBuf>,
    checkout: Option<PathBuf>,
    rows: HashMap<ServerId, Row>,
    notice: Option<String>,
}
impl LanguageServers {
    pub(crate) fn new(
        data: Option<PathBuf>,
        checkout: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows = ServerId::ALL
            .into_iter()
            .map(|s| {
                let pref = install::preferences(data.as_deref(), s);
                let mut input = |value: String, placeholder: &str| {
                    cx.new(|cx| {
                        let mut state = InputState::new(window, cx).placeholder(placeholder);
                        state.set_value(value, window, cx);
                        state
                    })
                };
                let executable = input(
                    pref.executable
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    "Automatic discovery, or absolute executable path",
                );
                let runtime = input(
                    pref.runtime
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    "Automatic runtime, or absolute Node / Go executable",
                );
                let root = input(
                    checkout
                        .as_ref()
                        .and_then(|p| pref.roots.get(p))
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    "Automatic project root, or an absolute directory in this checkout",
                );
                let settings = input(
                    if pref.settings.is_null() {
                        String::new()
                    } else {
                        pref.settings.to_string()
                    },
                    "Optional server settings as a JSON object",
                );
                (
                    s,
                    Row {
                        executable,
                        runtime,
                        root,
                        settings,
                        expanded: false,
                    },
                )
            })
            .collect();
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            data,
            checkout,
            rows,
            notice: None,
        }
    }
    fn save(&mut self, server: ServerId, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let data = self
                .data
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Device data is unavailable"))?;
            let row = &self.rows[&server];
            let path = |input: &Entity<InputState>| -> anyhow::Result<Option<PathBuf>> {
                let value = input.read(cx).value().trim().to_owned();
                if value.is_empty() {
                    return Ok(None);
                }
                let path = PathBuf::from(value);
                anyhow::ensure!(path.is_absolute(), "Use an absolute path");
                Ok(Some(path))
            };
            let mut pref = install::preferences(Some(data), server);
            pref.executable = path(&row.executable)?;
            pref.runtime = path(&row.runtime)?;
            if let Some(checkout) = &self.checkout {
                if let Some(root) = path(&row.root)? {
                    let root = root.canonicalize()?;
                    anyhow::ensure!(
                        root.is_dir() && root.starts_with(checkout),
                        "Root must be a directory inside this checkout"
                    );
                    pref.roots.insert(checkout.clone(), root);
                } else {
                    pref.roots.remove(checkout);
                }
            }
            let json = row.settings.read(cx).value();
            pref.settings = if json.trim().is_empty() {
                serde_json::Value::Null
            } else {
                let value: serde_json::Value = serde_json::from_str(&json)?;
                anyhow::ensure!(value.is_object(), "Settings must be a JSON object");
                value
            };
            install::save_preference(data, server, pref)
        })();
        self.notice = Some(match result {
            Ok(()) => "Saved. Open workspaces apply changes on their next check.".into(),
            Err(e) => format!("{e:#}"),
        });
        cx.notify();
    }
    fn action(&mut self, server: ServerId, action: Action, cx: &mut Context<Self>) {
        if let Some(data) = &self.data {
            install::begin(data.clone(), server, action);
        }
        cx.notify();
    }
    fn toggle(&mut self, server: ServerId, cx: &mut Context<Self>) {
        if let Some(data) = &self.data {
            let mut pref = install::preferences(Some(data), server);
            pref.disabled = !pref.disabled;
            if let Err(e) = install::save_preference(data, server, pref) {
                self.notice = Some(format!("{e:#}"));
            }
        }
        cx.notify();
    }
}
impl Render for LanguageServers {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().id("language-server-settings").gap_3().max_h(px(560.)).overflow_y_scroll()
            .child(div().text_sm().font_semibold().child("Language servers"))
            .child(div().text_xs().text_color(rgb(0x999fa8)).child("Install language support as needed. Managed servers are shared on this device. Existing runtimes are required."))
            .when_some(self.notice.clone(),|v,n|v.child(div().text_xs().child(n)))
            .children(ServerId::ALL.into_iter().map(|server| {
                let pref=install::preferences(self.data.as_deref(),server);
                let managed=self.data.as_deref().and_then(|r|install::installed(r,server).ok().flatten());
                let job=self.data.as_deref().and_then(|r|install::job(r,server));let busy=job.as_ref().is_some_and(|j|j.busy);
                let status=if pref.disabled {"Disabled".into()}else if let Some(p)=&pref.executable {format!("Custom · {}",p.display())}
                    else if let Some(v)=&managed {format!("Managed {v}")}
                    else if let Some(p)=crate::editor::find_executable(server.executable()) {format!("System · {}",p.display())}
                    else {"Not installed".into()};
                let row=&self.rows[&server];
                v_flex().gap_2().p_3().rounded_md().border_1().border_color(rgb(0x30343a))
                    .child(h_flex().gap_2().justify_between().child(div().text_sm().child(format!("{} · {}",server.label(),server.id())))
                        .child(Button::new(format!("lsp-enable-{}",server.id())).small().ghost().label(if pref.disabled{"Enable"}else{"Disable"}).disabled(self.data.is_none()).on_click(cx.listener(move |this,_,_,cx|this.toggle(server,cx)))))
                    .child(div().text_xs().text_color(rgb(0x999fa8)).child(status))
                    .when_some(job.clone(),|v,j|v.child(div().text_xs().child(j.status)))
                    .child(h_flex().gap_2()
                        .child(Button::new(format!("lsp-install-{}",server.id())).small().label(if managed.is_some(){"Check / update"}else{"Install"}).disabled(busy||self.data.is_none()).on_click(cx.listener(move |this,_,_,cx|this.action(server,Action::Install,cx))))
                        .child(Button::new(format!("lsp-existing-{}",server.id())).small().ghost().label("Use existing / configure").on_click(cx.listener(move |this,_,_,cx|{this.rows.get_mut(&server).unwrap().expanded^=true;cx.notify();})))
                        .when(managed.is_some(),|v|v.child(Button::new(format!("lsp-rollback-{}",server.id())).small().ghost().label("Roll back").disabled(busy).on_click(cx.listener(move |this,_,_,cx|this.action(server,Action::Rollback,cx))))
                            .child(Button::new(format!("lsp-remove-{}",server.id())).small().ghost().label("Remove").disabled(busy).on_click(cx.listener(move |this,_,_,cx|this.action(server,Action::Remove,cx)))))
                        .when(busy && job.as_ref().is_some_and(|j|j.cancellable),|v|v.child(Button::new(format!("lsp-cancel-{}",server.id())).small().ghost().label("Cancel").on_click(cx.listener(move |this,_,_,cx|{if let Some(j)=this.data.as_deref().and_then(|r|install::job(r,server)){j.cancel.store(true,Ordering::SeqCst);}cx.notify();})))))
                    .when(row.expanded,|v|v
                        .when_some(job.as_ref().map(|j|super::process::log_text(&j.log)).filter(|s|!s.is_empty()),|v,log|v.child(div().id(format!("lsp-log-{}",server.id())).max_h(px(140.)).overflow_y_scroll().text_xs().child(log)))
                        .child(div().text_xs().child("Server executable")).child(Input::new(&row.executable))
                        .child(div().text_xs().child("Node / Go runtime")).child(Input::new(&row.runtime))
                        .when(self.checkout.is_some(),|v|v.child(Input::new(&row.root)))
                        .child(Input::new(&row.settings))
                        .child(Button::new(format!("lsp-save-{}",server.id())).small().label("Save configuration").on_click(cx.listener(move |this,_,_,cx|this.save(server,cx)))))
            }))
    }
}
