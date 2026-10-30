//! The CortexCode home directory layout, the single-frame lock and `run/frame.json`.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Contents of `<home>/run/frame.json`, read by the UI and `cx`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrameInfo {
    pub pid: u32,
    pub port: u16,
    /// Only where there is no Keychain: on macOS the token is in the Keychain (`keychain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub socket: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn frame_json(&self) -> PathBuf {
        self.run().join("frame.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.run().join("frame.lock")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn journal(&self) -> PathBuf {
        self.root.join("journal")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn plugins_data(&self) -> PathBuf {
        self.root.join("plugins-data")
    }
    /// Installed packages: `<id>/<version>/`, and `.incoming/` for packages to install.
    pub fn plugins(&self) -> PathBuf {
        self.root.join("plugins")
    }
    /// The default live folder: every plugin's data as one git repository (the source manager
    /// creates and keeps it; plugins read their data from it).
    pub fn live(&self) -> PathBuf {
        self.root.join("live")
    }

    /// Create the directories the frame writes to. `run/` is private to the user.
    pub fn create_layout(&self) -> std::io::Result<()> {
        for dir in [self.run(), self.logs(), self.journal(), self.state(), self.plugins_data(), self.plugins()] {
            std::fs::create_dir_all(&dir)?;
        }
        std::fs::set_permissions(self.run(), std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// The Unix socket plugins connect to. macOS limits socket paths to 104 bytes; a home that
    /// is too deep gets a socket under /tmp named by a hash of the home path.
    pub fn socket_path(&self) -> PathBuf {
        let preferred = self.run().join("frame.sock");
        if preferred.as_os_str().len() < 100 {
            return preferred;
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.root.as_os_str().as_encoded_bytes());
        PathBuf::from(format!("/tmp/cortex-{}.sock", &hex::encode(digest)[..16]))
    }
}

/// An exclusive advisory lock on `<home>/run/frame.lock`, held for the frame's lifetime. The OS
/// releases it when the process dies, so a stale lock never blocks a restart.
pub struct HomeLock {
    _file: File,
}

pub enum LockOutcome {
    Acquired(HomeLock),
    /// Another live frame holds the lock; its frame.json if readable.
    Held(Option<FrameInfo>),
}

pub fn acquire_lock(home: &Home) -> std::io::Result<LockOutcome> {
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).open(home.lock_file())?;
    // SAFETY: flock on a valid, owned file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(LockOutcome::Acquired(HomeLock { _file: file }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(LockOutcome::Held(read_frame_info(&home.frame_json())));
    }
    Err(error)
}

/// Whether a live frame holds the home's lock now (the lock is tested, not kept).
pub fn lock_held(home: &Home) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(home.lock_file()) else { return false };
    // SAFETY: flock on a valid, owned file descriptor; closing the file releases a lock taken here.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

pub fn read_frame_info(path: &Path) -> Option<FrameInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `frame.json` atomically with owner-only permissions.
pub fn write_frame_info(path: &Path, info: &FrameInfo) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(info).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Write any file atomically: temp file, fsync, rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// A random hex token from the OS CSPRNG.
pub fn random_token(bytes: usize) -> std::io::Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(hex::encode(buffer))
}
