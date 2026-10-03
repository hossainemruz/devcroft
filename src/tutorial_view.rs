//! Tutorial artifact viewer.
//!
//! A tutorial is one self-contained HTML document. On macOS the document is
//! presented in the shared sandboxed webview host; elsewhere (and on demand
//! everywhere) the viewer writes a device-local cache copy and hands it to the
//! default browser. The view never bridges host capabilities into the
//! document, so scripts run without access to Devcroft.
use anyhow::{Context as _, Result};
#[cfg(not(target_os = "macos"))]
use gpui_kit::AnyElement;
#[cfg(target_os = "macos")]
use gpui_kit::Entity;
#[cfg(target_os = "macos")]
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::{
    Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) struct TutorialView {
    root: Option<crate::data::DataRoot>,
    artifact_id: Option<String>,
    title: SharedString,
    body: Option<SharedString>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    menu_open: bool,
    #[cfg(target_os = "macos")]
    webview: Option<Entity<gpui_wry::WebView>>,
    #[cfg(target_os = "macos")]
    appearance: Option<String>,
}

impl TutorialView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            root: None,
            artifact_id: None,
            title: "".into(),
            body: None,
            error: None,
            focus_handle: cx.focus_handle(),
            menu_open: false,
            #[cfg(target_os = "macos")]
            webview: None,
            #[cfg(target_os = "macos")]
            appearance: None,
        }
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Native child views otherwise cover GPUI popup rows and intercept clicks.
    pub(crate) fn set_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.menu_open = open;
        if open {
            self.set_visible(false, cx);
        }
        cx.notify();
    }

    /// Native child views do not follow GPUI layout visibility, so the host
    /// tab must hide the webview when its pane stops rendering. Rendering the
    /// view again shows it on the next frame.
    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(webview) = &self.webview {
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
        #[cfg(not(target_os = "macos"))]
        let _ = (visible, cx);
    }

    /// Point the viewer at one tutorial revision. The native document is
    /// rebuilt on the next frame; the previous webview is dropped first so a
    /// stale document never lingers behind a new revision.
    pub(crate) fn show(
        &mut self,
        root: Option<crate::data::DataRoot>,
        artifact_id: &str,
        title: &str,
        body: String,
        cx: &mut Context<Self>,
    ) {
        self.root = root;
        self.artifact_id = Some(artifact_id.to_owned());
        self.title = title.to_owned().into();
        self.body = Some(body.into());
        self.error = None;
        #[cfg(target_os = "macos")]
        {
            self.webview.take();
        }
        cx.notify();
    }

    /// Write the current revision to the device-local cache and open it with
    /// the platform browser. The cache is disposable and never syncs.
    pub(crate) fn open_in_browser(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let result = self
            .write_browser_copy()
            .and_then(|path| launch_browser(&path));
        match result {
            Ok(()) => {
                self.error = None;
                Ok(())
            }
            Err(error) => {
                self.error =
                    Some(format!("Could not open the tutorial in a browser: {error:#}").into());
                cx.notify();
                Err(error)
            }
        }
    }

    fn write_browser_copy(&self) -> Result<PathBuf> {
        let root = self.root.as_ref().context("Tutorial has no data root")?;
        let id = self
            .artifact_id
            .as_deref()
            .context("Tutorial has no artifact ID")?;
        let body = self.body.as_deref().context("Tutorial has no document")?;
        let path = browser_cache_path(root, id);
        let dir = path.parent().context("Tutorial cache has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&path, reader_document(body, None))
            .with_context(|| format!("writing {}", path.display()))?;
        prune_browser_cache(dir, SystemTime::now());
        Ok(path)
    }

    #[cfg(target_os = "macos")]
    fn build_webview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.webview.is_some() {
            return;
        }
        let Some(body) = self.body.clone() else {
            return;
        };
        let appearance = reader_appearance(cx);
        let html = crate::webview::sandboxed_document(&reader_document(&body, Some(&appearance)));
        match crate::webview::create(window, cx, |builder| builder.with_html(html)) {
            Ok(webview) => {
                self.webview = Some(webview);
                self.appearance = Some(appearance);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("Could not create the tutorial view: {error:#}").into());
            }
        }
        cx.notify();
    }
}

