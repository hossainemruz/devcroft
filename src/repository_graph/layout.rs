//! Deterministic SCC hierarchy and device-local positions. No portable writes.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::data::{DataRoot, relationships::Relationship};

pub(crate) const NODE_WIDTH: f32 = 224.;
pub(crate) const NODE_HEIGHT: f32 = 104.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Point {
    pub x: f32,
    pub y: f32,
}
impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.x.abs() < 1e7 && self.y.abs() < 1e7
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Viewport {
    pub offset: Point,
    pub zoom: f32,
}
impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: Point::new(40., 40.),
            zoom: 1.,
        }
    }
}
impl Viewport {
    pub fn screen(self, p: Point) -> Point {
        Point::new(
            p.x * self.zoom + self.offset.x,
            p.y * self.zoom + self.offset.y,
        )
    }
    pub fn world(self, p: Point) -> Point {
        Point::new(
            (p.x - self.offset.x) / self.zoom,
            (p.y - self.offset.y) / self.zoom,
        )
    }
    pub fn zoom_at(&mut self, pointer: Point, factor: f32) {
        let anchor = self.world(pointer);
        self.zoom = (self.zoom * factor).clamp(0.25, 2.5);
        self.offset = Point::new(
            pointer.x - anchor.x * self.zoom,
            pointer.y - anchor.y * self.zoom,
        );
    }
    pub fn fit(&mut self, points: impl Iterator<Item = Point>, size: Point) {
        let points: Vec<_> = points.collect();
        if points.is_empty() {
            *self = Self::default();
            return;
        }
        let left = points.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let top = points.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let right = points
            .iter()
            .map(|p| p.x + NODE_WIDTH)
            .fold(f32::NEG_INFINITY, f32::max);
        let bottom = points
            .iter()
            .map(|p| p.y + NODE_HEIGHT)
            .fold(f32::NEG_INFINITY, f32::max);
        self.zoom = ((size.x - 80.).max(80.) / (right - left))
            .min((size.y - 80.).max(80.) / (bottom - top))
            .clamp(0.25, 1.5);
        self.offset = Point::new(
            (size.x - (right - left) * self.zoom) / 2. - left * self.zoom,
            (size.y - (bottom - top) * self.zoom) / 2. - top * self.zoom,
        );
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SpaceLayout {
    pub positions: BTreeMap<String, Point>,
    pub viewport: Viewport,
}

/// Device-local canvas state, one entry per space. The active space comes
/// from `device.json`, so no "last space" is stored here: the map keeps
/// whatever each space's canvas last looked like.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Layouts {
    pub schema_version: u32,
    /// `groups` is the pre-spaces name; aliased so existing local layouts
    /// keep their positions instead of re-arranging once.
    #[serde(alias = "groups")]
    pub spaces: BTreeMap<String, SpaceLayout>,
}
impl Default for Layouts {
    fn default() -> Self {
        Self {
            schema_version: 1,
            spaces: BTreeMap::new(),
        }
    }
}
impl Layouts {
    pub fn load(root: &DataRoot) -> Result<Self> {
        let path = root.root().join("repository-graph-layout.json");
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        ensure!(
            std::fs::symlink_metadata(&path)?.file_type().is_file(),
            "graph layout must be a regular file"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "graph layout exceeds size limit"
        );
        let layouts: Self =
            serde_json::from_slice(&bytes).context("malformed local graph layout")?;
        ensure!(
            layouts.schema_version == 1,
            "unsupported graph layout version"
        );
        ensure!(
            layouts
                .spaces
                .values()
                .all(|g| g.positions.values().all(|p| p.finite())
                    && g.viewport.offset.finite()
                    && (0.25..=2.5).contains(&g.viewport.zoom)),
            "invalid graph layout coordinates"
        );
        Ok(layouts)
    }
    pub fn save(&self, root: &DataRoot) -> Result<()> {
        crate::data::write_json_atomic(&root.root().join("repository-graph-layout.json"), self)
    }
}

/// Kosaraju with iterative DFS, followed by longest-path ranks on the condensed
/// DAG. Stable keys resolve ties; cyclic components get compact two-column blocks.
pub(crate) fn arrange(keys: &[String], edges: &[Relationship]) -> BTreeMap<String, Point> {
    let keys: BTreeSet<_> = keys.iter().cloned().collect();
    let mut forward: BTreeMap<String, BTreeSet<String>> =
        keys.iter().map(|k| (k.clone(), BTreeSet::new())).collect();
    let mut reverse = forward.clone();
    for edge in edges {
        if keys.contains(&edge.from) && keys.contains(&edge.to) {
            forward.get_mut(&edge.from).unwrap().insert(edge.to.clone());
            reverse.get_mut(&edge.to).unwrap().insert(edge.from.clone());
        }
    }
    let isolated: BTreeSet<_> = keys
        .iter()
        .filter(|key| forward[*key].is_empty() && reverse[*key].is_empty())
        .cloned()
        .collect();
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    for key in &keys {
        let mut stack = vec![(key.clone(), false)];
        while let Some((key, done)) = stack.pop() {
            if done {
                order.push(key);
                continue;
            }
            if !seen.insert(key.clone()) {
                continue;
            }
            stack.push((key.clone(), true));
            for next in forward[&key].iter().rev() {
                stack.push((next.clone(), false));
            }
        }
    }
    seen.clear();
    let mut components = Vec::new();
    for key in order.into_iter().rev() {
        if seen.contains(&key) {
            continue;
        }
        let mut members = Vec::new();
        let mut stack = vec![key];
        while let Some(key) = stack.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            stack.extend(reverse[&key].iter().cloned());
            members.push(key);
        }
        members.sort();
        components.push(members);
    }
    components.sort();
    let membership: BTreeMap<_, _> = components
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.iter().map(move |k| (k.clone(), i)))
        .collect();
    let mut outgoing = vec![BTreeSet::new(); components.len()];
    let mut incoming = vec![0; components.len()];
    for (from, targets) in forward {
        for to in targets {
            let (a, b) = (membership[&from], membership[&to]);
            if a != b && outgoing[a].insert(b) {
                incoming[b] += 1;
            }
        }
    }
    let mut ready: BTreeSet<_> = incoming
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| i)
        .collect();
    let mut ranks = vec![0; components.len()];
    while let Some(i) = ready.pop_first() {
        for &next in &outgoing[i] {
            ranks[next] = ranks[next].max(ranks[i] + 1);
            incoming[next] -= 1;
            if incoming[next] == 0 {
                ready.insert(next);
            }
        }
    }
    let mut result = BTreeMap::new();
    let mut y = 0.;
    for rank in 0..=ranks.iter().copied().max().unwrap_or(0) {
        let mut x = 0.;
        let mut height: f32 = 0.;
        for (i, members) in components
            .iter()
            .enumerate()
            .filter(|(i, members)| ranks[*i] == rank && !isolated.contains(&members[0]))
        {
            let _ = i;
            for (index, key) in members.iter().enumerate() {
                result.insert(
                    key.clone(),
                    Point::new(x + (index % 2) as f32 * 300., y + (index / 2) as f32 * 180.),
                );
            }
            x += if members.len() > 1 { 640. } else { 300. };
            height = height.max(members.len().div_ceil(2) as f32 * 180.);
        }
        if height > 0. {
            y += height + 60.;
        }
    }
    // Unconnected repositories have no hierarchy. Keep them in a compact grid
    // below the connected graph instead of stretching the canvas horizontally.
    let columns = (isolated.len() as f32).sqrt().ceil().clamp(1., 4.) as usize;
    for (index, key) in isolated.into_iter().enumerate() {
        result.insert(
            key,
            Point::new(
                (index % columns) as f32 * 300.,
                y + (index / columns) as f32 * 180.,
            ),
        );
    }
    result
}

