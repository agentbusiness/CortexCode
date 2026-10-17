//! `cx`: the CortexCode command line for the long tail. It reads `<CORTEX_HOME>/run/frame.json`,
//! talks to the frame over its WebSocket and prints plain text (or `--json`).

pub mod args;
pub mod client;
pub mod registry;

use args::Inputs;
use client::Client;
use registry::{Command, Context, Prepare};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub json: bool,
    pub home: Option<String>,
    pub project: Option<String>,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub help: bool,
}

/// What `cx` prints and how it exits.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    fn ok(stdout: String) -> Self {
        Self { code: 0, stdout, stderr: String::new() }
    }
    fn fail(code: i32, stderr: String) -> Self {
        Self { code, stdout: String::new(), stderr }
    }
}

/// Split global options (anywhere on the line) from the command words and its arguments.
pub fn split_options(argv: &[String]) -> Result<(Options, Vec<String>), String> {
    let mut options = Options::default();
    let mut rest = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        let mut value = |name: &str| -> Result<String, String> {
            index += 1;
            argv.get(index).cloned().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--json" => options.json = true,
            "--help" | "-h" => options.help = true,
            "--home" => options.home = Some(value("--home")?),
            "--project" => options.project = Some(value("--project")?),
            "--workspace" => options.workspace = Some(value("--workspace")?),
            "--session" => options.session = Some(value("--session")?),
            "--" => {
                rest.extend(argv[index..].iter().cloned());
                break;
            }
            other => {
                if let Some(v) = other.strip_prefix("--home=") {
                    options.home = Some(v.to_owned());
                } else if let Some(v) = other.strip_prefix("--project=") {
                    options.project = Some(v.to_owned());
                } else if let Some(v) = other.strip_prefix("--workspace=") {
                    options.workspace = Some(v.to_owned());
                } else if let Some(v) = other.strip_prefix("--session=") {
                    options.session = Some(v.to_owned());
                } else {
                    rest.push(other.to_owned());
                }
            }
        }
        index += 1;
    }
    Ok((options, rest))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn find_command<'a>(commands: &'a [Command], words: &[String]) -> Option<(&'a Command, usize)> {
    for take in (1..=2).rev() {
        if words.len() >= take {
            let path = words[..take].join(" ");
            if let Some(command) = commands.iter().find(|command| command.path == path) {
                return Some((command, take));
            }
        }
    }
    None
}

/// The project whose root (or whose agent workspaces) contains `dir`: longest root wins.
async fn project_for_dir(client: &mut Client, home: &Path, dir: &Path) -> Option<String> {
    let projects = client.call("frame.project.list", json!({})).await.ok()?;
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for project in projects.get("projects")?.as_array()? {
        let id = project.get("id")?.as_str()?.to_owned();
        let root = PathBuf::from(project.get("root")?.as_str()?);
        let root = root.canonicalize().unwrap_or(root);
        let workspaces = home.join("workspaces").join(&id);
        let workspaces = workspaces.canonicalize().unwrap_or(workspaces);
        for candidate in [root, workspaces] {
            if dir.starts_with(&candidate) {
                let depth = candidate.components().count();
                if best.as_ref().is_none_or(|(best_depth, _)| depth > *best_depth) {
                    best = Some((depth, id.clone()));
                }
            }
        }
    }
    best.map(|(_, id)| id)
}

async fn author(client: &mut Client, session: Option<&str>) -> String {
    if let Some(name) = env("CORTEX_AGENT") {
        return name;
    }
    let Some(session) = session else { return "user".to_owned() };
    match client.call("frame.session.get", json!({ "id": session })).await {
        Ok(value) => match value.get("agent").and_then(Value::as_str) {
            Some(agent) if !agent.is_empty() => agent.to_owned(),
            _ => "chat".to_owned(),
        },
        Err(_) => "chat".to_owned(),
    }
}

fn error_text(error: &cc_protocol::RpcError, method: &str) -> String {
    let hint = match (error.code.as_str(), method.split('.').next().unwrap_or("")) {
        ("unavailable" | "unknown_method", "browser") => " (the browser is served by the CortexCode window; it is not running)",
        ("unavailable" | "unknown_method", "agents") => " (the agents plugin is not running)",
        ("unavailable" | "unknown_method", "memory") => " (the memory plugin is not running)",
        _ => "",
    };
    format!("cx: {}: {}{hint}\n", error.code, error.message)
}

