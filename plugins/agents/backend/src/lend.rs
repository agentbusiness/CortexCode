//! This computer's frontier CLIs, run for whoever asks (`agents.frontier.*`,
//! `cc_protocol::contracts::frontier`): this app's own agents and, over the mesh, a member's.
//!
//! - **One way for both.** A run from this app and a run lent to a member start the same CLI the
//!   same way; the agents plugin itself asks here for its own runs ([`crate::frontier`]).
//! - **The CLI only thinks here.** It starts in an empty folder (`<home>/frontier/<session>`),
//!   with no tools of its own. Its one MCP server is this plugin's own binary as a pipe
//!   (`cc-agents frontier-mcp <socket>`, [`relay`]): each MCP message the CLI writes goes to the
//!   caller as a stream item, and the caller's answer ([`Lender::mcp`]) goes back down the pipe.
//!   The caller's tools do the work where the project is; nothing of this computer's disk is read
//!   or changed, and a caller never names a command to start here.
//! - **Places.** As many runs at once as the data set says (`[scheduler] frontier_parallel`); a
//!   run beyond that waits its turn, oldest first, and tells its caller how many are ahead.
//! - **No cut tool results.** The CLIs' own caps on an MCP tool's output and time are lifted
//!   ([`MCP_OUTPUT_TOKENS`], [`MCP_TOOL_SECONDS`]): a test log or a long file reaches the model
//!   whole, and a long build is not ended by the CLI.

use crate::frontier::Frontier;
use cc_protocol::conn::ItemSink;
use cc_protocol::contracts::agents::workers;
use cc_protocol::contracts::frontier::{Item, Offer, Offers, RunParams, RunResult};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

/// The MCP server's name in the CLI's configuration; its tools are `mcp__cortex__<tool>` there.
pub const MCP_SERVER: &str = "cortex";
/// The argument that makes this binary the CLI's MCP pipe ([`relay`]).
pub const RELAY_ARG: &str = "frontier-mcp";
/// The most tokens one MCP tool result may have before the CLI cuts it or puts it in a file the
/// model cannot read here (Claude Code: `MAX_MCP_OUTPUT_TOKENS`, 25,000 unless set; Codex:
/// `tool_output_token_limit`). Set past any result a model's context holds.
const MCP_OUTPUT_TOKENS: u64 = 1_000_000;
/// The longest one MCP tool call may take before the CLI gives it up (Codex: 60 s unless set).
const MCP_TOOL_SECONDS: u64 = 24 * 60 * 60;
/// What of a CLI's error output is kept for the run's failure text.
const STDERR_TAIL: usize = 4000;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// What a run's caller sends it while it goes.
enum Input {
    Say(String),
    Close,
    Mcp(Value),
}

/// The CLI runs going here, and the places they take.
#[derive(Default)]
pub struct Lender {
    /// Runs holding a place.
    used: Mutex<u32>,
    /// Runs waiting for one.
    waiting: Mutex<u32>,
    freed: Notify,
    live: Mutex<HashMap<String, mpsc::UnboundedSender<Input>>>,
}

/// A run's place, given back when the run ends.
struct Place<'a>(&'a Lender);

impl Drop for Place<'_> {
    fn drop(&mut self) {
        let mut used = lock(&self.0.used);
        *used = used.saturating_sub(1);
        drop(used);
        self.0.freed.notify_waiters();
    }
}

/// A run's entry in the live list, removed when the run ends.
struct Listed<'a>(&'a Lender, String);

impl Drop for Listed<'_> {
    fn drop(&mut self) {
        lock(&self.0.live).remove(&self.1);
    }
}

/// A run's files on this computer (its socket, its folder), removed when the run ends. The CLI
/// keeps its own session elsewhere (`~/.claude`, `~/.codex`), found again by the folder's path.
struct Scratch {
    socket: PathBuf,
    folder: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

/// Letters, digits, `-` and `_` of a name (a folder or socket name from a caller's text).
fn plain(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect()
}

/// A TOML basic string (a JSON string's escapes are TOML's).
fn toml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

impl Lender {
    /// CLI runs holding a place here now.
    pub fn running(&self) -> u32 {
        *lock(&self.used)
    }

