//! Bounded subprocesses for discovery and installation; never run on the UI thread.
use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const LOG_LIMIT: usize = 64 * 1024;
pub(crate) type Log = Arc<Mutex<Vec<u8>>>;
pub(crate) fn drain(mut reader: impl Read + Send + 'static, log: Log) {
    std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(n) = reader.read(&mut bytes) {
            if n == 0 {
                break;
            }
            let mut log = log.lock().unwrap();
            log.extend_from_slice(&bytes[..n]);
            let excess = log.len().saturating_sub(LOG_LIMIT);
            if excess > 0 {
                log.drain(..excess);
            }
        }
    });
}
pub(crate) fn log_text(log: &Log) -> String {
    String::from_utf8_lossy(&log.lock().unwrap()).into_owned()
}

pub(crate) struct ChildGuard(pub Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub(crate) fn run(
    command: &mut Command,
    cancel: &AtomicBool,
    timeout: Duration,
    log: Log,
) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = ChildGuard(command.spawn().context("Could not launch tool")?);
    if let Some(stdout) = child.0.stdout.take() {
        drain(stdout, log.clone());
    }
    if let Some(stderr) = child.0.stderr.take() {
        drain(stderr, log.clone());
    }
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            bail!("Cancelled");
        }
        if started.elapsed() > timeout {
            bail!("Tool timed out after {} seconds", timeout.as_secs());
        }
        if let Some(status) = child.0.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("Tool exited with {status}: {}", log_text(&log));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn cancellation_and_timeout_terminate_subprocesses() {
        for cancelled in [true, false] {
            let start = Instant::now();
            let result = run(
                Command::new("/bin/sleep").arg("30"),
                &AtomicBool::new(cancelled),
                Duration::from_millis(60),
                Log::default(),
            );
            assert!(result.unwrap_err().to_string().contains(if cancelled {
                "Cancelled"
            } else {
                "timed out"
            }));
            assert!(start.elapsed() < Duration::from_secs(3));
        }
    }
}
