//! PR 1 task domain and store. No artifact, CLI, or UI implementation yet.
//! See docs/task-storage.md for the serialized and concurrency contracts.
// Public-to-crate APIs land before their CLI/UI consumers in later PRs.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, hash_map::RandomState};
use std::fs::{self, OpenOptions};
use std::hash::BuildHasher;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::store_lock::{ensure_directory, portable_gate, reject_symlink, task_lock};
use super::{DataRoot, RepositoryMetadata, require_repository_key};

const SCHEMA_VERSION: u32 = 3;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SUBTASKS: usize = 1000;
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
type Extra = BTreeMap<String, Value>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    #[default]
    Todo,
    Doing,
    Blocked,
    Done,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Subtask {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) repository: String,
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
    #[serde(default)]
    pub(crate) status: Status,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Task {
    schema_version: u32,
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) repositories: Vec<String>,
    #[serde(default)]
    pub(crate) subtasks: Vec<Subtask>,
    /// High-water mark survives removal, so a deleted ID is never reused.
    next_subtask_id: u64,
    #[serde(default)]
    pub(crate) archived: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    pub(crate) completed: usize,
    pub(crate) total: usize,
}

impl Progress {
    pub(crate) fn is_planned(&self) -> bool {
        self.total != 0
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.is_planned() && self.completed == self.total
    }
}

impl Task {
    pub(crate) fn progress(&self) -> Progress {
        Progress {
            completed: self
                .subtasks
                .iter()
                .filter(|s| s.status == Status::Done)
                .count(),
            total: self.subtasks.len(),
        }
    }

