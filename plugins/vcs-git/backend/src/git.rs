//! Running the `git` (and `gh`) command-line tools with a controlled environment.
//!
//! Every invocation:
//! - drops repository-location variables inherited from our parent (`GIT_DIR`, `GIT_WORK_TREE`,
//!   `GIT_INDEX_FILE`, ...) so a command only ever touches the repository we name explicitly;
//! - never prompts (`GIT_TERMINAL_PROMPT=0`, no editor, no pager) and has no stdin unless we feed it;
//! - takes no optional locks (`GIT_OPTIONAL_LOCKS=0`): read commands such as `status` must not
//!   rewrite the user's index, otherwise every read would itself look like an index change;
//! - treats pathspecs literally (`GIT_LITERAL_PATHSPECS=1`): paths are file names, never globs;
//! - prints paths verbatim (`core.quotePath=false`) and never colours output.

use cc_protocol::{RpcError, codes};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Variables that would redirect git to another repository, index or object store.
const LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_INTERNAL_SUPER_PREFIX",
];

/// Captured result of one process run.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

/// One command to run.
pub struct Cmd {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    envs: Vec<(OsString, OsString)>,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
    /// Run in a new session without a controlling terminal, so ssh/credential helpers can never
    /// open `/dev/tty` and block waiting for a password.
    detach: bool,
    read_sandbox: Option<PathBuf>,
}

impl Cmd {
    pub fn new(program: &Path, cwd: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            envs: Vec::new(),
            stdin: None,
            timeout: None,
            detach: false,
            read_sandbox: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    pub fn stdin(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    pub fn timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    pub fn detach(mut self) -> Self {
        self.detach = true;
        self
    }

    fn display(&self) -> String {
        let name = self.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let args: Vec<String> = self.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        format!("{name} {}", args.join(" "))
    }

    /// Run to completion and capture everything. Spawn failures and timeouts are errors; a nonzero
    /// exit status is not (callers decide).
    pub async fn output(self) -> Result<Output, RpcError> {
        let display = self.display();
        let mut command = if let Some(scratch) = &self.read_sandbox {
            let resolved = std::fs::canonicalize(scratch).map_err(|error| RpcError::internal(format!("cannot resolve Git inspection scratch: {error}")))?;
            if &resolved != scratch {
                return Err(RpcError::new(codes::CONTRACT_VIOLATION, "Git inspection scratch was redirected; restore the plugin data directory in the app"));
            }
            let mut command = confined(&self.program, scratch)?;
            command.env("TMPDIR", scratch).env("TMP", scratch).env("TEMP", scratch);
            command
        } else {
            Command::new(&self.program)
        };
        command.args(&self.args).current_dir(&self.cwd);
        for var in LOCATION_VARS {
            command.env_remove(var);
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("GIT_EDITOR", ":")
            .env("GIT_SEQUENCE_EDITOR", ":")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .env("LC_ALL", "C")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1");
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        command
            .stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if self.detach {
            // SAFETY: setsid is async-signal-safe and touches no Rust state in the forked child.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                RpcError::new(codes::UNAVAILABLE, format!("`{}` is not installed or not executable", self.program.display()))
            } else {
                RpcError::internal(format!("cannot run `{display}`: {error}"))
            }
        })?;
        if let Some(data) = self.stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            // Feed stdin concurrently so a large input cannot deadlock against a full stdout pipe.
            tokio::spawn(async move {
                let _ = pipe.write_all(&data).await;
                let _ = pipe.shutdown().await;
            });
        }
        let waited = child.wait_with_output();
        let output = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, waited).await {
                Ok(result) => result,
                Err(_) => {
                    return Err(RpcError::new(codes::TIMEOUT, format!("`{display}` did not finish within {limit:?}")));
                }
            },
            None => waited.await,
        }
        .map_err(|error| RpcError::internal(format!("`{display}` failed: {error}")))?;
        Ok(Output { code: output.status.code(), stdout: output.stdout, stderr: output.stderr })
    }

    /// Run and require exit status 0; returns stdout.
    pub async fn ok(self) -> Result<Vec<u8>, RpcError> {
        let display = self.display();
        let output = self.output().await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(command_error(&display, &output))
        }
    }
}

/// The frame's credentials, which a Git inspection never reads.
fn hidden_trees() -> [PathBuf; 2] {
    let home = std::env::var_os("CORTEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".cortexcode")
        });
    let home = home.canonicalize().unwrap_or(home);
    [home.join("run"), home.join("plugins-data/ui/electron")]
}

