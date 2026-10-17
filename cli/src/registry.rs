//! Every `cx` command: its frame method, argument schema (the source of its help text), how its
//! arguments become request params, and how the result is shown as plain text.

use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// Who and where `cx` runs for (resolved from flags, the environment and the frame).
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub workspace: String,
    /// Author of board posts: the agent's name, `chat` in a chat session, else `user`.
    pub author: String,
}

impl Context {
    pub fn project(&self) -> Result<String, String> {
        self.project_id
            .clone()
            .ok_or_else(|| "no project: run inside a project directory, set CORTEX_PROJECT_ID or pass --project <id>".to_owned())
    }
}

/// Extra frame interaction some commands need before their main call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepare {
    None,
    /// Fill `tab_id` from `browser.tabs` (the most recent tab) when not given.
    BrowserTab,
}

pub struct Command {
    pub path: &'static str,
    pub method: &'static str,
    pub summary: &'static str,
    pub positional: &'static [&'static str],
    pub schema: Value,
    pub prepare: Prepare,
    pub build: fn(&Context, Map<String, Value>) -> Result<Value, String>,
    pub format: fn(&Value) -> String,
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

/// A name argument: `@name` is the name itself, not a file to read.
fn name(description: &str) -> Value {
    json!({"type": "string", "description": description, "x-cx-literal": true})
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn strings(description: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": description})
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

fn with(mut base: Map<String, Value>, extra: Value) -> Value {
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            base.insert(key, value);
        }
    }
    Value::Object(base)
}

fn project_args(context: &Context, args: Map<String, Value>) -> Result<Value, String> {
    Ok(with(args, json!({"project_id": context.project()?})))
}

fn scoped_args(context: &Context, args: Map<String, Value>) -> Result<Value, String> {
    Ok(with(args, json!({"project_id": context.project()?, "workspace": context.workspace})))
}

fn plain_args(_context: &Context, args: Map<String, Value>) -> Result<Value, String> {
    Ok(Value::Object(args))
}

fn session_args(context: &Context, args: Map<String, Value>) -> Result<Value, String> {
    let mut args = args;
    if let Some(session) = &context.session_id {
        args.entry("session_id").or_insert(json!(session));
    }
    Ok(Value::Object(args))
}

// ----- formatting -----

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn format_json(value: &Value) -> String {
    match value {
        Value::Null => "ok\n".to_owned(),
        Value::String(text) => format!("{text}\n"),
        other => format!("{}\n", serde_json::to_string_pretty(other).unwrap_or_default()),
    }
}

fn task_line(task: &Value) -> String {
    let assignee = task.get("assignee").and_then(Value::as_str).map(|a| format!("  @{a}")).unwrap_or_default();
    format!("{}  [{}]  {}{}", s(task, "id"), s(task, "status"), s(task, "title"), assignee)
}

fn format_tasks(value: &Value) -> String {
    let tasks = value.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    if tasks.is_empty() {
        return "no tasks\n".to_owned();
    }
    tasks.iter().map(|task| task_line(task) + "\n").collect()
}

fn format_task(task: &Value) -> String {
    let mut out = task_line(task) + "\n";
    for (key, label) in [("parent_id", "parent"), ("session_id", "session")] {
        if let Some(value) = task.get(key).and_then(Value::as_str) {
            let _ = writeln!(out, "{label}: {value}");
        }
    }
    for (key, label) in [("depends_on", "depends on"), ("touches", "touches")] {
        let items: Vec<&str> =
            task.get(key).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if !items.is_empty() {
            let _ = writeln!(out, "{label}: {}", items.join(", "));
        }
    }
    for key in ["brief", "plan", "result"] {
        let text = s(task, key);
        if !text.is_empty() {
            let _ = writeln!(out, "{key}:\n{}", text.trim_end());
        }
    }
    if let Some(checks) = task.pointer("/contract/checks").and_then(Value::as_array)
        && !checks.is_empty()
    {
        out.push_str("contract:\n");
        for check in checks {
            let expect = check.get("expect").and_then(Value::as_str).map(|e| format!("  expect: {e}")).unwrap_or_default();
            let _ = writeln!(out, "- {} ({}): {}{expect}", s(check, "name"), s(check, "kind"), s(check, "spec"));
        }
    }
    out
}

fn format_board(value: &Value) -> String {
    let messages = value.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return "the board is empty\n".to_owned();
    }
    let mut out = String::new();
    for message in &messages {
        let to = message.get("to").and_then(Value::as_str).map(|to| format!(" -> {to}")).unwrap_or_default();
        let _ =
            writeln!(out, "[{}] {} {}{to}: {}", s(message, "id"), s(message, "kind"), s(message, "author"), s(message, "text").trim_end());
    }
    out
}

fn format_posted(value: &Value) -> String {
    format!("posted {} ({} by {})\n", s(value, "id"), s(value, "kind"), s(value, "author"))
}