    /// The CLIs this computer can run now, with its places.
    pub async fn offers(&self, frontier: &Frontier, limit: u32) -> Offers {
        let running = self.running();
        let workers = frontier
            .local(false)
            .await
            .into_iter()
            .map(|(id, version, problem)| Offer { label: crate::frontier::label(&id).to_owned(), id, version, problem, running, limit })
            .collect();
        Offers { workers }
    }

    /// Take a place, waiting for one when all are taken (the caller is told how many are ahead).
    async fn place(&self, limit: u32, items: &ItemSink, cancel: &CancellationToken) -> Result<Place<'_>, RpcError> {
        let mut told = false;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut used = lock(&self.used);
                if *used < limit.max(1) {
                    *used += 1;
                    if told {
                        let mut waiting = lock(&self.waiting);
                        *waiting = waiting.saturating_sub(1);
                    }
                    return Ok(Place(self));
                }
            }
            if !told {
                told = true;
                let ahead = {
                    let mut waiting = lock(&self.waiting);
                    *waiting += 1;
                    *waiting - 1
                };
                items.send(json!(Item { queued: Some(ahead), ..Item::default() }));
            }
            tokio::select! {
                _ = &mut freed => {}
                _ = cancel.cancelled() => {
                    let mut waiting = lock(&self.waiting);
                    *waiting = waiting.saturating_sub(1);
                    return Err(RpcError::cancelled());
                }
            }
        }
    }

    fn input(&self, run: &str, input: Input) -> Result<Value, RpcError> {
        match lock(&self.live).get(run) {
            Some(sender) if sender.send(input).is_ok() => Ok(json!({})),
            _ => Err(RpcError::not_found("that run is not going here")),
        }
    }

    /// The run's next message.
    pub fn say(&self, run: &str, text: String) -> Result<Value, RpcError> {
        self.input(run, Input::Say(text))
    }

    /// No further message comes.
    pub fn close(&self, run: &str) -> Result<Value, RpcError> {
        self.input(run, Input::Close)
    }

    /// One MCP message for the run's CLI.
    pub fn mcp(&self, run: &str, message: Value) -> Result<Value, RpcError> {
        self.input(run, Input::Mcp(message))
    }

    /// One CLI process, from its first message to its end. Its output lines and its MCP messages
    /// go to `items`; a cancelled call ends the CLI.
    pub async fn run(&self, frontier: &Frontier, home: &Path, limit: u32, params: RunParams, items: ItemSink, cancel: CancellationToken) -> Result<RunResult, RpcError> {
        if !workers::frontier(&params.worker) {
            return Err(RpcError::invalid_params(format!("`{}` is not a frontier CLI", params.worker)));
        }
        let run = plain(&params.run);
        if run.is_empty() {
            return Err(RpcError::invalid_params("the run needs a name"));
        }
        let cli = match frontier.local(false).await.into_iter().find(|(id, _, _)| id == &params.worker) {
            Some((_, _, Some(problem))) => return Err(RpcError::new(codes::UNAVAILABLE, format!("{} cannot work here now: {problem}", crate::frontier::label(&params.worker)))),
            Some(_) => frontier.cli_path(&params.worker).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the CLI is not installed on this computer"))?,
            None => return Err(RpcError::new(codes::UNAVAILABLE, "the CLI was not looked for")),
        };
        let _place = self.place(limit, &items, &cancel).await?;

        // The run's folder (empty: the CLI has nothing to look at here) and its MCP pipe.
        let session = plain(&params.session);
        let folder = home.join("frontier").join(if session.is_empty() { run.clone() } else { session });
        let run_dir = home.join("run");
        std::fs::create_dir_all(&folder).and_then(|_| std::fs::create_dir_all(&run_dir)).map_err(|error| RpcError::internal(format!("cannot make the run's folder: {error}")))?;
        let socket = run_dir.join(format!("fr-{}.sock", &run[run.len().saturating_sub(12)..]));
        let _ = std::fs::remove_file(&socket);
        let scratch = Scratch { socket: socket.clone(), folder: folder.clone() };
        let listener = UnixListener::bind(&socket).map_err(|error| RpcError::internal(format!("cannot open the run's tool pipe: {error}")))?;
        let relay = std::env::current_exe().map_err(|error| RpcError::internal(format!("cannot find this plugin's own binary: {error}")))?;
        let relay = relay.to_string_lossy().into_owned();
        let socket_text = socket.to_string_lossy().into_owned();

        let claude = params.worker == workers::CLAUDE;
        let mut command = Command::new(&cli);
        if claude {
            let mcp = json!({"mcpServers": {MCP_SERVER: {"type": "stdio", "command": relay, "args": [RELAY_ARG, socket_text], "env": {}}}});
            command
                .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
                .arg("--model")
                .arg(&params.model)
                .arg("--effort")
                .arg(&params.effort)
                // No built-in tools: the caller's tools over MCP are its only hands.
                .args(["--tools", ""])
                .args(["--strict-mcp-config", "--mcp-config"])
                .arg(mcp.to_string())
                .args(["--permission-mode", "dontAsk", "--allowedTools"])
                .arg(format!("mcp__{MCP_SERVER}"))
                .arg("--disable-slash-commands")
                .arg("--append-system-prompt")
                .arg(&params.prompt)
                .arg("--json-schema")
                .arg(params.schema.to_string())
                .env("MAX_MCP_OUTPUT_TOKENS", MCP_OUTPUT_TOKENS.to_string())
                .env("MCP_TOOL_TIMEOUT", (MCP_TOOL_SECONDS * 1000).to_string());
            match &params.resume {
                Some(id) => {
                    command.arg("--resume").arg(id);
                }
                // The caller keeps the id only once the CLI says it (its `system` event).
                None => {
                    command.arg("--session-id").arg(uuid::Uuid::new_v4().to_string());
                }
            }
        } else {
            // Codex reads the answer's schema from a file: in the run's folder, gone with it.
            let schema_file = folder.join("answer-schema.json");
            std::fs::write(&schema_file, params.schema.to_string()).map_err(|error| RpcError::internal(format!("cannot write the answer's schema: {error}")))?;
            command.arg("exec");
            if let Some(id) = &params.resume {
                command.arg("resume").arg(id);
            }
            command
                .args(["--json", "--skip-git-repo-check", "-m"])
                .arg(&params.model)
                .arg("--output-schema")
                .arg(&schema_file)
                // Its own shell may look but not change, and its folder is empty: the caller's
                // tools over MCP are its hands and its eyes.
                .args(["-c", "sandbox_mode=\"read-only\"", "-c", "approval_policy=\"never\""])
                .arg("-c")
                .arg(format!("model_reasoning_effort={}", toml_string(&params.effort)))
                .arg("-c")
                .arg(format!("developer_instructions={}", toml_string(&params.prompt)))
                .arg("-c")
                .arg(format!("mcp_servers.{MCP_SERVER}.command={}", toml_string(&relay)))
                .arg("-c")
                .arg(format!("mcp_servers.{MCP_SERVER}.args=[{}, {}]", toml_string(RELAY_ARG), toml_string(&socket_text)))
                .arg("-c")
                .arg(format!("mcp_servers.{MCP_SERVER}.tool_timeout_sec={MCP_TOOL_SECONDS}"))
                .arg("-c")
                .arg(format!("mcp_servers.{MCP_SERVER}.startup_timeout_sec=120"))
                .arg("-c")
                .arg(format!("tool_output_token_limit={MCP_OUTPUT_TOKENS}"))
                // The message comes on stdin.
                .arg("-");
        }
        command
            .current_dir(&folder)
            // The CLI is not this app's command: it gets no way into the frame.
            .env_remove("CORTEX_FRAME_TOKEN")
            .env_remove("CORTEX_FRAME_SOCKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| RpcError::new(codes::UNAVAILABLE, format!("cannot start {}: {error}", cli.display())))?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| RpcError::internal("the CLI has no stdout"))?;
        let stderr = tail_of(child.stderr.take());

        // The first message: Claude Code takes messages as JSON lines and stays open for more;
        // Codex takes this one as text and its input ends.
        if let Some(input) = stdin.as_mut() {
            if claude {
                send_user(input, &params.message).await?;
            } else {
                input.write_all(params.message.as_bytes()).await.map_err(|error| RpcError::internal(format!("cannot send the message to the CLI: {error}")))?;
            }
        }
        if !claude {
            stdin.take();
        }

        let (sender, mut inputs) = mpsc::unbounded_channel();
        lock(&self.live).insert(params.run.clone(), sender);
        let _listed = Listed(self, params.run.clone());

        // The CLI's MCP pipe: each connection is one start of its MCP server (Claude Code: one per
        // run). Its lines come in on `from_cli`; the newest connection takes the answers.
        let (from_cli_tx, mut from_cli) = mpsc::unbounded_channel::<Value>();
        let mut to_cli: Option<tokio::net::unix::OwnedWriteHalf> = None;
        let mut lines = BufReader::new(stdout).lines();
        let mut gone = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    return Err(RpcError::cancelled());
                }
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Ok(event) = serde_json::from_str::<Value>(&line)
                            && !items.send(json!(Item { event: Some(event), ..Item::default() }))
                        {
                            gone = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => return Err(RpcError::internal(format!("reading the CLI's output failed: {error}"))),
                },
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let (read, write) = stream.into_split();
                        to_cli = Some(write);
                        let from_cli_tx = from_cli_tx.clone();
                        tokio::spawn(async move {
                            let mut lines = BufReader::new(read).lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                if let Ok(message) = serde_json::from_str::<Value>(&line)
                                    && from_cli_tx.send(message).is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                }
                Some(message) = from_cli.recv() => {
                    if !items.send(json!(Item { mcp: Some(message), ..Item::default() })) {
                        gone = true;
                        break;
                    }
                }
                Some(input) = inputs.recv() => match input {
                    Input::Say(text) => {
                        if claude && let Some(input) = stdin.as_mut() {
                            send_user(input, &text).await?;
                        }
                    }
                    Input::Close => {
                        stdin.take();
                    }
                    Input::Mcp(message) => {
                        if let Some(pipe) = to_cli.as_mut() {
                            let mut line = message.to_string();
                            line.push('\n');
                            if pipe.write_all(line.as_bytes()).await.is_err() || pipe.flush().await.is_err() {
                                to_cli = None;
                            }
                        }
                    }
                },
            }
        }
        if gone {
            // The caller is gone: nobody reads the CLI's work or answers its tools.
            let _ = child.start_kill();
            return Err(RpcError::new(codes::CANCELLED, "the caller went away"));
        }
        let status = child.wait().await.map_err(|error| RpcError::internal(format!("the CLI did not end cleanly: {error}")))?;
        drop(scratch);
        let error = if status.success() { String::new() } else { stderr.await.unwrap_or_default() };
        Ok(RunResult { ok: status.success(), error })
    }
}

