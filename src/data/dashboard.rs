//! Portable Home lists. Stale snapshots are rejected rather than overwriting a sync.
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::{DataRoot, write_json_atomic};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    #[default]
    Todo,
    PullRequest,
    Reading,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Category {
    #[default]
    ToReview,
    WaitingForReview,
    Watching,
}

impl Category {
    pub(crate) const ALL: [Self; 3] = [Self::ToReview, Self::WaitingForReview, Self::Watching];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ToReview => "To Review",
            Self::WaitingForReview => "Waiting for Review",
            Self::Watching => "Watching",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) kind: Kind,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) category: Category,
    #[serde(default)]
    pub(crate) completed: bool,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

impl Item {
    pub(crate) fn new(kind: Kind) -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            id: format!(
                "{time:x}-{:x}-{:x}",
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            kind,
            ..Self::default()
        }
    }

    pub(crate) fn validate(&mut self) -> Result<()> {
        self.title = self.title.trim().to_owned();
        self.url = self.url.trim().to_owned();
        self.label = self.label.trim().to_owned();
        if self.kind != Kind::Todo {
            if self.kind == Kind::PullRequest {
                self.url = github_pr_url(&self.url)?;
            } else if !safe_web_url(&self.url) {
                bail!("Enter an http:// or https:// URL with a host");
            }
            if self.title.is_empty() {
                self.title = self.url.clone();
            }
        }
        if self.title.is_empty() {
            bail!("A title is required");
        }
        Ok(())
    }
}

pub(crate) fn safe_web_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
        && !host.contains('@')
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
}

fn github_pr_url(url: &str) -> Result<String> {
    let parts: Vec<_> = url
        .strip_prefix("https://github.com/")
        .unwrap_or_default()
        .trim_end_matches('/')
        .split('/')
        .collect();
    if parts.len() != 4
        || parts[2] != "pull"
        || !parts[3].bytes().all(|c| c.is_ascii_digit())
        || parts[3].parse::<u64>().unwrap_or(0) == 0
        || parts[..2].iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        bail!("Use a GitHub PR URL: https://github.com/owner/repository/pull/123");
    }
    Ok(format!(
        "https://github.com/{}/{}/pull/{}",
        parts[0],
        parts[1],
        parts[3].parse::<u64>()?
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Dashboard {
    #[serde(default)]
    pub(crate) items: Vec<Item>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

impl Dashboard {
    pub(crate) fn load(root: &DataRoot) -> Result<Self> {
        match std::fs::read(root.portable_dir().join("dashboard.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Reading dashboard.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("Reading dashboard.json"),
        }
    }

    pub(crate) fn save(&self, root: &DataRoot, expected: &Self) -> Result<()> {
        if Self::load(root)? != *expected {
            bail!("Home data changed on disk. Reload Home before saving again.");
        }
        write_json_atomic(&root.portable_dir().join("dashboard.json"), self)
    }

    pub(crate) fn upsert(&mut self, item: Item) -> Result<()> {
        let mut item = item;
        item.validate()?;
        if item.kind == Kind::PullRequest
            && self
                .items
                .iter()
                .any(|i| i.kind == Kind::PullRequest && i.id != item.id && i.url == item.url)
        {
            bail!("This pull request is already tracked");
        }
        if let Some(existing) = self.items.iter_mut().find(|i| i.id == item.id) {
            *existing = item;
        } else {
            self.items.push(item);
        }
        Ok(())
    }

    pub(crate) fn move_todo(&mut self, source: &str, target: &str) {
        let eligible = |i: &Item| i.kind == Kind::Todo && !i.completed;
        let from = self
            .items
            .iter()
            .position(|i| i.id == source && eligible(i));
        let to = self
            .items
            .iter()
            .position(|i| i.id == target && eligible(i));
        if let (Some(from), Some(to)) = (from, to) {
            let item = self.items.remove(from);
            self.items.insert(to, item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_completion_and_unknown_fields_survive_roundtrip() {
        let mut doc: Dashboard = serde_json::from_value(serde_json::json!({
            "futureSetting": true,
            "items": [{ "id": "todo", "kind": "Todo", "title": "Old", "futureField": 42 }]
        }))
        .unwrap();
        let mut todo = doc.items[0].clone();
        todo.title = "  Next item  ".into();
        todo.description = "More context".into();
        todo.label = "work".into();
        todo.completed = true;
        doc.upsert(todo).unwrap();
        let mut reading = Item::new(Kind::Reading);
        reading.url = "https://example.org/article".into();
        doc.upsert(reading).unwrap();
        for category in Category::ALL {
            let mut pr = Item::new(Kind::PullRequest);
            pr.url = format!("https://github.com/a/b/pull/{}", category as usize + 1);
            pr.category = category;
            doc.upsert(pr).unwrap();
        }
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["futureSetting"], true);
        assert_eq!(json["items"][0]["futureField"], 42);
        assert_eq!(doc.items[0].title, "Next item");
        assert_eq!(doc.items[1].title, "https://example.org/article");
        assert_eq!(serde_json::from_value::<Dashboard>(json).unwrap(), doc);
        assert!(doc.upsert(Item::new(Kind::Todo)).is_err());
    }
    #[test]
    fn portable_roundtrip_and_stale_write_protection() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(temp.path().to_owned());
        let initial = Dashboard::load(&root).unwrap();
        let mut next = initial.clone();
        let mut item = Item::new(Kind::Todo);
        item.title = "Write tests".into();
        next.upsert(item).unwrap();
        next.save(&root, &initial).unwrap();
        assert_eq!(Dashboard::load(&root).unwrap(), next);
        assert!(initial.save(&root, &initial).is_err());
        assert!(!root.device_path().exists());
        std::fs::write(root.portable_dir().join("dashboard.json"), "broken").unwrap();
        assert!(Dashboard::load(&root).is_err());
        assert!(next.save(&root, &next).is_err());
    }
    #[test]
    fn urls_and_duplicate_prs() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "https://a b",
            "https://user@host",
        ] {
            assert!(!safe_web_url(url));
        }
        assert!(safe_web_url("https://example.org/article?q=1"));
        assert!(github_pr_url("https://github.com/a/b/pull/1").is_ok());
        for url in [
            "https://github.com.evil/a/b/pull/1",
            "https://github.com/a/b/pull/0",
            "https://github.com/a/b/issues/1",
        ] {
            assert!(github_pr_url(url).is_err());
        }
        let mut doc = Dashboard::default();
        let mut pr = Item::new(Kind::PullRequest);
        pr.url = "https://github.com/a/b/pull/1".into();
        doc.upsert(pr.clone()).unwrap();
        pr.id = "other".into();
        assert!(doc.upsert(pr).is_err());
    }
    #[test]
    fn reorder_both_directions_preserves_items() {
        let mut doc = Dashboard::default();
        for title in ["a", "b", "c"] {
            let mut item = Item::new(Kind::Todo);
            item.id = title.into();
            item.title = title.into();
            doc.upsert(item).unwrap();
        }
        doc.move_todo("a", "c");
        assert_eq!(
            doc.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        doc.move_todo("a", "b");
        assert_eq!(
            doc.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        doc.items[1].completed = true;
        doc.move_todo("b", "a");
        assert_eq!(doc.items[1].id, "b");
    }
}