/// Run one `cx` invocation.
pub async fn run(argv: Vec<String>, inputs: &mut Inputs) -> Outcome {
    let (options, words) = match split_options(&argv) {
        Ok(split) => split,
        Err(message) => return Outcome::fail(2, format!("cx: {message}\n")),
    };
    let commands = registry::commands();
    if words.is_empty() || matches!(words[0].as_str(), "tools" | "help") {
        return Outcome::ok(registry::listing(&commands));
    }
    let is_call = words[0] == "call";
    let found = if is_call { None } else { find_command(&commands, &words) };
    if !is_call && found.is_none() {
        let group: Vec<&str> = commands.iter().filter(|c| c.path.starts_with(&format!("{} ", words[0]))).map(|c| c.path).collect();
        let message = if group.is_empty() {
            format!("cx: unknown command `{}`; run `cx tools` for the list\n", words.join(" "))
        } else {
            format!("cx: `{}` needs a subcommand: {}\n", words[0], group.join(", "))
        };
        return Outcome::fail(2, message);
    }
    if let Some((command, _)) = found
        && options.help
    {
        return Outcome::ok(args::help(command.path, command.summary, &command.schema, command.positional));
    }

    // Arguments first (so usage errors do not need a frame).
    let (method, mut params) = match found {
        None => {
            let Some(method) = words.get(1) else { return Outcome::fail(2, "cx: call needs a method name\n".to_owned()) };
            let params = match words.get(2) {
                None => json!({}),
                Some(raw) => {
                    let text = match inputs.expand(raw) {
                        Ok(text) => text,
                        Err(error) => return Outcome::fail(2, format!("cx: {error}\n")),
                    };
                    match serde_json::from_str::<Value>(&text) {
                        Ok(value) => value,
                        Err(error) => return Outcome::fail(2, format!("cx: params must be JSON: {error}\n")),
                    }
                }
            };
            (method.clone(), params)
        }
        Some((command, taken)) => match args::parse(&command.schema, command.positional, &words[taken..], inputs) {
            Ok(parsed) => (command.method.to_owned(), Value::Object(parsed)),
            Err(error) => {
                return Outcome::fail(
                    2,
                    format!("cx: {error}\nusage: {}\n", args::usage(command.path, &command.schema, command.positional)),
                );
            }
        },
    };

    let home = client::home_dir(options.home.as_deref());
    let info = match client::read_frame_info(&home) {
        Ok(info) => info,
        Err(message) => return Outcome::fail(1, format!("cx: {message}\n")),
    };
    let mut client = match Client::connect(&info).await {
        Ok(client) => client,
        Err(message) => return Outcome::fail(1, format!("cx: {message}\n")),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let workspace = options.workspace.clone().or_else(|| env("CORTEX_WORKSPACE")).unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let session_id = options.session.clone().or_else(|| env("CORTEX_SESSION_ID"));
    let mut context =
        Context { project_id: options.project.clone().or_else(|| env("CORTEX_PROJECT_ID")), session_id, workspace, author: String::new() };

    if let Some((command, _)) = found {
        if context.project_id.is_none() {
            context.project_id = project_for_dir(&mut client, &home, Path::new(&context.workspace)).await;
        }
        if command.path == "board post" {
            context.author = author(&mut client, context.session_id.as_deref()).await;
        }
        let Value::Object(map) = params else { return Outcome::fail(2, "cx: internal: arguments are not an object\n".to_owned()) };
        params = match (command.build)(&context, map) {
            Ok(params) => params,
            Err(message) => return Outcome::fail(2, format!("cx: {message}\n")),
        };
        if command.prepare == Prepare::BrowserTab && params.get("tab_id").is_none() {
            match client.call("browser.tabs", json!({})).await {
                Ok(tabs) => {
                    match tabs.get("tabs").and_then(Value::as_array).and_then(|tabs| tabs.last()).and_then(|tab| tab.get("tab_id")).cloned()
                    {
                        Some(tab) => params["tab_id"] = tab,
                        None => return Outcome::fail(1, "cx: the browser has no open tab; run `cx browser open <url>` first\n".to_owned()),
                    }
                }
                Err(error) => return Outcome::fail(1, error_text(&error, "browser.tabs")),
            }
        }
    }

    let mut result = client.call(&method, params.clone()).await;
    // Agent records also live in memory; use them when the agents plugin is not running.
    if method == "agents.list"
        && let Err(error) = &result
        && matches!(error.code.as_str(), "unavailable" | "unknown_method")
    {
        result = client.call("memory.agent.list", params.clone()).await;
    }
    client.close().await;
    match result {
        Ok(value) => {
            if options.json || is_call {
                Outcome::ok(format!("{}\n", serde_json::to_string_pretty(&value).unwrap_or_default()))
            } else {
                let format = found.map(|(command, _)| command.format).unwrap_or(registry::format_json);
                Outcome::ok(format(&value))
            }
        }
        Err(error) => {
            if options.json {
                Outcome { code: 1, stdout: format!("{}\n", json!({"error": error})), stderr: String::new() }
            } else {
                Outcome::fail(1, error_text(&error, &method))
            }
        }
    }
}
