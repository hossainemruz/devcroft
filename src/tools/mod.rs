//! Developer tools opened from the command bar's `Tools` section.
//!
//! A tool is a dialog with persisted inputs and a result derived from them.
//! Every tool input is machine-local: it lives under `tools/<tool-id>/` in
//! the data root, never inside `portable/`, so scratch content cannot ride
//! portable sync. [`ToolKind`] is the registry the palette and the dialog
//! read from, so a new tool is a variant plus its slots and compute step.
//!
//! [`ToolResultKind`] says how that result is presented: an in-place tool
//! edits one document, a text-result tool keeps input and output separate,
//! and a diff tool compares two documents.

mod base64;
mod diff;
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
    DiffChecker,
    Base64Encoder,
    Base64Decoder,
}

/// What a run of a tool produces from its inputs.
#[derive(Debug)]
pub(crate) enum ToolOutput {
    /// Rewritten text for the tool's document (its first input).
    Text(String),
    /// Text derived from, but kept separate from, the tool's input.
    ResultText(String),
    /// A line diff between the tool's two inputs.
    Diff(diff::DiffResult),
}

/// How a tool presents its result, and therefore how its dialog is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolResultKind {
    /// One document; the result replaces its text in place.
    InPlaceText,
    /// One paste editor and a separate, copyable result.
    SeparateText,
    /// Two paste editors; the result is a diff rendered below them.
    Diff,
}

/// One persisted input of a tool. `file_name` is the storage key under
/// `tools/<tool-id>/`; the placeholder is the dialog editor's hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolInput {
    pub(crate) file_name: &'static str,
    pub(crate) placeholder: &'static str,
}

impl ToolKind {
    pub(crate) const ALL: [Self; 4] = [
        Self::JsonFormatter,
        Self::DiffChecker,
        Self::Base64Encoder,
        Self::Base64Decoder,
    ];

    /// Stable storage key. Never reused for a different tool, so old
    /// persisted content cannot surface under a new tool's name.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::JsonFormatter => "json-formatter",
            Self::DiffChecker => "diff-checker",
            Self::Base64Encoder => "base64-encoder",
            Self::Base64Decoder => "base64-decoder",
        }
    }

    /// Palette row label and dialog title.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::JsonFormatter => "Format JSON",
            Self::DiffChecker => "Diff Checker",
            Self::Base64Encoder => "Base64 Encoder",
            Self::Base64Decoder => "Base64 Decoder",
        }
    }

    /// Extra search terms besides the label.
    pub(crate) fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::JsonFormatter => &["json", "format", "pretty", "print", "tool"],
            Self::DiffChecker => &["diff", "compare", "text", "checker", "patch", "tool"],
            Self::Base64Encoder => &["base64", "encode", "text", "tool"],
            Self::Base64Decoder => &["base64", "decode", "text", "tool"],
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::JsonFormatter => IconName::Braces,
            Self::DiffChecker => IconName::Diff,
            Self::Base64Encoder | Self::Base64Decoder => IconName::Braces,
        }
    }

    /// Persisted inputs in display order. The diff tool's two sides are its
    /// old and new documents, in that order.
    pub(crate) fn inputs(self) -> &'static [ToolInput] {
        match self {
            Self::JsonFormatter => &[ToolInput {
                file_name: "input",
                placeholder: "Paste JSON here",
            }],
            Self::DiffChecker => &[
                ToolInput {
                    file_name: "old",
                    placeholder: "Paste the old text here",
                },
                ToolInput {
                    file_name: "new",
                    placeholder: "Paste the new text here",
                },
            ],
            Self::Base64Encoder => &[ToolInput {
                file_name: "input",
                placeholder: "Paste text to encode",
            }],
            Self::Base64Decoder => &[ToolInput {
                file_name: "input",
                placeholder: "Paste Base64 to decode",
            }],
        }
    }

    /// Syntax the dialog's editors highlight with. `None` keeps them plain,
    /// which is what free-form text tools (a diff's two sides) want.
    pub(crate) fn editor_language(self) -> Option<&'static str> {
        match self {
            Self::JsonFormatter => Some("json"),
            Self::DiffChecker => None,
            Self::Base64Encoder | Self::Base64Decoder => None,
        }
    }

    /// How the dialog presents this tool's result. Must agree with the
    /// [`ToolOutput`] variant [`Self::compute`] returns.
    pub(crate) fn result_kind(self) -> ToolResultKind {
        match self {
            Self::JsonFormatter => ToolResultKind::InPlaceText,
            Self::DiffChecker => ToolResultKind::Diff,
            Self::Base64Encoder | Self::Base64Decoder => ToolResultKind::SeparateText,
        }
    }

    /// Label of the dialog's primary action button.
    pub(crate) fn action_label(self) -> &'static str {
        match self {
            Self::JsonFormatter => "Format",
            Self::DiffChecker => "Diff",
            Self::Base64Encoder => "Encode",
            Self::Base64Decoder => "Decode",
        }
    }

    /// Test id of the primary action button.
    pub(crate) fn action_id(self) -> &'static str {
        match self {
            Self::JsonFormatter => "tool-format",
            Self::DiffChecker => "tool-diff",
            Self::Base64Encoder => "tool-encode",
            Self::Base64Decoder => "tool-decode",
        }
    }

    /// Derive the tool's result from the current inputs, in slot order.
    /// Errors are user-facing text and leave the input untouched.
    pub(crate) fn compute(self, inputs: &[String]) -> Result<ToolOutput, String> {
        match self {
            Self::JsonFormatter => {
                json::format(inputs.first().map(String::as_str).unwrap_or_default())
                    .map(ToolOutput::Text)
            }
            Self::DiffChecker => Ok(ToolOutput::Diff(diff::compare(
                inputs.first().map(String::as_str).unwrap_or_default(),
                inputs.get(1).map(String::as_str).unwrap_or_default(),
            ))),
            Self::Base64Encoder => Ok(ToolOutput::ResultText(base64::encode(
                inputs.first().map(String::as_str).unwrap_or_default(),
            ))),
            Self::Base64Decoder => {
                base64::decode(inputs.first().map(String::as_str).unwrap_or_default())
                    .map(ToolOutput::ResultText)
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

    /// The dialog lays itself out from `result_kind` and then renders whatever
    /// `compute` returns, so the two must agree for every tool.
    #[test]
    fn compute_returns_the_result_kind_the_tool_declares() {
        for tool in ToolKind::ALL {
            let inputs: Vec<String> = vec![String::new(); tool.inputs().len()];
            let output = tool.compute(&inputs);
            match (tool.result_kind(), output) {
                (ToolResultKind::InPlaceText, Ok(ToolOutput::Text(_))) => {}
                (ToolResultKind::SeparateText, Ok(ToolOutput::ResultText(_))) => {}
                (ToolResultKind::Diff, Ok(ToolOutput::Diff(_))) => {}
                (kind, output) => panic!("{tool:?} declares {kind:?} but computed {output:?}"),
            }
        }
    }

    #[test]
    fn the_diff_tool_compares_its_two_slots() {
        let inputs = vec!["one\ntwo\n".to_owned(), "one\nTWO\n".to_owned()];
        let Ok(ToolOutput::Diff(result)) = ToolKind::DiffChecker.compute(&inputs) else {
            panic!("the diff tool must produce a diff");
        };
        assert_eq!((result.additions, result.deletions), (1, 1));
        assert_eq!(result.hunks.len(), 1);
    }
}