fn format_agents(value: &Value) -> String {
    let agents = value.get("agents").and_then(Value::as_array).cloned().unwrap_or_default();
    if agents.is_empty() {
        return "no agents\n".to_owned();
    }
    let mut out = String::new();
    for agent in &agents {
        let task = agent.get("task_id").and_then(Value::as_str).map(|t| format!("  task {t}")).unwrap_or_default();
        let _ = writeln!(out, "@{}  {}  [{}]{task}", s(agent, "name"), s(agent, "role"), s(agent, "status"));
    }
    out
}

fn symbol_line(symbol: &Value) -> String {
    format!(
        "{} {}  {}:{}-{}  sid={}",
        s(symbol, "kind"),
        s(symbol, "qname"),
        s(symbol, "path"),
        symbol.get("start_line").and_then(Value::as_u64).unwrap_or(0),
        symbol.get("end_line").and_then(Value::as_u64).unwrap_or(0),
        s(symbol, "sid")
    )
}

fn format_search(value: &Value) -> String {
    let hits = value.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
    if hits.is_empty() {
        return "no matches\n".to_owned();
    }
    let mut out = String::new();
    for hit in &hits {
        let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        let _ = writeln!(out, "{}  [{} {score:.2}]", symbol_line(&hit["symbol"]), s(hit, "matched"));
        let signature = s(&hit["symbol"], "signature");
        if !signature.is_empty() {
            let _ = writeln!(out, "    {}", signature.lines().next().unwrap_or(""));
        }
    }
    out
}

fn format_symbol(value: &Value) -> String {
    let matches = value.get("matches").and_then(Value::as_array).cloned().unwrap_or_default();
    if matches.is_empty() {
        return "no such symbol\n".to_owned();
    }
    let mut out = String::new();
    for detail in &matches {
        let symbol = &detail["symbol"];
        let _ = writeln!(out, "{}", symbol_line(symbol));
        for key in ["signature", "description"] {
            let text = s(symbol, key);
            if !text.is_empty() {
                let _ = writeln!(out, "{key}: {text}");
            }
        }
        let _ = writeln!(
            out,
            "callers: {}  callees: {}",
            detail.get("callers").and_then(Value::as_u64).unwrap_or(0),
            detail.get("callees").and_then(Value::as_u64).unwrap_or(0)
        );
        if let Some(code) = detail.get("code").and_then(Value::as_str) {
            let _ = writeln!(out, "{}", code.trim_end());
        }
    }
    out
}

fn format_sites(value: &Value) -> String {
    let sites = value.get("sites").or_else(|| value.get("call_sites")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = String::new();
    for site in &sites {
        let args: Vec<&str> =
            site.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let _ = writeln!(
            out,
            "{} -> {}({})  {}:{}  [{}]",
            s(&site["caller"], "qname"),
            s(&site["callee"], "qname"),
            args.join(", "),
            s(site, "path"),
            site.get("line").and_then(Value::as_u64).unwrap_or(0),
            s(site, "confidence")
        );
    }
    if sites.is_empty() {
        out.push_str("no call sites\n");
    }
    out
}

fn format_impact(value: &Value) -> String {
    let mut out = String::from("changed:\n");
    for symbol in value.get("changed").and_then(Value::as_array).cloned().unwrap_or_default() {
        let _ = writeln!(out, "- {}", symbol_line(&symbol));
    }
    out.push_str("call sites:\n");
    out.push_str(&format_sites(value));
    out.push_str("tests:\n");
    for symbol in value.get("tests").and_then(Value::as_array).cloned().unwrap_or_default() {
        let _ = writeln!(out, "- {}", symbol_line(&symbol));
    }
    out
}

fn format_path(value: &Value) -> String {
    if !value.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return "no path\n".to_owned();
    }
    let mut out = String::new();
    for step in value.get("steps").and_then(Value::as_array).cloned().unwrap_or_default() {
        let _ = writeln!(out, "{}", symbol_line(&step["symbol"]));
        if let Some(via) = step.get("via").and_then(Value::as_str) {
            let _ = writeln!(out, "  --{via}-->");
        }
    }
    out
}

fn format_outline(value: &Value) -> String {
    let mut out = format!("{} ({})\n", s(value, "path"), s(value, "language"));
    for symbol in value.get("symbols").and_then(Value::as_array).cloned().unwrap_or_default() {
        let _ = writeln!(
            out,
            "{}-{} {} {}",
            symbol.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            symbol.get("end_line").and_then(Value::as_u64).unwrap_or(0),
            s(&symbol, "kind"),
            s(&symbol, "qname")
        );
    }
    out
}

fn format_tab(value: &Value) -> String {
    format!("tab {}  {}  {}\n", s(value, "tab_id"), s(value, "url"), s(value, "title"))
}

fn format_snapshot(value: &Value) -> String {
    format!("{}  {}\n{}\n", s(value, "url"), s(value, "title"), s(value, "tree").trim_end())
}

fn format_console(value: &Value) -> String {
    let messages = value.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    if messages.is_empty() {
        return "no console messages\n".to_owned();
    }
    messages.iter().map(|m| format!("[{}] {}\n", s(m, "level"), s(m, "text"))).collect()
}

