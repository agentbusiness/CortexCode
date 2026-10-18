//! Background jobs: commands that run detached (own session and process group, output written
//! straight to an artifact file), so they survive a hot-swap of this plugin. A monitor publishes a
//! `tools.job` event when the job exits and, optionally, when its output matches a regex or has been
//! silent for N seconds (wake rules). A successor instance adopts running jobs from disk.

use crate::shell::kill_group;
use crate::util;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Wake {
    /// Regex matched against each output line; fires once.
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
}

/// Something that can publish events (the frame connection, or a test sink).
pub trait Publisher: Send + Sync {
    fn publish(&self, topic: &str, data: Value);
}

pub struct Jobs {
    jobs_dir: PathBuf,
    artifacts_dir: PathBuf,
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

fn tail_of(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else { return String::new() };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    let _ = file.read_to_end(&mut buffer);
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
        let mut partial = String::new();
        let mut last_output = tokio::time::Instant::now();
        let mut silence_fired = false;
        let mut final_pass = false;
        loop {
            // New output.
            let len = std::fs::metadata(&output_path).map(|meta| meta.len()).unwrap_or(offset);
            if len > offset
                && let Ok(mut file) = std::fs::File::open(&output_path)
                && file.seek(SeekFrom::Start(offset)).is_ok()
            {
                let mut buffer = Vec::with_capacity((len - offset) as usize);
                let _ = (&mut file).take(len - offset).read_to_end(&mut buffer);
                offset += buffer.len() as u64;
                if !buffer.is_empty() {
                    last_output = tokio::time::Instant::now();
                    silence_fired = false;
                }
                if let Some(regex) = &pattern
                    && !matched
                {
                    partial.push_str(&String::from_utf8_lossy(&buffer));
                    let complete_upto = partial.rfind('\n').map(|index| index + 1).unwrap_or(0);
                    let mut hit = None;
                    for line in partial[..complete_upto].lines() {
                        if regex.is_match(line) {
                            hit = Some(line.to_owned());
                            break;
                        }
                    }
                    partial.drain(..complete_upto);
                    if hit.is_none() && regex.is_match(&partial) {
                        hit = Some(partial.clone());
                    }
                    if let Some(line) = hit {
                        matched = true;
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
                                "output_tail": tail_of(&output_path, 2048),
                            }));
                        }
                    }
                }
            }
            // Exit.
            let exited = std::fs::read_to_string(dir.join("exit")).ok().and_then(|text| text.trim().parse::<i32>().ok());
            let own_child_done = self.reap(&meta.job_id);
            let alive = exited.is_none() && !own_child_done && process_group_alive(meta.pid);
            if exited.is_some() || !alive {
                // One more pass over the output written just before the exit, then report.
                if !final_pass && std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(offset) > offset {
                    final_pass = true;
                    continue;
                }
                let current = self.meta(&meta.job_id).unwrap_or(meta.clone());
                meta.state = if current.state == "killed" {
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
                        "output_tail": tail_of(&output_path, 4096),
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
                    "output_tail": tail_of(&output_path, 2048),
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
        let tail = tail_of(&self.output_path(&meta), 4096);
        Some((meta, tail))
    }

    pub async fn kill(&self, job_id: &str) -> Option<JobMeta> {
        let mut meta = self.meta(job_id)?;
        if meta.state == "running" {
            meta.state = "killed".to_owned();
            self.write_meta(&meta);
            kill_group(meta.pid).await;
            self.reap(job_id);
        }
        Some(meta)
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
