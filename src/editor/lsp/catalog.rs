//! Reviewed first-party catalog. Server-specific policy stays out of the client.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(crate) enum ServerId {
    Rust,
    Go,
    TypeScript,
    Python,
}

impl ServerId {
    pub const ALL: [Self; 4] = [Self::Rust, Self::Go, Self::TypeScript, Self::Python];
    pub fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Go => "gopls",
            Self::TypeScript => "typescript-language-server",
            Self::Python => "basedpyright",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::TypeScript => "JavaScript / TypeScript",
            Self::Python => "Python",
        }
    }
    pub fn executable(self) -> &'static str {
        match self {
            Self::Python => "basedpyright-langserver",
            _ => self.id(),
        }
    }
    pub fn args(self) -> Vec<std::ffi::OsString> {
        match self {
            Self::TypeScript | Self::Python => vec!["--stdio".into()],
            _ => vec![],
        }
    }
    pub fn initialization(self) -> Value {
        match self {
            Self::Rust => {
                json!({"checkOnSave": false, "cargo": {"buildScripts": {"enable": false}}, "procMacro": {"enable": false}})
            }
            _ => json!({}),
        }
    }
    pub fn settings(self) -> Value {
        match self {
            Self::Rust => json!({"rust-analyzer": self.initialization()}),
            Self::Python => {
                json!({"basedpyright": {"analysis": {"diagnosticMode": "openFilesOnly"}}})
            }
            _ => json!({}),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Preference {
    pub disabled: bool,
    pub executable: Option<PathBuf>,
    pub runtime: Option<PathBuf>,
    pub settings: Value,
    /// Overrides are device-local and keyed by canonical checkout path.
    pub roots: HashMap<PathBuf, PathBuf>,
}

/// Read only a small manifest; never execute project tooling to detect a root.
fn manifest(path: &Path) -> String {
    use std::io::Read;
    let mut value = String::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(128 * 1024).read_to_string(&mut value);
    }
    value
}

pub(crate) fn root_for(
    server: ServerId,
    file: &Path,
    checkout: &Path,
    preference: &Preference,
) -> PathBuf {
    if let Some(root) = preference
        .roots
        .get(checkout)
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir() && p.starts_with(checkout) && file.starts_with(p))
    {
        return root;
    }
    let mut nearest = None;
    for dir in file
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|p| p.starts_with(checkout))
    {
        let markers: &[&str] = match server {
            ServerId::Rust => &["Cargo.toml", "rust-project.json"],
            ServerId::Go => &["go.mod", "go.work"],
            ServerId::TypeScript => &["tsconfig.json", "jsconfig.json", "package.json"],
            ServerId::Python => &[
                "pyrightconfig.json",
                "pyproject.toml",
                "setup.cfg",
                "setup.py",
            ],
        };
        if nearest.is_none() && markers.iter().any(|m| dir.join(m).is_file()) {
            nearest = Some(dir.to_owned());
        }
        let project = nearest.as_deref().unwrap_or(dir);
        let relative = project.strip_prefix(dir).unwrap_or(project);
        let matches = |pattern: &str| {
            globset::Glob::new(pattern.trim_start_matches("./"))
                .is_ok_and(|g| g.compile_matcher().is_match(relative))
        };
        let workspace = match server {
            ServerId::Rust => {
                let parsed = toml::from_str::<toml::Value>(&manifest(&dir.join("Cargo.toml"))).ok();
                parsed
                    .as_ref()
                    .and_then(|v| v.get("workspace"))
                    .is_some_and(|ws| {
                        let contains = |key: &str| {
                            ws.get(key)
                                .and_then(toml::Value::as_array)
                                .is_some_and(|a| {
                                    a.iter().filter_map(toml::Value::as_str).any(matches)
                                })
                        };
                        project == dir || (contains("members") && !contains("exclude"))
                    })
            }
            ServerId::Go => {
                let text = manifest(&dir.join("go.work"));
                let mut block = false;
                text.lines().any(|line| {
                    let line = line.split("//").next().unwrap_or("").trim();
                    if line == "use (" {
                        block = true;
                        return false;
                    }
                    if line == ")" {
                        block = false;
                        return false;
                    }
                    let path = if block {
                        Some(line)
                    } else {
                        line.strip_prefix("use ")
                    };
                    path.is_some_and(|p| {
                        dir.join(p.trim_matches('"')).canonicalize().ok()
                            == project.canonicalize().ok()
                    })
                })
            }
            ServerId::TypeScript => {
                let manifest =
                    serde_json::from_str::<Value>(&manifest(&dir.join("package.json"))).ok();
                manifest
                    .as_ref()
                    .and_then(|v| v.get("workspaces"))
                    .and_then(|v| {
                        v.as_array()
                            .or_else(|| v.get("packages").and_then(Value::as_array))
                    })
                    .is_some_and(|a| a.iter().filter_map(Value::as_str).any(matches))
            }
            ServerId::Python => false,
        };
        if workspace {
            return dir.to_owned();
        }
    }
    nearest.unwrap_or_else(|| checkout.to_owned())
}

pub(crate) fn merge_settings(base: &mut Value, extra: &Value) {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            merge_settings(base.entry(key).or_insert(Value::Null), value);
        }
    } else if !extra.is_null() {
        *base = extra.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cargo_excluded_project_is_not_owned_by_ancestor_workspace() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/b/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['crates/*']\nexclude = ['crates/b']",
        )
        .unwrap();
        for name in ["a", "b"] {
            std::fs::write(
                root.join(format!("crates/{name}/Cargo.toml")),
                "[package]\nname = 'fixture'",
            )
            .unwrap();
        }
        let p = Preference::default();
        assert_eq!(
            root_for(ServerId::Rust, &root.join("crates/a/src/lib.rs"), root, &p),
            root
        );
        assert_eq!(
            root_for(ServerId::Rust, &root.join("crates/b/src/lib.rs"), root, &p),
            root.join("crates/b")
        );
    }
    #[test]
    fn independent_roots_and_workspace_markers() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("a/src")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/go.mod"), "module a").unwrap();
        std::fs::write(root.join("b/go.mod"), "module b").unwrap();
        let p = Preference::default();
        assert_eq!(
            root_for(ServerId::Go, &root.join("a/src/a.go"), root, &p),
            root.join("a")
        );
        assert_eq!(
            root_for(ServerId::Go, &root.join("b/b.go"), root, &p),
            root.join("b")
        );
        std::fs::write(root.join("go.work"), "go 1.26\nuse (\n./a\n)\n").unwrap();
        assert_eq!(
            root_for(ServerId::Go, &root.join("a/src/a.go"), root, &p),
            root
        );
    }
}
