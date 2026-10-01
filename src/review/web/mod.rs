#[cfg(target_os = "macos")]
mod accessibility;
mod bridge;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod native;

#[cfg(target_os = "macos")]
pub(crate) use native::{open, open_pr, open_with_bundle};

#[cfg(not(target_os = "macos"))]
pub(crate) fn open(
    _: std::path::PathBuf,
    _: super::model::ReviewDiff,
    _: String,
    _: super::git::ReviewScope,
    _: &mut gpui_kit::App,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "The visual review webview is currently enabled on macOS. Source review remains available; Windows/Linux native hosting must pass integration checks before it is enabled."
    )
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn open_with_bundle(
    cwd: std::path::PathBuf,
    diff: super::model::ReviewDiff,
    label: String,
    scope: super::git::ReviewScope,
    _: Option<std::path::PathBuf>,
    cx: &mut gpui_kit::App,
) -> anyhow::Result<()> {
    open(cwd, diff, label, scope, cx)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn open_pr(
    _: super::session::Capture,
    _: Option<std::path::PathBuf>,
    _: &mut gpui_kit::App,
) -> anyhow::Result<()> {
    anyhow::bail!("Visual PR review native hosting is currently enabled on macOS only")
}
