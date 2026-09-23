//! Component defaults plus the extra icons used by the workspace chrome.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

gpui_kit::assets::icon_assets!(
    WorkspaceIcons,
    [GitBranch, GitCommitHorizontal, Keyboard, Braces, Diff]
);

pub(crate) struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = WorkspaceIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(WorkspaceIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn workspace_icons_load_alongside_component_defaults() {
        for icon in [
            IconName::GitBranch,
            IconName::GitCommitHorizontal,
            IconName::Keyboard,
            IconName::Braces,
            IconName::Diff,
            IconName::Search,
            IconName::ChevronLeft,
            IconName::TriangleAlert,
        ] {
            let path = icon.path();
            let bytes = AppAssets
                .load(&path)
                .unwrap_or_else(|error| panic!("{path}: {error}"))
                .unwrap_or_else(|| panic!("missing icon: {path}"));
            assert!(
                std::str::from_utf8(&bytes).unwrap().contains("<svg"),
                "{path} must contain an SVG"
            );
        }
    }
}