impl Render for TutorialView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        let native = {
            if self
                .appearance
                .as_ref()
                .is_some_and(|style| *style != reader_appearance(cx))
            {
                self.webview.take();
                self.appearance = None;
            }
            if self.webview.is_none() && self.body.is_some() && self.error.is_none() {
                let view = cx.entity();
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| view.build_webview(window, cx));
                });
            }
            let webview = self.webview.clone().filter(|_| self.error.is_none());
            let visible =
                !self.menu_open && !window.has_active_dialog(cx) && !window.has_active_sheet(cx);
            if let Some(webview) = &webview {
                webview.update(cx, |view, _| {
                    if visible && !view.visible() {
                        view.show();
                    } else if !visible && view.visible() {
                        view.hide();
                    }
                });
                let _ = crate::webview::accessibility::attach(webview.read(cx).raw());
            }
            webview.map(|webview| {
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(webview)
                    .into_any_element()
            })
        };
        #[cfg(not(target_os = "macos"))]
        let native: Option<AnyElement> = None;

        let mut frame = v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0();
        if let Some(error) = &self.error {
            frame = frame.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        if let Some(native) = native {
            frame = frame.child(native);
        } else if self.error.is_none() {
            frame = frame.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().child(self.title.clone()))
                    .child(if cfg!(target_os = "macos") {
                        "Rendering tutorial…"
                    } else {
                        "Use Artifact options → Open in browser to read this tutorial."
                    }),
            );
        }
        frame
    }
}

/// Add reader presentation to the display copy, never to the stored artifact.
/// This script runs in the same opaque frame as the authored content. It does
/// not communicate with the host or change the sandbox's capabilities.
fn reader_document(body: &str, appearance: Option<&str>) -> String {
    let mode = if appearance.is_some() {
        "embedded"
    } else {
        "browser"
    };
    format!(
        "{body}<style>{}\n{}</style><script>\
         document.documentElement.dataset.devcroftTutorial='{mode}';\n{}\n</script>",
        include_str!("tutorial_view/reader.css"),
        appearance.unwrap_or_default(),
        include_str!("tutorial_view/reader.js"),
    )
}

#[cfg(target_os = "macos")]
fn reader_appearance(cx: &gpui_kit::App) -> String {
    let theme = cx.theme();
    let color = |value: gpui_kit::Hsla| format!("#{:08x}", u32::from(value.to_rgb()));
    format!(
        ":root[data-devcroft-tutorial] {{ color-scheme: {}; \
         --bg: {} !important; --surface: {} !important; --border: {} !important; \
         --text: {} !important; --muted: {} !important; \
         --reader-active: {}; --reader-active-text: {}; --reader-ring: {}; }}",
        if theme.is_dark() { "dark" } else { "light" },
        color(theme.background),
        color(theme.muted),
        color(theme.border),
        color(theme.foreground),
        color(theme.muted_foreground),
        color(theme.accent),
        color(theme.accent_foreground),
        color(theme.ring),
    )
}

/// Disposable browser copy under the device-local data root. It never lives
/// in `portable/` and never participates in sync.
fn browser_cache_path(root: &crate::data::DataRoot, id: &str) -> PathBuf {
    root.root()
        .join("cache")
        .join("tutorials")
        .join(format!("{id}.html"))
}

/// Browser copies are disposable: entries untouched for this long are removed
/// best-effort the next time a tutorial is opened. The current revision is
/// written first, so its fresh timestamp always spares it.
const BROWSER_CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn prune_browser_cache(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("html") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > BROWSER_CACHE_TTL)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(target_os = "macos")]
fn launch_browser(path: &Path) -> Result<()> {
    spawn_browser(std::process::Command::new("open"), path)
}

#[cfg(target_os = "linux")]
fn launch_browser(path: &Path) -> Result<()> {
    spawn_browser(std::process::Command::new("xdg-open"), path)
}