    pub(crate) fn involved_repositories(&self) -> BTreeSet<String> {
        self.repositories
            .iter()
            .cloned()
            .chain(self.subtasks.iter().map(|s| s.repository.clone()))
            .collect()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported task schema {}",
            self.schema_version
        );
        validate_id(&self.id)?;
        nonblank(&self.title, "task title")?;
        ensure!(
            self.updated_at >= self.created_at,
            "updatedAt precedes createdAt"
        );
        ensure!(self.next_subtask_id > 0, "nextSubtaskId must be positive");
        unique(&self.repositories, "repositories")?;
        for key in self.involved_repositories() {
            require_repository_key(&key)?;
        }
        ensure!(
            self.subtasks.len() <= MAX_SUBTASKS,
            "too many subtasks (maximum {MAX_SUBTASKS})"
        );
        let mut numbers = BTreeSet::new();
        for subtask in &self.subtasks {
            let number = subtask_number(&subtask.id)?;
            ensure!(numbers.insert(number), "duplicate subtask {}", subtask.id);
            ensure!(
                number < self.next_subtask_id,
                "nextSubtaskId must exceed existing subtask IDs"
            );
            nonblank(&subtask.title, "subtask title")?;
            unique(&subtask.dependencies, "dependencies")?;
        }
        // Kahn's algorithm avoids stack recursion on externally edited graphs.
        let indices: HashMap<_, _> = self
            .subtasks
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();
        let mut incoming = vec![0; self.subtasks.len()];
        let mut outgoing = vec![Vec::new(); self.subtasks.len()];
        for (i, s) in self.subtasks.iter().enumerate() {
            for dep in &s.dependencies {
                ensure!(dep != &s.id, "subtask {} depends on itself", s.id);
                let &j = indices
                    .get(dep.as_str())
                    .with_context(|| format!("unknown dependency {dep}"))?;
                incoming[i] += 1;
                outgoing[j].push(i);
            }
        }
        let mut ready: Vec<_> = incoming
            .iter()
            .enumerate()
            .filter_map(|(i, n)| (*n == 0).then_some(i))
            .collect();
        let mut visited = 0;
        while let Some(i) = ready.pop() {
            visited += 1;
            for &j in &outgoing[i] {
                incoming[j] -= 1;
                if incoming[j] == 0 {
                    ready.push(j);
                }
            }
        }
        ensure!(visited == self.subtasks.len(), "subtask dependency cycle");
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NewTask {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) repositories: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskPatch {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) repositories: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NewSubtask {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) repository: String,
    pub(crate) dependencies: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SubtaskPatch {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) repository: Option<String>,
    pub(crate) dependencies: Option<Vec<String>>,
    pub(crate) status: Option<Status>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) task: Task,
    pub(crate) revision: String,
    /// Includes missing/malformed repository records, without making tasks unreadable.
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ListOptions {
    pub(crate) repository: Option<String>,
    pub(crate) include_archived: bool,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct TaskList {
    pub(crate) tasks: Vec<Snapshot>,
    pub(crate) errors: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskStore {
    root: DataRoot,
}

impl TaskStore {
    pub(crate) fn new(root: &DataRoot) -> Self {
        Self { root: root.clone() }
    }

    pub(crate) fn create(&self, input: NewTask) -> Result<Snapshot> {
        self.create_with_ids(input, random_id)
    }

    fn create_with_ids(
        &self,
        input: NewTask,
        mut generate: impl FnMut() -> String,
    ) -> Result<Snapshot> {
        let _gate = portable_gate(&self.root, false)?;
        self.ensure_tasks_dir()?;
        for _ in 0..64 {
            let id = generate();
            validate_id(&id)?;
            let _record = task_lock(&self.root, &id, true)?;
            let dir = self.root.portable_dir().join("tasks").join(&id);
            // Reserve the entire directory; even an incomplete old creation is
            // never overwritten or adopted by collision retry.
            if fs::symlink_metadata(&dir).is_ok() {
                continue;
            }
            let now = timestamp()?;
            let task = Task {
                schema_version: SCHEMA_VERSION,
                id,
                title: input.title.trim().to_owned(),
                description: input.description.clone(),
                repositories: input.repositories.clone(),
                subtasks: Vec::new(),
                next_subtask_id: 1,
                archived: false,
                created_at: now,
                updated_at: now,
                extra: Extra::new(),
            };
            task.validate()?;
            self.validate_new_repositories(&task, None)?;
            match fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
            let path = dir.join("task.json");
            let result = self.write(&path, &task);
            if result.is_err() {
                let _ = fs::remove_dir(&dir);
            }
            return result;
        }
        bail!("could not allocate a unique task ID after 64 attempts")
    }

    pub(crate) fn get(&self, id: &str) -> Result<Snapshot> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        let _record = task_lock(&self.root, id, false)?;
        self.read(id)
    }

    pub(crate) fn list(&self, options: &ListOptions) -> Result<TaskList> {
        if let Some(key) = &options.repository {
            require_repository_key(key)?;
        }
        let _gate = portable_gate(&self.root, false)?;
        let dir = self.checked_path(None)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TaskList::default()),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        let mut result = TaskList::default();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    result.errors.push(e.to_string());
                    continue;
                }
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            // Historical sequence files aren't records. Legacy task directories
            // are reported below, rather than treated as new-format records.
            if id == "sequence.json" {
                continue;
            }
            let loaded = (|| {
                validate_id(&id)?;
                let _record = task_lock(&self.root, &id, false)?;
                self.read(&id)
            })();
            match loaded {
                Ok(snapshot) => {
                    if !options.include_archived && snapshot.task.archived {
                        continue;
                    }
                    if options
                        .repository
                        .as_ref()
                        .is_some_and(|key| !snapshot.task.involved_repositories().contains(key))
                    {
                        continue;
                    }
                    result.tasks.push(snapshot);
                }
                Err(e) => result
                    .errors
                    .push(format!("{}: {e:#}", entry.path().display())),
            }
        }
        result.tasks.sort_by(|a, b| {
            b.task
                .updated_at
                .cmp(&a.task.updated_at)
                .then_with(|| a.task.id.cmp(&b.task.id))
        });
        result.errors.sort();
        let limit = options.limit.unwrap_or(50);
        result.truncated = result.tasks.len() > limit;
        result.tasks.truncate(limit);
        Ok(result)
    }

    pub(crate) fn update(&self, id: &str, revision: &str, patch: TaskPatch) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            if let Some(v) = patch.title {
                task.title = v.trim().to_owned();
            }
            if let Some(v) = patch.description {
                task.description = v;
            }
            if let Some(v) = patch.repositories {
                task.repositories = v;
            }
            Ok(())
        })
    }

    pub(crate) fn set_archived(
        &self,
        id: &str,
        revision: &str,
        archived: bool,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            task.archived = archived;
            Ok(())
        })
    }

    pub(crate) fn create_subtask(
        &self,
        id: &str,
        revision: &str,
        input: NewSubtask,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            let id = format!("s{}", task.next_subtask_id);
            task.next_subtask_id = task
                .next_subtask_id
                .checked_add(1)
                .context("subtask ID space exhausted")?;
            task.subtasks.push(Subtask {
                id,
                title: input.title.trim().to_owned(),
                description: input.description,
                repository: input.repository,
                dependencies: input.dependencies,
                status: Status::Todo,
                extra: Extra::new(),
            });
            Ok(())
        })
    }

    pub(crate) fn update_subtask(
        &self,
        id: &str,
        revision: &str,
        subtask_id: &str,
        patch: SubtaskPatch,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            let s = task
                .subtasks
                .iter_mut()
                .find(|s| s.id == subtask_id)
                .with_context(|| format!("missing subtask {subtask_id}"))?;
            if let Some(v) = patch.title {
                s.title = v.trim().to_owned();
            }
            if let Some(v) = patch.description {
                s.description = v;
            }
            if let Some(v) = patch.repository {
                s.repository = v;
            }
            if let Some(v) = patch.dependencies {
                s.dependencies = v;
            }
            if let Some(v) = patch.status {
                s.status = v;
            }
            Ok(())
        })
    }

    pub(crate) fn remove_subtask(
        &self,
        id: &str,
        revision: &str,
        subtask_id: &str,
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            ensure!(
                !task
                    .subtasks
                    .iter()
                    .any(|s| s.dependencies.iter().any(|d| d == subtask_id)),
                "subtask {subtask_id} is still referenced by dependencies"
            );
            let index = task
                .subtasks
                .iter()
                .position(|s| s.id == subtask_id)
                .with_context(|| format!("missing subtask {subtask_id}"))?;
            task.subtasks.remove(index);
            Ok(())
        })
    }

    pub(crate) fn reorder_subtasks(
        &self,
        id: &str,
        revision: &str,
        order: &[String],
    ) -> Result<Snapshot> {
        self.mutate(id, revision, |task| {
            unique(order, "subtask order")?;
            ensure!(
                order.len() == task.subtasks.len(),
                "order must contain every subtask exactly once"
            );
            let mut reordered = Vec::with_capacity(order.len());
            for id in order {
                reordered.push(
                    task.subtasks
                        .iter()
                        .find(|s| &s.id == id)
                        .with_context(|| format!("missing subtask {id}"))?
                        .clone(),
                );
            }
            task.subtasks = reordered;
            Ok(())
        })
    }

    fn mutate(
        &self,
        id: &str,
        revision: &str,
        change: impl FnOnce(&mut Task) -> Result<()>,
    ) -> Result<Snapshot> {
        validate_id(id)?;
        let _gate = portable_gate(&self.root, false)?;
        let _record = task_lock(&self.root, id, true)?;
        let old = self.read(id)?;
        ensure!(
            old.revision == revision,
            "task_changed: {id} changed on disk; read the latest version and retry"
        );
        let mut task = old.task.clone();
        change(&mut task)?;
        task.updated_at = timestamp()?.max(
            task.updated_at
                .checked_add(1)
                .context("task timestamp exhausted")?,
        );
        task.validate()?;
        self.validate_new_repositories(&task, Some(&old.task))?;
        self.write(&self.checked_path(Some(id))?.join("task.json"), &task)
    }

    fn checked_path(&self, id: Option<&str>) -> Result<PathBuf> {
        let mut path = self.root.portable_dir();
        reject_symlink(&path)?;
        path.push("tasks");
        reject_symlink(&path)?;
        if let Some(id) = id {
            validate_id(id)?;
            path.push(id);
            reject_symlink(&path)?;
        }
        Ok(path)
    }

    fn ensure_tasks_dir(&self) -> Result<()> {
        ensure_directory(&self.root.portable_dir())?;
        ensure_directory(&self.checked_path(None)?)
    }

    fn read(&self, id: &str) -> Result<Snapshot> {
        let path = self.checked_path(Some(id))?.join("task.json");
        let bytes = read_bounded(&path)?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        ensure!(
            value.get("schemaVersion").and_then(Value::as_u64) == Some(SCHEMA_VERSION.into()),
            "unsupported task schema in {} (expected {SCHEMA_VERSION}); record was not modified",
            path.display()
        );
        let task: Task = serde_json::from_value(value)
            .with_context(|| format!("decoding {}", path.display()))?;
        task.validate()
            .with_context(|| format!("validating {}", path.display()))?;
        ensure!(
            task.id == id,
            "task ID does not match directory {}",
            path.display()
        );
        self.snapshot(task, &bytes)
    }

    fn write(&self, path: &Path, task: &Task) -> Result<Snapshot> {
        let mut bytes = serde_json::to_vec_pretty(task)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "task exceeds {MAX_BYTES} byte limit"
        );
        // Prepare the return value before committing: errors must not report a
        // failed mutation after a successful rename.
        let snapshot = self.snapshot(task.clone(), &bytes)?;
        atomic_replace(path, &bytes, || Ok(()))?;
        Ok(snapshot)
    }

    fn snapshot(&self, task: Task, bytes: &[u8]) -> Result<Snapshot> {
        let warnings = task
            .involved_repositories()
            .iter()
            .filter_map(|key| self.require_repository(key).err().map(|e| format!("{e:#}")))
            .collect();
        let revision = format!(
            "git-blob-sha1:{}",
            gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)?
        );
        Ok(Snapshot {
            task,
            revision,
            warnings,
        })
    }

    fn require_repository(&self, key: &str) -> Result<()> {
        require_repository_key(key)?;
        let base = self.root.portable_dir().join("repositories");
        reject_symlink(&base)?;
        let dir = base.join(key);
        reject_symlink(&dir)?;
        let path = dir.join("repository.json");
        let bytes = read_bounded(&path)
            .with_context(|| format!("unknown or unreadable repository {key}"))?;
        serde_json::from_slice::<RepositoryMetadata>(&bytes)
            .with_context(|| format!("malformed repository {key} at {}", path.display()))?;
        Ok(())
    }

    fn validate_new_repositories(&self, task: &Task, old: Option<&Task>) -> Result<()> {
        // Check newly introduced associations, not all existing references. A
        // removed repository must not stop status edits or erase old context.
        for key in &task.repositories {
            if !old.is_some_and(|t| t.repositories.contains(key)) {
                self.require_repository(key)?;
            }
        }
        for s in &task.subtasks {
            if !old.is_some_and(|t| {
                t.subtasks
                    .iter()
                    .any(|before| before.id == s.id && before.repository == s.repository)
            }) {
                self.require_repository(&s.repository)?;
            }
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.strip_prefix("task-").is_some_and(
            |suffix| suffix.len() == 8 && suffix.bytes().all(|b| ALPHABET.contains(&b))
        ),
        "invalid or unsupported task ID {id:?}"
    );
    Ok(())
}

