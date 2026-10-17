//! Running external tools (git, cargo, npm): captured or streamed line by line, each in its own
//! process group so a cancelled call kills the whole tree (cargo's rustc children, npm's scripts).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Standard install locations searched after `PATH` (rustup's `~/.cargo/bin`, Homebrew), so tools
/// are found when the frame was started from Finder with the minimal GUI `PATH`.
pub fn extra_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// `PATH` followed by the extra install locations not already in it.
pub fn search_path() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for extra in extra_path_dirs() {
        if !dirs.contains(&extra) {
            dirs.push(extra);
        }
    }
    dirs
}

pub fn search_path_os() -> OsString {
    std::env::join_paths(search_path()).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// First executable named `name` on the search path (like `which`).
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in search_path() {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if let Ok(metadata) = std::fs::metadata(&candidate)
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Some(candidate);
        }
    }
    None
}

/// Environment variables removed from every build: set by an enclosing cargo or rustup (e.g. when
/// the frame itself was started through `cargo run`), they would redirect the build to another
/// toolchain or target directory instead of the component's own `rust-toolchain.toml` and `target/`.
pub const SCRUBBED_ENV: &[&str] = &[
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTDOC",
    "RUSTC_BOOTSTRAP",
];

/// A command with the augmented `PATH`, the scrubbed environment, stdin closed and its own
/// process group.
pub fn command(program: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd).env("PATH", search_path_os()).stdin(Stdio::null()).kill_on_drop(true).process_group(0);
    for name in SCRUBBED_ENV {
        cmd.env_remove(name);
    }
    cmd
}

/// Kills the child's whole process group if the future running it is dropped before it exits.
struct GroupGuard {
    pid: Option<u32>,
}

impl GroupGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && let Ok(pgid) = libc::pid_t::try_from(pid)
        {
            // SAFETY: killpg with a process group id we created (process_group(0) => pgid = pid).
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn ok(&self) -> bool {
        self.status.success()
    }

    /// First non-empty line of stdout, trimmed.
    pub fn first_line(&self) -> Option<String> {
        self.stdout.lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned)
    }

    /// A one-line description of a failure (stderr, else stdout, else the status).
    pub fn failure(&self) -> String {
        let text = if self.stderr.trim().is_empty() { self.stdout.trim() } else { self.stderr.trim() };
        if text.is_empty() { format!("exited with {}", self.status) } else { text.to_owned() }
    }
}

/// Run to completion and capture both streams.
pub async fn capture(mut cmd: Command) -> std::io::Result<Captured> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn()?;
    let mut guard = GroupGuard { pid: child.id() };
    let output = child.wait_with_output().await?;
    guard.disarm();
    Ok(Captured {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of output. `overwrite` is true when the line ended with a carriage return (a progress
/// meter that the next line replaces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stream: Stream,
    pub text: String,
    pub overwrite: bool,
}

async fn read_lines<R: AsyncRead + Unpin>(mut reader: R, stream: Stream, tx: mpsc::UnboundedSender<Line>) {
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        // Structural split on line terminators: `\n`, or `\r` (progress meters).
        let mut start = 0;
        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            if byte == b'\n' || byte == b'\r' {
                let overwrite = byte == b'\r' && buffer.get(i + 1) != Some(&b'\n');
                let text = String::from_utf8_lossy(&buffer[start..i]).into_owned();
                if byte == b'\r' && buffer.get(i + 1) == Some(&b'\n') {
                    i += 1;
                } else if byte == b'\r' && i + 1 == buffer.len() {
                    // A `\r` at the end of the chunk may be the first half of `\r\n`: wait for more.
                    break;
                }
                let _ = tx.send(Line { stream, text, overwrite });
                start = i + 1;
            }
            i += 1;
        }
        buffer.drain(..start);
    }
    if !buffer.is_empty() {
        let text = String::from_utf8_lossy(&buffer).trim_end_matches('\r').to_owned();
        let _ = tx.send(Line { stream, text, overwrite: false });
    }
}

/// Run to completion, delivering every output line (both streams, in arrival order) to `on_line`.
pub async fn stream(mut cmd: Command, mut on_line: impl FnMut(Line)) -> std::io::Result<ExitStatus> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut guard = GroupGuard { pid: child.id() };
    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_lines(stdout, Stream::Stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(read_lines(stderr, Stream::Stderr, tx.clone()));
    }
    drop(tx);
    while let Some(line) = rx.recv().await {
        on_line(line);
    }
    let status = child.wait().await?;
    guard.disarm();
    Ok(status)
}