#[cfg(target_os = "windows")]
fn launch_browser(path: &Path) -> Result<()> {
    let mut command = std::process::Command::new("cmd");
    command.args(["/C", "start", ""]);
    spawn_browser(command, path)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn launch_browser(_: &Path) -> Result<()> {
    anyhow::bail!("Opening a tutorial in a browser is not supported on this platform")
}

fn spawn_browser(mut command: std::process::Command, path: &Path) -> Result<()> {
    command
        .arg(path)
        .spawn()
        .with_context(|| format!("launching {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AppContext as _;

    #[test]
    fn browser_cache_stays_out_of_portable_data() {
        let root = crate::data::DataRoot::new(PathBuf::from("/data"));
        let path = browser_cache_path(&root, "art-23456789");
        assert_eq!(
            path,
            PathBuf::from("/data/cache/tutorials/art-23456789.html")
        );
        assert!(!path.starts_with(root.portable_dir()));
    }

    #[gpui_kit::test]
    fn browser_copy_writes_the_current_revision_to_the_cache(cx: &mut gpui_kit::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::data::DataRoot::new(dir.path().to_owned());
        let view = cx.new(TutorialView::new);
        view.update(cx, |view, cx| {
            view.show(
                Some(root.clone()),
                "art-23456789",
                "Two records",
                "<!doctype html><p>hello</p>".into(),
                cx,
            );
            let path = view.write_browser_copy().unwrap();
            assert_eq!(
                path,
                root.root()
                    .join("cache")
                    .join("tutorials")
                    .join("art-23456789.html")
            );
            let copy = std::fs::read_to_string(&path).unwrap();
            assert!(copy.starts_with("<!doctype html><p>hello</p>"));
            assert!(copy.contains("devcroftTutorial='browser'"));
            assert_eq!(view.body.as_deref(), Some("<!doctype html><p>hello</p>"));
            view.show(
                Some(root),
                "art-23456789",
                "Revised",
                "<!doctype html><h1>Revised</h1>".into(),
                cx,
            );
            view.write_browser_copy().unwrap();
            let copy = std::fs::read_to_string(path).unwrap();
            assert!(copy.contains("<h1>Revised</h1>"));
            assert!(!copy.contains("<p>hello</p>"));
        });
    }

    #[test]
    fn reader_presentation_stays_inside_the_sandbox() {
        let body = "<html><head></head><body><h1>Test</h1></body></html>";
        let reader = reader_document(body, Some(":root { --bg: #123456; }"));
        assert!(reader.starts_with(body));
        assert!(reader.contains("devcroftTutorial='embedded'"));
        let host = crate::webview::sandboxed_document(&reader);
        assert!(host.contains("sandbox=\"allow-scripts\""));
        assert_eq!(host.matches("<script>").count(), 1);
        assert_eq!(host.matches("</script>").count(), 1);
        assert_eq!(host.matches("connect-src 'none'").count(), 2);
        assert!(!host.contains("postMessage"));
        assert!(!host.contains("messageHandlers"));
    }

    #[test]
    fn browser_cache_prunes_stale_entries_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh.html");
        let stale = dir.path().join("stale.html");
        let other = dir.path().join("notes.txt");
        std::fs::write(&fresh, "fresh").unwrap();
        std::fs::write(&stale, "stale").unwrap();
        std::fs::write(&other, "keep").unwrap();
        let old = SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        prune_browser_cache(dir.path(), SystemTime::now());
        assert!(fresh.exists());
        assert!(!stale.exists(), "entries older than the TTL are removed");
        assert!(other.exists(), "non-tutorial files are left alone");
    }

    /// Rendering a shown tutorial must either host the document or surface a
    /// clear error. Headless test windows cannot provide a native handle, so
    /// this covers the request path and the no-panic error state; real
    /// hosting is checked manually in the workspace.
    #[cfg(target_os = "macos")]
    #[gpui_kit::test]
    fn a_shown_tutorial_hosts_or_explains_native_setup_failure(cx: &mut gpui_kit::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::data::DataRoot::new(dir.path().to_owned());
        cx.update(gpui_kit::init);
        let view = cx.new(TutorialView::new);
        let (_, cx) = cx.add_window_view({
            let view = view.clone();
            move |window, cx| gpui_kit::component::Root::new(view, window, cx)
        });
        view.update(cx, |view, cx| {
            view.show(
                Some(root),
                "art-23456789",
                "Two records",
                "<!doctype html><html><body><p>Two records, one commit.</p></body></html>".into(),
                cx,
            )
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert!(
                view.webview.is_some() || view.error.is_some(),
                "the viewer must host the document or explain the failure"
            );
            if view.webview.is_none() {
                assert!(
                    view.error
                        .as_deref()
                        .is_some_and(|error| error.contains("tutorial view")),
                    "the surfaced error must name the tutorial view"
                );
            }
        });
    }
}