fn format_network(value: &Value) -> String {
    let entries = value.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
    if entries.is_empty() {
        return "no requests\n".to_owned();
    }
    entries
        .iter()
        .map(|e| {
            let status = e
                .get("status")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .or_else(|| e.get("failed").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            format!("{} {} {}\n", s(e, "method"), status, s(e, "url"))
        })
        .collect()
}

fn format_screenshot(value: &Value) -> String {
    format!(
        "{} ({}x{})\n",
        s(value, "path"),
        value.get("width").and_then(Value::as_u64).unwrap_or(0),
        value.get("height").and_then(Value::as_u64).unwrap_or(0)
    )
}

fn job_line(job: &Value) -> String {
    let exit = job.get("exit_code").and_then(Value::as_i64).map(|c| format!(" exit {c}")).unwrap_or_default();
    format!("{}  [{}{exit}]", s(job, "job_id"), s(job, "state"))
}

fn format_job(value: &Value) -> String {
    let tail = s(value, "output_tail");
    if tail.is_empty() {
        format!("{}\n", job_line(value))
    } else {
        format!("{}\n--- output (tail) ---\n{}\n", job_line(value), tail.trim_end())
    }
}

fn format_jobs(value: &Value) -> String {
    let jobs = value.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default();
    if jobs.is_empty() {
        return "no jobs\n".to_owned();
    }
    jobs.iter().map(|job| job_line(job) + "\n").collect()
}

// ----- research -----

/// `--sources a,b` and `--sources a --sources b` both give `["a", "b"]`.
fn research_args(_context: &Context, mut args: Map<String, Value>) -> Result<Value, String> {
    if let Some(Value::Array(items)) = args.get("sources").cloned() {
        let split: Vec<Value> = items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|item| item.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| json!(s)).collect::<Vec<_>>())
            .collect();
        args.insert("sources".to_owned(), Value::Array(split));
    }
    Ok(Value::Object(args))
}

/// Research notes are global (kept by the memory plugin); `--scoped` keeps them to the current
/// project.
fn note_scope(context: &Context, mut args: Map<String, Value>) -> Result<Value, String> {
    if args.remove("scoped").and_then(|v| v.as_bool()) == Some(true) { project_args(context, args) } else { Ok(Value::Object(args)) }
}

fn format_research_search(value: &Value) -> String {
    let hits = value.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let date = hit.get("date").and_then(Value::as_str).map(|d| format!(" ({d})")).unwrap_or_default();
        let _ = writeln!(out, "{}. [{}] {}{date}\n   {}", i + 1, s(hit, "source"), s(hit, "title"), s(hit, "url"));
        let snippet: String = s(hit, "snippet").chars().take(200).collect();
        if !snippet.is_empty() {
            let _ = writeln!(out, "   {snippet}");
        }
    }
    if hits.is_empty() {
        out.push_str("no results\n");
    }
    for source in value.get("sources").and_then(Value::as_array).cloned().unwrap_or_default() {
        let ok = source.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let error = source.get("error").and_then(Value::as_str).map(|e| format!(": {e}")).unwrap_or_default();
        let count = source.get("count").and_then(Value::as_u64).unwrap_or(0);
        let _ = writeln!(out, "  {} {}{error}", s(&source, "source"), if ok { format!("{count} via {}", s(&source, "via")) } else { "FAILED".to_owned() });
    }
    out
}

fn format_research_fetch(value: &Value) -> String {
    let total = value.get("total_chars").and_then(Value::as_u64).unwrap_or(0);
    let published = value.get("published").and_then(Value::as_str).map(|d| format!(", published {d}")).unwrap_or_default();
    let cached = if value.get("stale").and_then(Value::as_bool) == Some(true) {
        ", cached copy (offline)"
    } else if value.get("cached").and_then(Value::as_bool) == Some(true) {
        ", cached"
    } else {
        ""
    };
    let text = s(value, "text");
    let mut out = format!("{}\n{} ({}{published}, {total} chars{cached})\n---\n{}\n", s(value, "title"), s(value, "url"), s(value, "kind"), text.trim_end());
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str("--- (more: --offset <chars>)\n");
    }
    out
}

fn note_line(note: &Value) -> String {
    let date = note.get("date").and_then(Value::as_str).map(|d| format!(", {d}")).unwrap_or_default();
    format!("{} [{}] {}\n  \"{}\" — {}{date} <{}>\n", s(note, "id"), s(note, "topic"), s(note, "claim"), s(note, "quote"), s(note, "title"), s(note, "url"))
}

fn format_research_notes(value: &Value) -> String {
    match value.get("notes").and_then(Value::as_array) {
        Some(notes) if notes.is_empty() => "no notes\n".to_owned(),
        Some(notes) => notes.iter().map(note_line).collect(),
        None => note_line(value),
    }
}

