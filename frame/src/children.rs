//! The processes the frame started (plugin backends and the Electron shell), recorded in
//! `<home>/run/children.json` while they run. One CortexCode runs at a time: the home lock keeps
//! a second frame out ([`crate::home::acquire_lock`]), and this record keeps a dead frame's
//! processes out. A frame that was killed without its shutdown (a SIGKILL, a crash) cannot stop
//! its children, which lead their own process groups; the next frame stops what the record still
//! names before it starts its own, so two model engines never hold the GPU at once (seen: 29 GB
//! and 20 GB of "CortexCode" processes beside a fresh start).
//!
//! A recorded pid is stopped only when the process behind it still runs the recorded executable:
//! a pid the system reused for something else is left alone.

use crate::home::{Home, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const FILE: &str = "children.json";
/// How long a leftover process gets to end on SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(5);

/// The record is read, changed and written under this lock (several tasks start and stop children).
static RECORD: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Child {
    pub pid: u32,
    pub exec: PathBuf,
    /// `plugin` or `shell`.
    pub kind: String,
}

fn path(home: &Home) -> PathBuf {
    home.run().join(FILE)
}

fn read(home: &Home) -> Vec<Child> {
    std::fs::read(path(home)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write(home: &Home, children: &[Child]) {
    match serde_json::to_vec_pretty(children) {
        Ok(bytes) => {
            if let Err(error) = write_atomic(&path(home), &bytes) {
                tracing::warn!(%error, "cannot record the frame's child processes");
            }
        }
        Err(error) => tracing::warn!(%error, "cannot record the frame's child processes"),
    }
}

/// A child started: remember it until it exits.
pub fn record(home: &Home, pid: u32, exec: &Path, kind: &str) {
    if pid == 0 {
        return;
    }
    let _guard = RECORD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut children = read(home);
    children.retain(|child| child.pid != pid);
    children.push(Child { pid, exec: exec.to_path_buf(), kind: kind.to_owned() });
    write(home, &children);
}

/// A child exited: it needs no stopping later.
pub fn forget(home: &Home, pid: u32) {
    let _guard = RECORD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut children = read(home);
    let before = children.len();
    children.retain(|child| child.pid != pid);
    if children.len() != before {
        write(home, &children);
    }
}

/// At frame start: stop every recorded process that still runs its recorded executable (SIGTERM to
/// its process group, SIGKILL after [`GRACE`]), then clear the record. Returns how many were
/// stopped. A clean shutdown leaves an empty record, so this costs nothing then.
pub async fn reap(home: &Home) -> usize {
    let children = {
        let _guard = RECORD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        read(home)
    };
    let mut stopping: Vec<Child> = Vec::new();
    for child in children {
        if !alive(child.pid) {
            continue;
        }
        match exec_of(child.pid) {
            Some(exec) if same_file(&exec, &child.exec) => {
                tracing::warn!(pid = child.pid, kind = %child.kind, exec = %exec.display(), "stopping a process a previous frame left running");
                signal_group(child.pid, libc::SIGTERM);
                stopping.push(child);
            }
            Some(exec) => tracing::info!(pid = child.pid, now = %exec.display(), "a recorded pid runs something else now; left alone"),
            None => tracing::info!(pid = child.pid, "a recorded process cannot be inspected; left alone"),
        }
    }
    if !stopping.is_empty() {
        let deadline = tokio::time::Instant::now() + GRACE;
        while stopping.iter().any(|child| alive(child.pid)) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for child in &stopping {
            if alive(child.pid) {
                tracing::warn!(pid = child.pid, kind = %child.kind, "the process did not end on SIGTERM; killing it");
                signal_group(child.pid, libc::SIGKILL);
            }
        }
    }
    let _ = std::fs::remove_file(path(home));
    stopping.len()
}

/// Whether a process with this pid exists (a permission error means it does).
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 checks for existence only.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Signal a child and its process group (plugins and the shell lead their own groups).
fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 1 {
        return;
    }
    // SAFETY: signalling processes this frame's predecessor started, verified by executable path.
    unsafe {
        libc::killpg(pid, signal);
        libc::kill(pid, signal);
    }
}

/// The executable a running process was started from.
#[cfg(target_os = "macos")]
fn exec_of(pid: u32) -> Option<PathBuf> {
    let Ok(pid) = i32::try_from(pid) else { return None };
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as large as declared; the call writes at most that many bytes.
    let written = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if written <= 0 {
        return None;
    }
    buffer.truncate(written as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

#[cfg(not(target_os = "macos"))]
fn exec_of(_pid: u32) -> Option<PathBuf> {
    None
}

/// Two paths name the same file (through links), or are spelled the same.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
