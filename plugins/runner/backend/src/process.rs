//! Running a build, a walk's command or a release script: a program with its arguments as a list
//! (never a shell string put together from a caller's text), in its own process group, with a
//! time limit, its output kept by the line.

use cc_protocol::{RpcError, codes};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Lines of output kept per run (the newest).
const KEEP_LINES: usize = 4000;
/// Characters kept per line.
const KEEP_LINE_CHARS: usize = 2000;

/// What a run left.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    /// None when a signal ended it.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// Standard output and error, in the order they came, the newest [`KEEP_LINES`].
    pub lines: Vec<String>,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    pub fn tail(&self, lines: usize) -> Vec<String> {
        self.lines[self.lines.len().saturating_sub(lines)..].to_vec()
    }

    /// The output's end within `bytes`, cut at a line.
    pub fn text(&self, bytes: usize) -> String {
        let mut kept: Vec<&str> = Vec::new();
        let mut size = 0;
        for line in self.lines.iter().rev() {
            size += line.len() + 1;
            if size > bytes && !kept.is_empty() {
                break;
            }
            kept.push(line);
        }
        kept.reverse();
        kept.join("\n")
    }
}

/// The person's own `PATH` (their login shell's): a service started by the operating system has
/// a bare one, without the Rust and Node toolchains. Asked once; the question is a fixed text.
pub async fn login_path() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    if let Some(path) = PATH.get() {
        return path.clone();
    }
    let inherited = std::env::var("PATH").unwrap_or_default();
    let shell = std::env::var("SHELL").ok().filter(|shell| Path::new(shell).is_absolute()).unwrap_or_else(|| "/bin/sh".to_owned());
    let asked = Command::new(&shell).args(["-lc", "printf %s \"$PATH\""]).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).output();
    let from_login = match tokio::time::timeout(Duration::from_secs(10), asked).await {
        Ok(Ok(output)) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        _ => String::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    let cargo = home_dir().join(".cargo").join("bin");
    let candidates = from_login.split(':').chain(inherited.split(':')).map(str::to_owned).chain(cargo.is_dir().then(|| cargo.to_string_lossy().into_owned()));
    for part in candidates {
        if !part.is_empty() && !parts.contains(&part) {
            parts.push(part);
        }
    }
    let path = parts.join(":");
    PATH.get_or_init(|| path).clone()
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// An executable named `name` on `path`.
pub fn find_on(path: &str, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|full| std::fs::metadata(full).map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// Run `program` with `args` in `cwd` until it ends, `limit` passes or `cancel` fires (then its
/// whole process group is stopped). `envs` are set on top of this process's environment, with the
/// login `PATH`.
pub async fn run(program: &Path, args: &[String], cwd: &Path, envs: &[(String, String)], limit: Duration, cancel: &CancellationToken) -> Result<Ran, RpcError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env("PATH", login_path().await).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).process_group(0);
    for (name, value) in envs {
        command.env(name, value);
    }
    let name = program.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RpcError::new(codes::UNAVAILABLE, format!("`{}` is not installed or not executable", program.display()))
        } else {
            RpcError::internal(format!("cannot run `{name}` in {}: {error}", cwd.display()))
        }
    })?;
    let pid = child.id();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut kept: VecDeque<String> = VecDeque::new();
    let mut keep = |line: String| {
        if kept.len() == KEEP_LINES {
            kept.pop_front();
        }
        kept.push_back(if line.len() > KEEP_LINE_CHARS { line.chars().take(KEEP_LINE_CHARS).collect() } else { line });
    };
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let mut stopped = false;
    let status = loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(line) => keep(line),
                // Both pipes closed: the program (and what it started) is done writing.
                None => break child.wait().await,
            },
            _ = &mut deadline, if !timed_out && !stopped => {
                timed_out = true;
                stop_group(pid);
            }
            _ = cancel.cancelled(), if !timed_out && !stopped => {
                stopped = true;
                stop_group(pid);
            }
        }
    };
    if stopped {
        return Err(RpcError::cancelled());
    }
    let status = status.map_err(|error| RpcError::internal(format!("`{name}` failed: {error}")))?;
    if timed_out {
        keep(format!("(stopped: `{name}` did not finish within {} s)", limit.as_secs()));
    }
    Ok(Ran { code: status.code(), timed_out, lines: kept.into_iter().collect() })
}

/// End the process group `pid` leads (the program and everything it started).
fn stop_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: a signal to a process group this process started.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}
