//! Machine-local persistence for tool inputs and settings.
//!
//! One file per input slot: `tools/<tool-id>/<slot>.txt` under the data
//! root, beside `device.json` and never inside `portable/`. Tool scratch
//! content is machine-local by design, so it must not ride portable sync,
//! and keeping it out of `device.json` keeps arbitrarily large pastes away
//! from the small startup state file.
//!
//! Small tool preferences (the diff tool's language) use the same directory
//! with a `.setting` extension, so a setting name can never collide with an
//! input slot's file.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use super::{ToolInput, ToolKind};
use crate::data::{DataRoot, write_text_atomic};

/// Reader/writer for one data root. Cheap to clone (the root is a path).
#[derive(Clone)]
pub(crate) struct ToolStore {
    root: DataRoot,
}

impl ToolStore {
    pub(crate) fn new(root: &DataRoot) -> Self {
        Self { root: root.clone() }
    }

    /// The file backing one input slot.
    pub(crate) fn path(&self, tool: ToolKind, input: &ToolInput) -> PathBuf {
        self.root
            .root()
            .join("tools")
            .join(tool.id())
            .join(format!("{}.txt", input.file_name))
    }

    /// The file backing one named tool setting.
    pub(crate) fn setting_path(&self, tool: ToolKind, name: &str) -> PathBuf {
        self.root
            .root()
            .join("tools")
            .join(tool.id())
            .join(format!("{name}.setting"))
    }

    /// Read a persisted input. A missing file is an empty input, not an
    /// error: a tool opens empty the first time and after an explicit clear.
    pub(crate) fn load(&self, tool: ToolKind, input: &ToolInput) -> Result<String> {
        read_optional(&self.path(tool, input))
    }

    /// Replace a persisted input atomically. The empty string clears it.
    pub(crate) fn save(&self, tool: ToolKind, input: &ToolInput, text: &str) -> Result<()> {
        write_text_atomic(&self.path(tool, input), text)
    }

    /// Read a persisted setting. A missing file is the default, not an error.
    pub(crate) fn load_setting(&self, tool: ToolKind, name: &str) -> Result<String> {
        read_optional(&self.setting_path(tool, name))
    }

    /// Replace a persisted setting atomically.
    pub(crate) fn save_setting(&self, tool: ToolKind, name: &str, text: &str) -> Result<()> {
        write_text_atomic(&self.setting_path(tool, name), text)
    }
}

/// A missing file is an absent value, not a failure: a tool opens with its
/// defaults the first time and after an explicit clear.
fn read_optional(path: &std::path::Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> ToolInput {
        ToolKind::JsonFormatter.inputs()[0]
    }

    #[test]
    fn paths_live_under_the_machine_local_tools_directory() {
        let root = DataRoot::new(PathBuf::from("/tmp/data"));
        let store = ToolStore::new(&root);
        assert_eq!(
            store.path(ToolKind::JsonFormatter, &slot()),
            PathBuf::from("/tmp/data/tools/json-formatter/input.txt")
        );
    }

    #[test]
    fn round_trips_text_and_treats_missing_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolStore::new(&DataRoot::new(dir.path().to_owned()));
        let tool = ToolKind::JsonFormatter;
        let slot = slot();
        assert_eq!(store.load(tool, &slot).unwrap(), "");
        store.save(tool, &slot, "{\"a\":1}").unwrap();
        assert_eq!(store.load(tool, &slot).unwrap(), "{\"a\":1}");
        // Multiline and non-ASCII content survives byte-for-byte.
        let text = "{\n  \"é\": \"✓\"\n}\n";
        store.save(tool, &slot, text).unwrap();
        assert_eq!(store.load(tool, &slot).unwrap(), text);
        // An explicit clear leaves a readable empty file, not a missing one.
        store.save(tool, &slot, "").unwrap();
        assert_eq!(store.load(tool, &slot).unwrap(), "");
        assert!(store.path(tool, &slot).is_file());
        // No temp litter beside the target.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("tools/json-formatter"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn settings_round_trip_beside_the_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolStore::new(&DataRoot::new(dir.path().to_owned()));
        let tool = ToolKind::DiffChecker;
        // Missing settings read as the default rather than failing.
        assert_eq!(store.load_setting(tool, "language").unwrap(), "");
        store.save_setting(tool, "language", "Rust").unwrap();
        assert_eq!(store.load_setting(tool, "language").unwrap(), "Rust");
        // The setting lives beside the tool's inputs, with its own extension
        // so it can never collide with an input slot.
        assert_eq!(
            store.setting_path(tool, "language"),
            dir.path().join("tools/diff-checker/language.setting")
        );
        for input in tool.inputs() {
            assert_ne!(
                store.setting_path(tool, "language"),
                store.path(tool, input)
            );
        }
    }

    #[test]
    fn save_creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolStore::new(&DataRoot::new(dir.path().join("fresh")));
        store.save(ToolKind::JsonFormatter, &slot(), "x").unwrap();
        assert!(store.path(ToolKind::JsonFormatter, &slot()).is_file());
    }
}
