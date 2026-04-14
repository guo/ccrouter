use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub port: u16,
    pub host: String,
    pub started_at: u64,
    pub config_path: String,
    pub log_path: String,
}

pub fn state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("ccrouter");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/state/ccrouter")
}

pub fn pid_path() -> PathBuf {
    state_dir().join("daemon.pid")
}

pub fn state_path() -> PathBuf {
    state_dir().join("daemon.json")
}

pub fn log_path() -> PathBuf {
    state_dir().join("daemon.log")
}

pub fn ensure_state_dir() -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create state dir: {}", dir.display()))?;
    Ok(())
}

/// Check whether `pid` is alive by sending signal 0.
pub fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub fn read_pid() -> Option<u32> {
    let content = std::fs::read_to_string(pid_path()).ok()?;
    content.trim().parse().ok()
}

pub fn read_state() -> Option<DaemonState> {
    let content = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn remove_runtime_files() {
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(state_path());
}

/// Write the pid file atomically with O_EXCL. Fails if it already exists.
pub fn write_pid_exclusive(pid: u32) -> Result<()> {
    ensure_state_dir()?;
    let path = pid_path();
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("Cannot create pid file: {}", path.display()))?;
    writeln!(f, "{}", pid)?;
    Ok(())
}

pub fn write_state(state: &DaemonState) -> Result<()> {
    ensure_state_dir()?;
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Clean stale runtime files if the recorded pid is no longer alive.
/// Returns true if a live daemon was detected, false otherwise.
pub fn check_and_clean_stale() -> bool {
    match read_pid() {
        Some(pid) if is_alive(pid) => true,
        Some(_) => {
            remove_runtime_files();
            false
        }
        None => false,
    }
}

/// Spawn a detached child process running the daemon worker.
/// Returns the child's PID.
pub fn spawn_detached(
    config_path: &Path,
    port_override: Option<u16>,
) -> Result<u32> {
    ensure_state_dir()?;
    let exe = std::env::current_exe().context("Cannot locate current executable")?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .with_context(|| format!("Cannot open log file: {}", log_path().display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--config").arg(config_path);
    cmd.arg("start");
    cmd.arg("--daemon-child");
    if let Some(p) = port_override {
        cmd.arg("--port").arg(p.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("Failed to spawn daemon child")?;
    Ok(child.id())
}

/// Wait for the child to write its state file, indicating successful startup.
pub fn wait_for_ready(timeout: Duration) -> Option<DaemonState> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(state) = read_state() {
            if is_alive(state.pid) {
                return Some(state);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Send SIGTERM to the daemon and wait up to `timeout` for it to exit.
pub fn stop_daemon(timeout: Duration) -> Result<StopOutcome> {
    let Some(pid) = read_pid() else {
        return Ok(StopOutcome::NotRunning);
    };

    if !is_alive(pid) {
        remove_runtime_files();
        return Ok(StopOutcome::Stale);
    }

    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("Failed to send SIGTERM to pid {}: {}", pid, err);
    }

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            remove_runtime_files();
            return Ok(StopOutcome::Stopped(pid));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(StopOutcome::Timeout(pid))
}

pub enum StopOutcome {
    NotRunning,
    Stale,
    Stopped(u32),
    Timeout(u32),
}

/// Touch the log file so the state dir is populated before any spawn.
pub fn touch_log() -> Result<()> {
    ensure_state_dir()?;
    let _ = File::options()
        .create(true)
        .append(true)
        .open(log_path())?;
    Ok(())
}
