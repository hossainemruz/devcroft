//! Provider-neutral handoff for a turn in an interactive agent session.
use std::{io::Read as _, path::Path};

use anyhow::{Context as _, Result, ensure};

const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

pub(super) struct Exchange {
    directory: tempfile::TempDir,
}

impl Exchange {
    pub fn new(prompt: &str) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("devcroft-review-")
            .tempdir()?;
        let result = directory.path().join("result.txt");
        let complete = directory.path().join("complete");
        let instructions = format!(
            "{prompt}\n\nDevcroft result delivery (the only permitted file writes for this task):\nWrite the full requested response to {} as UTF-8. After closing that file, write `done` to {}. Do not create the completion marker before the entire response is saved. Do not modify repository files. Explain your progress in the terminal; the app loads the saved response. If a reference is unavailable, say so rather than inventing its contents.\n",
            quoted(&result),
            quoted(&complete),
        );
        std::fs::write(directory.path().join("instructions.md"), instructions)?;
        Ok(Self { directory })
    }

    pub fn task(&self) -> String {
        format!(
            "# Devcroft review task: Read {} and carry out the tutorial instructions, including result delivery. Keep repository files unchanged.",
            quoted(&self.directory.path().join("instructions.md")),
        )
    }

    /// A separate marker prevents partially-written output from being loaded.
    pub fn read_completed(&self) -> Result<Option<String>> {
        if !self.directory.path().join("complete").try_exists()? {
            return Ok(None);
        }
        let path = self.directory.path().join("result.txt");
        let metadata = std::fs::symlink_metadata(&path)
            .context("Agent completed without saving a response")?;
        ensure!(metadata.is_file(), "Agent response must be a regular file");
        ensure!(
            metadata.len() <= OUTPUT_LIMIT,
            "Agent response exceeds 2 MiB"
        );
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(OUTPUT_LIMIT + 1)
            .read_to_string(&mut text)?;
        ensure!(
            text.len() as u64 <= OUTPUT_LIMIT,
            "Agent response exceeds 2 MiB"
        );
        ensure!(!text.trim().is_empty(), "Agent saved an empty response");
        Ok(Some(text))
    }
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_complete_output_and_cleans_up() {
        let exchange = Exchange::new("Generate JSON").unwrap();
        let directory = exchange.directory.path().to_owned();
        std::fs::write(directory.join("result.txt"), "partial").unwrap();
        assert!(exchange.read_completed().unwrap().is_none());
        std::fs::write(directory.join("result.txt"), "finished").unwrap();
        std::fs::write(directory.join("complete"), "done").unwrap();
        assert_eq!(
            exchange.read_completed().unwrap().as_deref(),
            Some("finished")
        );
        drop(exchange);
        assert!(!directory.exists());
    }

    #[test]
    fn rejects_missing_empty_and_oversized_responses() {
        let exchange = Exchange::new("Generate JSON").unwrap();
        let directory = exchange.directory.path();
        std::fs::write(directory.join("complete"), "done").unwrap();
        assert!(exchange.read_completed().is_err());
        std::fs::write(directory.join("result.txt"), " ").unwrap();
        assert!(exchange.read_completed().is_err());
        std::fs::File::create(directory.join("result.txt"))
            .unwrap()
            .set_len(OUTPUT_LIMIT + 1)
            .unwrap();
        assert!(exchange.read_completed().is_err());
    }

    #[test]
    fn each_turn_has_an_isolated_result_and_shell_safe_task() {
        let first = Exchange::new("First").unwrap();
        let second = Exchange::new("Second").unwrap();
        assert_ne!(first.directory.path(), second.directory.path());
        assert!(first.task().starts_with("# "));
        assert!(!first.task().contains(['\n', '\r', '\x1b']));
        let instructions =
            std::fs::read_to_string(first.directory.path().join("instructions.md")).unwrap();
        assert!(instructions.contains("First"));
        assert!(instructions.contains("only permitted file writes"));
    }
}