/// `program` under the macOS sandbox: repository-controlled config, hooks and filters run without
/// network, process information of others, the frame's credentials, or writes outside `scratch`.
#[cfg(target_os = "macos")]
fn confined(program: &Path, scratch: &Path) -> Result<Command, RpcError> {
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err(RpcError::new(
            codes::UNAVAILABLE,
            "Git inspection needs the macOS sandbox; open this repository in the app on macOS",
        ));
    }
    let quote = |path: &Path| -> Result<String, RpcError> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let text = path
            .to_str()
            .ok_or_else(|| RpcError::invalid_params("Git sandbox path is not UTF-8"))?;
        if text.chars().any(char::is_control) {
            return Err(RpcError::invalid_params(
                "Git sandbox path contains control characters",
            ));
        }
        Ok(format!(
            "\"{}\"",
            text.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    };
    let [run, electron] = hidden_trees();
    let profile = format!(
        "(version 1)(allow default)\n(deny network*)\n(deny process-info*)\n(allow process-info* (target self))\n(deny sysctl-read (sysctl-name \"kern.procargs2\"))\n(deny file-read* (subpath {}) (subpath {}))\n(deny file-write* (require-not (require-any (subpath {}) (subpath \"/dev\"))))\n",
        quote(&run)?,
        quote(&electron)?,
        quote(scratch)?
    );
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &profile]).arg(program);
    Ok(command)
}

/// `program` under Landlock and seccomp, the same rules as on macOS (see [`crate::landlock`] for
/// what differs).
#[cfg(target_os = "linux")]
fn confined(program: &Path, scratch: &Path) -> Result<Command, RpcError> {
    let mut command = Command::new(program);
    crate::landlock::confine(&mut command, scratch, &hidden_trees())
        .map_err(|missing| RpcError::new(codes::UNAVAILABLE, format!("Git inspection needs {missing}; run the app on a kernel with Landlock enabled")))?;
    Ok(command)
}

/// Error for a git command that exited nonzero, carrying git's own message.
pub fn command_error(display: &str, output: &Output) -> RpcError {
    let stderr = output.stderr_text();
    let message = if stderr.is_empty() { output.stdout_text().trim().to_owned() } else { stderr };
    RpcError::new(codes::CONFLICT, format!("`{display}` failed: {message}")).with_data(serde_json::json!({
        "exit_code": output.code,
        "stderr": output.stderr_text(),
    }))
}

/// The git executable plus the fixed configuration overrides every command gets.
#[derive(Debug, Clone)]
pub struct Git {
    pub exe: PathBuf,
    read_sandbox: Option<PathBuf>,
}

impl Git {
    pub fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            read_sandbox: None,
        }
    }

    /// A git command running in `cwd` with our configuration overrides.
    pub fn cmd(&self, cwd: &Path) -> Cmd {
        let mut command = Cmd::new(&self.exe, cwd).args([
            "-c",
            "core.quotePath=false",
            "-c",
            "color.ui=false",
            "-c",
            "core.pager=cat",
            "-c",
            "core.fsmonitor=false",
        ]);
        command.read_sandbox = self.read_sandbox.clone();
        command
    }

    /// Read-side Git may invoke repository-configured clean/process filters for its scratch index.
    pub fn confined(&self, scratch: &Path) -> Self {
        Self {
            exe: self.exe.clone(),
            read_sandbox: Some(scratch.to_path_buf()),
        }
    }

    /// Run `git <args>` in `cwd`, requiring success.
    pub async fn run<I, S>(&self, cwd: &Path, args: I) -> Result<Vec<u8>, RpcError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.cmd(cwd).args(args).ok().await
    }

    /// Run `git <args>` in `cwd`, returning trimmed stdout as text.
    pub async fn text<I, S>(&self, cwd: &Path, args: I) -> Result<String, RpcError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let out = self.run(cwd, args).await?;
        Ok(String::from_utf8_lossy(&out).trim_end_matches(['\n', '\r']).to_owned())
    }

    /// Run `git <args>` in `cwd`; `None` when the command exits nonzero.
    pub async fn try_text<I, S>(&self, cwd: &Path, args: I) -> Result<Option<String>, RpcError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let output = self.cmd(cwd).args(args).output().await?;
        if output.success() {
            Ok(Some(String::from_utf8_lossy(&output.stdout).trim_end_matches(['\n', '\r']).to_owned()))
        } else {
            Ok(None)
        }
    }
}

/// Find an executable by name on `PATH` (or accept an explicit path).
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return is_executable(candidate).then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|full| is_executable(full))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Split NUL-terminated records (the `-z` output formats). A trailing empty record is dropped.
pub fn split_nul(data: &[u8]) -> Vec<&[u8]> {
    let mut parts: Vec<&[u8]> = data.split(|byte| *byte == 0).collect();
    if parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    parts
}

/// Bytes of a path as git prints it (`-z` formats are verbatim), decoded for the contract.
pub fn path_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
