//! Persistent per-session shells.
//!
//! Each session gets a real bash process (its own session and process group, no controlling
//! terminal, stdin of every command is /dev/null). Commands are written to a file and sourced in
//! the shell, so exported variables and functions persist. Every command starts in the workspace:
//! a `cd` lasts for its own command (a directory carried over from an earlier one was the model's
//! blind spot: it lost the project after one `cd` into a temp folder). A sentinel line carrying a random
//! nonce and the exit status delimits each command's output. After every command the shell saves its
//! working directory and exported environment, so a shell that had to be killed (timeout, cancel)
//! or that exited is restarted transparently with the same state.

use crate::util;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Who a shell belongs to; exported into its environment.
#[derive(Debug, Clone)]
pub struct Identity {
    pub session_id: String,
    pub project_id: String,
    pub workspace: PathBuf,
    pub home: PathBuf,
    /// The write sandbox the shell runs in (None: unconfined, used only by tests of the shell itself).
    pub sandbox: Option<crate::sandbox::Sandbox>,
    /// The Python venv the shell activates before each command.
    pub venv: Option<PathBuf>,
    pub command_access: Option<cc_protocol::contracts::frame::CommandAccessResult>,
}

#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Directory holding per-session state (saved cwd/env, command files).
    pub state_root: PathBuf,
    /// Directory prepended to PATH (holds the `cx` link).
    pub bin_dir: Option<PathBuf>,
    pub bash: PathBuf,
}

/// Pick the newest bash available (structural: first existing path).
pub fn find_bash(configured: Option<&str>) -> PathBuf {
    if let Some(path) = configured {
        return PathBuf::from(path);
    }
    for candidate in ["/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/bin/bash"] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("bash")
}

/// Captured output: in memory until it grows past `spill_at`, then spooled to a file with the head
/// and tail kept in memory for the inline view.
pub struct Capture {
    mem: Vec<u8>,
    spill: Option<(PathBuf, std::fs::File)>,
    spill_dir: PathBuf,
    spill_at: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    keep: usize,
    pub total: u64,
    pub newlines: u64,
}

pub enum Captured {
    Mem(Vec<u8>),
    File { path: PathBuf, head: Vec<u8>, tail: Vec<u8>, total: u64, newlines: u64 },
}

impl Captured {
    /// The whole output as text, when it is at most `limit` bytes (a spilled capture is read back
    /// from its file).
    pub fn whole(&self, limit: u64) -> Option<String> {
        match self {
            Captured::Mem(bytes) => (bytes.len() as u64 <= limit).then(|| String::from_utf8_lossy(bytes).into_owned()),
            Captured::File { path, total, .. } => {
                (*total <= limit).then(|| std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok().and_then(|mut file| {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).ok()?;
                    Some(String::from_utf8_lossy(&bytes).into_owned())
                })).flatten()
            }
        }
    }
}

impl Capture {
    pub fn new(spill_dir: &Path) -> Self {
        Self {
            mem: Vec::new(),
            spill: None,
            spill_dir: spill_dir.to_path_buf(),
            spill_at: 8 * 1024 * 1024,
            head: Vec::new(),
            tail: Vec::new(),
            keep: 64 * 1024,
            total: 0,
            newlines: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.total += bytes.len() as u64;
        self.newlines += bytes.iter().filter(|b| **b == b'\n').count() as u64;
        if let Some((_, file)) = &mut self.spill {
            let _ = file.write_all(bytes);
            self.tail.extend_from_slice(bytes);
            if self.tail.len() > self.keep * 2 {
                let cut = self.tail.len() - self.keep;
                self.tail.drain(..cut);
            }
            return;
        }
        self.mem.extend_from_slice(bytes);
        if self.mem.len() > self.spill_at && std::fs::create_dir_all(&self.spill_dir).is_ok() {
            let path = self.spill_dir.join(format!(".spool-{}", util::new_id("o")));
            if let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).open(&path)
                && file.write_all(&self.mem).is_ok()
            {
                self.head = self.mem[..self.keep.min(self.mem.len())].to_vec();
                let start = self.mem.len().saturating_sub(self.keep);
                self.tail = self.mem[start..].to_vec();
                self.mem = Vec::new();
                self.spill = Some((path, file));
            }
        }
    }

    pub fn finish(mut self) -> Captured {
        match self.spill.take() {
            Some((path, mut file)) => {
                let _ = file.flush();
                let start = self.tail.len().saturating_sub(self.keep);
                Captured::File { path, head: self.head, tail: self.tail[start..].to_vec(), total: self.total, newlines: self.newlines }
            }
            None => Captured::Mem(self.mem),
        }
    }
}

