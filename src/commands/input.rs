use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};

use crate::cli::planning::{DescriptionInput, DescriptionPatch};

const MAX_MARKDOWN_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn markdown(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        return read_utf8(io::stdin().lock(), "stdin");
    }
    let label = path.display().to_string();
    // Check before opening so directories/devices/FIFOs are not read or waited on.
    let metadata =
        std::fs::metadata(path).with_context(|| format!("reading Markdown file {label}"))?;
    ensure!(
        metadata.is_file(),
        "Markdown input must be a regular file or - for stdin: {label}"
    );
    ensure!(
        metadata.len() <= MAX_MARKDOWN_BYTES,
        "Markdown input {label} exceeds {MAX_MARKDOWN_BYTES} bytes (4 MiB); reduce the document"
    );
    read_utf8(
        File::open(path).with_context(|| format!("opening Markdown file {label}"))?,
        &label,
    )
}

fn read_utf8(reader: impl Read, label: &str) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MARKDOWN_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading Markdown from {label}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_MARKDOWN_BYTES,
        "Markdown input {label} exceeds {MAX_MARKDOWN_BYTES} bytes (4 MiB); reduce the document"
    );
    String::from_utf8(bytes).with_context(|| {
        format!("Markdown input {label} is not valid UTF-8; save or convert it to UTF-8")
    })
}

pub(super) fn description(input: DescriptionInput) -> Result<String> {
    Ok(input
        .description_file
        .as_deref()
        .map(markdown)
        .transpose()?
        .unwrap_or_default())
}

pub(super) fn description_patch(patch: DescriptionPatch) -> Result<Option<String>> {
    text_patch(
        patch.input.description_file.as_deref(),
        patch.clear_description,
    )
}

pub(super) fn text_patch(path: Option<&Path>, clear: bool) -> Result<Option<String>> {
    if clear {
        Ok(Some(String::new()))
    } else {
        path.map(markdown).transpose()
    }
}

pub(super) fn list_patch(values: Vec<String>, clear: bool) -> Option<Vec<String>> {
    (clear || !values.is_empty()).then_some(values)
}