fn format_research_job(value: &Value) -> String {
    let mut out = format!(
        "{}  [{}] {}  round {}/{}  phase {}  read {} (failed {})  notes {}\n",
        s(value, "job_id"),
        s(value, "state"),
        s(value, "topic"),
        value.get("round").and_then(Value::as_u64).unwrap_or(0),
        value.get("depth").and_then(Value::as_u64).unwrap_or(0),
        s(value, "phase"),
        value.get("sources_read").and_then(Value::as_u64).unwrap_or(0),
        value.get("sources_failed").and_then(Value::as_u64).unwrap_or(0),
        value.get("notes").and_then(Value::as_u64).unwrap_or(0),
    );
    if let Some(path) = value.get("report_path").and_then(Value::as_str) {
        let _ = writeln!(out, "report: {path}");
    }
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        let _ = writeln!(out, "error: {error}");
    }
    out
}

fn format_research_jobs(value: &Value) -> String {
    let jobs = value.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default();
    if jobs.is_empty() {
        return "no research jobs\n".to_owned();
    }
    jobs.iter().map(format_research_job).collect()
}

fn format_research_sources(value: &Value) -> String {
    value
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| sources.iter().map(|source| format!("{}: {} — {}\n", s(source, "source"), s(source, "via"), s(source, "detail"))).collect())
        .unwrap_or_default()
}

// ----- the table -----