impl SpaceLayout {
    pub fn include_new(&mut self, keys: &[String], edges: &[Relationship]) {
        if self.positions.is_empty() {
            self.positions = arrange(keys, edges);
            return;
        }
        let mut x = self.positions.values().map(|p| p.x).fold(0., f32::max) + 300.;
        for key in keys {
            if !self.positions.contains_key(key) {
                self.positions.insert(key.clone(), Point::new(x, 0.));
                x += 300.;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_repositories_wrap_without_overlapping_the_hierarchy() {
        let keys: Vec<_> = (0..17).map(|index| format!("repo-{index:02}")).collect();
        let edge = Relationship {
            id: "connection".into(),
            from: keys[0].clone(),
            to: keys[1].clone(),
            description: "Uses the provider".into(),
            extra: BTreeMap::new(),
        };
        let positions = arrange(&keys, &[edge]);
        assert_eq!(positions.len(), keys.len());
        assert!(positions[&keys[0]].y < positions[&keys[1]].y);
        for key in &keys[2..] {
            assert!(positions[key].y > positions[&keys[1]].y + NODE_HEIGHT);
            assert!(positions[key].x <= 900.);
        }
        for (index, key) in keys.iter().enumerate() {
            for other in &keys[index + 1..] {
                let (a, b) = (positions[key], positions[other]);
                assert!((a.x - b.x).abs() >= NODE_WIDTH || (a.y - b.y).abs() >= NODE_HEIGHT);
            }
        }
        let mut reversed = keys.clone();
        reversed.reverse();
        assert_eq!(arrange(&keys, &[]), arrange(&reversed, &[]));
        assert!(arrange(&[], &[]).is_empty());
    }

    #[test]
    fn coordinates_zoom_anchor_and_fit() {
        let mut viewport = Viewport::default();
        let p = Point::new(75., 160.);
        assert!(viewport.world(viewport.screen(p)).distance(p) < 0.001);
        let anchor = viewport.world(p);
        viewport.zoom_at(p, 1.7);
        assert!(viewport.world(p).distance(anchor) < 0.001);
        viewport.fit(
            [Point::default(), Point::new(300., 240.)].into_iter(),
            Point::new(1200., 800.),
        );
        assert!(viewport.screen(Point::default()).x >= 0.);
    }
    #[test]
    fn hierarchy_cycles_isolates_and_saved_positions() {
        let keys: Vec<_> = ["api", "backend", "ui", "cli", "isolated"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut edges: Vec<_> = [
            ("api", "backend"),
            ("backend", "ui"),
            ("backend", "cli"),
            ("ui", "backend"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (a, b))| Relationship {
            id: i.to_string(),
            from: a.into(),
            to: b.into(),
            description: "text".into(),
            extra: BTreeMap::new(),
        })
        .collect();
        let positions = arrange(&keys, &edges);
        assert_eq!(positions.len(), 5);
        assert!(positions["api"].y < positions["backend"].y);
        assert_eq!(positions["backend"].y, positions["ui"].y);
        assert!(positions["cli"].y > positions["backend"].y);
        edges.reverse();
        assert_eq!(positions, arrange(&keys, &edges));
        let mut saved = SpaceLayout {
            positions: positions.clone(),
            ..Default::default()
        };
        let mut keys = keys;
        keys.push("new".into());
        saved.include_new(&keys, &edges);
        for (key, point) in positions {
            assert_eq!(saved.positions[&key], point);
        }
    }
}