struct Shell {
    pid: i32,
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Bytes that arrived after the previous sentinel (e.g. from `&` background processes).
    leftover: Vec<u8>,
    last_used: Instant,
    /// Key of the sandbox the shell was started in.
    sandbox_key: Option<String>,
}

pub struct RunRequest<'a> {
    pub command: &'a str,
    /// Run in this directory for this call only (does not change the shell's directory).
    pub cwd: Option<PathBuf>,
    /// Extra environment for this call only.
    pub env: Vec<(String, String)>,
    /// Toolchains of the language plugins (directories put in front of PATH, variables), set in the
    /// persistent shell itself so they stay with `cd` and exports.
    pub toolchain: Toolchain,
    pub timeout: Duration,
}

/// What the language plugins' installed toolchains need in commands (`lang@1` `path` / `env`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Toolchain {
    pub path: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Toolchain {
    /// Shell lines that put the directories in front of PATH (once) and export the variables.
    pub fn script(&self) -> String {
        let mut script = String::new();
        for dir in self.path.iter().rev() {
            let dir = util::shell_quote(dir);
            script.push_str(&format!("case \":$PATH:\" in *:{dir}:*) ;; *) export PATH={dir}:\"$PATH\" ;; esac; "));
        }
        for (key, value) in &self.env {
            if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                script.push_str(&format!("export {key}={}; ", util::shell_quote(value)));
            }
        }
        script
    }
}

pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// The shell process itself ended during the command (e.g. `exit`); it restarts on next use.
    pub shell_exited: bool,
    pub output: Captured,
    pub duration_ms: u64,
    /// Each stage's exit status of the command's last pipeline (one entry when it is no pipe;
    /// empty when the shell did not get that far).
    pub pipe_status: Vec<i32>,
    /// Why the machine guard stopped the command (its note), when it did.
    pub stopped: Option<String>,
    /// At a timeout: what the command was really running when it was killed, in words.
    pub running: Option<String>,
}

pub struct Shells {
    config: ShellConfig,
    shells: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Shell>>>>>,
    /// The shells' process groups, for the machine guard.
    pub groups: Arc<crate::guard::ShellGroups>,
}

fn sanitize(key: &str) -> String {
    let clean: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if clean.len() > 80 { clean[..80].to_owned() } else { clean }
}

impl Shells {
    pub fn new(config: ShellConfig) -> Self {
        Self { config, shells: Mutex::new(HashMap::new()), groups: Arc::new(crate::guard::ShellGroups::default()) }
    }

    pub fn config(&self) -> &ShellConfig {
        &self.config
    }

    /// State directory of a session's shell.
    pub fn state_dir(&self, session: &str) -> PathBuf {
        self.config.state_root.join(sanitize(session))
    }

    /// The session shell's last saved working directory (or None before its first command).
    pub fn saved_cwd(&self, session: &str) -> Option<PathBuf> {
        let text = std::fs::read_to_string(self.state_dir(session).join("cwd")).ok()?;
        let path = PathBuf::from(text.trim_end_matches('\n'));
        path.is_dir().then_some(path)
    }

    /// The session shell's saved environment file, if any.
    pub fn saved_env(&self, session: &str) -> Option<PathBuf> {
        let path = self.state_dir(session).join("env");
        path.exists().then_some(path)
    }

