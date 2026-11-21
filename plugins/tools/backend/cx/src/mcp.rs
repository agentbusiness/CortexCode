//! `cx mcp`: an MCP server over stdio for a frontier CLI (Claude Code, Codex) working as one of
//! CortexCode's agents (docs/FRONTIER-AGENTS.md). It is the CLI's only way to change files: every
//! tool call goes to the tools plugin as a call of the agent's own session (`tools.mcp.call`, held
//! to that session by the frame's command access), so the file reservations, the write sandbox,
//! the step log and undo cover the frontier agent as they cover a local one.
//!
//! Beside the session's model tools it offers lead questions/progress, the project's board and
//! `view_page`: fresh, session-owned browser evidence delivered as MCP image content.
//!
//! The wire: JSON-RPC 2.0, one message per line on stdin and stdout (MCP's stdio transport).
//! Requests are served one after another, in the order they come.

use crate::client::{self, Client};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The MCP version answered when the client asks for none this server knows.
const PROTOCOL: &str = "2025-06-18";
const KNOWN_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

const ASK_LEAD: &str = "ask_lead";
const TELL_LEAD: &str = "tell_lead";
const BOARD: &str = "board";
const VIEW_PAGE: &str = "view_page";

/// Where the agent works, from the environment the agents plugin started the CLI with.
struct Scope {
    workspace: String,
    project_id: Option<String>,
    agent: String,
    /// The lead's tools only (a planning agent: read and run, never edit).
    lead: bool,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct Server {
    scope: Scope,
    client: Option<Client>,
}

impl Server {
    /// The frame connection, made on first use and made again after it broke.
    async fn client(&mut self) -> Result<&mut Client, String> {
        if self.client.is_none() {
            let home = client::home_dir(None);
            let info = client::read_frame_info(&home)?;
            self.client = Some(Client::connect(&info).await?);
        }
        Ok(self.client.as_mut().expect("connected above"))
    }