pub fn commands() -> Vec<Command> {
    let tab = || string("Tab id (default: the most recent tab).");
    vec![
        Command {
            path: "task list",
            method: "memory.task.list",
            summary: "List the project's tasks.",
            positional: &[],
            schema: schema(
                json!({"status": string("Only tasks with this status (pending, in_progress, verifying, done, failed, cancelled)."), "parent_id": string("Only subtasks of this task.")}),
                &[],
            ),
            prepare: Prepare::None,
            build: project_args,
            format: format_tasks,
        },
        Command {
            path: "task show",
            method: "memory.task.get",
            summary: "Show one task with its brief, plan and contract.",
            positional: &["id"],
            schema: schema(json!({"id": string("Task id.")}), &["id"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_task,
        },
        Command {
            path: "task create",
            method: "memory.task.create",
            summary: "Create a task (use @file or - for long text).",
            positional: &["title"],
            schema: schema(
                json!({
                    "title": string("Short title."),
                    "brief": string("Goal and context in detail."),
                    "plan": string("The steps."),
                    "parent_id": string("Parent task id."),
                    "assignee": name("Agent name."),
                    "depends_on": strings("Task ids this one waits for (repeatable)."),
                    "touches": strings("Paths or symbol ids the task touches (repeatable)."),
                    "contract": json!({"type": "object", "description": "Contract as JSON: {\"checks\": [{\"name\", \"kind\": \"command\"|\"manual\", \"spec\", \"expect\"}]}."})
                }),
                &["title", "brief"],
            ),
            prepare: Prepare::None,
            build: project_args,
            format: format_task,
        },
        Command {
            path: "task update",
            method: "memory.task.update",
            summary: "Change fields of a task.",
            positional: &["id"],
            schema: schema(
                json!({
                    "id": string("Task id."),
                    "title": string("New title."),
                    "brief": string("New brief."),
                    "plan": string("New plan."),
                    "status": json!({"type": "string", "enum": ["pending", "in_progress", "verifying", "done", "failed", "cancelled"]}),
                    "assignee": name("Agent name."),
                    "result": string("Outcome summary."),
                    "touches": strings("Paths or symbol ids (repeatable; replaces the list)."),
                    "contract": json!({"type": "object", "description": "Contract as JSON."})
                }),
                &["id"],
            ),
            prepare: Prepare::None,
            build: plain_args,
            format: format_task,
        },
        Command {
            path: "task done",
            method: "memory.task.update",
            summary: "Mark a task done, with a result summary.",
            positional: &["id"],
            schema: schema(json!({"id": string("Task id."), "result": string("What was done (use @file or - for long text).")}), &["id"]),
            prepare: Prepare::None,
            build: |_, args| Ok(with(args, json!({"status": "done"}))),
            format: format_task,
        },
        Command {
            path: "board post",
            method: "memory.board.post",
            summary: "Post a message to the project board.",
            positional: &["text"],
            schema: schema(
                json!({
                    "text": string("The message."),
                    "kind": json!({"type": "string", "enum": ["proposal", "idea", "claim", "blocker", "question", "handoff", "review_request", "patch_offer", "decision", "status"], "description": "Message kind (default status)."}),
                    "to": name("Addressed agent (optional)."),
                    "refs": strings("Referenced task ids, paths or symbols (repeatable).")
                }),
                &["text"],
            ),
            prepare: Prepare::None,
            build: |context, mut args| {
                args.entry("kind").or_insert(json!("status"));
                if let Some(Value::String(to)) = args.get_mut("to") {
                    *to = to.trim_start_matches('@').to_owned();
                }
                Ok(with(args, json!({"project_id": context.project()?, "author": context.author})))
            },
            format: format_posted,
        },
        Command {
            path: "board list",
            method: "memory.board.list",
            summary: "Read the project board.",
            positional: &[],
            schema: schema(
                json!({"since_ms": integer("Only messages after this time (ms since epoch)."), "to": name("Only messages to this agent.")}),
                &[],
            ),
            prepare: Prepare::None,
            build: project_args,
            format: format_board,
        },
        Command {
            path: "agent list",
            method: "agents.list",
            summary: "List the project's agents.",
            positional: &[],
            schema: schema(json!({}), &[]),
            prepare: Prepare::None,
            build: project_args,
            format: format_agents,
        },
        Command {
            path: "agent send",
            method: "agents.steer",
            summary: "Send a message to an agent (steers it now; --followup queues it for after its current step).",
            positional: &["agent", "text"],
            schema: schema(
                json!({"agent": name("Agent name (@name works too)."), "text": string("The message."), "followup": boolean("Deliver after the current step instead of now.")}),
                &["agent", "text"],
            ),
            prepare: Prepare::None,
            build: |context, mut args| {
                let followup = args.remove("followup").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Some(Value::String(agent)) = args.get_mut("agent") {
                    *agent = agent.trim_start_matches('@').to_owned();
                }
                Ok(with(args, json!({"project_id": context.project()?, "kind": if followup { "followup" } else { "steer" }})))
            },
            format: format_json,
        },
        Command {
            path: "agent stop",
            method: "agents.stop",
            summary: "Stop an agent.",
            positional: &["agent"],
            schema: schema(json!({"agent": name("Agent name (@name works too).")}), &["agent"]),
            prepare: Prepare::None,
            build: |context, mut args| {
                if let Some(Value::String(agent)) = args.get_mut("agent") {
                    *agent = agent.trim_start_matches('@').to_owned();
                }
                Ok(with(args, json!({"project_id": context.project()?})))
            },
            format: format_json,
        },
        Command {
            path: "graph search",
            method: "memory.search",
            summary: "Search the code graph (names, descriptions, code).",
            positional: &["text"],
            schema: schema(
                json!({"text": string("What to look for."), "limit": integer("Maximum results (default 20)."), "kinds": strings("Only these symbol kinds (repeatable).")}),
                &["text"],
            ),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_search,
        },
        Command {
            path: "graph symbol",
            method: "memory.symbol",
            summary: "Show a symbol: signature, description, callers/callees counts, code.",
            positional: &["name"],
            schema: schema(json!({"name": string("Name, qualified name or sid.")}), &["name"]),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_symbol,
        },
        Command {
            path: "graph callers",
            method: "memory.callers",
            summary: "Call sites that call a symbol.",
            positional: &["sid"],
            schema: schema(json!({"sid": string("Symbol id."), "depth": integer("Levels to follow (default 1).")}), &["sid"]),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_sites,
        },
        Command {
            path: "graph callees",
            method: "memory.callees",
            summary: "What a symbol calls.",
            positional: &["sid"],
            schema: schema(json!({"sid": string("Symbol id."), "depth": integer("Levels to follow (default 1).")}), &["sid"]),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_sites,
        },
        Command {
            path: "graph impact",
            method: "memory.impact",
            summary: "What a change to symbols (or files) affects: call sites, tests, tasks, rules.",
            positional: &["sids"],
            schema: schema(
                json!({"sids": strings("Changed symbol ids."), "paths": strings("Changed files (repeatable)."), "depth": integer("Levels (default 3).")}),
                &[],
            ),
            prepare: Prepare::None,
            build: |context, mut args| {
                args.entry("sids").or_insert(json!([]));
                scoped_args(context, args)
            },
            format: format_impact,
        },
        Command {
            path: "graph path",
            method: "memory.path",
            summary: "How one symbol reaches another.",
            positional: &["from", "to"],
            schema: schema(
                json!({"from": string("Start symbol."), "to": string("End symbol."), "max_hops": integer("Maximum hops (default 8).")}),
                &["from", "to"],
            ),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_path,
        },
        Command {
            path: "graph outline",
            method: "memory.outline",
            summary: "Symbols of a file with their line spans.",
            positional: &["path"],
            schema: schema(json!({"path": string("File path.")}), &["path"]),
            prepare: Prepare::None,
            build: scoped_args,
            format: format_outline,
        },
        Command {
            path: "project open",
            method: "memory.project.open",
            summary: "Open a repository in the code graph; --reference opens another repository read-only (indexed in the background, queried as graph://<name>/..., symbol://<name>/...).",
            positional: &["root"],
            schema: schema(
                json!({"root": string("The repository folder."), "reference": boolean("Read-only reference project (never written to)."), "project_id": string("Project id (default: the id of that root).")}),
                &["root"],
            ),
            prepare: Prepare::None,
            build: |_context, mut args| {
                // Memory needs an absolute path; a relative one is relative to where cx runs.
                if let Some(Value::String(root)) = args.get("root").cloned() {
                    let absolute = std::env::current_dir().map_err(|error| error.to_string())?.join(&root);
                    args.insert("root".to_owned(), json!(absolute.to_string_lossy()));
                }
                Ok(Value::Object(args))
            },
            format: format_project,
        },
        Command {
            path: "project close",
            method: "memory.project.close",
            summary: "Close a read-only reference project: the code graph forgets it (its index and its name); the repository on disk is untouched. Session projects are not closed.",
            positional: &["name"],
            schema: schema(json!({"name": name("The reference's name or id (cx project list).")}), &["name"]),
            prepare: Prepare::None,
            build: |_context, args| Ok(json!({"project_id": args.get("name").cloned().unwrap_or(Value::Null)})),
            format: |value| format!("closed {}  {}  {}", s(value, "name"), s(value, "project_id"), s(value, "root")),
        },
        Command {
            path: "project list",
            method: "memory.project.list",
            summary: "Projects the code graph knows, with the names that qualify URIs (graph://<name>/search/...).",
            positional: &[],
            schema: schema(json!({}), &[]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_projects,
        },
        Command {
            path: "browser open",
            method: "browser.open",
            summary: "Open a URL in the built-in browser (shown in the right pane).",
            positional: &["url"],
            schema: schema(json!({"url": string("Address to open.")}), &["url"]),
            prepare: Prepare::None,
            build: session_args,
            format: format_tab,
        },
        Command {
            path: "browser snapshot",
            method: "browser.snapshot",
            summary: "The page as an accessibility tree with element refs.",
            positional: &[],
            schema: schema(json!({"tab_id": tab()}), &[]),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_snapshot,
        },
        Command {
            path: "browser console",
            method: "browser.console",
            summary: "Console messages of a tab.",
            positional: &[],
            schema: schema(json!({"tab_id": tab(), "since_ms": integer("Only messages after this time.")}), &[]),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_console,
        },
        Command {
            path: "browser network",
            method: "browser.network",
            summary: "Network requests of a tab.",
            positional: &[],
            schema: schema(json!({"tab_id": tab(), "since_ms": integer("Only requests after this time.")}), &[]),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_network,
        },
        Command {
            path: "browser eval",
            method: "browser.eval",
            summary: "Evaluate JavaScript in the page; prints the JSON result.",
            positional: &["expression"],
            schema: schema(
                json!({"expression": string("JavaScript expression (use @file or - for scripts)."), "tab_id": tab()}),
                &["expression"],
            ),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_json,
        },
        Command {
            path: "browser click",
            method: "browser.click",
            summary: "Click an element by ref (ref=N from a snapshot) or CSS selector.",
            positional: &["target"],
            schema: schema(json!({"target": string("ref=N or a CSS selector."), "tab_id": tab()}), &["target"]),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_json,
        },
        Command {
            path: "browser type",
            method: "browser.type",
            summary: "Type text into an element by ref or CSS selector.",
            positional: &["target", "text"],
            schema: schema(
                json!({"target": string("ref=N or a CSS selector."), "text": string("Text to type."), "tab_id": tab()}),
                &["target", "text"],
            ),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_json,
        },
        Command {
            path: "browser screenshot",
            method: "browser.screenshot",
            summary: "Save a PNG screenshot of a tab; prints its path.",
            positional: &[],
            schema: schema(json!({"tab_id": tab()}), &[]),
            prepare: Prepare::BrowserTab,
            build: plain_args,
            format: format_screenshot,
        },
        Command {
            path: "research search",
            method: "research.search",
            summary: "Search papers, code and discussions online: arXiv, Semantic Scholar, GitHub, Reddit, Hacker News, the web.",
            positional: &["query"],
            schema: schema(
                json!({
                    "query": string("What to look for."),
                    "sources": strings("Sources (repeatable or comma-separated): arxiv, semantic_scholar, github, github_issues, github_code, reddit, hackernews, web (default: all but github_issues/github_code)."),
                    "since": string("Only results from this date on (YYYY-MM-DD)."),
                    "limit": integer("Results per source (default 5)."),
                    "sort": json!({"type": "string", "enum": ["relevance", "recent", "popular"], "description": "Order (default relevance)."}),
                }),
                &["query"],
            ),
            prepare: Prepare::None,
            build: research_args,
            format: format_research_search,
        },
        Command {
            path: "research fetch",
            method: "research.fetch",
            summary: "A web page or PDF as clean text (cached under CORTEX_HOME/research/cache).",
            positional: &["url"],
            schema: schema(
                json!({
                    "url": string("http(s) URL."),
                    "offset": integer("Characters to skip (continue a long document)."),
                    "max_chars": integer("Most characters to print (default 20000)."),
                    "refresh": boolean("Fetch again even when cached."),
                }),
                &["url"],
            ),
            prepare: Prepare::None,
            build: |_, mut args| {
                args.entry("max_chars").or_insert(json!(20_000));
                Ok(Value::Object(args))
            },
            format: format_research_fetch,
        },
        Command {
            path: "research note",
            method: "research.note.add",
            summary: "Keep a cited research note (claim + verbatim quote + source) in memory; global unless --scoped.",
            positional: &["topic", "claim"],
            schema: schema(
                json!({
                    "topic": string("Topic the note belongs to."),
                    "claim": string("The finding, in your words."),
                    "quote": string("Verbatim text from the source that supports it."),
                    "url": string("The source."),
                    "title": string("The source's title."),
                    "date": string("The source's date (YYYY-MM-DD)."),
                    "scoped": boolean("Keep it to the current project (default: global)."),
                }),
                &["topic", "claim", "quote", "url"],
            ),
            prepare: Prepare::None,
            build: |context, mut args| {
                if !context.author.is_empty() {
                    args.entry("by").or_insert(json!(context.author));
                }
                note_scope(context, args)
            },
            format: format_research_notes,
        },
        Command {
            path: "research notes",
            method: "research.notes",
            summary: "Research notes, oldest first (of one topic with --topic; the project's with --scoped).",
            positional: &[],
            schema: schema(json!({"topic": string("Only this topic."), "scoped": boolean("The current project's notes (default: global).")}), &[]),
            prepare: Prepare::None,
            build: note_scope,
            format: format_research_notes,
        },
        Command {
            path: "research find",
            method: "research.note.search",
            summary: "Search research notes by meaning (the project's with --scoped).",
            positional: &["text"],
            schema: schema(
                json!({"text": string("What to look for."), "topic": string("Only this topic."), "limit": integer("Most notes (default 20)."), "scoped": boolean("The current project's notes (default: global).")}),
                &["text"],
            ),
            prepare: Prepare::None,
            build: note_scope,
            format: format_research_notes,
        },
        Command {
            path: "research deep",
            method: "research.deep",
            summary: "Deep research as a background job: plan, search, read with the local model, synthesize; report in docs/research/<topic>.md.",
            positional: &["topic"],
            schema: schema(
                json!({
                    "topic": string("What to research."),
                    "questions": strings("Questions to answer (repeatable; default: planned by the model)."),
                    "depth": integer("Rounds of search and reading (default 2, at most 4)."),
                    "sources": strings("Sources to search (repeatable or comma-separated)."),
                    "since": string("Only sources from this date on (YYYY-MM-DD)."),
                    "max_sources": integer("Sources read per round."),
                    "fresh": boolean("Start over instead of resuming the topic's unfinished job."),
                }),
                &["topic"],
            ),
            prepare: Prepare::None,
            build: |context, args| {
                let Value::Object(mut args) = research_args(context, args)? else { unreachable!("research_args builds an object") };
                if let Some(session) = &context.session_id {
                    args.entry("session_id").or_insert(json!(session));
                }
                project_args(context, args)
            },
            format: format_research_job,
        },
        Command {
            path: "research job",
            method: "research.job.status",
            summary: "State and progress of a deep research job.",
            positional: &["job_id"],
            schema: schema(json!({"job_id": string("Job id (rj-...).")}), &["job_id"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_research_job,
        },
        Command {
            path: "research jobs",
            method: "research.jobs",
            summary: "The project's deep research jobs, newest first.",
            positional: &[],
            schema: schema(json!({}), &[]),
            prepare: Prepare::None,
            build: project_args,
            format: format_research_jobs,
        },
        Command {
            path: "research cancel",
            method: "research.job.cancel",
            summary: "Cancel a deep research job (its notes stay; `cx research deep` on the topic resumes it).",
            positional: &["job_id"],
            schema: schema(json!({"job_id": string("Job id (rj-...).")}), &["job_id"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_research_job,
        },
        Command {
            path: "research sources",
            method: "research.sources",
            summary: "What each research source uses right now (gh sign-in, keys, the browser).",
            positional: &[],
            schema: schema(json!({}), &[]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_research_sources,
        },
        Command {
            path: "job list",
            method: "tools.job.list",
            summary: "Background jobs of this session (--all for every session).",
            positional: &[],
            schema: schema(json!({"all": boolean("Every session's jobs.")}), &[]),
            prepare: Prepare::None,
            build: |context, mut args| {
                let all = args.remove("all").and_then(|v| v.as_bool()).unwrap_or(false);
                if !all && let Some(session) = &context.session_id {
                    args.insert("session_id".to_owned(), json!(session));
                }
                Ok(Value::Object(args))
            },
            format: format_jobs,
        },
        Command {
            path: "job status",
            method: "tools.job.status",
            summary: "State, exit code and output tail of a background job.",
            positional: &["job_id"],
            schema: schema(json!({"job_id": string("Job id.")}), &["job_id"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_job,
        },
        Command {
            path: "job kill",
            method: "tools.job.kill",
            summary: "Stop a background job (its whole process group).",
            positional: &["job_id"],
            schema: schema(json!({"job_id": string("Job id.")}), &["job_id"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_job,
        },
        Command {
            path: "plugin list",
            method: "frame.plugins.list",
            summary: "The installed plugins of the running CortexCode: version, category, enabled.",
            positional: &[],
            schema: schema(json!({}), &[]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_plugins,
        },
        Command {
            path: "plugin install",
            method: "frame.plugins.install",
            summary: "Install a .ccplugin (from `cortex-pack build <plugin folder>`): backend and UI swapped live; `cx plugin rollback` undoes it.",
            positional: &["path"],
            schema: schema(json!({"path": string("The .ccplugin file.")}), &["path"]),
            prepare: Prepare::None,
            build: |_context, mut args| {
                // The frame needs an absolute path; a relative one is relative to where cx runs.
                if let Some(Value::String(path)) = args.get("path").cloned() {
                    let absolute = std::env::current_dir().map_err(|error| error.to_string())?.join(&path);
                    args.insert("path".to_owned(), json!(absolute.to_string_lossy()));
                }
                Ok(Value::Object(args))
            },
            format: format_package_swap,
        },
        Command {
            path: "plugin rollback",
            method: "frame.plugins.rollback",
            summary: "Restore the version a plugin had before its last install (backend and UI).",
            positional: &["plugin"],
            schema: schema(json!({"plugin": name("Plugin id (cx plugin list).")}), &["plugin"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_package_swap,
        },
        Command {
            path: "plugin disable",
            method: "frame.plugins.disable",
            summary: "Stop a plugin's backend and remove its UI until it is enabled again.",
            positional: &["plugin"],
            schema: schema(json!({"plugin": name("Plugin id (cx plugin list).")}), &["plugin"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_toggle,
        },
        Command {
            path: "plugin enable",
            method: "frame.plugins.enable",
            summary: "Start a disabled plugin again (backend and UI).",
            positional: &["plugin"],
            schema: schema(json!({"plugin": name("Plugin id (cx plugin list).")}), &["plugin"]),
            prepare: Prepare::None,
            build: plain_args,
            format: format_toggle,
        },
    ]
}

fn project_line(project: &Value) -> String {
    let reference = if project.get("reference").and_then(Value::as_bool).unwrap_or(false) { "  read-only reference" } else { "" };
    format!("{}  {}  {}  [{}]{reference}", s(project, "name"), s(project, "project_id"), s(project, "root"), s(project, "state"))
}

fn format_project(value: &Value) -> String {
    let mut out = project_line(value);
    if let Some(files) = value.get("files").and_then(Value::as_u64).filter(|files| *files > 0) {
        let _ = write!(out, "\n{files} files, {} symbols", value.get("symbols").and_then(Value::as_u64).unwrap_or(0));
    }
    out
}

fn format_projects(value: &Value) -> String {
    let projects = value.get("projects").and_then(Value::as_array).cloned().unwrap_or_default();
    if projects.is_empty() {
        return "no projects".to_owned();
    }
    projects.iter().map(project_line).collect::<Vec<_>>().join("\n")
}

fn format_plugins(value: &Value) -> String {
    let units = value.get("units").and_then(Value::as_array).cloned().unwrap_or_default();
    if units.is_empty() {
        return "no plugins".to_owned();
    }
    let mut out = String::new();
    for unit in units {
        let text = |key: &str| unit.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
        let enabled = unit.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let previous = unit.get("previous_version").and_then(Value::as_str).map(|version| format!(" (rollback to {version})")).unwrap_or_default();
        let category = unit.get("category").and_then(Value::as_str).map(|category| format!(" [{category}]")).unwrap_or_default();
        let _ = writeln!(out, "{} {}{}{}{}", text("id"), text("version"), category, if enabled { "" } else { " DISABLED" }, previous);
    }
    out.trim_end().to_owned()
}

fn format_steps(value: &Value) -> String {
    value
        .get("steps")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .map(|step| {
                    let ok = step.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let detail = step.get("detail").and_then(Value::as_str).filter(|detail| !detail.is_empty()).map(|detail| format!(": {detail}")).unwrap_or_default();
                    format!("  {} {}{}", if ok { "ok" } else { "FAILED" }, step.get("step").and_then(Value::as_str).unwrap_or(""), detail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn format_package_swap(value: &Value) -> String {
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let mut out = if ok {
        format!("{} {} -> {}: live", text("plugin"), text("from_version"), text("to_version"))
    } else {
        let restored = if value.get("rolled_back").and_then(Value::as_bool).unwrap_or(false) { "; the previous version is running again" } else { "; nothing changed" };
        format!("{}: FAILED: {}{}", text("plugin"), value.get("error").and_then(Value::as_str).unwrap_or("see the steps"), restored)
    };
    let steps = format_steps(value);
    if !steps.is_empty() {
        out.push('\n');
        out.push_str(&steps);
    }
    out
}

fn format_toggle(value: &Value) -> String {
    let unit = value.get("plugin").cloned().unwrap_or_default();
    let id = unit.get("id").and_then(Value::as_str).unwrap_or("");
    let enabled = unit.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let changed = value.get("changed").and_then(Value::as_bool).unwrap_or(false);
    let mut out = format!("{id}: {}{}", if enabled { "enabled" } else { "disabled" }, if changed { "" } else { " (already)" });
    let steps = format_steps(value);
    if !steps.is_empty() {
        out.push('\n');
        out.push_str(&steps);
    }
    out
}

/// The `cx tools` listing: every command with usage and summary, generated from the schemas.
pub fn listing(commands: &[Command]) -> String {
    let mut out = String::from("cx commands (add --json for JSON output; @file or - reads text from a file or stdin):\n");
    let mut group = "";
    for command in commands {
        let head = command.path.split(' ').next().unwrap_or("");
        if head != group {
            out.push('\n');
            group = head;
        }
        let _ = writeln!(out, "{}", crate::args::usage(command.path, &command.schema, command.positional));
        let _ = writeln!(out, "    {}", command.summary);
    }
    out.push_str(
        "\ncx call <method> [json|@file|-]   call any frame method directly\ncx <command> --help                details of one command\n",
    );
    out
}
