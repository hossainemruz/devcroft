//! Bundled CLI guidance and conservative, provider-independent installation.
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MANIFEST: &str = ".devcroft-install.json";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const FILES: &[(&str, &str)] = &[
    (
        "SKILL.md",
        include_str!("../assets/skills/devcroft/SKILL.md"),
    ),
    (
        "references/resources.md",
        include_str!("../assets/skills/devcroft/references/resources.md"),
    ),
    (
        "references/review.md",
        include_str!("../assets/skills/devcroft/references/review.md"),
    ),
    (
        "references/relationships.md",
        include_str!("../assets/skills/devcroft/references/relationships.md"),
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Target {
    Claude,
    Agents,
}
impl Target {
    pub(crate) const ALL: [Self; 2] = [Self::Claude, Self::Agents];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Agents => "Codex / shared agents",
        }
    }
    fn path(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Claude => ".claude",
            Self::Agents => ".agents",
        })
        .join("skills/devcroft")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::Subcommand)]
pub(crate) enum Action {
    /// Install or update the bundled skill, preserving modified installations.
    Install,
    /// Report installed versions, destinations, and CLI environment.
    Status,
    /// Remove only unmodified Devcroft-managed installations.
    Uninstall,
}

#[derive(Debug, PartialEq, Eq, clap::Args)]
pub(crate) struct Args {
    /// Limit the operation to one destination (default: both).
    #[arg(long, global = true, value_enum)]
    target: Option<Target>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    owner: String,
    format_version: u32,
    version: String,
    // Exact installed bytes are small and allow comparison across app upgrades.
    files: BTreeMap<String, String>,
}
fn bundle() -> BTreeMap<String, String> {
    FILES
        .iter()
        .map(|(p, s)| ((*p).into(), (*s).into()))
        .collect()
}

fn home() -> Result<PathBuf> {
    let path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    if !path.is_absolute() {
        bail!("HOME must be absolute");
    }
    Ok(path)
}

// Reject links and special files inside the installation. Never follow a user
// replacement to another directory during update/removal.
fn snapshot(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    if dir != root && fs::read_dir(dir)?.next().is_none() {
        bail!(
            "skill contains an unexpected empty directory: {}; preserved",
            dir.display()
        );
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("non-UTF-8 skill path")?
            .replace('\\', "/");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            snapshot(root, &path, files)?;
        } else if kind.is_file() {
            if relative != MANIFEST {
                files.insert(relative, fs::read_to_string(path)?);
            }
        } else {
            bail!(
                "skill contains a symlink or special file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn installed(path: &Path) -> Result<Option<Manifest>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            bail!("destination is not a regular directory; preserved")
        }
        _ => {}
    }
    let manifest_path = path.join(MANIFEST);
    let meta = fs::symlink_metadata(&manifest_path).context("unmanaged installation; preserved")?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        bail!("invalid installation manifest; preserved");
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)
        .context("invalid installation manifest; preserved")?;
    if manifest.owner != "devcroft" || manifest.format_version != 1 {
        bail!("unknown installation owner/format; preserved");
    }
    let mut current = BTreeMap::new();
    snapshot(path, path, &mut current)?;
    if current != manifest.files {
        bail!(
            "skill files were modified, added, or removed; preserved. Move your copy aside before reinstalling"
        );
    }
    Ok(Some(manifest))
}

struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn operate(home: &Path, target: Target, action: Action) -> Result<String> {
    let path = target.path(home);
    if action == Action::Status {
        return Ok(match installed(&path)? {
            None => "Not installed".into(),
            Some(m) if m.files == bundle() => format!("Installed · {} · current", m.version),
            Some(m) => format!("Installed · {} · update available", m.version),
        });
    }
    // Uninstalling an absent target must not create its parent directories.
    if action == Action::Uninstall
        && fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok("Not installed".into());
    }
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let lock_path = parent.join(".devcroft-skill.lock");
    let _file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "cannot acquire installer lock {}; another operation may be running",
                lock_path.display()
            )
        })?;
    let _lock = Lock(lock_path);
    let previous = installed(&path)?;
    if action == Action::Uninstall {
        if previous.is_some() {
            fs::remove_dir_all(&path)?;
        }
        return Ok("Removed".into());
    }
    if previous.as_ref().is_some_and(|m| m.files == bundle()) {
        return Ok("Already current".into());
    }
    // Stage complete content on the destination filesystem before replacement.
    let stage = parent.join(".devcroft-skill-stage");
    let backup = parent.join(".devcroft-skill-backup");
    if fs::symlink_metadata(&backup).is_ok() {
        bail!(
            "previous backup exists at {}; preserved for recovery",
            backup.display()
        );
    }
    fs::create_dir(&stage)
        .context("cannot create staging directory; inspect any previous .devcroft-skill-stage")?;
    let staged = (|| -> Result<()> {
        for (name, content) in FILES {
            let dest = stage.join(name);
            fs::create_dir_all(dest.parent().unwrap())?;
            fs::write(dest, content)?;
        }
        fs::write(
            stage.join(MANIFEST),
            serde_json::to_vec_pretty(&Manifest {
                owner: "devcroft".into(),
                format_version: 1,
                version: VERSION.into(),
                files: bundle(),
            })?,
        )?;
        // Recheck before replacing, in case an editor changed the files while staging.
        installed(&path)?;
        if previous.is_some() {
            fs::rename(&path, &backup)?;
        }
        if let Err(error) = fs::rename(&stage, &path) {
            if previous.is_some() {
                fs::rename(&backup, &path).with_context(|| {
                    format!(
                        "installation failed ({error}); restore backup from {}",
                        backup.display()
                    )
                })?;
            }
            return Err(error.into());
        }
        if previous.is_some() {
            fs::remove_dir_all(&backup)
                .context("installed, but could not remove previous backup")?;
        }
        Ok(())
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    staged?;
    Ok(format!(
        "{} · {VERSION}",
        if previous.is_some() {
            "Updated"
        } else {
            "Installed"
        }
    ))
}

pub(crate) struct Report {
    pub(crate) text: String,
    pub(crate) failed: bool,
}
pub(crate) fn perform(action: Action, target: Option<Target>) -> Report {
    let home = match home() {
        Ok(h) => h,
        Err(e) => {
            return Report {
                text: format!("{e:#}"),
                failed: true,
            };
        }
    };
    let mut lines = Vec::new();
    let mut failed = false;
    for t in Target::ALL
        .into_iter()
        .filter(|t| target.is_none_or(|selected| selected == *t))
    {
        let result = match operate(&home, t, action) {
            Ok(message) => message,
            Err(e) => {
                failed = true;
                format!("Needs attention: {e:#}")
            }
        };
        lines.push(format!(
            "{}: {result}\n{}",
            t.label(),
            t.path(&home).display()
        ));
    }
    Report {
        text: lines.join("\n\n"),
        failed,
    }
}

/// Read-only environment diagnostics; never launch a PATH candidate.
pub(crate) fn environment() -> String {
    let binary = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("devcroft"))
            .find(|p| {
                let Ok(meta) = fs::metadata(p) else {
                    return false;
                };
                if !meta.is_file() {
                    return false;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    true
                }
            })
    });
    let cli = binary.map_or_else(|| "CLI not found on this process's PATH. Add devcroft to the agent's PATH or use its absolute path.".into(), |p| format!("CLI on this process's PATH: {}", p.display()));
    let root = match crate::data::resolve_data_root() {
        Ok(root) => format!(
            "Data root: {}. Agents must use the same root.",
            root.root().display()
        ),
        Err(e) => format!("Data root unavailable: {e:#}"),
    };
    format!(
        "{cli}\n{root}\nExternal agent environments may differ. Restart the agent if the skill does not appear."
    )
}

pub(crate) fn run(args: Args) -> Result<()> {
    let report = perform(args.command, args.target);
    println!("{}\n\n{}", report.text, environment());
    if report.failed {
        bail!("one or more skill destinations need attention (see above)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_update_remove_and_preserve_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let h = tmp.path();
        let target = Target::Agents;
        let path = target.path(h);
        assert_eq!(operate(h, target, Action::Status).unwrap(), "Not installed");
        operate(h, target, Action::Install).unwrap();
        assert_eq!(
            operate(h, target, Action::Install).unwrap(),
            "Already current"
        );
        // Simulate a managed older bundle, then upgrade it.
        let mut m = installed(&path).unwrap().unwrap();
        m.files.insert("SKILL.md".into(), "old skill".into());
        fs::write(path.join("SKILL.md"), "old skill").unwrap();
        fs::write(path.join(MANIFEST), serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(
            operate(h, target, Action::Status)
                .unwrap()
                .contains("update available")
        );
        operate(h, target, Action::Install).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("SKILL.md")).unwrap(),
            FILES[0].1
        );
        fs::write(path.join("SKILL.md"), "user edit").unwrap();
        assert!(operate(h, target, Action::Install).is_err());
        assert!(operate(h, target, Action::Uninstall).is_err());
        assert_eq!(
            fs::read_to_string(path.join("SKILL.md")).unwrap(),
            "user edit"
        );
        fs::write(path.join("SKILL.md"), FILES[0].1).unwrap();
        operate(h, target, Action::Uninstall).unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn unmanaged_and_extra_files_are_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Target::Claude.path(tmp.path());
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("SKILL.md"), "custom").unwrap();
        assert!(operate(tmp.path(), Target::Claude, Action::Install).is_err());
        fs::remove_dir_all(&path).unwrap();
        operate(tmp.path(), Target::Claude, Action::Install).unwrap();
        fs::write(path.join("notes.txt"), "keep").unwrap();
        assert!(operate(tmp.path(), Target::Claude, Action::Uninstall).is_err());
        assert!(path.join("notes.txt").exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_destination_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let path = Target::Agents.path(tmp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(operate(tmp.path(), Target::Agents, Action::Install).is_err());
        assert!(operate(tmp.path(), Target::Agents, Action::Uninstall).is_err());
        assert!(outside.exists());
    }
}