    fn slot(&self, session: &str) -> Arc<tokio::sync::Mutex<Option<Shell>>> {
        let mut shells = match self.shells.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        shells.entry(session.to_owned()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))).clone()
    }

    fn spawn(&self, identity: &Identity) -> std::io::Result<Shell> {
        let state = self.state_dir(&identity.session_id);
        std::fs::create_dir_all(&state)?;
        let start_dir = if identity.workspace.is_dir() { identity.workspace.clone() } else { std::env::temp_dir() };
        let scratch = if identity.workspace.is_dir() { util::ensure_scratch(&identity.workspace) } else { std::env::temp_dir() };
        let (reader, writer) = std::io::pipe()?;
        let writer_err = writer.try_clone()?;
        let mut command = match &identity.sandbox {
            Some(sandbox) => sandbox.command(&self.config.bash)?,
            None => std::process::Command::new(&self.config.bash),
        };
        command
            .args(["--noprofile", "--norc"])
            // A venv active where the frame was started is not the session's environment.
            .env_remove("VIRTUAL_ENV")
            .env_remove("PYTHONHOME")
            .env_remove("CORTEX_VENV")
            .env_remove("CORTEX_FRAME_TOKEN")
            .stdin(Stdio::piped())
            .stdout(writer)
            .stderr(writer_err)
            .current_dir(&start_dir)
            .env("CORTEX_SESSION_ID", &identity.session_id)
            .env("CORTEX_PROJECT_ID", &identity.project_id)
            .env("CORTEX_HOME", &identity.home)
            .env("CORTEX_WORKSPACE", &identity.workspace)
            // A check written in the scratch folder imports the project's code as usual: Python
            // looks next to the script and Node resolves packages from the script's folder.
            .env("PYTHONPATH", &identity.workspace)
            .env("NODE_PATH", identity.workspace.join("node_modules"))
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("TERM", "dumb")
            // The project's own environment (`.cortex-env`): a toolchain this machine lacks on PATH.
            .envs(util::project_env(&identity.workspace));
        // SAFETY: setsid is async-signal-safe; it makes the shell a session and process-group
        // leader without a controlling terminal, so the group can be killed as a whole and
        // programs cannot block on /dev/tty.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(command);
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("shell stdin unavailable"))?;
        let pid = child.id() as i32;
        self.groups.started(&identity.session_id, pid, &identity.workspace.to_string_lossy());
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new().name(format!("shell-{pid}")).spawn(move || {
            let mut reader = reader;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;
        let sandbox_key = identity.sandbox.as_ref().map(crate::sandbox::Sandbox::key);
        let mut shell = Shell { pid, child, stdin, rx, leftover: Vec::new(), last_used: Instant::now(), sandbox_key };
        let q = |path: &Path| util::shell_quote(&path.to_string_lossy());
        let mut init = String::new();
        init.push_str("set +o history >/dev/null 2>&1\nshopt -s expand_aliases >/dev/null 2>&1\n");
        // Restore what the previous shell of this session saved: exported variables, functions
        // and aliases (a shell is replaced after a timeout, a cancel or `exit`).
        for name in ["env", "functions", "aliases"] {
            let file = state.join(name);
            if file.exists() {
                init.push_str(&format!(". {} >/dev/null 2>&1\n", q(&file)));
            }
        }
        // Identity always wins over a restored environment.
        init.push_str(&format!(
            "export CORTEX_SESSION_ID={} CORTEX_PROJECT_ID={} CORTEX_HOME={} CORTEX_WORKSPACE={}\n",
            util::shell_quote(&identity.session_id),
            util::shell_quote(&identity.project_id),
            q(&identity.home),
            q(&identity.workspace)
        ));
        // `$TMPDIR` in the session's commands is its scratch folder (throwaway checks; `edit` writes
        // there too), but the programs they start do not get it: a temp folder inside the project
        // puts their temp files under its own config and git root (seen: tests that make a temp
        // folder and expect no justfile above it found the project's and failed). They use the
        // system's temp folder.
        init.push_str(&format!("TMPDIR={}; export -n TMPDIR\n", q(&scratch)));
        if let Some(access) = &identity.command_access {
            init.push_str(&format!(
                "export CORTEX_COMMAND_URL={} CORTEX_COMMAND_TOKEN={}\n",
                util::shell_quote(&access.url),
                util::shell_quote(&access.token)
            ));
        }
        if let Some(bin) = &self.config.bin_dir {
            let bin = q(bin);
            init.push_str(&format!("case \":$PATH:\" in *:{bin}:*) ;; *) export PATH={bin}:\"$PATH\" ;; esac\n"));
        }
        let cwd = self.saved_cwd(&identity.session_id).unwrap_or(start_dir);
        init.push_str(&format!("cd {} >/dev/null 2>&1 || cd {} >/dev/null 2>&1\n", q(&cwd), q(&identity.workspace)));
        shell.stdin.write_all(init.as_bytes())?;
        shell.stdin.flush()?;
        Ok(shell)
    }

    /// Run one command in the session's shell. Commands of one session run one at a time.
    pub async fn run(
        &self,
        identity: &Identity,
        request: RunRequest<'_>,
        cancel: CancellationToken,
        mut on_output: impl FnMut(&[u8]) + Send,
    ) -> std::io::Result<RunOutput> {
        let slot = self.slot(&identity.session_id);
        let mut guard = slot.lock().await;
        let started = Instant::now();
        if guard.as_ref().is_some_and(|shell| shell.rx.is_closed()) {
            *guard = None;
        }
        // A shell started under another sandbox (an approval widened it) is replaced; its state is
        // on disk, so the new shell continues in the same directory with the same environment.
        let wanted_key = identity.sandbox.as_ref().map(crate::sandbox::Sandbox::key);
        if guard.as_ref().is_some_and(|shell| shell.sandbox_key != wanted_key)
            && let Some(mut old) = guard.take()
        {
            terminate(&mut old).await;
        }
        let mut shell = match guard.take() {
            Some(shell) => shell,
            None => self.spawn(identity)?,
        };
        let state = self.state_dir(&identity.session_id);
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let command_file = state.join(format!("cmd-{nonce}.sh"));
        let q = |path: &Path| util::shell_quote(&path.to_string_lossy());
        // Each stage's exit status of the command's last pipeline, kept with the status the command
        // ends with (a `| tail` returns its own 0 and hides the failure before it).
        let pipe_file = state.join(format!("pipe-{nonce}"));
        let script = format!(
            "{}\n__cx_ps=\"${{PIPESTATUS[*]}}\" __cx_pr=$?; printf '%s' \"$__cx_ps\" >{} 2>/dev/null; (exit $__cx_pr)\n",
            request.command,
            q(&pipe_file)
        );
        std::fs::write(&command_file, &script)?;
        let mut prefix = String::new();
        for (key, value) in &request.env {
            if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                prefix.push_str(&format!("export {key}={}; ", util::shell_quote(value)));
            }
        }
        // Every command starts in the workspace (or the directory asked for this call).
        let start = request.cwd.clone().unwrap_or_else(|| identity.workspace.clone());
        let body = if request.cwd.is_some() || !prefix.is_empty() {
            format!("( {prefix}cd -- {} && . {} ) </dev/null", q(&start), q(&command_file))
        } else {
            format!("cd -- {} >/dev/null 2>&1; . {} </dev/null", q(&start), q(&command_file))
        };
        let marker = format!("__CX_DONE_{nonce}_");
        let activate = identity.venv.as_deref().map(crate::pyenv::activation).unwrap_or_default();
        let toolchain = request.toolchain.script();
        let line = format!(
            "{toolchain}{activate}{body}; __cx_rc=$?; pwd -P >{} 2>/dev/null; export -p >{} 2>/dev/null; declare -f >{} 2>/dev/null; alias -p >{} 2>/dev/null; rm -f {}; printf '%s%s__\\n' '{marker}' \"$__cx_rc\"\n",
            q(&state.join("cwd")),
            q(&state.join("env")),
            q(&state.join("functions")),
            q(&state.join("aliases")),
            q(&command_file),
        );
        if let Err(error) = shell.stdin.write_all(line.as_bytes()).and_then(|_| shell.stdin.flush()) {
            // The shell died between commands; start a fresh one and retry once.
            terminate(&mut shell).await;
            shell = self.spawn(identity)?;
            std::fs::write(&command_file, &script)?;
            shell.stdin.write_all(line.as_bytes()).and_then(|_| shell.stdin.flush()).map_err(|_| error)?;
        }

        let shell_pid = shell.pid;
        let mut capture = Capture::new(&identity.home.join("artifacts"));
        let mut pending = std::mem::take(&mut shell.leftover);
        let deadline = tokio::time::sleep(request.timeout);
        tokio::pin!(deadline);
        let marker_bytes = marker.as_bytes().to_vec();
        let hold = marker_bytes.len() + 16;
        let mut exit_code = None;
        let mut timed_out = false;
        let mut cancelled = false;
        let mut shell_exited = false;
        let mut finished = false;
        let mut running = None;
        loop {
            // Look for the sentinel in what we have.
            if let Some(position) = find(&pending, &marker_bytes) {
                let after = &pending[position + marker_bytes.len()..];
                if let Some(end) = find(after, b"__\n") {
                    let code: Option<i32> = std::str::from_utf8(&after[..end]).ok().and_then(|text| text.parse().ok());
                    let output = pending[..position].to_vec();
                    on_output(&output);
                    capture.push(&output);
                    shell.leftover = after[end + 3..].to_vec();
                    exit_code = code;
                    finished = true;
                }
            } else if pending.len() > hold {
                let commit = pending.len() - hold;
                let chunk: Vec<u8> = pending.drain(..commit).collect();
                on_output(&chunk);
                capture.push(&chunk);
            }
            if finished {
                break;
            }
            tokio::select! {
                chunk = shell.rx.recv() => match chunk {
                    Some(chunk) => pending.extend_from_slice(&chunk),
                    None => {
                        // The shell exited (e.g. the command ran `exit`).
                        on_output(&pending);
                        capture.push(&pending);
                        let status = wait_child(&mut shell.child, Duration::from_secs(2)).await;
                        exit_code = status;
                        shell_exited = true;
                        break;
                    }
                },
                _ = &mut deadline => {
                    timed_out = true;
                    break;
                }
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
            }
        }
        if timed_out || cancelled {
            // Collect what is buffered, then kill the whole process group.
            while let Ok(chunk) = shell.rx.try_recv() {
                pending.extend_from_slice(&chunk);
            }
            let tail = match find(&pending, &marker_bytes) {
                Some(position) => pending[..position].to_vec(),
                None => pending.clone(),
            };
            on_output(&tail);
            capture.push(&tail);
            if timed_out {
                running = crate::guard::describe(&crate::guard::tree(shell.pid), &start.to_string_lossy());
            }
            terminate(&mut shell).await;
            let _ = std::fs::remove_file(&command_file);
            // The shell is gone; the next command starts a new one with the saved state.
            *guard = None;
        } else if shell_exited {
            let _ = std::fs::remove_file(&command_file);
            *guard = None;
        } else {
            shell.last_used = Instant::now();
            *guard = Some(shell);
        }
        let pipe_status = std::fs::read_to_string(&pipe_file)
            .map(|text| text.split_whitespace().filter_map(|code| code.parse().ok()).collect())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&pipe_file);
        let stopped = if shell_exited { self.groups.take_stopped(shell_pid) } else { None };
        Ok(RunOutput {
            exit_code,
            timed_out,
            cancelled,
            shell_exited,
            output: capture.finish(),
            duration_ms: started.elapsed().as_millis() as u64,
            pipe_status,
            stopped,
            running,
        })
    }

    /// Copy a session's saved state (directory, environment, functions, aliases) to another session,
    /// so a one-off shell starts where the session's shell is.
    pub fn copy_state(&self, from: &str, to: &str) -> std::io::Result<()> {
        let source = self.state_dir(from);
        let target = self.state_dir(to);
        std::fs::create_dir_all(&target)?;
        for name in ["cwd", "env", "functions", "aliases"] {
            let file = source.join(name);
            if file.exists() {
                std::fs::copy(&file, target.join(name))?;
            }
        }
        Ok(())
    }

    /// Kill a session's shell and forget its saved state.
    pub async fn drop_session(&self, session: &str) {
        let slot = {
            let mut shells = match self.shells.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            shells.remove(session)
        };
        if let Some(slot) = slot {
            let mut guard = slot.lock().await;
            if let Some(mut shell) = guard.take() {
                terminate(&mut shell).await;
            }
        }
        let _ = std::fs::remove_dir_all(self.state_dir(session));
    }

    /// Stop shells that have been idle longer than `idle` (their state is on disk; the next command
    /// of that session starts a new shell in the same directory with the same environment).
    pub async fn reap_idle(&self, idle: Duration) -> usize {
        let slots: Vec<_> = match self.shells.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
        };
        let mut reaped = 0;
        for slot in slots {
            // A busy shell is in use, not idle.
            let Ok(mut guard) = slot.try_lock() else { continue };
            if guard.as_ref().is_some_and(|shell| shell.last_used.elapsed() >= idle)
                && let Some(mut shell) = guard.take()
            {
                terminate(&mut shell).await;
                reaped += 1;
            }
        }
        reaped
    }

    /// Kill every shell (plugin shutdown).
    pub async fn shutdown(&self) {
        let slots: Vec<_> = match self.shells.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
        };
        for slot in slots {
            let mut guard = slot.lock().await;
            if let Some(mut shell) = guard.take() {
                terminate(&mut shell).await;
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

async fn wait_child(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                return status.code().or_else(|| status.signal().map(|signal| 128 + signal));
            }
            Ok(None) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(20)).await,
            _ => return None,
        }
    }
}

/// Kill a process group: SIGTERM, then SIGKILL until no member is left.
pub async fn kill_group(pgid: i32) {
    if pgid <= 1 {
        return;
    }
    // SAFETY: plain syscalls on a process group id we created with setsid.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks for existence.
        if unsafe { libc::killpg(pgid, 0) } == -1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for _ in 0..80 {
        // SAFETY: as above.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        // SAFETY: as above.
        if unsafe { libc::killpg(pgid, 0) } == -1 {
            return;
        }
    }
}

async fn terminate(shell: &mut Shell) {
    kill_group(shell.pid).await;
    let _ = wait_child(&mut shell.child, Duration::from_secs(2)).await;
}
