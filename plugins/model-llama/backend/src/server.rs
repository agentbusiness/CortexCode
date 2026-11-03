//! The supervised `llama-server` child: started on a free loopback port, health-waited, recorded
//! in `<data>/llama-server.pid` (a stale one left by a killed plugin is reclaimed at start), and
//! killed when it is dropped, unloaded or the plugin ends.

use crate::catalog::Spec;
use crate::config::Config;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PID_FILE: &str = "llama-server.pid";

pub struct Server {
    pub spec: &'static Spec,
    pub base: String,
    pub http: reqwest::Client,
    pub kv_type: String,
    pub load_ms: f64,
    pub ctx_native: u32,
    child: std::sync::Mutex<Option<tokio::process::Child>>,
    pid: u32,
    pid_file: PathBuf,
}

/// The llama-server binary: beside this executable (the package's `backend/`), else the
/// configured path.
pub fn binary(config: &Config) -> Result<PathBuf, String> {
    if let Some(path) = &config.server_bin {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let path = exe.parent().map(|dir| dir.join("llama").join("llama-server")).ok_or("the executable has no directory")?;
    if path.is_file() { Ok(path) } else { Err(format!("llama-server is missing beside {}", exe.display())) }
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    listener.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn command_of(pid: u32) -> String {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn terminate(pid: u32) {
    if !alive(pid) {
        return;
    }
    // SAFETY: plain signals to a process this plugin started.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let started = Instant::now();
    // Reap it when it is this process's child (a zombie still answers signal 0).
    let gone = || {
        // SAFETY: non-blocking wait on one pid; fails harmlessly for a process that is not our child.
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
        !alive(pid)
    };
    while !gone() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if alive(pid) {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}

/// Kill a llama-server a previous run of this plugin left behind (its pid file survived).
pub fn reclaim_stale(data_dir: &Path) {
    let path = data_dir.join(PID_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    if let Ok(pid) = text.trim().parse::<u32>()
        && alive(pid)
        && command_of(pid).ends_with("llama-server")
    {
        tracing::warn!(pid, "stopping a llama-server left behind by an earlier run");
        terminate(pid);
    }
    remove_if(&path, &text);
}

/// Remove the pid file only while it still names the process it named (a successor may have
/// written its own).
fn remove_if(path: &Path, expected: &str) {
    if std::fs::read_to_string(path).is_ok_and(|now| now == expected) {
        let _ = std::fs::remove_file(path);
    }
}

pub struct Launch<'a> {
    pub spec: &'static Spec,
    /// The shared models folder the variant's files are in.
    pub models: PathBuf,
    pub config: &'a Config,
    pub data_dir: PathBuf,
    pub kv_type: String,
    /// The GPU's llama.cpp device (`gpu::Gpu::device`); the drafter follows it.
    pub device: String,
}

impl Server {
    pub async fn start(launch: Launch<'_>) -> Result<Server, String> {
        let Launch { spec, models, config, data_dir, kv_type, device } = launch;
        let bin = binary(config)?;
        let port = free_port()?;
        let mut args: Vec<String> = vec![
            "-m".into(),
            spec.weights.path(&models).display().to_string(),
            // DFlash 2: the drafter proposes a block of tokens in one pass; Qwen verifies them.
            "--spec-type".into(),
            "draft-dflash".into(),
            "--spec-draft-model".into(),
            spec.drafter.path(&models).display().to_string(),
            "--spec-draft-n-max".into(),
            config.draft_max.to_string(),
            "--mmproj".into(),
            spec.mmproj.path(&models).display().to_string(),
            "--device".into(),
            device,
            "-ngl".into(),
            "999".into(),
            "-c".into(),
            (config.ctx * config.parallel).to_string(),
            "--parallel".into(),
            config.parallel.to_string(),
            "-ub".into(),
            config.ubatch.to_string(),
            "-b".into(),
            (config.ubatch.max(512)).to_string(),
            "--jinja".into(),
            "--no-webui".into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
        ];
        if !spec.mmproj_on_gpu {
            args.push("--no-mmproj-offload".into());
        }
        if kv_type != "f16" {
            // A quantized V cache exists only on the flash-attention path.
            args.extend(["-fa".into(), "on".into(), "-ctk".into(), kv_type.clone(), "-ctv".into(), kv_type.clone()]);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("llama-server.log"))
            .map_err(|e| format!("cannot open the server log: {e}"))?;
        let started = Instant::now();
        tracing::info!(bin = %bin.display(), ?args, "starting llama-server");
        let mut child = tokio::process::Command::new(&bin)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
        let pid = child.id().unwrap_or(0);
        let pid_file = data_dir.join(PID_FILE);
        let _ = std::fs::write(&pid_file, pid.to_string());
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
        // Health: 503 while loading, 200 when ready.
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let _ = std::fs::remove_file(&pid_file);
                return Err(format!("llama-server exited while loading ({status}); see {}", data_dir.join("llama-server.log").display()));
            }
            if let Ok(response) = http.get(format!("{base}/health")).timeout(Duration::from_secs(2)).send().await
                && response.status().is_success()
            {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&pid_file);
                return Err("llama-server did not become healthy within 300 s".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let load_ms = started.elapsed().as_secs_f64() * 1000.0;
        let ctx_native = match http.get(format!("{base}/v1/models")).send().await {
            Ok(response) => response.json::<serde_json::Value>().await.ok().and_then(|v| v["data"][0]["meta"]["n_ctx_train"].as_u64()).unwrap_or(0) as u32,
            Err(_) => 0,
        };
        tracing::info!(variant = spec.id, load_ms, port, pid, "llama-server is ready");
        Ok(Server { spec, base, http, kv_type, load_ms, ctx_native, child: std::sync::Mutex::new(Some(child)), pid, pid_file })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn alive(&self) -> bool {
        alive(self.pid)
    }

    /// Stop the child (graceful, then forced) and forget its pid file.
    pub fn stop(&self) {
        let child = self.child.lock().ok().and_then(|mut slot| slot.take());
        if child.is_some() {
            terminate(self.pid);
            remove_if(&self.pid_file, &self.pid.to_string());
            tracing::info!(variant = self.spec.id, pid = self.pid, "llama-server stopped");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Kill the child recorded in the pid file (signal handler path: no async, no locks).
pub fn kill_recorded(data_dir: &Path) {
    reclaim_stale(data_dir);
}
