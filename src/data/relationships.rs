//! Provider → consumer knowledge, shared by the native canvas and headless CLI.
//! Lock order: portable gate, repository/graph lock. Catalog helpers below never
//! reacquire either lock. Layout is deliberately absent from this portable store.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::record::{MAX_BYTES, atomic_replace, random_id, read_bounded, revision};
use super::repositories::{all_repositories_unlocked, get_repository_metadata_unlocked};
use super::store_lock::{portable_gate, reject_symlink, repository_graph_lock};
use super::{DataRoot, RepositoryEntry, require_repository_key};

const MAX_EDGES: usize = 5000;
const MAX_NODES: usize = 1000;
pub(crate) const MAX_DEPTH: u8 = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Relationship {
    pub id: String,
    pub from: String,
    pub to: String,
    pub description: String,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    schema_version: u32,
    relationships: Vec<Relationship>,
    #[serde(flatten, default)]
    extra: BTreeMap<String, Value>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema_version: 1,
            relationships: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Query {
    pub repository: Option<String>,
    /// None on whole-graph reads; repository reads default to one hop.
    pub depth: Option<u8>,
    /// Active-space filter (see [`space_matches_filter`]).
    pub space: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Node {
    #[serde(flatten)]
    pub repository: RepositoryEntry,
    pub unresolved: bool,
    pub distance: Option<usize>,
    /// One shortest undirected neighborhood path, starting at the queried key.
    pub path: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Graph {
    pub format_version: u32,
    pub revision: String,
    pub query: Query,
    pub nodes: Vec<Node>,
    pub relationships: Vec<Relationship>,
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
    /// Relative to query.repository; expanded edges are marked "neighborhood".
    pub directions: BTreeMap<String, String>,
    /// Original cross-group records, so filtering cannot hide their existence.
    pub excluded_relationships: Vec<Relationship>,
    pub diagnostics: Vec<String>,
    pub truncated: bool,
}

impl Node {
    pub(crate) fn key(&self) -> &str {
        &self.repository.key
    }
    pub(crate) fn label(&self) -> &str {
        self.repository
            .display_name
            .as_deref()
            .unwrap_or(self.key())
    }
}

/// Space filter for graph reads: `None` and `All` show every space; any
/// other value matches case-insensitively, with blank stored values counting
/// as the default space (records written before spaces).
pub(crate) fn space_matches_filter(stored: Option<&str>, filter: Option<&str>) -> bool {
    match filter.map(str::trim) {
        None | Some("All") => true,
        Some(filter) => super::spaces::space_matches(stored, filter),
    }
}

fn read_document(root: &DataRoot) -> Result<(Document, String)> {
    reject_symlink(&root.portable_dir())?;
    let path = root.portable_dir().join("repository-relationships.json");
    reject_symlink(&path)?;
    if !path.try_exists()? {
        return Ok((Document::default(), revision(b"")?));
    }
    let bytes = read_bounded(&path)?;
    let doc: Document = serde_json::from_slice(&bytes)
        .context("malformed repository relationships; file left untouched")?;
    validate(&doc)?;
    Ok((doc, revision(&bytes)?))
}

fn validate(doc: &Document) -> Result<()> {
    ensure!(
        doc.schema_version == 1,
        "unsupported repository relationship schema version {}",
        doc.schema_version
    );
    ensure!(
        doc.relationships.len() <= MAX_EDGES,
        "relationship limit is {MAX_EDGES}"
    );
    let mut ids = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    for edge in &doc.relationships {
        ensure!(
            !edge.id.trim().is_empty() && edge.id.len() <= 128,
            "invalid relationship ID"
        );
        require_repository_key(&edge.from)?;
        require_repository_key(&edge.to)?;
        ensure!(edge.from != edge.to, "self-connections are not allowed");
        ensure!(
            !edge.description.trim().is_empty(),
            "relationship description must not be blank"
        );
        ensure!(
            edge.description.len() <= 16_384,
            "relationship description exceeds 16384 bytes"
        );
        ensure!(
            ids.insert(&edge.id),
            "duplicate relationship ID {}",
            edge.id
        );
        ensure!(
            pairs.insert((&edge.from, &edge.to)),
            "duplicate relationship {} → {}; edit the existing connection",
            edge.from,
            edge.to
        );
    }
    Ok(())
}

pub(crate) fn load(root: &DataRoot, query: Query) -> Result<Graph> {
    ensure!(
        query.depth.is_none_or(|d| (1..=MAX_DEPTH).contains(&d)),
        "depth must be 1–{MAX_DEPTH}"
    );
    ensure!(
        query.repository.is_some() || query.depth.is_none(),
        "--depth requires a repository key"
    );
    let _gate = portable_gate(root, false)?;
    let _lock = repository_graph_lock(root, false)?;
    let (doc, revision) = read_document(root)?;
    let (catalog, mut diagnostics) = all_repositories_unlocked(root)?;
    let mut nodes: BTreeMap<String, Node> = catalog
        .into_iter()
        .map(|repository| {
            (
                repository.key.clone(),
                Node {
                    repository,
                    unresolved: false,
                    distance: None,
                    path: Vec::new(),
                },
            )
        })
        .collect();
    for edge in &doc.relationships {
        for key in [&edge.from, &edge.to] {
            if !nodes.contains_key(key) {
                diagnostics.push(format!(
                    "unresolved repository {key}; connections preserved"
                ));
                nodes.insert(
                    key.clone(),
                    Node {
                        repository: RepositoryEntry {
                            key: key.clone(),
                            revision: String::new(),
                            display_name: None,
                            description: None,
                            space: super::spaces::DEFAULT_SPACE.to_owned(),
                            owner: None,
                            name: None,
                            checkout_path: None,
                            checkout_missing: false,
                            last_opened_at: None,
                            warnings: Vec::new(),
                        },
                        unresolved: true,
                        distance: None,
                        path: Vec::new(),
                    },
                );
            }
        }
    }
    let all_keys: BTreeSet<String> = nodes.keys().cloned().collect();
    let mut included = all_keys.clone();
    if let Some(key) = &query.repository {
        ensure!(nodes.contains_key(key), "unknown repository {key}");
        included.clear();
        included.insert(key.clone());
        let node = nodes.get_mut(key).unwrap();
        node.distance = Some(0);
        node.path = vec![key.clone()];
        let mut queue = VecDeque::from([key.clone()]);
        // Sorted adjacency makes shortest-path choice deterministic in cycles.
        let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for edge in &doc.relationships {
            adjacency.entry(&edge.from).or_default().insert(&edge.to);
            adjacency.entry(&edge.to).or_default().insert(&edge.from);
        }
        while let Some(current) = queue.pop_front() {
            let distance = nodes[&current].distance.unwrap();
            if distance >= query.depth.unwrap_or(1) as usize {
                continue;
            }
            let route = nodes[&current].path.clone();
            for next in adjacency.get(current.as_str()).into_iter().flatten() {
                if included.insert((*next).to_owned()) {
                    let node = nodes.get_mut(*next).unwrap();
                    node.distance = Some(distance + 1);
                    node.path = route.clone();
                    node.path.push((*next).to_owned());
                    queue.push_back((*next).to_owned());
                }
            }
        }
    }
    let neighborhood = included.clone();
    included.retain(|key| {
        space_matches_filter(Some(&nodes[key].repository.space), query.space.as_deref())
    });
    let filtered = included.clone();
    let truncated = included.len() > MAX_NODES;
    if truncated {
        included = included.into_iter().take(MAX_NODES).collect();
        // A bounded response must retain the requested node if it matches the filter.
        if let Some(key) = query.repository.as_ref().filter(|k| filtered.contains(*k))
            && !included.contains(key)
        {
            included.pop_last();
            included.insert(key.clone());
        }
        diagnostics.push(format!("node results truncated to {MAX_NODES}"));
    }
    let mut dependencies = BTreeSet::new();
    let mut dependents = BTreeSet::new();
    let mut directions = BTreeMap::new();
    let mut relationships = Vec::new();
    let mut excluded_relationships = Vec::new();
    for edge in doc.relationships {
        if included.contains(&edge.from) && included.contains(&edge.to) {
            if let Some(key) = &query.repository {
                let direction = if &edge.to == key {
                    dependencies.insert(edge.from.clone());
                    "dependency"
                } else if &edge.from == key {
                    dependents.insert(edge.to.clone());
                    "dependent"
                } else {
                    "neighborhood"
                };
                directions.insert(edge.id.clone(), direction.to_owned());
            }
            relationships.push(edge);
        } else if query.space.is_some()
            && (neighborhood.contains(&edge.from) || neighborhood.contains(&edge.to))
            && (!filtered.contains(&edge.from) || !filtered.contains(&edge.to))
            && (filtered.contains(&edge.from) || filtered.contains(&edge.to))
        {
            excluded_relationships.push(edge);
        }
    }
    relationships.sort_by(|a, b| (&a.from, &a.to, &a.id).cmp(&(&b.from, &b.to, &b.id)));
    excluded_relationships.sort_by(|a, b| a.id.cmp(&b.id));
    diagnostics.sort();
    diagnostics.dedup();
    Ok(Graph {
        format_version: 1,
        revision,
        query,
        nodes: nodes
            .into_values()
            .filter(|node| included.contains(node.key()))
            .collect(),
        relationships,
        dependencies: dependencies.into_iter().collect(),
        dependents: dependents.into_iter().collect(),
        directions,
        excluded_relationships,
        diagnostics,
        truncated,
    })
}

#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    Create {
        from: String,
        to: String,
        description: String,
    },
    Update {
        id: String,
        from: Option<String>,
        to: Option<String>,
        description: Option<String>,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MutationResult {
    pub format_version: u32,
    pub revision: String,
    pub id: String,
}

pub(crate) fn mutate(
    root: &DataRoot,
    expected_revision: &str,
    mutation: Mutation,
) -> Result<MutationResult> {
    let _gate = portable_gate(root, false)?;
    let _lock = repository_graph_lock(root, true)?;
    let (mut doc, old_revision) = read_document(root)?;
    ensure!(
        old_revision == expected_revision,
        "stale graph revision; reload before saving (draft was not saved)"
    );
    let id = match mutation {
        Mutation::Create {
            from,
            to,
            description,
        } => {
            let id = loop {
                let id = random_id().replacen("art-", "rel-", 1);
                if !doc.relationships.iter().any(|edge| edge.id == id) {
                    break id;
                }
            };
            let edge = Relationship {
                id: id.clone(),
                from,
                to,
                description,
                extra: BTreeMap::new(),
            };
            validate_endpoints(root, &edge)?;
            reject_duplicate(&doc, &edge)?;
            doc.relationships.push(edge);
            id
        }
        Mutation::Update {
            id,
            from,
            to,
            description,
        } => {
            let old = doc
                .relationships
                .iter()
                .find(|edge| edge.id == id)
                .with_context(|| format!("unknown relationship {id}"))?;
            let mut edge = old.clone();
            if let Some(from) = from {
                edge.from = from;
            }
            if let Some(to) = to {
                edge.to = to;
            }
            if let Some(description) = description {
                edge.description = description;
            }
            // Existing unresolved edges can still have their descriptions repaired.
            if edge.from != old.from || edge.to != old.to {
                validate_endpoints(root, &edge)?;
            }
            reject_duplicate(&doc, &edge)?;
            *doc.relationships.iter_mut().find(|e| e.id == id).unwrap() = edge;
            id
        }
        Mutation::Delete { id } => {
            let before = doc.relationships.len();
            doc.relationships.retain(|edge| edge.id != id);
            ensure!(
                doc.relationships.len() != before,
                "unknown relationship {id}"
            );
            id
        }
    };
    validate(&doc)?;
    doc.relationships.sort_by(|a, b| a.id.cmp(&b.id));
    let mut bytes = serde_json::to_vec_pretty(&doc)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "relationship document exceeds size limit"
    );
    std::fs::create_dir_all(root.portable_dir())?;
    atomic_replace(
        &root.portable_dir().join("repository-relationships.json"),
        &bytes,
        || {
            ensure!(
                read_document(root)?.1 == expected_revision,
                "stale graph revision; reload before saving"
            );
            Ok(())
        },
    )?;
    Ok(MutationResult {
        format_version: 1,
        revision: revision(&bytes)?,
        id,
    })
}

fn validate_endpoints(root: &DataRoot, edge: &Relationship) -> Result<()> {
    for key in [&edge.from, &edge.to] {
        get_repository_metadata_unlocked(root, key)
            .with_context(|| format!("unknown or unreadable endpoint {key}"))?;
    }
    Ok(())
}

fn reject_duplicate(doc: &Document, edge: &Relationship) -> Result<()> {
    if let Some(existing) = doc
        .relationships
        .iter()
        .find(|e| e.id != edge.id && e.from == edge.from && e.to == edge.to)
    {
        bail!(
            "duplicate relationship {} → {}; edit existing connection {}",
            edge.from,
            edge.to,
            existing.id
        );
    }
    Ok(())
}

pub(super) fn ensure_repository_removable_unlocked(root: &DataRoot, key: &str) -> Result<()> {
    let (doc, _) = read_document(root)?;
    let count = doc
        .relationships
        .iter()
        .filter(|edge| edge.from == key || edge.to == key)
        .count();
    ensure!(
        count == 0,
        "repository {key} has {count} relationships; delete or rewire them before removing the repository"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
