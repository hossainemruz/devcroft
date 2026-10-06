//! Device-local, versioned packages. All acquisition/probing runs off the UI thread.
use super::{
    catalog::{Preference, ServerId, merge_settings},
    process::{self, Log},
};
use crate::{
    data::{DataRoot, DeviceStore},
    editor::find_executable,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
pub(crate) struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub initialization: Value,
    pub settings: Value,
    pub description: String,
    pub lease: Option<Arc<File>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    active: String,
    previous: Option<String>,
}

pub(crate) fn version(server: ServerId) -> &'static str {
    match server {
        ServerId::Rust => "2026-09-28",
        ServerId::Go => "0.23.0",
        ServerId::TypeScript => "6.0.1-ts6.0.3",
        ServerId::Python => "1.40.2",
    }
}
fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
fn base(root: &Path, server: ServerId) -> PathBuf {
    root.join("language-servers").join(server.id())
}
fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() < 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        && !value.starts_with('.')
}
fn package(root: &Path, server: ServerId, version: &str) -> Result<PathBuf> {
    ensure!(valid_version(version), "Invalid package version");
    Ok(base(root, server).join(version).join(platform()))
}
fn receipt(root: &Path, server: ServerId) -> Result<Option<Receipt>> {
    let file = base(root, server).join("active.json");
    if !file.exists() {
        return Ok(None);
    }
    ensure!(
        fs::metadata(&file)?.len() < 4096,
        "Invalid installation receipt"
    );
    let receipt: Receipt = serde_json::from_slice(&fs::read(file)?)?;
    ensure!(
        valid_version(&receipt.active) && receipt.previous.as_deref().is_none_or(valid_version),
        "Invalid installation receipt"
    );
    Ok(Some(receipt))
}
fn publish(root: &Path, server: ServerId, value: &Receipt) -> Result<()> {
    let _guard = lock(root, server)?;
    let dir = base(root, server);
    fs::create_dir_all(&dir)?;
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(dir.join("active.json"))?;
    Ok(())
}
fn lock(root: &Path, server: ServerId) -> Result<File> {
    lock_named(root, server, "package.lock")
}
fn operation_lock(root: &Path, server: ServerId) -> Result<File> {
    lock_named(root, server, "install.lock")
}
fn lock_named(root: &Path, server: ServerId, name: &str) -> Result<File> {
    let dir = base(root, server);
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(name))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if name == "package.lock" && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(error).context(
                    "Another window is managing this language server; try again shortly",
                ));
            }
        }
    }
    Ok(file)
}
fn lease_file(dir: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lease.lock"))?)
}
fn binary_name(server: ServerId) -> String {
    format!("{}{}", server.executable(), std::env::consts::EXE_SUFFIX)
}
fn entry(server: ServerId) -> PathBuf {
    match server {
        ServerId::TypeScript => "node_modules/typescript-language-server/lib/cli.mjs".into(),
        ServerId::Python => "node_modules/basedpyright/langserver.index.js".into(),
        _ => binary_name(server).into(),
    }
}
pub(crate) fn preferences(root: Option<&Path>, server: ServerId) -> Preference {
    root.and_then(|root| {
        DeviceStore::new(&DataRoot::new(root.to_owned()))
            .load()
            .ok()
    })
    .and_then(|s| s.language_servers)
    .and_then(|mut p| p.remove(server.id()))
    .unwrap_or_default()
}
pub(crate) fn save_preference(root: &Path, server: ServerId, preference: Preference) -> Result<()> {
    DeviceStore::new(&DataRoot::new(root.to_owned())).update(|state| {
        state
            .language_servers
            .get_or_insert_with(HashMap::new)
            .insert(server.id().into(), preference);
    })?;
    Ok(())
}
fn safe_program(path: PathBuf, checkout: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Choose an absolute executable path");
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Executable unavailable: {}", path.display()))?;
    ensure!(
        !canonical.starts_with(checkout),
        "Language-server executables must be outside the checkout"
    );
    ensure!(canonical.is_file(), "Executable is not a file");
    Ok(path)
}
fn runtime(name: &str, preference: &Preference, checkout: &Path) -> Result<PathBuf> {
    safe_program(
        preference
            .runtime
            .clone()
            .or_else(|| find_executable(name))
            .with_context(|| {
                format!(
                    "Missing runtime: install {name}, or select its executable in Language servers"
                )
            })?,
        checkout,
    )
}
fn prepend_path(path: &Path) -> Result<OsString> {
    let mut paths = vec![path.parent().context("Runtime has no parent")?.to_owned()];
    paths.extend(
        std::env::var_os("PATH")
            .map(|s| std::env::split_paths(&s).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    Ok(std::env::join_paths(paths)?)
}
pub(crate) fn resolve(
    root: Option<&Path>,
    checkout: &Path,
    server: ServerId,
    pref: &Preference,
) -> Result<Launch> {
    ensure!(!pref.disabled, "Language server disabled");
    let _package_guard = if pref.executable.is_none() {
        root.map(|r| lock(r, server)).transpose()?
    } else {
        None
    };
    let mut lease = None;
    let mut managed = false;
    let mut source = "System".to_owned();
    let path = if let Some(path) = &pref.executable {
        source = "Custom".into();
        safe_program(path.clone(), checkout)?
    } else if let Some((root, r)) = root
        .map(|r| receipt(r, server).map(|x| x.map(|x| (r, x))))
        .transpose()?
        .flatten()
    {
        let dir = package(root, server, &r.active)?;
        let file = lease_file(&dir)?;
        file.lock_shared()?;
        lease = Some(Arc::new(file));
        managed = true;
        source = format!("Managed {}", r.active);
        safe_program(dir.join(entry(server)), checkout)?
    } else {
        safe_program(
            find_executable(server.executable()).with_context(|| {
                format!("{} is not installed — Install or Use existing", server.id())
            })?,
            checkout,
        )?
    };
    let mut args = server.args();
    let mut env = Vec::new();
    let program = if managed && matches!(server, ServerId::TypeScript | ServerId::Python) {
        let node = runtime("node", pref, checkout)?;
        env.push(("PATH".into(), prepend_path(&node)?));
        args.insert(0, path.clone().into_os_string());
        node
    } else {
        if server == ServerId::TypeScript || (server == ServerId::Python && pref.runtime.is_some())
        {
            let node = runtime("node", pref, checkout)?;
            env.push(("PATH".into(), prepend_path(&node)?));
        } else if server == ServerId::Python
            && let Ok(node) = runtime("node", pref, checkout)
        {
            env.push(("PATH".into(), prepend_path(&node)?));
        }
        path.clone()
    };
    if server == ServerId::Go {
        let go = runtime("go", pref, checkout)?;
        env.push(("PATH".into(), prepend_path(&go)?));
    }
    let mut settings = server.settings();
    merge_settings(&mut settings, &pref.settings);
    Ok(Launch {
        description: format!(
            "{source} · {} · launched with {}",
            path.display(),
            program.display()
        ),
        program,
        args,
        env,
        initialization: server.initialization(),
        settings,
        lease,
    })
}
pub(crate) fn installed(root: &Path, server: ServerId) -> Result<Option<String>> {
    Ok(receipt(root, server)?.map(|r| r.active))
}

#[derive(Clone)]
pub(crate) struct Job {
    pub busy: bool,
    pub cancellable: bool,
    pub status: String,
    pub cancel: Arc<AtomicBool>,
    pub log: Log,
}
type Jobs = Mutex<HashMap<(PathBuf, ServerId), Job>>;
fn jobs() -> &'static Jobs {
    static JOBS: OnceLock<Jobs> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub(crate) fn job(root: &Path, server: ServerId) -> Option<Job> {
    jobs()
        .lock()
        .unwrap()
        .get(&(root.to_owned(), server))
        .cloned()
}
#[derive(Clone, Copy)]
pub(crate) enum Action {
    Install,
    Rollback,
    Remove,
}
pub(crate) fn begin(root: PathBuf, server: ServerId, action: Action) {
    let key = (root.clone(), server);
    let mut jobs_guard = jobs().lock().unwrap();
    if jobs_guard.get(&key).is_some_and(|j| j.busy) {
        return;
    }
    let job = Job {
        busy: true,
        cancellable: matches!(action, Action::Install),
        status: "Working…".into(),
        cancel: Arc::new(AtomicBool::new(false)),
        log: Log::default(),
    };
    jobs_guard.insert(key.clone(), job.clone());
    drop(jobs_guard);
    std::thread::spawn(move || {
        let result = (|| {
            let _guard = operation_lock(&root, server)?;
            match action {
                Action::Install => install(&root, server, &job),
                Action::Rollback => rollback(&root, server),
                Action::Remove => remove(&root, server),
            }
        })();
        let status = match result {
            Ok(()) => "Done — restart the language server to apply".into(),
            Err(e) => format!("{e:#}"),
        };
        if let Some(j) = jobs().lock().unwrap().get_mut(&key) {
            j.busy = false;
            j.status = status;
        }
    });
}
fn rollback(root: &Path, server: ServerId) -> Result<()> {
    let r = receipt(root, server)?.context("No managed installation")?;
    let previous = r.previous.context("No previous version to restore")?;
    ensure!(
        package(root, server, &previous)?
            .join(entry(server))
            .is_file(),
        "Previous installation missing"
    );
    publish(
        root,
        server,
        &Receipt {
            active: previous,
            previous: Some(r.active),
        },
    )
}
fn remove(root: &Path, server: ServerId) -> Result<()> {
    let _guard = lock(root, server)?;
    receipt(root, server)?.context("No managed installation")?;
    let mut locks = vec![];
    let mut paths = vec![];
    // Include superseded versions, not just the active/rollback pair.
    for item in fs::read_dir(base(root, server))? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !valid_version(&name) || !item.file_type()?.is_dir() {
            continue;
        }
        let dir = item.path().join(platform());
        if !dir.is_dir() || dir.symlink_metadata()?.file_type().is_symlink() {
            continue;
        }
        let lease = lease_file(&dir)?;
        lease
            .try_lock()
            .context("Server is running; disable it and close its workspaces before removal")?;
        locks.push(lease);
        paths.push(dir);
    }
    // Clear selection first so a failed deletion never leaves a dangling active receipt.
    fs::remove_file(base(root, server).join("active.json"))?;
    // package.lock still excludes new leases; release file handles for Windows deletion.
    drop(locks);
    for dir in paths {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}
fn run(command: &mut Command, job: &Job) -> Result<()> {
    process::run(
        command,
        &job.cancel,
        Duration::from_secs(600),
        job.log.clone(),
    )
}
fn install(root: &Path, server: ServerId, job: &Job) -> Result<()> {
    ensure!(!job.cancel.load(Ordering::SeqCst), "Cancelled");
    let pref = preferences(Some(root), server);
    let previous = receipt(root, server)?;
    let target = package(root, server, version(server))?;
    if target.join(entry(server)).is_file() {
        return publish(
            root,
            server,
            &Receipt {
                active: version(server).into(),
                previous: previous.and_then(|r| {
                    if r.active != version(server) {
                        Some(r.active)
                    } else {
                        r.previous
                    }
                }),
            },
        );
    }
    let dir = base(root, server);
    let staging = tempfile::Builder::new()
        .prefix("staging-")
        .tempdir_in(&dir)?;
    let stage = staging.path();
    match server {
        ServerId::Rust => install_rust(stage, job)?,
        ServerId::Go => {
            let go = runtime("go", &pref, stage)?;
            run(
                Command::new(go)
                    .current_dir(stage)
                    .env("GOBIN", stage)
                    .env("GOCACHE", root.join("language-servers/cache/go/build"))
                    .env("GOMODCACHE", root.join("language-servers/cache/go/modules"))
                    .env("GOWORK", "off")
                    .env("GOTOOLCHAIN", "local")
                    .env("GOPROXY", "https://proxy.golang.org")
                    .env("GOSUMDB", "sum.golang.org")
                    .env("GOPRIVATE", "")
                    .env("GONOSUMDB", "")
                    .env("GONOPROXY", "")
                    .args(["install", "golang.org/x/tools/gopls@v0.23.0"]),
                job,
            )?;
        }
        ServerId::TypeScript | ServerId::Python => {
            let node = runtime("node", &pref, stage)?;
            let mut npm = npm_command(&node)?;
            let (manifest, lock) = npm_files(server);
            fs::write(stage.join("package.json"), manifest)?;
            fs::write(stage.join("package-lock.json"), lock)?;
            run(
                npm.current_dir(stage)
                    .env("PATH", prepend_path(&node)?)
                    .env("npm_config_cache", root.join("language-servers/cache/npm"))
                    .args([
                        "ci",
                        "--ignore-scripts",
                        "--no-audit",
                        "--no-fund",
                        "--engine-strict",
                        "--registry=https://registry.npmjs.org",
                    ]),
                job,
            )?;
        }
    }
    ensure!(!job.cancel.load(Ordering::SeqCst), "Cancelled");
    ensure!(
        stage.join(entry(server)).is_file(),
        "Package did not contain its expected executable"
    );
    // Probe before publication, from a neutral directory, with no project configuration.
    let probe = if matches!(server, ServerId::TypeScript | ServerId::Python) {
        runtime("node", &pref, stage)?
    } else {
        stage.join(entry(server))
    };
    let mut cmd = Command::new(probe);
    cmd.current_dir(stage);
    if matches!(server, ServerId::TypeScript | ServerId::Python) {
        // The Python language-server entry does not expose a version flag; use its CLI sibling.
        cmd.arg(if server == ServerId::Python {
            stage.join("node_modules/basedpyright/index.js")
        } else {
            stage.join(entry(server))
        });
    }
    cmd.arg(if server == ServerId::Go {
        "version"
    } else {
        "--version"
    });
    process::run(
        &mut cmd,
        &job.cancel,
        Duration::from_secs(10),
        job.log.clone(),
    )?;
    ensure!(!job.cancel.load(Ordering::SeqCst), "Cancelled");
    fs::create_dir_all(target.parent().unwrap())?;
    ensure!(
        !target.exists(),
        "An incomplete package already exists; remove it before retrying"
    );
    fs::rename(stage, &target)?;
    publish(
        root,
        server,
        &Receipt {
            active: version(server).into(),
            previous: previous.map(|r| r.active),
        },
    )
}

fn npm_command(node: &Path) -> Result<Command> {
    let mut candidates = Vec::new();
    for executable in [node.to_owned(), node.canonicalize()?] {
        if let Some(bin) = executable.parent() {
            candidates.push(bin.join("node_modules/npm/bin/npm-cli.js"));
            candidates.push(bin.join("../lib/node_modules/npm/bin/npm-cli.js"));
        }
    }
    if let Some(npm) = find_executable("npm").and_then(|p| p.canonicalize().ok())
        && npm.extension().is_some_and(|s| s == "js")
    {
        candidates.push(npm);
    }
    if let Some(cli) = candidates.into_iter().find(|p| p.is_file()) {
        let mut cmd = Command::new(node);
        cmd.arg(cli);
        return Ok(cmd);
    }
    Ok(Command::new(find_executable("npm").context(
        "Missing runtime: npm is required alongside Node.js",
    )?))
}

fn npm_files(server: ServerId) -> (&'static str, &'static str) {
    match server {
        ServerId::TypeScript => (
            include_str!("../../../assets/language-servers/typescript/package.json"),
            include_str!("../../../assets/language-servers/typescript/package-lock.json"),
        ),
        _ => (
            include_str!("../../../assets/language-servers/python/package.json"),
            include_str!("../../../assets/language-servers/python/package-lock.json"),
        ),
    }
}
fn install_rust(stage: &Path, job: &Job) -> Result<()> {
    let (target, sha) = rust_asset()?;
    let url = format!(
        "https://github.com/rust-lang/rust-analyzer/releases/download/{}/rust-analyzer-{target}.gz",
        version(ServerId::Rust)
    );
    let archive = stage.join("server.gz");
    let curl = find_executable("curl").context("curl is required to download rust-analyzer")?;
    run(
        Command::new(curl)
            .current_dir(stage)
            .args([
                "--fail",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                "120",
                "--max-filesize",
                "134217728",
                "--output",
            ])
            .arg(&archive)
            .arg(url),
        job,
    )?;
    use sha2::{Digest, Sha256};
    let bytes = fs::read(&archive)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == sha,
        "rust-analyzer checksum mismatch"
    );
    let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice()).take(256 * 1024 * 1024 + 1);
    let mut file = File::create(stage.join(entry(ServerId::Rust)))?;
    ensure!(
        std::io::copy(&mut decoder, &mut file)? <= 256 * 1024 * 1024,
        "Server exceeds size limit"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    fs::remove_file(archive)?;
    Ok(())
}
fn rust_asset() -> Result<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok((
            "aarch64-apple-darwin",
            "54ec873d8996e2c127d758bf45d4eacb6d3371dae4f6f6d5d3f05cedbae5fd59",
        )),
        ("macos", "x86_64") => Ok((
            "x86_64-apple-darwin",
            "d032c0eb75e4597cc8ffc35ea4cdbd9eecc8341936b6edac6749e679fc3f0682",
        )),
        ("linux", "x86_64") => Ok((
            "x86_64-unknown-linux-gnu",
            "23f711d86b5f826e22886f01d7355dc01e0f4c1357dafa29710a95b903b48c85",
        )),
        ("linux", "aarch64") => Ok((
            "aarch64-unknown-linux-gnu",
            "03bad9c3dabb0f07a2678d5f9f8f1575a3742ea141506e14b3a26b42a1f896f3",
        )),
        _ => bail!(
            "Managed rust-analyzer is not yet available for this platform; select an existing executable"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Opt-in network/install test. Packages and projects live only in a disposable
    /// directory (or the explicitly supplied test cache), never the user's store.
    #[test]
    fn cancelled_install_preserves_current_receipt_without_downloading() {
        let root = tempfile::tempdir().unwrap();
        publish(
            root.path(),
            ServerId::Rust,
            &Receipt {
                active: "old".into(),
                previous: None,
            },
        )
        .unwrap();
        let job = Job {
            busy: true,
            cancellable: true,
            status: String::new(),
            cancel: Arc::new(AtomicBool::new(true)),
            log: Log::default(),
        };
        assert!(install(root.path(), ServerId::Rust, &job).is_err());
        assert_eq!(
            installed(root.path(), ServerId::Rust).unwrap().as_deref(),
            Some("old")
        );
    }

    #[test]
    fn short_publication_lock_is_retried() {
        let root = tempfile::tempdir().unwrap();
        let held = lock(root.path(), ServerId::Rust).unwrap();
        let path = root.path().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(lock(&path, ServerId::Rust).is_ok()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held);
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn updating_does_not_prevent_leasing_current_installation() {
        let root = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let dir = package(root.path(), ServerId::Rust, "old").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(entry(ServerId::Rust)), "fixture").unwrap();
        publish(
            root.path(),
            ServerId::Rust,
            &Receipt {
                active: "old".into(),
                previous: None,
            },
        )
        .unwrap();
        let _updating = operation_lock(root.path(), ServerId::Rust).unwrap();
        assert!(operation_lock(root.path(), ServerId::Rust).is_err());
        let running = resolve(
            Some(root.path()),
            checkout.path(),
            ServerId::Rust,
            &Preference::default(),
        )
        .unwrap();
        assert!(remove(root.path(), ServerId::Rust).is_err());
        drop(running);
        remove(root.path(), ServerId::Rust).unwrap();
    }

    #[test]
    fn npm_locks_pin_every_download_to_integrity_checked_registry_tarballs() {
        for server in [ServerId::TypeScript, ServerId::Python] {
            let (_, lock) = npm_files(server);
            let lock: Value = serde_json::from_str(lock).unwrap();
            for (name, package) in lock["packages"].as_object().unwrap() {
                if name.is_empty() {
                    continue;
                }
                assert!(
                    package["resolved"]
                        .as_str()
                        .unwrap()
                        .starts_with("https://registry.npmjs.org/")
                );
                assert!(
                    package["integrity"]
                        .as_str()
                        .unwrap()
                        .starts_with("sha512-")
                );
            }
        }
    }

    #[test]
    #[ignore = "downloads pinned language servers and needs Node/npm, Go and curl"]
    fn managed_servers_live_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::env::var_os("DEVCROFT_LSP_SMOKE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp.path().join("data"));
        fs::create_dir_all(&root).unwrap();
        for server in ServerId::ALL {
            let job = Job {
                busy: true,
                cancellable: true,
                status: String::new(),
                cancel: Arc::new(AtomicBool::new(false)),
                log: Log::default(),
            };
            let _lock = operation_lock(&root, server).unwrap();
            install(&root, server, &job)
                .unwrap_or_else(|e| panic!("{} installation: {e:#}", server.id()));
            drop(_lock);
            let project = temp.path().join(server.id());
            fs::create_dir_all(&project).unwrap();
            let (name, text, manifest) = match server {
                ServerId::Rust => (
                    "main.rs",
                    "fn add(a: i32) -> i32 { a }\nfn main() { let _ = add(1); }\n",
                    (
                        "Cargo.toml",
                        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n[[bin]]\nname='fixture'\npath='main.rs'\n",
                    ),
                ),
                ServerId::Go => (
                    "main.go",
                    "package main\nfunc add(a int) int { return a }\nfunc main() { _ = add(1) }\n",
                    ("go.mod", "module example.com/fixture\n\ngo 1.26\n"),
                ),
                ServerId::TypeScript => (
                    "main.ts",
                    "function add(a: number): number { return a; }\nconst value = add(1);\n",
                    (
                        "tsconfig.json",
                        "{\"compilerOptions\":{\"strict\":true},\"include\":[\"*.ts\"]}",
                    ),
                ),
                ServerId::Python => (
                    "main.py",
                    "def add(a: int) -> int:\n    return a\n\nvalue = add(1)\n",
                    ("pyrightconfig.json", "{\"typeCheckingMode\":\"standard\"}"),
                ),
            };
            fs::write(project.join(manifest.0), manifest.1).unwrap();
            let path = project.join(name);
            fs::write(&path, text).unwrap();
            let project = project.canonicalize().unwrap();
            let path = path.canonicalize().unwrap();
            let spec = resolve(Some(&root), &project, server, &Preference::default()).unwrap();
            let (tx, rx) = async_channel::bounded(128);
            let client = super::super::Client::launch(spec, &project, tx)
                .unwrap_or_else(|e| panic!("{} handshake: {e:#}", server.id()));
            let uri = super::super::file_uri(&path).unwrap();
            client
                .did_open(&uri, crate::editor::languages::detect(&path).id, text)
                .unwrap();
            let offset = text.rfind("add(1)").unwrap();
            let prefix = &text[..offset];
            let position = lsp_types::Position::new(
                prefix.bytes().filter(|b| *b == b'\n').count() as u32,
                prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
            );
            let mut ready = false;
            for _ in 0..30 {
                let version = client.doc_version(uri.as_str());
                if client
                    .hover(&uri, version, position)
                    .ok()
                    .flatten()
                    .is_some()
                    && client
                        .definition(&uri, version, position)
                        .is_ok_and(|v| !v.is_empty())
                {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            assert!(
                ready,
                "{} semantic features failed: {}",
                server.id(),
                client.logs()
            );
            let completion = client
                .completion(&uri, client.doc_version(uri.as_str()), position)
                .unwrap();
            assert!(
                match completion {
                    lsp_types::CompletionResponse::Array(items) => !items.is_empty(),
                    lsp_types::CompletionResponse::List(list) => !list.items.is_empty(),
                },
                "{} completion empty",
                server.id()
            );
            let version = client
                .did_change(uri.as_str(), &format!("{text}\n???\n"))
                .unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(20);
            let mut diagnostic = false;
            while std::time::Instant::now() < until {
                if let Ok(event) = rx.try_recv()
                    && event.uri == uri.as_str()
                    && event.version.is_none_or(|v| v == version)
                    && !event.diagnostics.is_empty()
                {
                    diagnostic = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(
                diagnostic,
                "{} diagnostics missing: {}",
                server.id(),
                client.logs()
            );
            client.did_close(uri.as_str());
            client.shutdown();
            drop(client);
            eprintln!(
                "{}: install, initialize, hover, definition, completion and diagnostics passed",
                server.id()
            );
        }
    }
    #[test]
    fn receipts_reject_paths_and_rollback_keeps_previous() {
        let root = tempfile::tempdir().unwrap();
        let s = ServerId::Go;
        assert!(package(root.path(), s, "../../escape").is_err());
        for v in ["superseded", "old", "new"] {
            let p = package(root.path(), s, v).unwrap();
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join(entry(s)), "stub").unwrap();
        }
        publish(
            root.path(),
            s,
            &Receipt {
                active: "new".into(),
                previous: Some("old".into()),
            },
        )
        .unwrap();
        rollback(root.path(), s).unwrap();
        assert_eq!(installed(root.path(), s).unwrap().unwrap(), "old");
        let lease = lease_file(&package(root.path(), s, "old").unwrap()).unwrap();
        lease.lock_shared().unwrap();
        assert!(remove(root.path(), s).is_err());
        assert!(receipt(root.path(), s).unwrap().is_some());
        drop(lease);
        remove(root.path(), s).unwrap();
        assert!(receipt(root.path(), s).unwrap().is_none());
        for v in ["superseded", "old", "new"] {
            assert!(!package(root.path(), s, v).unwrap().exists());
        }
    }
    #[test]
    fn explicit_missing_executable_does_not_fall_back() {
        let d = tempfile::tempdir().unwrap();
        let p = Preference {
            executable: Some(d.path().join("missing")),
            ..Default::default()
        };
        assert!(resolve(None, d.path(), ServerId::Rust, &p).is_err());
    }
}