fn subtask_number(id: &str) -> Result<u64> {
    let number = id
        .strip_prefix('s')
        .and_then(|n| n.parse::<u64>().ok())
        .context("invalid subtask ID")?;
    ensure!(
        number > 0 && id == format!("s{number}"),
        "invalid subtask ID {id:?}"
    );
    Ok(number)
}

fn nonblank(value: &str, name: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{name} must not be blank");
    Ok(())
}

fn unique(values: &[String], name: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        nonblank(value, name)?;
        ensure!(seen.insert(value), "duplicate {name}: {value}");
    }
    Ok(())
}

fn timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn random_id() -> String {
    // RandomState supplies independently randomized hash keys from std. These
    // are short collision-checked identifiers, not secrets or access tokens.
    let mut bits = RandomState::new().hash_one((SystemTime::now(), std::process::id()));
    let mut id = String::from("task-");
    for _ in 0..8 {
        id.push(ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char);
        bits /= ALPHABET.len() as u64;
    }
    id
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    reject_symlink(path)?;
    ensure!(
        fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .is_file(),
        "not a regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "{} exceeds {MAX_BYTES} byte limit",
        path.display()
    );
    Ok(bytes)
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    reject_symlink(path)?;
    let tmp = path.with_file_name(format!(".task-{}.tmp", random_id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let result: Result<()> = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_rename()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("atomically writing {}", path.display()))
}

#[cfg(test)]
mod tests;