    /// One call to the frame; a broken connection is made again once.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        for attempt in 0..2 {
            let client = self.client().await?;
            match client.call(method, params.clone()).await {
                Ok(value) => return Ok(value),
                Err(error)
                    if attempt == 0
                        && (error.code == cc_protocol::codes::CONNECTION_CLOSED
                            || (error.code == cc_protocol::codes::INTERNAL && (error.message.starts_with("send failed") || error.message.starts_with("connection error")))) =>
                {
                    self.client = None;
                }
                Err(error) => return Err(format!("{}: {}", error.code, error.message)),
            }
        }
        Err("the app is not reachable".to_owned())
    }

    async fn handle(&mut self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // A notification (no id) gets no answer.
        let id = id?;
        let result = match method.as_str() {
            "initialize" => Ok(self.initialize(&params).await),
            "ping" => Ok(json!({})),
            "tools/list" => self.list().await,
            "tools/call" => Ok(self.tool_call(&params).await),
            "resources/list" => Ok(json!({"resources": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    async fn initialize(&mut self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
        let version = if KNOWN_PROTOCOLS.contains(&asked) { asked } else { PROTOCOL };
        // The tools guide is the server's instructions: how each result reads and which tool to use.
        let guide = self
            .call("tools.mcp.list", json!({"workspace": self.scope.workspace, "lead": self.scope.lead}))
            .await
            .ok()
            .and_then(|listed| listed.get("guide").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let guide = format!("{guide}\nFor rendered pixels, use view_page with your tab id from run `cx browser open <url>`. Use run for browser actions; take a fresh snapshot after the page changes.");
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "cortex", "version": env!("CARGO_PKG_VERSION")},
            "instructions": guide,
        })
    }

    async fn list(&mut self) -> Result<Value, (i64, String)> {
        let listed = self.call("tools.mcp.list", json!({"workspace": self.scope.workspace, "lead": self.scope.lead})).await.map_err(|error| (-32603, error))?;
        let mut tools: Vec<Value> = listed
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|tool| {
                json!({
                    "name": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "inputSchema": tool.get("input_schema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({"type": "object"})),
                })
            })
            .collect();
        tools.push(json!({
            "name": ASK_LEAD,
            "description": "Ask your lead when the task is unclear or blocked; then end the turn as waiting.",
            "inputSchema": {
                "type": "object",
                "properties": {"question": {"type": "string", "description": "What you need decided or known, with what you found."}},
                "required": ["question"],
            },
        }));
        tools.push(json!({
            "name": TELL_LEAD,
            "description": "Tell your lead where the work stands (a step done, a finding); then go on working.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "One or two sentences: what is done, what you found, what comes next."}},
                "required": ["text"],
            },
        }));
        tools.push(json!({
            "name": BOARD,
            "description": "Read the project board: other agents' claims, hand-offs and blockers.",
            "inputSchema": {
                "type": "object",
                "properties": {"minutes": {"type": "integer", "description": "Only messages of the last N minutes (default 120)."}},
            },
        }));
        tools.push(json!({
            "name": VIEW_PAGE,
            "description": "See the current page or visible element; use run for browser actions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "string", "description": "Your tab id returned by run `cx browser open <url>`."},
                    "target": {"type": ["string", "null"], "description": "ref=N from the latest snapshot, or a CSS selector. Empty or null views the viewport."},
                },
                "required": ["tab_id"],
                "additionalProperties": false,
            },
        }));
        Ok(json!({"tools": tools}))
    }

    async fn tool_call(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if name == VIEW_PAGE {
            return match self.view_page(&arguments).await {
                Ok(result) => result,
                Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
            };
        }
        let (ok, text) = match name.as_str() {
            ASK_LEAD => self.ask_lead(&arguments).await,
            TELL_LEAD => self.tell_lead(&arguments).await,
            BOARD => self.board(&arguments).await,
            _ => match self.call("tools.mcp.call", json!({"workspace": self.scope.workspace, "name": name, "arguments": arguments, "lead": self.scope.lead})).await {
                Ok(result) => (
                    result.get("ok").and_then(Value::as_bool).unwrap_or(false),
                    result.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
                ),
                Err(error) => (false, format!("The tool could not run ({error}). Try the call again; if it fails the same way, ask your lead.")),
            },
        };
        json!({"content": [{"type": "text", "text": text}], "isError": !ok})
    }

    /// The browser owns capture, tab authorization and artifact recording. The worker supplies
    /// no path and cx reads no file; the frame stamps this command connection's session.
    async fn view_page(&mut self, arguments: &Value) -> Result<Value, String> {
        let tab = arguments.get("tab_id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty())
            .ok_or_else(|| "Give your tab_id returned by run `cx browser open <url>`.".to_owned())?;
        let target = match arguments.get("target") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if text.trim().is_empty() => None,
            Some(Value::String(text)) => Some(text.trim()),
            _ => return Err("Give target as ref=N or a CSS selector, or leave it empty for the viewport.".to_owned()),
        };
        let view = self.call("browser.view", json!({"tab_id": tab, "target": target})).await
            .map_err(|error| format!("The page could not be viewed ({error}). Use run to inspect the browser state."))?;
        let image = view.get("image").ok_or_else(|| "The browser returned no pixels. Update the browser plugin before viewing the page again.".to_owned())?;
        let mime = image.get("mime_type").and_then(Value::as_str).unwrap_or_default();
        let data = image.get("data").and_then(Value::as_str).unwrap_or_default();
        let side = |name: &str| view.get(name).and_then(Value::as_u64).is_some_and(|n| n > 0 && n <= 1536);
        if mime != "image/jpeg" || data.is_empty() || data.len() > 4 * (2 * 1024 * 1024_usize).div_ceil(3) || !side("width") || !side("height") {
            return Err("The browser returned an unsupported image. View a smaller visible element.".to_owned());
        }
        let field = |name: &str| view.get(name).and_then(Value::as_str).unwrap_or_default();
        let text = format!("{}\n{}\nScreenshot: {}", field("title"), field("url"), field("path"));
        Ok(json!({
            "content": [{"type": "text", "text": text}, {"type": "image", "data": data, "mimeType": mime}],
            "structuredContent": {
                "path": view.get("path"), "url": view.get("url"), "title": view.get("title"),
                "width": view.get("width"), "height": view.get("height"), "clip": view.get("clip"),
                "target": view.get("target"), "captured_ms": view.get("captured_ms"),
            },
            "isError": false,
        }))
    }

    /// A progress line for the lead (board kind `progress`, to the lead): the agents plugin puts it
    /// in the lead's conversation; the agent does not wait.
    async fn tell_lead(&mut self, arguments: &Value) -> (bool, String) {
        let text = arguments.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
        if text.is_empty() {
            return (false, "Write what to tell in `text`.".to_owned());
        }
        let Some(project_id) = self.scope.project_id.clone() else { return (true, "There is no lead to tell. Go on with the task.".to_owned()) };
        let post = json!({"project_id": project_id, "kind": "progress", "author": self.scope.agent, "text": text, "refs": [], "to": "lead"});
        match self.call("memory.board.post", post).await {
            Ok(_) => (true, "Your lead has it. Go on with the task.".to_owned()),
            Err(error) => (false, format!("It did not reach the lead ({error}). Go on with the task; say it in your report.")),
        }
    }

    async fn ask_lead(&mut self, arguments: &Value) -> (bool, String) {
        let question = arguments.get("question").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
        if question.is_empty() {
            return (false, "Write the question in `question`.".to_owned());
        }
        let Some(project_id) = self.scope.project_id.clone() else {
            return (false, "This agent has no project, so there is no lead to ask. Go on with the task as it is written.".to_owned());
        };
        // A `question` to the lead: the agents plugin wakes the lead when the turn ends `waiting`
        // (a `need` would wake it now, and again then).
        let post = json!({"project_id": project_id, "kind": "question", "author": self.scope.agent, "text": question, "refs": [], "to": "lead"});
        match self.call("memory.board.post", post).await {
            Ok(_) => (true, "Your lead has the question. End your turn now: status `waiting`, the question in `question`. The answer comes as your next message.".to_owned()),
            Err(error) => (false, format!("The question did not reach the lead ({error}). End your turn with status `blocked` and the question in `summary`.")),
        }
    }

    async fn board(&mut self, arguments: &Value) -> (bool, String) {
        let Some(project_id) = self.scope.project_id.clone() else { return (true, "There is no project board for this agent.".to_owned()) };
        let minutes = arguments.get("minutes").and_then(Value::as_u64).unwrap_or(120).max(1);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as u64).unwrap_or(0);
        let since = now.saturating_sub(minutes * 60_000);
        match self.call("memory.board.list", json!({"project_id": project_id, "since_ms": since})).await {
            Ok(listed) => {
                let lines: Vec<String> = listed
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|message| {
                        let field = |name: &str| message.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
                        let to = message.get("to").and_then(Value::as_str).map(|to| format!(" to {to}")).unwrap_or_default();
                        format!("{} ({}{to}): {}", field("author"), field("kind"), field("text"))
                    })
                    .collect();
                if lines.is_empty() { (true, format!("Nothing on the board in the last {minutes} minutes.")) } else { (true, lines.join("\n")) }
            }
            Err(error) => (false, format!("The board could not be read ({error}).")),
        }
    }
}

/// Serve MCP on stdin and stdout until the client closes stdin. Returns the exit code.
pub async fn serve() -> i32 {
    let workspace = env("CORTEX_WORKSPACE").unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned());
    let scope = Scope { workspace, project_id: env("CORTEX_PROJECT_ID"), agent: env("CORTEX_AGENT").unwrap_or_else(|| "agent".to_owned()), lead: env("CORTEX_MCP_LEAD").is_some() };
    let mut server = Server { scope, client: None };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("cx mcp: cannot read stdin: {error}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            // A batch (an array) is answered as one array of the answers its requests get.
            Ok(Value::Array(batch)) => {
                let mut answers = Vec::new();
                for message in batch {
                    if let Some(answer) = server.handle(message).await {
                        answers.push(answer);
                    }
                }
                (!answers.is_empty()).then(|| Value::Array(answers))
            }
            Ok(message) => server.handle(message).await,
            Err(error) => Some(json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": format!("parse error: {error}")}})),
        };
        if let Some(reply) = reply {
            let mut text = serde_json::to_string(&reply).unwrap_or_default();
            text.push('\n');
            if stdout.write_all(text.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    }
    if let Some(client) = server.client.take() {
        client.close().await;
    }
    0
}
