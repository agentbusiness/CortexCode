//! Background jobs: commands that run detached (own session and process group, output written
//! straight to an artifact file), so they survive a hot-swap of this plugin. A monitor publishes a
//! `tools.job` event when the job exits and, optionally, when its output matches a regex or has been
//! silent for N seconds (wake rules). A successor instance adopts running jobs from disk.

use crate::util;
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OUTPUT_CHUNK: u64 = 64 * 1024;
const MATCH_WINDOW: usize = 64 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Wake {
    /// Regex matched within rolling 64 KiB output windows; fires once. Full output is retained.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Fire when the job has printed nothing for this many seconds (re-arms after new output).
    #[serde(default)]
    pub silence_s: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMeta {
    pub job_id: String,
    pub session_id: String,
    pub project_id: String,
    pub command: String,
    pub cwd: String,
    pub pid: i32,
    pub started_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// running | exited | killed | lost
    pub state: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub wake: Wake,
    /// Artifact id of the job's full output.
    pub artifact: String,
    /// Why the job was stopped, or what it left running (the host's note at the end of its output).
    #[serde(default)]
    pub note: Option<String>,
}

/// How a job is stopped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StopMode {
    /// SIGTERM only: the program may clean up (it can ignore it).
    Term,
    /// SIGKILL now.
    Kill,
    /// SIGTERM, then SIGKILL to what is left after 1.5 s.
    Escalate,
}

/// What a command runs, for telling a repeat of it: the first step that is not `cd`, `export` or
/// `source`, without its redirections, in single spaces (`cargo test csp:: 2>&1 | tail -30` and
/// `cargo test csp:: > log` both run `cargo test csp::`).
pub fn command_head(command: &str) -> String {
    let mut steps = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some(_), _) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            // `2>&1`, `>&2` and `&>file` are redirections, not a background `&`.
            (None, '&') if current.ends_with(['>', '<']) || chars.peek() == Some(&'>') => current.push(c),
            (None, ';' | '|' | '&' | '\n') => {
                steps.push(std::mem::take(&mut current));
                while chars.peek().is_some_and(|next| matches!(next, '|' | '&')) {
                    chars.next();
                }
            }
            (None, _) => current.push(c),
        }
    }
    steps.push(current);
    let skip = ["cd", "export", "source", ".", "set", "ulimit"];
    let step = steps.iter().map(|step| step.trim()).find(|step| !step.is_empty() && !step.split_whitespace().next().is_some_and(|word| skip.contains(&word))).unwrap_or("");
    let mut words = Vec::new();
    let mut redirect_target = false;
    for word in step.split_whitespace() {
        if redirect_target {
            redirect_target = false;
            continue;
        }
        let bare = word.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches('&');
        if bare.starts_with('>') || bare.starts_with('<') {
            // `>file` carries its target; `>` alone takes the next word.
            redirect_target = bare.trim_start_matches(['>', '<', '&']).is_empty();
            continue;
        }
        words.push(word);
    }
    words.join(" ")
}

/// Something that can publish events (the frame connection, or a test sink).
pub trait Publisher: Send + Sync {
    fn publish(&self, topic: &str, data: Value);
}

pub struct Jobs {
    jobs_dir: PathBuf,
    artifacts_dir: PathBuf,
    artifacts: crate::artifacts::Artifacts,
    /// Holds the `cx` link; kept on every job's PATH.
    bin_dir: PathBuf,
    bash: PathBuf,
    publisher: Mutex<Option<Arc<dyn Publisher>>>,
    running: Mutex<HashMap<String, std::process::Child>>,
    monitored: Mutex<std::collections::HashSet<String>>,
}

pub struct StartRequest<'a> {
    pub session_id: &'a str,
    pub project_id: &'a str,
    pub command: &'a str,
    pub cwd: &'a Path,
    /// Environment file (`export -p` output) of the session shell to start from.
    pub env_file: Option<&'a Path>,
    pub identity_env: Vec<(String, String)>,
    /// The language plugins' toolchains (PATH directories, variables).
    pub toolchain: crate::shell::Toolchain,
    pub wake: Wake,
    /// The write sandbox the job runs in (None only where no sandbox is wanted, e.g. tests).
    pub sandbox: Option<&'a crate::sandbox::Sandbox>,
    /// The Python venv to activate.
    pub venv: Option<&'a Path>,
}

