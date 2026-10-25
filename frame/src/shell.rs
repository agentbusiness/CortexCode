//! The Electron shell: a supervised child process. Restarted when it crashes; when the user quits
//! it (closes the window or quits, exit code 0) the whole app quits: the frame shuts down. Started
//! headless (`--no-ui`) or closed otherwise, `frame.ui.open` opens it.

use crate::hub::Frame;
use cc_protocol::{RpcError, codes};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub struct Shell {
    ui_dir: PathBuf,
    want: AtomicBool,
    started: AtomicBool,
    pid: AtomicU32,
    wake: Notify,
    /// Signalled when the user quit the UI: the frame shuts down.
    pub quit: Notify,
    crashes: Mutex<Vec<Instant>>,
}

impl Shell {
    pub fn new(ui_dir: PathBuf) -> Self {
        Self {
            ui_dir,
            want: AtomicBool::new(false),
            started: AtomicBool::new(false),
            pid: AtomicU32::new(0),
            wake: Notify::new(),
            quit: Notify::new(),
            crashes: Mutex::new(Vec::new()),
        }
    }

    /// Electron's own executable, as its package names it (`node_modules/electron/path.txt`, relative
    /// to `dist/`): started directly, so a Mac without Node runs the app bundle's UI.
    pub fn electron(&self) -> PathBuf {
        let package = self.ui_dir.join("node_modules/electron");
        let relative = std::fs::read_to_string(package.join("path.txt")).map(|text| text.trim().to_owned()).unwrap_or_default();
        package.join("dist").join(relative)
    }

    pub fn available(&self) -> bool {
        self.electron().is_file()
    }

    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::SeqCst)
    }

    /// Start the supervision loop (once) and ask for the window to be open.
    pub fn launch(&self, frame: &Arc<Frame>) -> bool {
        if !self.available() {
            tracing::warn!(electron = %self.electron().display(), "Electron is not installed; running headless");
            return false;
        }
        self.want.store(true, Ordering::SeqCst);
        if !self.started.swap(true, Ordering::SeqCst) {
            tokio::spawn(supervise(frame.clone()));
        }
        self.wake.notify_one();
        true
    }

    /// `frame.ui.open`.
    pub async fn open(&self, frame: &Arc<Frame>) -> Result<(bool, u32), RpcError> {
        if !self.available() {
            return Err(RpcError::new(
                codes::NOT_FOUND,
                format!("Electron is not installed at {}", self.electron().display()),
            ));
        }
        self.launch(frame);
        for _ in 0..100 {
            if self.pid() != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok((self.pid() != 0, self.pid()))
    }
}

async fn supervise(frame: Arc<Frame>) {
    let shell = &frame.shell;
    loop {
        while !shell.want.load(Ordering::SeqCst) {
            tokio::select! {
                _ = shell.wake.notified() => {}
                _ = frame.shutdown.cancelled() => return,
            }
        }
        if frame.is_shutting_down() {
            return;
        }
        let log_path = frame.home.logs().join("ui.log");
        let log = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(log) => log,
            Err(error) => {
                tracing::error!(%error, "cannot open the UI log");
                return;
            }
        };
        let log_err = match log.try_clone() {
            Ok(file) => file,
            Err(error) => {
                tracing::error!(%error, "cannot open the UI log");
                return;
            }
        };
        let port = frame.port.load(Ordering::SeqCst);
        // The shell loads exactly the shell modules of enabled plugins (and follows `plugin.unit`
        // events while it runs); folders named in the frame's own CORTEX_SHELL_MODULES come first.
        let mut modules: Vec<std::path::PathBuf> =
            std::env::var_os("CORTEX_SHELL_MODULES").map(|value| std::env::split_paths(&value).collect()).unwrap_or_default();
        modules.extend(frame.units.shell_dirs());
        let modules = std::env::join_paths(&modules).unwrap_or_default();
        let mut command = tokio::process::Command::new(shell.electron());
        command
            .arg(&shell.ui_dir)
            .current_dir(&shell.ui_dir)
            .env("CORTEX_FRAME_URL", format!("http://127.0.0.1:{port}"))
            .env("CORTEX_FRAME_TOKEN", &frame.token)
            .env("CORTEX_HOME", &frame.home.root)
            .env("CORTEX_SHELL_MODULES", modules)
            .env("CORTEX_SHELL_MODULES_EXCLUSIVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::error!(%error, "cannot start Electron");
                shell.want.store(false, Ordering::SeqCst);
                continue;
            }
        };
        let pid = child.id().unwrap_or(0);
        shell.pid.store(pid, Ordering::SeqCst);
        tracing::info!(pid, "Electron shell started");
        let status = tokio::select! {
            status = child.wait() => status,
            _ = frame.shutdown.cancelled() => {
                // SAFETY: signalling our own child process.
                unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(_) => {}
                    Err(_) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                }
                shell.pid.store(0, Ordering::SeqCst);
                tracing::info!("Electron shell stopped");
                return;
            }
        };
        shell.pid.store(0, Ordering::SeqCst);
        match status {
            Ok(status) if status.code() == Some(0) => {
                tracing::info!("the UI was closed by the user; quitting");
                shell.want.store(false, Ordering::SeqCst);
                shell.quit.notify_one();
                return;
            }
            other => {
                let detail = match &other {
                    Ok(status) => status.code().map(|c| format!("code {c}")).or(status.signal().map(|s| format!("signal {s}"))).unwrap_or_default(),
                    Err(error) => error.to_string(),
                };
                let recent = {
                    let mut crashes = shell.crashes.lock().unwrap_or_else(|p| p.into_inner());
                    let now = Instant::now();
                    crashes.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                    crashes.push(now);
                    crashes.len()
                };
                let delay = Duration::from_millis(500 * 2_u64.pow(recent.min(6) as u32 - 1)).min(Duration::from_secs(30));
                tracing::warn!(%detail, recent, ?delay, "Electron shell crashed; restarting");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = frame.shutdown.cancelled() => return,
                }
            }
        }
    }
}
