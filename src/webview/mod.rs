//! Shared native webview hosting with one hardening policy.
//!
//! Every webview this application creates is incognito, cannot navigate away
//! from local `about:` documents, and cannot open windows or downloads.
//! Authored content runs inside a sandboxed frame whose document policy
//! blocks the network, forms, nested frames, and object embedding. The review
//! workspace adds its capability bridge on top; tutorial artifacts use the
//! same host with no bridge at all.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(target_os = "macos")]
pub(crate) mod accessibility;
#[cfg(target_os = "macos")]
mod input;

/// The document policy for authored HTML. Inline styles and scripts are
/// allowed so visual explanations work; every remote or local resource load,
/// connection, form, frame, and object is denied.
pub(crate) const SANDBOX_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

/// The host shell policy: the shell itself loads nothing, but may host the
/// sandboxed `about:srcdoc` frame that carries the authored document.
const HOST_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; frame-src 'self' about:; connect-src 'none'; base-uri 'none'; form-action 'none'";

#[cfg(target_os = "macos")]
use anyhow::Result;
#[cfg(target_os = "macos")]
use gpui_kit::{App, AppContext as _, Entity, Window};

/// Build a hardened child webview inside `window`.
///
/// `configure` supplies the document and, only when the caller needs it, an
/// IPC handler. Everything else is fixed: incognito storage, navigation
/// limited to `about:blank`/`about:srcdoc`, denied new windows and downloads.
#[cfg(target_os = "macos")]
pub(crate) fn create(
    window: &mut Window,
    cx: &mut App,
    configure: impl FnOnce(wry::WebViewBuilder) -> wry::WebViewBuilder,
) -> Result<Entity<gpui_wry::WebView>> {
    let builder = wry::WebViewBuilder::new()
        .with_incognito(true)
        .with_navigation_handler(|url| url == "about:blank" || url == "about:srcdoc")
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false);
    let handle = raw_window_handle::HasWindowHandle::window_handle(window)?;
    let native = configure(builder).build_as_child(&handle)?;
    Ok(cx.new(|cx| gpui_wry::WebView::new(native, window, cx)))
}

/// Build the host document that presents one authored HTML body inside a
/// sandboxed `about:srcdoc` frame.
///
/// The body keeps its own document (styles, scripts, and layout) but receives
/// the sandbox policy inside its head, so the frame cannot reach the network
/// even if the host policy were bypassed. Bare fragments are wrapped in a
/// minimal document instead of being rejected.
pub(crate) fn sandboxed_document(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{HOST_POLICY}\">\
         <style>html,body{{margin:0;height:100%}}\
         iframe{{display:block;width:100%;height:100%;border:0;background:transparent}}</style>\
         </head><body>\
         <iframe id=\"sandbox\" title=\"Tutorial\" sandbox=\"allow-scripts\" referrerpolicy=\"no-referrer\"></iframe>\
         <script>\"use strict\";document.getElementById(\"sandbox\").srcdoc={};</script>\
         </body></html>",
        js_string(&with_policy(body))
    )
}

/// Insert the sandbox policy meta into the document head. A full document
/// keeps its own markup; a fragment gets a minimal document wrapper.
fn with_policy(body: &str) -> String {
    let meta =
        format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{SANDBOX_POLICY}\">");
    if let Some(end) = tag_end(body, "<head") {
        return format!("{}{meta}{}", &body[..end], &body[end..]);
    }
    if let Some(end) = tag_end(body, "<html") {
        return format!("{}{meta}{}", &body[..end], &body[end..]);
    }
    format!("<!doctype html><html><head>{meta}</head><body>{body}</body></html>")
}

/// Byte index just past the `>` that closes the first case-insensitive
/// occurrence of `tag` (for example `<head`).
fn tag_end(body: &str, tag: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let start = lower.find(tag)?;
    lower[start..].find('>').map(|offset| start + offset + 1)
}

/// Serialize `value` as a JavaScript string literal that cannot close the
/// host `<script>` element (`<` is escaped) or inject line separators.
fn js_string(value: &str) -> String {
    serde_json::to_string(value)
        .expect("strings always serialize")
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_injected_into_the_document_head() {
        let document =
            with_policy("<!doctype html><html><head><title>x</title></head><body></body></html>");
        assert!(document.contains("<head><meta http-equiv=\"Content-Security-Policy\""));
        assert_eq!(document.matches("Content-Security-Policy").count(), 1);
        assert!(document.contains("<title>x</title>"));
    }

    #[test]
    fn attributed_and_implicit_heads_still_receive_the_policy() {
        let attributed = with_policy(
            "<html><head lang=\"en\"><link rel=\"icon\" href=\"data:,\"></head></html>",
        );
        assert!(
            attributed.contains("<head lang=\"en\"><meta http-equiv=\"Content-Security-Policy\"")
        );
        let implicit = with_policy("<html><body>hello</body></html>");
        assert!(implicit.contains("<html><meta http-equiv=\"Content-Security-Policy\""));
        assert!(implicit.contains("<body>hello</body>"));
    }

    #[test]
    fn fragments_are_wrapped_with_the_policy() {
        let wrapped = with_policy("<p>fragment</p>");
        assert!(wrapped.starts_with(
            "<!doctype html><html><head><meta http-equiv=\"Content-Security-Policy\""
        ));
        assert!(wrapped.ends_with("<body><p>fragment</p></body></html>"));
    }

    #[test]
    fn authored_script_cannot_terminate_the_host_script() {
        let host = sandboxed_document("</script><script>alert(1)</script>");
        assert_eq!(host.matches("</script>").count(), 1);
        assert!(host.contains("\\u003c/script"));
        assert!(host.contains("connect-src 'none'"));
        assert_eq!(host.matches("connect-src 'none'").count(), 2);
    }

    /// Tutorial webviews are built without an IPC handler. The host shell
    /// must not install any bridge of its own: the only script is the srcdoc
    /// setter, and the authored markup stays an escaped string.
    #[test]
    fn the_host_shell_adds_no_capability_bridge() {
        let host = sandboxed_document("<script>parent.postMessage('x', '*')</script>");
        assert_eq!(host.matches("<script").count(), 1);
        assert!(!host.contains("__dc"));
        assert!(!host.contains("messageHandlers"));
        assert!(!host.contains("addEventListener"));
        assert!(host.contains("sandbox=\"allow-scripts\""));
        assert!(host.contains("\\u003cscript"), "authored script stays text");
    }
}
