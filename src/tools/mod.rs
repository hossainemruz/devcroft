//! Developer tools opened from the command bar's `Tools` section.
//!
//! A tool is a dialog with persisted inputs and an output derived from them.
//! Every tool input is machine-local: it lives under `tools/<tool-id>/` in
//! the data root, never inside `portable/`, so scratch content cannot ride
//! portable sync. [`ToolKind`] is the registry the palette and the dialog
//! read from, so a new tool is a variant plus its slots and compute step.

mod json;
mod store;
mod view;

pub(crate) use store::ToolStore;
pub(crate) use view::ToolView;

use gpui_kit::assets::IconName;

/// Every tool the command bar can open, in canonical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ToolKind {
    JsonFormatter,
}

/// One persisted input of a tool. `file_name` is the storage key under
/// `tools/<tool-id>/`; the placeholder is the dialog editor's hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolInput {
    pub(crate) file_name: &'static str,
    pub(crate) placeholder: &'static str,
}

impl ToolKind {
    pub(crate) const ALL: [Self; 1] = [Self::JsonFormatter];

    /// Stable storage key. Never reused for a different tool, so old
    /// persisted content cannot surface under a new tool's name.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::JsonFormatter => "json-formatter",
        }
    }

    /// Palette row label and dialog title.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::JsonFormatter => "Format JSON",
        }
    }

    /// Extra search terms besides the label.
    pub(crate) fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::JsonFormatter => &["json", "format", "pretty", "print", "tool"],
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::JsonFormatter => IconName::Braces,
        }
    }

    /// Persisted inputs in display order.
    pub(crate) fn inputs(self) -> &'static [ToolInput] {
        match self {
            Self::JsonFormatter => &[ToolInput {
                file_name: "input",
                placeholder: "Paste JSON here",
            }],
        }
    }

    /// Syntax the dialog's editors highlight with. `None` keeps them plain,
    /// which is what free-form text tools (a diff's two sides) want.
    pub(crate) fn editor_language(self) -> Option<&'static str> {
        match self {
            Self::JsonFormatter => Some("json"),
        }
    }

    /// Derive the tool's result from the current inputs, in slot order. For
    /// the formatter that result is the document rewritten in place; errors
    /// are user-facing text and leave the input untouched.
    pub(crate) fn compute(self, inputs: &[String]) -> Result<String, String> {
        match self {
            Self::JsonFormatter => {
                json::format(inputs.first().map(String::as_str).unwrap_or_default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_labels_are_distinct_and_nonempty() {
        let mut ids = Vec::new();
        let mut labels = Vec::new();
        for tool in ToolKind::ALL {
            assert!(!tool.id().is_empty(), "{tool:?} has no storage id");
            assert!(!tool.label().is_empty(), "{tool:?} has no label");
            assert!(!tool.inputs().is_empty(), "{tool:?} has no inputs");
            ids.push(tool.id());
            labels.push(tool.label());
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ToolKind::ALL.len());
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), ToolKind::ALL.len());
    }

    #[test]
    fn input_slots_are_stable_storage_keys() {
        for tool in ToolKind::ALL {
            let mut names: Vec<_> = tool.inputs().iter().map(|input| input.file_name).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                tool.inputs().len(),
                "{tool:?} reuses an input file name"
            );
            for input in tool.inputs() {
                assert!(
                    !input.file_name.is_empty() && !input.file_name.contains('/'),
                    "{tool:?} has an unusable input file name: {:?}",
                    input.file_name
                );
            }
        }
    }
}
