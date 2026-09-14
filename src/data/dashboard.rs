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
    pub(crate) const ALL: [Self; 3] = [Self::WaitingForReview, Self::ToReview, Self::Watching];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ToReview => "To Review",
            Self::WaitingForReview => "Waiting for Approval",
            Self::Watching => "Watching",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Group {
    #[default]
    Personal,
    Work,
}

impl Group {
    pub(crate) const ALL: [Self; 2] = [Self::Personal, Self::Work];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Personal => "Personal",
            Self::Work => "Work",
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
    /// Retired free-form tag, superseded by `project`. No UI reads or writes
    /// it; the value round-trips untouched so older files lose nothing.
    #[serde(default)]
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) category: Category,
    /// Personal/Work scope for pull requests and todos. Legacy records
    /// stored this as `pr_group`; `Dashboard::load` migrates that key.
    #[serde(default)]
    pub(crate) group: Group,
    /// Repository key this todo belongs to. Empty means unscoped (no
    /// project). Only meaningful for `Kind::Todo`; other kinds ignore it.
    #[serde(default)]
    pub(crate) project: String,
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

    /// Repository key this todo is scoped to, or `None` when unscoped.
    /// Whitespace-only counts as unscoped so hand-edited JSON stays forgiving.
    pub(crate) fn project_key(&self) -> Option<&str> {
        let trimmed = self.project.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    pub(crate) fn validate(&mut self) -> Result<()> {
        self.title = self.title.trim().to_owned();
        self.url = self.url.trim().to_owned();
        self.label = self.label.trim().to_owned();
        self.project = self.project.trim().to_owned();
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

pub(crate) fn github_pr_url(url: &str) -> Result<String> {
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
        let bytes = match std::fs::read(root.portable_dir().join("dashboard.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).context("Reading dashboard.json"),
        };
        // Migration: the Personal/Work scope used to serialize as `pr_group`.
        // Prefer `group` when both keys are present (an older client may have
        // round-tripped an unknown `group` through `extra` while writing its
        // own `pr_group`), then drop the legacy key so saves stay clean.
        let mut value: Value = serde_json::from_slice(&bytes).context("Reading dashboard.json")?;
        if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
            for item in items.iter_mut() {
                if let Some(record) = item.as_object_mut() {
                    if record.contains_key("group") {
                        record.remove("pr_group");
                    } else if let Some(legacy) = record.remove("pr_group") {
                        record.insert("group".to_owned(), legacy);
                    }
                }
            }
        }
        serde_json::from_value(value).context("Reading dashboard.json")
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

    pub(crate) fn pull_requests(&self, category: Category, group: Option<Group>) -> Vec<Item> {
        self.items
            .iter()
            .filter(|item| {
                item.kind == Kind::PullRequest
                    && item.category == category
                    && group.is_none_or(|group| item.group == group)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn move_pr(&mut self, original: &Item, category: Category) -> Result<()> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == original.id)
            .ok_or_else(|| anyhow::anyhow!("This PR was removed. Reload the board"))?;
        anyhow::ensure!(
            item.kind == Kind::PullRequest && item == original,
            "This PR changed. Try moving it again"
        );
        item.category = category;
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

    /// Scope a todo to a repository (`None`/empty clears to Unscoped).
    /// Non-todo items are left untouched so a stale drop cannot corrupt them.
    pub(crate) fn set_todo_project(&mut self, id: &str, project: Option<&str>) {
        let normalized = project.unwrap_or_default().trim().to_owned();
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|i| i.id == id && i.kind == Kind::Todo)
        {
            item.project = normalized;
        }
    }

    /// Move a todo into a project column, keeping it last among that
    /// project's incomplete todos. A no-op for unknown ids or completed
    /// items (completed cards are not draggable).
    pub(crate) fn move_todo_to_project(&mut self, source: &str, project: Option<&str>) {
        let normalized = project.unwrap_or_default().trim().to_owned();
        let Some(source_index) = self
            .items
            .iter()
            .position(|i| i.id == source && i.kind == Kind::Todo && !i.completed)
        else {
            return;
        };
        let mut item = self.items.remove(source_index);
        item.project = normalized.clone();
        // Append after the last remaining incomplete todo of the target
        // project so the card lands last in its new column; when the column
        // is empty, push to the end (still last within that column).
        let insert_at = self
            .items
            .iter()
            .rposition(|i| i.kind == Kind::Todo && !i.completed && i.project == normalized)
            .map(|index| index + 1)
            .unwrap_or_else(|| self.items.len());
        self.items.insert(insert_at, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_prs_keep_their_column_and_gain_personal_group() {
        let mut doc: Dashboard = serde_json::from_value(serde_json::json!({"items":[{
            "id":"legacy", "kind":"PullRequest", "title":"PR", "url":"https://github.com/a/b/pull/1",
            "category":"WaitingForReview", "futureField":42
        }]})).unwrap();
        let original = doc.items[0].clone();
        assert_eq!(original.group, Group::Personal);
        assert_eq!(original.category.label(), "Waiting for Approval");
        assert_eq!(
            doc.pull_requests(Category::WaitingForReview, Some(Group::Personal))
                .len(),
            1
        );
        assert!(
            doc.pull_requests(Category::WaitingForReview, Some(Group::Work))
                .is_empty()
        );
        doc.move_pr(&original, Category::Watching).unwrap();
        assert!(doc.move_pr(&original, Category::ToReview).is_err());
        let mut moved = doc.items[0].clone();
        moved.group = Group::Work;
        doc.upsert(moved).unwrap();
        assert_eq!(
            doc.pull_requests(Category::Watching, Some(Group::Work))
                .len(),
            1
        );
        assert_eq!(doc.pull_requests(Category::Watching, None).len(), 1);
        assert!(
            doc.pull_requests(Category::WaitingForReview, None)
                .is_empty()
        );
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["items"][0]["futureField"], 42);
        assert_eq!(serde_json::from_value::<Dashboard>(json).unwrap(), doc);
        doc.items.clear();
        assert!(doc.move_pr(&original, Category::Watching).is_err());
    }

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
    #[test]
    fn legacy_todos_gain_personal_group_and_unscoped_project() {
        let mut doc: Dashboard = serde_json::from_value(serde_json::json!({"items":[{
            "id":"legacy", "kind":"Todo", "title":"Old", "futureField": 7
        }]}))
        .unwrap();
        assert_eq!(doc.items[0].group, Group::Personal);
        assert_eq!(doc.items[0].project_key(), None);
        let mut scoped = doc.items[0].clone();
        scoped.group = Group::Work;
        scoped.project = "  website  ".into();
        doc.upsert(scoped).unwrap();
        assert_eq!(doc.items[0].group, Group::Work);
        assert_eq!(doc.items[0].project_key(), Some("website"));
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["items"][0]["futureField"], 7);
        assert_eq!(json["items"][0]["group"], "Work");
        assert_eq!(json["items"][0]["project"], "website");
        assert_eq!(serde_json::from_value::<Dashboard>(json).unwrap(), doc);
    }
    #[test]
    fn legacy_pr_group_key_migrates_to_group_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(temp.path().to_owned());
        std::fs::create_dir_all(root.portable_dir()).unwrap();
        // Pre-rename record: `pr_group` with no `group`.
        std::fs::write(
            root.portable_dir().join("dashboard.json"),
            serde_json::json!({"items":[{
                "id": "legacy", "kind": "Todo", "title": "Old",
                "pr_group": "Work", "project": "website",
            }]})
            .to_string(),
        )
        .unwrap();
        let loaded = Dashboard::load(&root).unwrap();
        assert_eq!(loaded.items[0].group, Group::Work);
        assert_eq!(loaded.items[0].project_key(), Some("website"));
        // Saving drops the legacy key so synced files converge on `group`.
        let snapshot = loaded.clone();
        loaded.save(&root, &snapshot).unwrap();
        let raw: Value = serde_json::from_slice(
            &std::fs::read(root.portable_dir().join("dashboard.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["items"][0]["group"], "Work");
        assert!(raw["items"][0].get("pr_group").is_none());
        // When both keys are present (an older client round-tripped an
        // unknown `group` while writing its own `pr_group`), `group` wins.
        std::fs::write(
            root.portable_dir().join("dashboard.json"),
            serde_json::json!({"items":[{
                "id": "mixed", "kind": "Todo", "title": "Mixed",
                "group": "Work", "pr_group": "Personal",
            }]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(Dashboard::load(&root).unwrap().items[0].group, Group::Work);
    }
    #[test]
    fn todo_project_moves_keep_column_order() {
        let mut doc = Dashboard::default();
        for (id, project) in [("a", ""), ("b", "website"), ("c", "")] {
            let mut item = Item::new(Kind::Todo);
            item.id = id.into();
            item.title = id.into();
            item.project = project.into();
            doc.upsert(item).unwrap();
        }
        doc.set_todo_project("a", Some("website"));
        assert_eq!(
            doc.items.iter().find(|i| i.id == "a").unwrap().project,
            "website"
        );
        doc.move_todo_to_project("c", Some("website"));
        assert_eq!(
            doc.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(doc.items.iter().all(|i| i.project == "website"));
        doc.move_todo_to_project("a", None);
        assert_eq!(
            doc.items
                .iter()
                .find(|i| i.id == "a")
                .unwrap()
                .project_key(),
            None
        );
        assert_eq!(
            doc.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        doc.items
            .iter_mut()
            .find(|i| i.id == "b")
            .unwrap()
            .completed = true;
        doc.move_todo_to_project("b", Some("other"));
        assert_eq!(
            doc.items.iter().find(|i| i.id == "b").unwrap().project,
            "website"
        );
    }
}
