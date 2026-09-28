//! Agent-authored explanations refer to immutable, app-owned diff excerpts.
use std::collections::HashSet;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::review::model::{FileContent, HunkLine, LineTag, ReviewDiff};
use crate::review::syntax::{self, SyntaxSpan};

const EXCERPT_LINES: usize = 48;
const PROMPT_BYTES: usize = 160_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Tutorial {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub concepts: Vec<Concept>,
    pub chapters: Vec<Chapter>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Concept {
    pub term: String,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Chapter {
    pub title: String,
    pub summary: String,
    pub rationale: String,
    pub checkpoints: Vec<String>,
    pub excerpt_ids: Vec<String>,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub invariants: Vec<Invariant>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    #[default]
    Inferred,
    Source,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inferred => "AI-inferred assumption · unverified",
            Self::Source => "Source-based interpretation · unverified",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Invariant {
    pub statement: String,
    pub origin: Origin,
    pub excerpt_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TestEvidence {
    pub behavior: String,
    /// None is an explicit evidence gap, never a claim that no test exists.
    pub test_name: Option<String>,
    pub excerpt_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Excerpt {
    pub id: String,
    pub path: String,
    pub lines: Vec<HunkLine>,
    pub supplied: bool,
    pub light_spans: Vec<Vec<SyntaxSpan>>,
    pub dark_spans: Vec<Vec<SyntaxSpan>>,
}

impl Excerpt {
    pub fn label(&self) -> String {
        let old = self
            .lines
            .iter()
            .filter_map(|line| line.old_no)
            .collect::<Vec<_>>();
        let new = self
            .lines
            .iter()
            .filter_map(|line| line.new_no)
            .collect::<Vec<_>>();
        let range = |lines: &[u32]| match (lines.first(), lines.last()) {
            (Some(first), Some(last)) => format!("{first}–{last}"),
            _ => "—".into(),
        };
        format!("{} · old {} / new {}", self.path, range(&old), range(&new))
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                let prefix = match line.tag {
                    LineTag::Context => ' ',
                    LineTag::Deletion => '-',
                    LineTag::Addition => '+',
                };
                format!("{prefix}{}\n", line.text)
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub capture: String,
    pub excerpts: Vec<Excerpt>,
    /// File-level gaps that cannot honestly count as covered by a text excerpt.
    pub gaps: Vec<String>,
    pub prompt: String,
}

impl Snapshot {
    pub fn new(diff: &ReviewDiff) -> Self {
        let highlights = syntax::highlight(diff);
        let mut snapshot = Self {
            capture: String::new(),
            excerpts: Vec::new(),
            gaps: Vec::new(),
            prompt: String::new(),
        };
        for (file_index, file) in diff.files.iter().enumerate() {
            match &file.content {
                FileContent::Text { hunks, truncated } => {
                    let before = snapshot.excerpts.len();
                    for (hunk_index, hunk) in hunks.iter().enumerate() {
                        for (chunk_index, lines) in hunk.lines.chunks(EXCERPT_LINES).enumerate() {
                            if !lines.iter().any(|line| line.tag != LineTag::Context) {
                                continue;
                            }
                            let mut excerpt = Excerpt {
                                id: format!("f{file_index}-h{hunk_index}-p{chunk_index}"),
                                path: file.path.clone(),
                                lines: lines.to_vec(),
                                supplied: false,
                                light_spans: (0..lines.len())
                                    .map(|line| {
                                        highlights
                                            .line(
                                                false,
                                                file_index,
                                                hunk_index,
                                                chunk_index * EXCERPT_LINES + line,
                                            )
                                            .to_vec()
                                    })
                                    .collect(),
                                dark_spans: (0..lines.len())
                                    .map(|line| {
                                        highlights
                                            .line(
                                                true,
                                                file_index,
                                                hunk_index,
                                                chunk_index * EXCERPT_LINES + line,
                                            )
                                            .to_vec()
                                    })
                                    .collect(),
                            };
                            let entry = format!(
                                "\nEXCERPT {} | {} ({})\n{}",
                                excerpt.id,
                                excerpt.label(),
                                file.status.label(),
                                excerpt.text()
                            );
                            if snapshot.prompt.len() + entry.len() <= PROMPT_BYTES {
                                snapshot.prompt.push_str(&entry);
                                excerpt.supplied = true;
                            }
                            snapshot.excerpts.push(excerpt);
                        }
                    }
                    if *truncated {
                        snapshot
                            .gaps
                            .push(format!("{} — diff truncated", file.path));
                    }
                    if snapshot.excerpts.len() == before && !*truncated {
                        snapshot
                            .gaps
                            .push(format!("{} — metadata-only change", file.path));
                    }
                }
                FileContent::Unavailable(reason) => {
                    snapshot
                        .gaps
                        .push(format!("{} — {}", file.path, reason.label()))
                }
            }
            if let Some(old) = &file.old_path {
                snapshot.gaps.push(format!(
                    "{} — renamed from {old}; check rename metadata",
                    file.path
                ));
            }
        }
        if snapshot.excerpts.iter().any(|excerpt| !excerpt.supplied) {
            snapshot.prompt.push_str("\nSome excerpts were omitted to fit the input limit. Do not claim complete coverage.\n");
        }
        snapshot.prompt.push_str(&format!(
            "\nAdditional changes requiring direct review:\n{}",
            snapshot.gaps.join("\n")
        ));
        // Include omitted excerpts and metadata, not only the prompt. Working-tree
        // changes can differ while base and HEAD remain identical.
        let identity = format!(
            "{}\n{}\n{:?}\n{:?}\n{:?}",
            diff.base_commit, diff.head_commit, diff.base_ref, diff.head_branch, diff.files
        );
        snapshot.capture = gix::objs::compute_hash(
            gix::hash::Kind::Sha1,
            gix::objs::Kind::Blob,
            identity.as_bytes(),
        )
        .expect("SHA-1 is enabled")
        .to_string();
        snapshot
    }

    pub fn uncovered<'a>(&'a self, tutorial: &Tutorial) -> Vec<&'a Excerpt> {
        let covered: HashSet<_> = tutorial
            .chapters
            .iter()
            .flat_map(|chapter| &chapter.excerpt_ids)
            .collect();
        self.excerpts
            .iter()
            .filter(|excerpt| !covered.contains(&excerpt.id))
            .collect()
    }
}

impl Tutorial {
    pub fn parse_generated(
        output: &str,
        snapshot: &Snapshot,
        include_concepts: bool,
    ) -> Result<Self> {
        let tutorial = Self::parse(output, snapshot, include_concepts)?;
        if tutorial.chapters.iter().any(|chapter| {
            chapter.before.trim().is_empty()
                || chapter.after.trim().is_empty()
                || chapter.invariants.is_empty()
                || chapter.tests.is_empty()
        }) {
            bail!(
                "generated behavior chapters need before/after descriptions, an invariant to investigate, and test evidence or an explicit gap"
            );
        }
        Ok(tutorial)
    }

    pub fn parse(output: &str, snapshot: &Snapshot, include_concepts: bool) -> Result<Self> {
        let trimmed = output.trim();
        let json = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .and_then(|body| body.strip_suffix("```"))
            .map(str::trim)
            .unwrap_or(trimmed);
        let mut tutorial: Self = serde_json::from_str(json).context("reading guide JSON")?;
        if tutorial.title.trim().is_empty()
            || tutorial.summary.trim().is_empty()
            || tutorial.chapters.is_empty()
            || tutorial.chapters.len() > 24
        {
            bail!("guide needs a title, summary and 1–24 chapters");
        }
        let supplied: HashSet<_> = snapshot
            .excerpts
            .iter()
            .filter(|excerpt| excerpt.supplied)
            .map(|excerpt| &excerpt.id)
            .collect();
        for chapter in &tutorial.chapters {
            if chapter.title.trim().is_empty()
                || chapter.summary.trim().is_empty()
                || chapter.rationale.trim().is_empty()
                || chapter.checkpoints.is_empty()
                || chapter.checkpoints.len() > 2
                || chapter
                    .checkpoints
                    .iter()
                    .any(|check| check.trim().is_empty())
            {
                bail!("each chapter needs a title, summary, rationale and one or two checks");
            }
            if chapter.excerpt_ids.is_empty() || chapter.excerpt_ids.len() > 8 {
                bail!("each chapter needs 1–8 actual diff excerpt IDs");
            }
            let mut seen = HashSet::new();
            for id in &chapter.excerpt_ids {
                if !supplied.contains(id) {
                    bail!("unknown or unsupplied diff excerpt: {id}");
                }
                if !seen.insert(id) {
                    bail!("duplicate excerpt in chapter: {id}");
                }
            }
            if chapter.before.trim().is_empty() != chapter.after.trim().is_empty() {
                bail!("before and after must both describe the behavior change");
            }
            if chapter.invariants.len() > 4 || chapter.tests.len() > 6 {
                bail!("each chapter allows at most four invariants and six test evidence entries");
            }
            let validate_evidence = |ids: &[String]| -> Result<()> {
                let mut seen = HashSet::new();
                for id in ids {
                    if !chapter.excerpt_ids.contains(id) || !seen.insert(id) {
                        bail!("evidence must reference distinct excerpts in its chapter: {id}");
                    }
                }
                Ok(())
            };
            for invariant in &chapter.invariants {
                if invariant.statement.trim().is_empty() || invariant.excerpt_ids.is_empty() {
                    bail!("an invariant needs a statement and captured evidence");
                }
                validate_evidence(&invariant.excerpt_ids)?;
            }
            for test in &chapter.tests {
                if test.behavior.trim().is_empty()
                    || test
                        .test_name
                        .as_ref()
                        .is_some_and(|name| name.trim().is_empty())
                    || test.test_name.is_some() == test.excerpt_ids.is_empty()
                {
                    bail!(
                        "test evidence needs a behavior and either a named test with excerpts or an explicit gap without excerpts"
                    );
                }
                validate_evidence(&test.excerpt_ids)?;
                if let Some(name) = &test.test_name {
                    let found = snapshot
                        .excerpts
                        .iter()
                        .filter(|excerpt| test.excerpt_ids.contains(&excerpt.id))
                        .flat_map(|excerpt| &excerpt.lines)
                        .any(|line| line.tag != LineTag::Deletion && line.text.contains(name));
                    if !found {
                        bail!("test name must appear in its cited current-side evidence: {name}");
                    }
                }
            }
        }
        if !include_concepts {
            tutorial.concepts.clear();
        }
        if (include_concepts && tutorial.concepts.is_empty())
            || tutorial.concepts.len() > 8
            || tutorial.concepts.iter().any(|concept| {
                concept.term.trim().is_empty() || concept.explanation.trim().is_empty()
            })
        {
            bail!("when requested, concepts must contain one to eight named explanations");
        }
        Ok(tutorial)
    }

    pub fn sample(snapshot: &Snapshot, include_concepts: bool) -> Self {
        Self {
            title: "Explore the chapter reader".into(),
            summary: "Layout preview using your actual diff. These sample explanations are placeholders, not an agent review.".into(),
            concepts: if include_concepts { vec![Concept { term: "Review checkpoints".into(), explanation: "A checkpoint is a question to verify in the code. Reading a chapter does not automatically mark it reviewed.".into() }] } else { vec![] },
            chapters: snapshot.excerpts.iter().filter(|excerpt| excerpt.supplied).take(3).enumerate().map(|(index, excerpt)| Chapter {
                title: format!("{} · Follow the changed behavior", index + 1),
                summary: "Start with the entry point and follow the changed value through its callers. The adjacent excerpt is from the selected comparison.".into(),
                rationale: "Understanding execution order helps connect a change to the behavior it is meant to provide.".into(),
                checkpoints: vec!["Check the failure path and how the caller handles it.".into()],
                excerpt_ids: vec![excerpt.id.clone()],
                details: Some("Use **Ask about this chapter** for a focused explanation. The generated guide will group related excerpts by behavior, including changes across files.".into()),
                before: "Sample: the caller proceeds without an explicit failure check.".into(),
                after: "Sample: the caller handles the failure before proceeding. Verify the actual change in the captured code.".into(),
                invariants: vec![Invariant {
                    statement: "Sample assumption: failure leaves the caller in a recoverable state.".into(),
                    origin: Origin::Inferred,
                    excerpt_ids: vec![excerpt.id.clone()],
                }],
                tests: vec![TestEvidence {
                    behavior: "Failure recovery".into(),
                    test_name: None,
                    excerpt_ids: vec![],
                }],
            }).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::model::{ChangedFile, FileStatus, Hunk};

    fn diff() -> ReviewDiff {
        ReviewDiff {
            files: vec![ChangedFile {
                path: "removed.rs".into(),
                old_path: None,
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 100,
                content: FileContent::Text {
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: 100,
                        new_start: 0,
                        new_lines: 0,
                        collapsed_before: 0,
                        lines: (1..=100)
                            .map(|n| HunkLine {
                                tag: LineTag::Deletion,
                                old_no: Some(n),
                                new_no: None,
                                text: format!("line {n}"),
                            })
                            .collect(),
                    }],
                    truncated: true,
                },
            }],
            base_commit: "base".into(),
            head_commit: "head".into(),
            base_ref: None,
            head_branch: None,
        }
    }

    #[test]
    fn syntax_spans_keep_multiline_context_across_excerpt_boundaries() {
        let mut diff = diff();
        let FileContent::Text { hunks, .. } = &mut diff.files[0].content else {
            unreachable!()
        };
        hunks[0].lines[0].text = "/* opening comment".into();
        hunks[0].lines[48].text = "fn still_in_comment() {}".into();
        hunks[0].lines[49].text = "*/".into();
        hunks[0].lines[50].text = "fn real_code() {}".into();
        let snapshot = Snapshot::new(&diff);
        let excerpt = &snapshot.excerpts[1];
        for spans in [&excerpt.light_spans, &excerpt.dark_spans] {
            assert_eq!(spans.len(), excerpt.lines.len());
            let comment_color = spans[0]
                .iter()
                .find(|span| span.range.contains(&0))
                .unwrap()
                .rgba;
            let keyword_color = spans[2]
                .iter()
                .find(|span| span.range.contains(&0))
                .unwrap()
                .rgba;
            assert_ne!(comment_color, keyword_color);
        }
    }

    #[test]
    fn chunks_are_bounded_and_keep_deleted_side_and_truncation() {
        let snapshot = Snapshot::new(&diff());
        assert_eq!(snapshot.excerpts.len(), 3);
        assert_eq!(snapshot.excerpts[0].lines.len(), 48);
        assert_eq!(snapshot.excerpts[2].lines[3].old_no, Some(100));
        assert!(snapshot.excerpts[0].label().contains("old 1–48 / new —"));
        assert_eq!(snapshot.gaps.len(), 1);
    }

    #[test]
    fn rejects_invented_references_and_measures_excerpt_coverage() {
        let snapshot = Snapshot::new(&diff());
        let mut tutorial = Tutorial::sample(&snapshot, true);
        tutorial.chapters.truncate(1);
        assert_eq!(snapshot.uncovered(&tutorial).len(), 2);
        let json = serde_json::to_string(&tutorial).unwrap();
        assert!(
            Tutorial::parse(&format!("```json\n{json}\n```"), &snapshot, false)
                .unwrap()
                .concepts
                .is_empty()
        );
        tutorial.chapters[0].excerpt_ids = vec!["made-up".into()];
        assert!(
            Tutorial::parse(&serde_json::to_string(&tutorial).unwrap(), &snapshot, true).is_err()
        );
        tutorial.chapters[0].excerpt_ids = vec![snapshot.excerpts[0].id.clone(); 2];
        assert!(
            Tutorial::parse(&serde_json::to_string(&tutorial).unwrap(), &snapshot, true).is_err()
        );
    }

    #[test]
    fn requested_concepts_cannot_silently_disappear() {
        let snapshot = Snapshot::new(&diff());
        let tutorial = Tutorial::sample(&snapshot, false);
        let json = serde_json::to_string(&tutorial).unwrap();
        assert!(Tutorial::parse(&json, &snapshot, true).is_err());
        assert!(Tutorial::parse(&json, &snapshot, false).is_ok());
    }

    #[test]
    fn input_limit_does_not_misrepresent_omitted_excerpts() {
        let mut diff = diff();
        if let FileContent::Text { hunks, .. } = &mut diff.files[0].content {
            for line in &mut hunks[0].lines {
                line.text = "x".repeat(4000);
            }
        }
        let snapshot = Snapshot::new(&diff);
        assert!(snapshot.excerpts.iter().any(|e| !e.supplied));
        let mut tutorial = Tutorial::sample(&Snapshot::new(&self::diff()), false);
        tutorial.chapters[0].excerpt_ids = vec![snapshot.excerpts[0].id.clone()];
        assert!(
            Tutorial::parse(&serde_json::to_string(&tutorial).unwrap(), &snapshot, false).is_err()
        );
    }

    #[test]
    fn evidence_cannot_escape_its_chapter_or_masquerade_as_a_test() {
        let mut diff = diff();
        if let FileContent::Text { hunks, .. } = &mut diff.files[0].content {
            hunks[0].lines[0].text = "fn test_failure() {".into();
            hunks[0].lines[0].tag = LineTag::Context;
        }
        let snapshot = Snapshot::new(&diff);
        let mut tutorial = Tutorial::sample(&snapshot, false);
        let parse = |tutorial: &Tutorial| {
            Tutorial::parse(&serde_json::to_string(tutorial).unwrap(), &snapshot, false)
        };
        assert!(parse(&tutorial).is_ok());
        tutorial.chapters[0].invariants[0].excerpt_ids = vec![snapshot.excerpts[1].id.clone()];
        assert!(parse(&tutorial).is_err());
        tutorial.chapters[0].invariants[0].excerpt_ids = vec![snapshot.excerpts[0].id.clone(); 2];
        assert!(parse(&tutorial).is_err());
        tutorial.chapters[0].invariants[0].excerpt_ids = vec![snapshot.excerpts[0].id.clone()];
        tutorial.chapters[0].tests[0].test_name = Some("test_failure".into());
        assert!(
            parse(&tutorial).is_err(),
            "a named test requires captured evidence"
        );
        tutorial.chapters[0].tests[0].excerpt_ids = vec![snapshot.excerpts[0].id.clone()];
        assert!(parse(&tutorial).is_ok());
        tutorial.chapters[0].tests[0].test_name = Some("invented_test".into());
        assert!(
            parse(&tutorial).is_err(),
            "names must occur in cited source"
        );
        tutorial.chapters[0].tests[0].test_name = None;
        assert!(
            parse(&tutorial).is_err(),
            "gaps cannot masquerade as evidence"
        );
    }

    #[test]
    fn older_guides_remain_readable_and_worktree_edits_change_capture_identity() {
        let original = diff();
        let snapshot = Snapshot::new(&original);
        let mut json = serde_json::to_value(Tutorial::sample(&snapshot, false)).unwrap();
        for chapter in json["chapters"].as_array_mut().unwrap() {
            for field in ["before", "after", "invariants", "tests"] {
                chapter.as_object_mut().unwrap().remove(field);
            }
        }
        let parsed = Tutorial::parse(&json.to_string(), &snapshot, false).unwrap();
        assert!(Tutorial::parse_generated(&json.to_string(), &snapshot, false).is_err());
        assert!(parsed.chapters[0].invariants.is_empty());
        let mut changed = original.clone();
        if let FileContent::Text { hunks, .. } = &mut changed.files[0].content {
            hunks[0].lines[99].text = "changed outside the first chapter".into();
        }
        assert_eq!(snapshot.capture, Snapshot::new(&original).capture);
        assert_ne!(snapshot.capture, Snapshot::new(&changed).capture);
        assert_eq!(original.head_commit, changed.head_commit);
    }
}