async fn send_user(stdin: &mut tokio::process::ChildStdin, text: &str) -> Result<(), RpcError> {
    let message = json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}});
    let mut line = message.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.map_err(|error| RpcError::internal(format!("cannot send a message to the CLI: {error}")))?;
    stdin.flush().await.map_err(|error| RpcError::internal(format!("cannot send a message to the CLI: {error}")))
}

/// The end of a CLI's error output, read while it runs (so a full pipe never stops it).
fn tail_of(stderr: Option<tokio::process::ChildStderr>) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let Some(mut stderr) = stderr else { return String::new() };
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    kept.extend_from_slice(&buffer[..read]);
                    if kept.len() > STDERR_TAIL * 2 {
                        kept.drain(..kept.len() - STDERR_TAIL);
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&kept).into_owned();
        let start = text.len().saturating_sub(STDERR_TAIL);
        let start = (start..text.len()).find(|index| text.is_char_boundary(*index)).unwrap_or(text.len());
        text[start..].trim().to_owned()
    })
}

/// `cc-agents frontier-mcp <socket>`: the CLI's MCP server as a pipe. What the CLI writes goes to
/// the run's socket, what comes from the socket goes to the CLI; it ends when either side does.
pub fn relay(socket: &str) -> std::process::ExitCode {
    use std::io::{Read, Write};
    let stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("cc-agents: the run's tool pipe is not there ({error})");
            return std::process::ExitCode::FAILURE;
        }
    };
    let Ok(mut up) = stream.try_clone() else { return std::process::ExitCode::FAILURE };
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 8192];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if up.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
            }
        }
        // The CLI closed its side: the run's end of the pipe sees it.
        let _ = up.shutdown(std::net::Shutdown::Both);
        std::process::exit(0);
    });
    let mut down = stream;
    let mut stdout = std::io::stdout().lock();
    let mut buffer = [0u8; 8192];
    loop {
        match down.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if stdout.write_all(&buffer[..read]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
    std::process::ExitCode::SUCCESS
}
