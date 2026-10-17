//! Session ownership across plugin instances.
//!
//! During a blue/green swap two chatflow instances live at once, and after a crash a successor
//! must take over. A run is driven only while its instance holds an exclusive OS file lock on
//! `<plugin data>/locks/<session>.lock`; the OS releases it when the process exits, so a crashed or
//! retired instance never blocks its successor, and two instances never drive one session.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct SessionLock {
    _file: File,
    path: PathBuf,
}

impl SessionLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_path(data_dir: &Path, session_id: &str) -> PathBuf {
    // Session ids are frame-generated; keep the file name to safe characters regardless.
    let safe: String = session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    data_dir.join("locks").join(format!("{safe}.lock"))
}

/// Try once.
pub fn try_lock(data_dir: &Path, session_id: &str) -> std::io::Result<Option<SessionLock>> {
    let path = lock_path(data_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SessionLock { _file: file, path })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Wait until the lock is ours (polling), or `cancel` fires.
pub async fn lock(data_dir: &Path, session_id: &str, cancel: &CancellationToken) -> std::io::Result<Option<SessionLock>> {
    loop {
        if let Some(lock) = try_lock(data_dir, session_id)? {
            return Ok(Some(lock));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = cancel.cancelled() => return Ok(None),
        }
    }
}
