//! The bundled notification app (`CortexCodeNotify.app`, built by build.rs from
//! helper/CortexNotify.swift), driven over its stdin/stdout with one JSON object per line.
//!
//! The app is started on first use and kept running, so a click on one of its notifications reaches
//! it and comes back here as an activation. It exits when its stdin closes (this plugin exits).

use super::NativeRequest;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

pub const EXECUTABLE: &str = "cortex-notify-helper";
pub const APP: &str = "CortexCodeNotify.app";

/// A click the app reported.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeActivation {
    pub id: String,
    pub action: Option<String>,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    alive: Arc<AtomicBool>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Helper {
    pub app: PathBuf,
    running: tokio::sync::Mutex<Option<Running>>,
    pending: Pending,
    next: AtomicU64,
    activations: mpsc::UnboundedSender<NativeActivation>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Helper {
    pub fn new(app: PathBuf, activations: mpsc::UnboundedSender<NativeActivation>) -> Self {
        Self { app, running: tokio::sync::Mutex::new(None), pending: Arc::new(Mutex::new(HashMap::new())), next: AtomicU64::new(1), activations }
    }

    pub fn executable(app: &Path) -> PathBuf {
        app.join("Contents/MacOS").join(EXECUTABLE)
    }

    /// Whether the bundle looks complete (executable and Info.plist present).
    pub fn present(app: &Path) -> bool {
        Self::executable(app).is_file() && app.join("Contents/Info.plist").is_file()
    }

    async fn spawn(&self) -> Result<Running, String> {
        let executable = Self::executable(&self.app);
        let mut child = Command::new(&executable)
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", executable.display()))?;
        let stdin = child.stdin.take().ok_or("the notification app has no stdin")?;
        let stdout = child.stdout.take().ok_or("the notification app has no stdout")?;
        let alive = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready_rx) = oneshot::channel::<Value>();
        let pending = self.pending.clone();
        let activations = self.activations.clone();
        let reader_alive = alive.clone();
        tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(%line, "unreadable line from the notification app");
                    continue;
                };
                if let Some(req) = message.get("req").and_then(Value::as_u64) {
                    if let Some(tx) = lock(&pending).remove(&req) {
                        let _ = tx.send(message);
                    }
                    continue;
                }
                match message.get("event").and_then(Value::as_str) {
                    Some("ready") => {
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(message);
                        }
                    }
                    Some("activated") => {
                        if let Some(id) = message.get("id").and_then(Value::as_str) {
                            let action = message.get("action").and_then(Value::as_str).map(str::to_owned);
                            let _ = activations.send(NativeActivation { id: id.to_owned(), action });
                        }
                    }
                    _ => tracing::debug!(%line, "notification app event"),
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
            // Fail every request still waiting: their senders are dropped.
            lock(&pending).clear();
        });
        match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
            Ok(Ok(ready)) => {
                tracing::info!(pid = ?ready.get("pid"), bundle = ?ready.get("bundle"), "notification app started");
                Ok(Running { child, stdin, alive })
            }
            _ => {
                let _ = child.start_kill();
                Err("the notification app did not start within 10 s".into())
            }
        }
    }

    /// Send one command and wait for its answer. Starts the app when it is not running.
    pub async fn request(&self, mut command: Value, timeout: Duration) -> Result<Value, String> {
        let req = self.next.fetch_add(1, Ordering::Relaxed);
        command["req"] = json!(req);
        let mut line = serde_json::to_string(&command).map_err(|e| e.to_string())?;
        line.push('\n');
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(req, tx);
        let written = {
            let mut running = self.running.lock().await;
            if running.as_ref().is_some_and(|r| !r.alive.load(Ordering::SeqCst))
                && let Some(mut dead) = running.take()
            {
                let _ = dead.child.start_kill();
            }
            if running.is_none() {
                match self.spawn().await {
                    Ok(started) => *running = Some(started),
                    Err(error) => {
                        lock(&self.pending).remove(&req);
                        return Err(error);
                    }
                }
            }
            match running.as_mut() {
                Some(r) => r.stdin.write_all(line.as_bytes()).await.and(r.stdin.flush().await).map_err(|e| e.to_string()),
                None => Err("the notification app is not running".into()),
            }
        };
        if let Err(error) = written {
            lock(&self.pending).remove(&req);
            return Err(format!("cannot talk to the notification app: {error}"));
        }
        let answer = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => return Err("the notification app exited before answering".into()),
            Err(_) => {
                lock(&self.pending).remove(&req);
                return Err(format!("the notification app did not answer within {timeout:?}"));
            }
        };
        if answer.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(answer)
        } else {
            Err(answer.get("error").and_then(Value::as_str).unwrap_or("the notification app reported an error").to_owned())
        }
    }

    /// `not_determined` | `denied` | `authorized` | `provisional` | `ephemeral`.
    pub async fn authorization(&self) -> Result<String, String> {
        let answer = self.request(json!({"op": "status"}), Duration::from_secs(10)).await?;
        Ok(answer.get("authorization").and_then(Value::as_str).unwrap_or("unknown").to_owned())
    }

    /// Ask macOS for permission (the system shows its one-time prompt). Resolves when the user has
    /// answered, which may be never; callers do not wait for it on the delivery path.
    pub async fn authorize(&self) -> Result<String, String> {
        let answer = self.request(json!({"op": "authorize"}), Duration::from_secs(24 * 3600)).await?;
        Ok(answer.get("authorization").and_then(Value::as_str).unwrap_or("unknown").to_owned())
    }

    pub async fn post(&self, request: &NativeRequest, spool: &Path) -> Result<(), String> {
        self.request(
            json!({"op": "post", "id": request.id, "title": request.title, "subtitle": request.subtitle, "body": request.body,
                   "thread": request.thread, "actions": request.actions, "spool": spool.to_string_lossy()}),
            Duration::from_secs(15),
        )
        .await
        .map(|_| ())
    }

    /// Identifiers of this app's notifications still in Notification Center.
    pub async fn delivered(&self) -> Result<Vec<String>, String> {
        let answer = self.request(json!({"op": "delivered"}), Duration::from_secs(10)).await?;
        Ok(answer
            .get("ids")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default())
    }

    /// Stop the app (closing its stdin ends it).
    pub async fn stop(&self) {
        let running = self.running.lock().await.take();
        if let Some(mut running) = running {
            drop(running.stdin);
            if tokio::time::timeout(Duration::from_secs(2), running.child.wait()).await.is_err() {
                let _ = running.child.start_kill();
            }
        }
    }
}
