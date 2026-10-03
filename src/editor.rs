//! Editor preference and argument-safe optional external editor launches.

pub(crate) mod lsp;
pub(crate) mod native;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EditorChoice {
    #[default]
    Neovim,
    BuiltIn,
}

impl EditorChoice {
    pub(crate) const ALL: [Self; 2] = [Self::Neovim, Self::BuiltIn];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Neovim => "neovim",
            Self::BuiltIn => "built_in",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Neovim => "Neovim",
            Self::BuiltIn => "Devcroft editor",
        }
    }

    pub(crate) fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.id() == id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalEditorKind {
    Zed,
    VsCode,
}

impl ExternalEditorKind {
    pub(crate) const ALL: [Self; 2] = [Self::Zed, Self::VsCode];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Zed => "zed",
            Self::VsCode => "vscode",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Zed => "Zed",
            Self::VsCode => "VS Code",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalEditor {
    pub(crate) kind: ExternalEditorKind,
    pub(crate) executable: Option<String>,
}

impl ExternalEditor {
    pub(crate) fn new(kind: ExternalEditorKind, executable: Option<String>) -> Self {
        Self { kind, executable }
    }

    /// Launch a checkout or a file within it. Locations are one-based.
    pub(crate) fn launch(
        &self,
        checkout: &Path,
        file: Option<&Path>,
        location: Option<(usize, usize)>,
    ) -> Result<()> {
        let (program, args) = self.command(checkout, file, location)?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(checkout)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn command(
        &self,
        checkout: &Path,
        file: Option<&Path>,
        location: Option<(usize, usize)>,
    ) -> Result<(PathBuf, Vec<OsString>)> {
        let configured = self
            .executable
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let (name, fallback): (&str, &[&str]) = match self.kind {
            ExternalEditorKind::Zed => ("zed", &["zeditor"]),
            ExternalEditorKind::VsCode => ("code", &[]),
        };
        let program = configured
            .and_then(find_executable)
            .or_else(|| if configured.is_some() { None } else { find_executable(name) })
            .or_else(|| fallback.iter().find_map(|name| find_executable(name)))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find {name}. Install its command-line launcher or set its executable path in Settings → General."
                )
            })?;
        let target = match file {
            Some(path) if path.is_absolute() => path.to_owned(),
            Some(path) => checkout.join(path),
            None => checkout.to_owned(),
        };
        let mut args = Vec::new();
        if let Some((line, column)) = location.filter(|_| file.is_some()) {
            if line == 0 || column == 0 {
                bail!("Line and column must start at 1.");
            }
            if self.kind == ExternalEditorKind::VsCode {
                args.push(OsString::from("--goto"));
            }
            let mut positioned = target.as_os_str().to_os_string();
            positioned.push(format!(":{line}:{column}"));
            args.push(positioned);
        } else {
            args.push(target.into_os_string());
        }
        Ok((program, args))
    }
}

pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable(path).then(|| path.to_owned());
    }
    let mut directories = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    // Desktop apps often inherit a smaller PATH than an interactive shell.
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        directories.push(home.join(".local/bin"));
        // rustup's default binary directory: GUI-launched apps never see
        // the shell's PATH, and rust-analyzer usually lives here.
        directories.push(home.join(".cargo/bin"));
    }
    #[cfg(target_os = "macos")]
    {
        directories.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
        if name == "code" {
            directories.push(PathBuf::from(
                "/Applications/Visual Studio Code.app/Contents/Resources/app/bin",
            ));
        }
    }
    for directory in directories {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        for suffix in [".exe", ".cmd", ".bat"] {
            let candidate = directory.join(format!("{name}{suffix}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn editor(kind: ExternalEditorKind) -> ExternalEditor {
        ExternalEditor::new(
            kind,
            Some(std::env::current_exe().unwrap().display().to_string()),
        )
    }

    #[test]
    fn external_arguments_keep_unicode_and_spaces_together() {
        let root = Path::new("/tmp/project space");
        let path = Path::new("src/বাংলা file.rs");
        let (_, zed) = editor(ExternalEditorKind::Zed)
            .command(root, Some(path), Some((12, 3)))
            .unwrap();
        assert_eq!(zed, [OsStr::new("/tmp/project space/src/বাংলা file.rs:12:3")]);
        let (_, code) = editor(ExternalEditorKind::VsCode)
            .command(root, Some(path), Some((12, 3)))
            .unwrap();
        assert_eq!(
            code,
            [
                OsStr::new("--goto"),
                OsStr::new("/tmp/project space/src/বাংলা file.rs:12:3")
            ]
        );
    }
}