fn tail_of(artifacts: &crate::artifacts::Artifacts, id: &str, bytes: u64) -> String {
    let Ok(mut file) = artifacts.open(id) else { return String::new() };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    let _ = file.take(len - start).read_to_end(&mut buffer);
    let text = String::from_utf8_lossy(&buffer).into_owned();
    if start > 0 {
        // Start at a line boundary.
        match text.find('\n') {
            Some(newline) if newline + 1 < text.len() => text[newline + 1..].to_owned(),
            _ => text,
        }
    } else {
        text
    }
}

impl Jobs {
    pub fn new(home: &Path, bash: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            jobs_dir: home.join("jobs"),
            artifacts_dir: home.join("artifacts"),
            artifacts: crate::artifacts::Artifacts::new(home),
            bin_dir: home.join("bin"),
            bash,
            publisher: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
            monitored: Mutex::new(Default::default()),
        })
    }

    pub fn set_publisher(&self, publisher: Arc<dyn Publisher>) {
        if let Ok(mut guard) = self.publisher.lock() {
            *guard = Some(publisher);
        }
    }

    fn publish(&self, data: Value) {
        let publisher = self.publisher.lock().ok().and_then(|guard| guard.clone());
        if let Some(publisher) = publisher {
            publisher.publish(cc_protocol::contracts::tools::topics::JOB_EVENT, data);
        }
    }

    fn dir(&self, job_id: &str) -> PathBuf {
        self.jobs_dir.join(job_id)
    }

    fn write_meta(&self, meta: &JobMeta) {
        let dir = self.dir(&meta.job_id);
        if let Ok(text) = serde_json::to_vec_pretty(meta) {
            let _ = util::atomic_write(&dir.join("meta.json"), &text);
        }
    }

    pub fn meta(&self, job_id: &str) -> Option<JobMeta> {
        if !crate::artifacts::valid_id(job_id) {
            return None;
        }
        let text = std::fs::read(self.dir(job_id).join("meta.json")).ok()?;
        serde_json::from_slice(&text).ok()
    }

    pub fn output_path(&self, meta: &JobMeta) -> PathBuf {
        self.artifacts_dir.join(&meta.artifact)
    }

    pub fn start(self: &Arc<Self>, request: StartRequest<'_>) -> std::io::Result<JobMeta> {
        if let Some(pattern) = &request.wake.pattern {
            Regex::new(pattern).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("wake pattern: {error}")))?;
        }
        let job_id = util::new_id("job");
        let dir = self.dir(&job_id);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&self.artifacts_dir)?;
        let artifact = job_id.clone();
        let output_path = self.artifacts_dir.join(&artifact);
        std::fs::write(dir.join("cmd.sh"), request.command)?;
        let q = |path: &Path| util::shell_quote(&path.to_string_lossy());
        let mut runner = String::new();
        if let Some(env_file) = request.env_file {
            runner.push_str(&format!(". {} >/dev/null 2>&1\n", q(env_file)));
        }
        for (key, value) in &request.identity_env {
            runner.push_str(&format!("export {key}={}\n", util::shell_quote(value)));
        }
        runner.push_str(&request.toolchain.script());
        runner.push('\n');
        let bin = q(&self.bin_dir);
        runner.push_str(&format!("case \":$PATH:\" in *:{bin}:*) ;; *) export PATH={bin}:\"$PATH\" ;; esac\n"));
        if let Some(venv) = request.venv {
            runner.push_str(&crate::pyenv::activation(venv));
        }
        runner.push_str(&format!(
            "cd {} || exit 1\n{} {} </dev/null\nrc=$?\necho \"$rc\" > {}.tmp && mv {}.tmp {}\n",
            q(request.cwd),
            q(&self.bash),
            q(&dir.join("cmd.sh")),
            q(&dir.join("exit")),
            q(&dir.join("exit")),
            q(&dir.join("exit")),
        ));
        std::fs::write(dir.join("runner.sh"), runner)?;
        let output = std::fs::File::create(&output_path)?;
        let errors = output.try_clone()?;
        let mut command = match request.sandbox {
            Some(sandbox) => {
                let mut command = std::process::Command::new(&sandbox.exec);
                command.args(sandbox.args()).arg(&self.bash);
                command
            }
            None => std::process::Command::new(&self.bash),
        };
        command
            .env_remove("VIRTUAL_ENV")
            .env_remove("PYTHONHOME")
            .env_remove("CORTEX_VENV")
            .env_remove("CORTEX_FRAME_TOKEN")
            .arg("--noprofile")
            .arg("--norc")
            .arg(dir.join("runner.sh"))
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            .envs(crate::util::project_env(request.cwd))
            .current_dir(request.cwd);
        // SAFETY: setsid is async-signal-safe; the job becomes its own session and group leader.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let meta = JobMeta {
            job_id: job_id.clone(),
            session_id: request.session_id.to_owned(),
            project_id: request.project_id.to_owned(),
            command: request.command.to_owned(),
            cwd: request.cwd.to_string_lossy().into_owned(),
            pid: child.id() as i32,
            started_ms: util::now_ms(),
            ended_ms: None,
            state: "running".to_owned(),
            exit_code: None,
            wake: request.wake,
            artifact,
            note: None,
        };
        self.write_meta(&meta);
        if let Ok(mut running) = self.running.lock() {
            running.insert(job_id.clone(), child);
        }
        self.monitor(meta.clone());
        Ok(meta)
    }

    /// Adopt jobs left running by a previous instance (and finalize ones that ended meanwhile).
    pub fn adopt(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else { return };
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Some(meta) = self.meta(&id)
                && meta.state == "running"
            {
                self.monitor(meta);
            }
        }
    }

    fn monitor(self: &Arc<Self>, meta: JobMeta) {
        {
            let Ok(mut monitored) = self.monitored.lock() else { return };
            if !monitored.insert(meta.job_id.clone()) {
                return;
            }
        }
        let jobs = self.clone();
        tokio::spawn(async move { jobs.watch(meta).await });
    }

    async fn watch(self: Arc<Self>, mut meta: JobMeta) {
        let dir = self.dir(&meta.job_id);
        let output_path = self.output_path(&meta);
        let pattern = meta.wake.pattern.as_deref().and_then(|pattern| Regex::new(pattern).ok());
        let silence = meta.wake.silence_s.filter(|seconds| *seconds > 0.0).map(Duration::from_secs_f64);
        let mut matched = dir.join("wake-match").exists();
        let mut offset: u64 = 0;
        let mut partial = Vec::new();
        let mut last_output = tokio::time::Instant::now();
        let mut silence_fired = false;
        loop {
            // New output.
            let len = std::fs::metadata(&output_path).map(|meta| meta.len()).unwrap_or(offset);
            let mut progressed = false;
            if len > offset
                && let Ok(mut file) = self.artifacts.open(&meta.artifact)
                && file.seek(SeekFrom::Start(offset)).is_ok()
            {
                let available = (len - offset).min(OUTPUT_CHUNK);
                let mut buffer = Vec::with_capacity(available as usize);
                let _ = (&mut file).take(available).read_to_end(&mut buffer);
                offset += buffer.len() as u64;
                if !buffer.is_empty() {
                    progressed = true;
                    last_output = tokio::time::Instant::now();
                    silence_fired = false;
                }
                if let Some(regex) = &pattern
                    && !matched
                {
                    // Keep bytes between reads so a UTF-8 character split by a chunk is intact.
                    partial.extend_from_slice(&buffer);
                    // Every scan is bounded, including output with no newline. Overlap keeps
                    // matches crossing a chunk boundary; output itself stays byte-exact on disk.
                    let mut hit = None;
                    while partial.len() > MATCH_WINDOW {
                        if regex.is_match(&partial[..MATCH_WINDOW]) {
                            hit = Some(String::from_utf8_lossy(&partial[..MATCH_WINDOW]).into_owned());
                            break;
                        }
                        partial.drain(..MATCH_WINDOW / 2);
                    }
                    let complete_upto = partial.iter().rposition(|byte| *byte == b'\n').map(|index| index + 1).unwrap_or(0);
                    if hit.is_none() {
                        for line in partial[..complete_upto].split(|byte| *byte == b'\n') {
                            let line = line.strip_suffix(b"\r").unwrap_or(line);
                            if regex.is_match(line) {
                                hit = Some(String::from_utf8_lossy(line).into_owned());
                                break;
                            }
                        }
                    }
                    partial.drain(..complete_upto);
                    if hit.is_none() && regex.is_match(&partial) {
                        hit = Some(String::from_utf8_lossy(&partial).into_owned());
                    }
                    if let Some(line) = hit {
                        matched = true;
                        partial.clear();
                        // Exclusive marker: only one instance publishes this wake.
                        if std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join("wake-match")).is_ok() {
                            self.publish(json!({
                                "job_id": meta.job_id,
                                "session_id": meta.session_id,
                                "project_id": meta.project_id,
                                "state": "running",
                                "reason": "match",
                                "matched": line,
                                "artifact": meta.artifact,
                                "output_tail": tail_of(&self.artifacts, &meta.artifact, 2048),
                            }));
                        }
                    }
                }
                if !buffer.is_empty() && offset < len {
                    tokio::task::yield_now().await;
                    continue;
                }
            }
            // Exit.
            let exited = std::fs::read_to_string(dir.join("exit")).ok().and_then(|text| text.trim().parse::<i32>().ok());
            // The runner ending is not the job ending: when it is killed first, the command it
            // started still runs in the job's group (seen: a `sleep` left running, the job shown
            // as ended and Kill finding nothing to kill).
            self.reap(&meta.job_id);
            let alive = exited.is_none() && process_group_alive(meta.pid);
            if exited.is_some() || !alive {
                // The job can write its final bytes after the size sample above. Drain them
                // in bounded chunks, but do not loop forever when the artifact cannot be read.
                let final_len = std::fs::metadata(&output_path).map(|meta| meta.len()).unwrap_or(offset);
                if final_len > offset && (len <= offset || progressed) {
                    tokio::task::yield_now().await;
                    continue;
                }
                let current = self.meta(&meta.job_id).unwrap_or(meta.clone());
                let mut notes = Vec::new();
                // What the job's command left running when it ended: nobody waits for it.
                let left = crate::guard::tree(meta.pid);
                if !left.is_empty() {
                    let what = crate::guard::describe(&left, &meta.cwd).unwrap_or_default();
                    crate::guard::stop(meta.pid).await;
                    notes.push(format!("[it left {what} running after it ended; the host stopped that. Start a program that must keep running as the background job itself, not with `&` inside one.]"));
                }
                let stopped = std::fs::read_to_string(dir.join("stop-note")).ok().filter(|note| !note.trim().is_empty());
                if let Some(note) = &stopped {
                    notes.insert(0, note.trim().to_owned());
                }
                if !notes.is_empty() {
                    let text = notes.join("\n");
                    if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&output_path) {
                        use std::io::Write;
                        let _ = writeln!(file, "\n{text}");
                    }
                    meta.note = Some(text);
                }
                meta.state = if current.state == "killed" || stopped.is_some() {
                    "killed".to_owned()
                } else if exited.is_some() {
                    "exited".to_owned()
                } else {
                    "lost".to_owned()
                };
                meta.exit_code = exited;
                meta.ended_ms = Some(util::now_ms());
                self.write_meta(&meta);
                if std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join("published-exit")).is_ok() {
                    self.publish(json!({
                        "job_id": meta.job_id,
                        "session_id": meta.session_id,
                        "project_id": meta.project_id,
                        "state": meta.state,
                        "exit_code": meta.exit_code,
                        "reason": "exit",
                        "artifact": meta.artifact,
                        "output_tail": tail_of(&self.artifacts, &meta.artifact, 4096),
                    }));
                }
                if let Ok(mut monitored) = self.monitored.lock() {
                    monitored.remove(&meta.job_id);
                }
                return;
            }
            // Silence.
            if let Some(limit) = silence
                && !silence_fired
                && last_output.elapsed() >= limit
            {
                silence_fired = true;
                self.publish(json!({
                    "job_id": meta.job_id,
                    "session_id": meta.session_id,
                    "project_id": meta.project_id,
                    "state": "running",
                    "reason": "silence",
                    "silent_s": limit.as_secs_f64(),
                    "artifact": meta.artifact,
                    "output_tail": tail_of(&self.artifacts, &meta.artifact, 2048),
                }));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Reap our own child if it exited (avoids zombies). Returns true once it has exited.
    fn reap(&self, job_id: &str) -> bool {
        let Ok(mut running) = self.running.lock() else { return false };
        let Some(child) = running.get_mut(job_id) else { return false };
        match child.try_wait() {
            Ok(Some(_)) => {
                running.remove(job_id);
                true
            }
            Ok(None) => false,
            Err(_) => {
                running.remove(job_id);
                true
            }
        }
    }

    pub fn status(&self, job_id: &str) -> Option<(JobMeta, String)> {
        let meta = self.meta(job_id)?;
        let tail = tail_of(&self.artifacts, &meta.artifact, 4096);
        Some((meta, tail))
    }

    pub async fn kill(&self, job_id: &str) -> Option<JobMeta> {
        let mut meta = self.meta(job_id)?;
        if meta.state == "running" {
            meta.state = "killed".to_owned();
            self.write_meta(&meta);
            crate::guard::stop(meta.pid).await;
            self.reap(job_id);
        }
        Some(meta)
    }

    /// Stop a running job, with the note its output ends with (why, and what to do next). The job
    /// counts as killed once its processes are gone.
    pub async fn stop(&self, job_id: &str, note: &str, mode: StopMode) -> Option<JobMeta> {
        let meta = self.meta(job_id)?;
        if meta.state != "running" && crate::guard::tree(meta.pid).is_empty() {
            return Some(meta);
        }
        let _ = util::atomic_write(&self.dir(job_id).join("stop-note"), note.as_bytes());
        match mode {
            StopMode::Escalate => crate::guard::stop(meta.pid).await,
            StopMode::Term => crate::guard::signal(meta.pid, &crate::guard::tree(meta.pid), libc::SIGTERM),
            StopMode::Kill => crate::guard::signal(meta.pid, &crate::guard::tree(meta.pid), libc::SIGKILL),
        }
        self.reap(job_id);
        Some(meta)
    }

    /// The running jobs.
    pub fn running(&self) -> Vec<JobMeta> {
        self.list(None).into_iter().filter(|meta| meta.state == "running").collect()
    }

    /// How much a job has printed so far, in bytes.
    pub fn output_len(&self, meta: &JobMeta) -> u64 {
        std::fs::metadata(self.output_path(meta)).map(|file| file.len()).unwrap_or(0)
    }

    /// The running jobs of `session` in `cwd` that run what `command` runs (see [`command_head`]).
    pub fn same_command(&self, session: &str, cwd: &Path, command: &str) -> Vec<JobMeta> {
        let head = command_head(command);
        if head.is_empty() {
            return Vec::new();
        }
        let cwd = cwd.to_string_lossy();
        self.running().into_iter().filter(|meta| meta.session_id == session && meta.cwd == cwd && command_head(&meta.command) == head).collect()
    }

    pub fn list(&self, session: Option<&str>) -> Vec<JobMeta> {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else { return Vec::new() };
        let mut out: Vec<JobMeta> = entries
            .flatten()
            .filter_map(|entry| self.meta(&entry.file_name().to_string_lossy()))
            .filter(|meta| session.is_none_or(|session| meta.session_id == session))
            .collect();
        out.sort_by_key(|meta| meta.started_ms);
        out
    }
}

fn process_group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the group has a member we may signal.
    pgid > 1 && unsafe { libc::killpg(pgid, 0) } == 0
}
