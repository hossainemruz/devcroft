//! Editor preference and argument-safe optional external editor launches.

mod drafts;
mod finder;
mod languages;
pub(crate) mod lsp;
pub(crate) mod native;
mod project;

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

    /// Shared discovery for Settings availability and actual launches.
    pub(crate) fn installed_executable(self) -> Option<PathBuf> {
        let names: &[&str] = match self {
            Self::Zed => &["zed", "zeditor"],
            Self::VsCode => &["code"],
        };
        if let Some(path) = names.iter().find_map(|name| find_executable(name)) {
            return Some(path);
        }
        #[cfg(target_os = "macos")]
        {
            let relative = match self {
                Self::Zed => "Zed.app/Contents/MacOS/cli",
                Self::VsCode => "Visual Studio Code.app/Contents/Resources/app/bin/code",
            };
            let mut roots = vec![PathBuf::from("/Applications")];
            if let Some(home) = std::env::var_os("HOME") {
                roots.push(PathBuf::from(home).join("Applications"));
            }
            roots
                .into_iter()
                .map(|root| root.join(relative))
                .find(|path| is_executable(path))
        }
        #[cfg(not(target_os = "macos"))]
        None
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
        let program = match configured {
            Some(path) => find_executable(path),
            None => self.kind.installed_executable(),
        }.ok_or_else(|| anyhow::anyhow!("Could not find {}. Install the application or its command-line launcher, then check Settings → Editor.", self.kind.label()))?;
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

/// Probe the same login-shell environment used by terminal editor sessions.
/// Runs on a background worker; never starts Neovim or loads its configuration.
pub(crate) fn neovim_available() -> Result<bool> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    probe_neovim(&shell, std::time::Duration::from_secs(3))
}

fn probe_neovim(shell: &Path, timeout: std::time::Duration) -> Result<bool> {
    let mut command = Command::new(shell);
    command
        .args(["-lic", "command -v nvim >/dev/null 2>&1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!("Could not check Neovim through your login shell: {error}")
    })?;
    let deadline = std::time::Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Ok(None) => {
                break Err(anyhow::anyhow!(
                    "Neovim check timed out. Check your shell startup files and try again."
                ));
            }
            Err(error) => break Err(error.into()),
        }
    };
    // Also stop any background children spawned by shell startup scripts.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    result
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
        directories.push(home.join("go/bin"));
        directories.push(home.join(".local/share/mise/shims"));
        directories.push(home.join(".volta/bin"));
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

    #[cfg(unix)]
    #[test]
    fn neovim_probe_uses_shell_environment_and_times_out() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("shell");
        let binary = dir.path().join("nvim");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The binary exists only on the PATH provided by shell startup.
        std::fs::write(&shell, format!("#!/bin/sh\n[ \"$1\" = '-lic' ] || exit 2\nexport PATH={}\nexec /bin/sh -c \"$2\"\n", crate::agent_activity::shell_quote(dir.path().to_str().unwrap()))).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(probe_neovim(&shell, Duration::from_secs(1)).unwrap());
        std::fs::remove_file(binary).unwrap();
        assert!(!probe_neovim(&shell, Duration::from_secs(1)).unwrap());
        std::fs::write(&shell, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
        let start = Instant::now();
        assert!(probe_neovim(&shell, Duration::from_millis(60)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

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
