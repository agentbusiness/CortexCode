//! Turning events into notifications.
//!
//! Two steps: [`Trigger`]s are recognized from event payloads by structure alone (the journal entry
//! kind, a status entry's enumerated `state`, `needs_approval`, a verification report's `ok`, a
//! note's `purpose`, `agents.changed`'s `status`, `plugin.swapped`'s `ok`/`rolled_back`, a job's
//! `state`). No text is interpreted. Then [`notification`] writes the title, subtitle, body, dedup
//! key and target from the trigger plus the names the service looked up ([`Context`]).
//!
//! Every target names the exact place to look: `{open, kind, project_id, session_id, seq}` (the
//! journal entry the notification is about), plus `task_id` and `agent` when known, and per kind
//! what the workbench opens there (the approval's call, the failing check, the conflicting file and
//! line).

use crate::prefs::Prefs;
use cc_protocol::contracts::chatflow::kinds as chatflow_kinds;
use cc_protocol::contracts::frame::{JournalPatch, SwapResult, kinds as entry_kinds};
use cc_protocol::contracts::notify::{Notification, kinds};
use serde_json::{Value, json};

/// Something that may deserve a notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    /// A `status` journal entry with a final state (done, failed, cancelled, blocked, stopped).
    Status { session_id: String, seq: u64, state: String, detail: String, run_id: Option<String>, task_id: Option<String> },
    /// An ask-mode `approval` entry waiting for the person (`state: "pending"`, `pending: true`;
    /// answerable with `chatflow.approve`), or a `tool_result` that needed approval (`needs_approval`,
    /// or `error_kind: "approval_required"` in auto mode, where the call was refused).
    Approval { session_id: String, seq: u64, call_id: String, command: Option<String>, classes: Vec<String>, output: String, tool: Option<String>, pending: bool },
    /// A `verification` entry.
    Verification { session_id: String, seq: u64, task_id: String, ok: bool, checks: Vec<Check>, merged: Option<String>, agent: Option<String> },
    /// An `agent_spawned` entry.
    Spawned { session_id: String, seq: u64, agent: String, task_id: Option<String>, agent_session: Option<String> },
    /// A `note` entry with `purpose: "merge"` (the merge steward handed a conflict back), with the
    /// conflicting files and their first conflict lines when the note carries them.
    Conflict { session_id: String, seq: u64, text: String, files: Vec<(String, Option<u64>)> },
    /// `agents.changed` with `status: "blocked"` (`session_id`: the agent's session).
    AgentBlocked { project_id: String, agent: String, task_id: Option<String>, session_id: Option<String> },
    /// `plugin.swapped` with `ok: false` or `rolled_back: true`.
    Swap { result: SwapResult },
    /// `plugin.state` saying a service crashed again and again (a crash loop): the frame keeps
    /// restarting it, and the person should know and be able to restart or roll it back.
    Crash { plugin_id: String, category: String, version: String, restarts: u64 },
    /// `tools.job` with a final state (exited, killed).
    Job { job_id: String, session_id: Option<String>, state: String, exit_code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output_tail: String,
}

impl Trigger {
    /// The session the trigger belongs to, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Trigger::Status { session_id, .. }
            | Trigger::Approval { session_id, .. }
            | Trigger::Verification { session_id, .. }
            | Trigger::Spawned { session_id, .. }
            | Trigger::Conflict { session_id, .. } => Some(session_id),
            Trigger::Job { session_id, .. } | Trigger::AgentBlocked { session_id, .. } => session_id.as_deref(),
            Trigger::Swap { .. } | Trigger::Crash { .. } => None,
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        match self {
            Trigger::Status { task_id, .. } | Trigger::Spawned { task_id, .. } | Trigger::AgentBlocked { task_id, .. } => task_id.as_deref(),
            Trigger::Verification { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// Where it came from, for the history.
    pub fn source(&self) -> String {
        match self {
            Trigger::Status { session_id, seq, .. }
            | Trigger::Approval { session_id, seq, .. }
            | Trigger::Verification { session_id, seq, .. }
            | Trigger::Spawned { session_id, seq, .. }
            | Trigger::Conflict { session_id, seq, .. } => format!("journal:{session_id}#{seq}"),
            Trigger::AgentBlocked { .. } => "agents.changed".into(),
            Trigger::Swap { .. } => "plugin.swapped".into(),
            Trigger::Crash { category, .. } => format!("plugin.state:{category}"),
            Trigger::Job { job_id, .. } => format!("tools.job:{job_id}"),
        }
    }
}

fn str_of(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A trigger from one `journal.<session>` patch. Only new entries count (`create`): appends and
/// sets stream text into entries that already exist.
pub fn from_journal_patch(data: &Value) -> Option<Trigger> {
    // Cheap structural pre-check: most patches are streaming appends.
    if data.get("op").and_then(Value::as_str) != Some("create") {
        return None;
    }
    // Only these entry kinds can matter; skip the rest before copying a possibly large entry.
    let kind = data.pointer("/entry/kind").and_then(Value::as_str)?;
    if ![entry_kinds::STATUS, entry_kinds::TOOL_RESULT, entry_kinds::VERIFICATION, entry_kinds::AGENT_SPAWNED, entry_kinds::NOTE, chatflow_kinds::APPROVAL]
        .contains(&kind)
    {
        return None;
    }
    if kind == entry_kinds::TOOL_RESULT && !needed_approval(data.pointer("/entry/data")?) {
        return None;
    }
    if kind == chatflow_kinds::APPROVAL && data.pointer("/entry/data/state").and_then(Value::as_str) != Some("pending") {
        return None;
    }
    if kind == entry_kinds::NOTE && data.pointer("/entry/data/purpose").and_then(Value::as_str) != Some("merge") {
        return None;
    }
    let JournalPatch::Create { entry, .. } = serde_json::from_value::<JournalPatch>(data.clone()).ok()? else {
        return None;
    };
    let d = &entry.data;
    let session_id = entry.session_id.clone();
    let seq = entry.seq;
    match entry.kind.as_str() {
        entry_kinds::STATUS => {
            let state = str_of(d, "state")?;
            // The run lifecycle's enumerated final states; `running` (and anything else) is not news.
            if !matches!(state.as_str(), "done" | "failed" | "cancelled" | "blocked" | "stopped") {
                return None;
            }
            Some(Trigger::Status {
                session_id,
                seq,
                state,
                detail: str_of(d, "detail").unwrap_or_default(),
                run_id: str_of(d, "run_id"),
                task_id: str_of(d, "task_id"),
            })
        }
        entry_kinds::TOOL_RESULT => {
            if !needed_approval(d) {
                return None;
            }
            let approval = d.get("display").and_then(|display| display.get("approval"));
            Some(Trigger::Approval {
                session_id,
                seq,
                call_id: str_of(d, "call_id").unwrap_or_else(|| seq.to_string()),
                command: approval.and_then(|a| str_of(a, "command")),
                classes: strings(approval.and_then(|a| a.get("classes"))),
                output: str_of(d, "output").unwrap_or_default(),
                tool: str_of(d, "tool"),
                pending: false,
            })
        }
        chatflow_kinds::APPROVAL => Some(Trigger::Approval {
            session_id,
            seq,
            call_id: str_of(d, "call_id").unwrap_or_else(|| seq.to_string()),
            command: d.get("args").and_then(|args| str_of(args, "command")),
            classes: strings(d.get("classes")),
            output: str_of(d, "question").unwrap_or_default(),
            tool: str_of(d, "tool"),
            pending: true,
        }),
        entry_kinds::VERIFICATION => {
            let report = d.get("report")?;
            let ok = report.get("ok").and_then(Value::as_bool)?;
            let checks = report
                .get("checks")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|check| Check {
                            name: str_of(check, "name").unwrap_or_else(|| "check".into()),
                            ok: check.get("ok").and_then(Value::as_bool).unwrap_or(false),
                            exit_code: check.get("exit_code").and_then(Value::as_i64).map(|c| c as i32),
                            output_tail: str_of(check, "output_tail").unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Trigger::Verification {
                session_id,
                seq,
                task_id: str_of(d, "task_id").or_else(|| str_of(report, "task_id"))?,
                ok,
                checks,
                merged: str_of(d, "merged"),
                agent: str_of(d, "agent"),
            })
        }
        entry_kinds::AGENT_SPAWNED => Some(Trigger::Spawned {
            session_id,
            seq,
            agent: str_of(d, "agent")?,
            task_id: str_of(d, "task_id"),
            agent_session: str_of(d, "session_id"),
        }),
        entry_kinds::NOTE if d.get("purpose").and_then(Value::as_str) == Some("merge") => {
            let files = d
                .get("conflicts")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(|c| Some((str_of(c, "path")?, c.get("line").and_then(Value::as_u64)))).collect())
                .unwrap_or_default();
            Some(Trigger::Conflict { session_id, seq, text: str_of(d, "text").unwrap_or_default(), files })
        }
        _ => None,
    }
}

/// A tool result the person had to approve: `needs_approval` (ask mode), or a refusal for want of
/// an approval in auto mode (`error_kind: "approval_required"`). Enumerated fields, no text.
fn needed_approval(data: &Value) -> bool {
    data.get("needs_approval").and_then(Value::as_bool) == Some(true) || data.get("error_kind").and_then(Value::as_str) == Some("approval_required")
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// A trigger from `agents.changed` (`{agent, project_id, status, task_id}`).
pub fn from_agents_changed(data: &Value) -> Option<Trigger> {
    if data.get("status").and_then(Value::as_str) != Some("blocked") {
        return None;
    }
    Some(Trigger::AgentBlocked {
        project_id: str_of(data, "project_id")?,
        agent: str_of(data, "agent")?,
        task_id: str_of(data, "task_id"),
        session_id: str_of(data, "session_id"),
    })
}

/// A trigger from `plugin.swapped` ([`SwapResult`]).
pub fn from_swapped(data: &Value) -> Option<Trigger> {
    let result: SwapResult = serde_json::from_value(data.clone()).ok()?;
    if result.ok && !result.rolled_back {
        return None;
    }
    Some(Trigger::Swap { result })
}

/// Crashes in a row before a service counts as crash-looping (the frame falls back at the same count).
pub const CRASH_LOOP: u64 = 3;

/// A trigger from `plugin.state` (a `PluginInfo`): a service that crashed [`CRASH_LOOP`] times or
/// more in a row.
pub fn from_plugin_state(data: &Value) -> Option<Trigger> {
    if str_of(data, "state")? != "crashed" {
        return None;
    }
    let restarts = data.get("restarts").and_then(Value::as_u64).unwrap_or(0);
    if restarts < CRASH_LOOP {
        return None;
    }
    Some(Trigger::Crash { plugin_id: str_of(data, "plugin_id")?, category: str_of(data, "category")?, version: str_of(data, "version").unwrap_or_default(), restarts })
}

/// A trigger from `tools.job` (`{job_id, session_id, state, exit_code?}`). `running` is a wake-rule
/// match, not an ending.
pub fn from_job(data: &Value) -> Option<Trigger> {
    let state = str_of(data, "state")?;
    if !matches!(state.as_str(), "exited" | "killed") {
        return None;
    }
    Some(Trigger::Job {
        job_id: str_of(data, "job_id")?,
        session_id: str_of(data, "session_id"),
        state,
        exit_code: data.get("exit_code").and_then(Value::as_i64).map(|c| c as i32),
    })
}

/// Names and facts looked up for a trigger. Everything is optional: a missing name is left out of
/// the text, never invented.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub session_title: Option<String>,
    /// Agent name of an agent session.
    pub agent: Option<String>,
    pub task_title: Option<String>,
    /// For jobs: when it started and its output tail (from `tools.job.status`).
    pub job_started_ms: Option<u64>,
    pub job_output_tail: Option<String>,
    /// The journal entry the notification is about, when it is not the trigger's own entry (a
    /// finished run's answer, a failed run's last error, an agent's blocking entry).
    pub focus_seq: Option<u64>,
    pub now_ms: u64,
}

/// `1h 2m`, `3m 5s`, `12s`.
pub fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// The last non-empty line of a text (a job's output tail, a check's tail): a structural pick.
pub fn last_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty()).map(str::to_owned)
}

fn join(parts: &[Option<&str>], separator: &str) -> Option<String> {
    let kept: Vec<&str> = parts.iter().flatten().copied().filter(|p| !p.is_empty()).collect();
    (!kept.is_empty()).then(|| kept.join(separator))
}

impl Context {
    /// What the notification is about: the task, else the session title, else the agent.
    fn subject(&self) -> String {
        self.task_title
            .clone()
            .or_else(|| self.session_title.clone())
            .or_else(|| self.agent.clone())
            .unwrap_or_else(|| "a session".into())
    }

    /// `project · agent` (or `project · session title` when the subject is not the title).
    fn subtitle(&self) -> Option<String> {
        let second = match (&self.agent, &self.task_title, &self.session_title) {
            (Some(agent), _, _) => Some(agent.as_str()),
            (None, Some(_), Some(title)) => Some(title.as_str()),
            _ => None,
        };
        join(&[self.project_name.as_deref(), second], " · ")
    }
}

fn session_target(open: &str, ctx: &Context, session_id: &str, seq: Option<u64>) -> Value {
    let mut target = json!({"open": open, "project_id": ctx.project_id, "session_id": session_id});
    if let Some(seq) = seq {
        target["seq"] = json!(seq);
    }
    target
}

fn base(kind: &str, title: String, body: String, key: String, ctx: &Context, session_id: Option<&str>, target: Value) -> Notification {
    Notification {
        kind: kind.to_owned(),
        title,
        body,
        key: Some(key),
        project_id: ctx.project_id.clone(),
        session_id: session_id.map(str::to_owned),
        target,
        actions: Vec::new(),
        subtitle: ctx.subtitle(),
    }
}

/// The notification for a trigger, or `None` when the trigger does not warrant one (a job that
/// ended before the "long job" threshold).
pub fn notification(trigger: &Trigger, ctx: &Context, prefs: &Prefs) -> Option<Notification> {
    let mut n = compose(trigger, ctx, prefs)?;
    complete_target(&mut n, trigger, ctx);
    Some(n)
}

/// The action a pending approval's notification offers; it answers with `chatflow.approve`.
pub const APPROVE_ACTION: &str = "Approve";

/// Every target names `kind`, `project_id`, `session_id` and `seq` (when the notification is about
/// a session), and `task_id` / `agent` when known.
fn complete_target(n: &mut Notification, trigger: &Trigger, ctx: &Context) {
    let Some(target) = n.target.as_object_mut() else { return };
    target.insert("kind".into(), json!(n.kind));
    if target.get("project_id").is_none_or(|v| v.is_null())
        && let Some(project) = n.project_id.clone().or_else(|| ctx.project_id.clone())
    {
        target.insert("project_id".into(), json!(project));
    }
    if target.get("session_id").is_none_or(|v| v.is_null())
        && let Some(session) = n.session_id.clone()
    {
        target.insert("session_id".into(), json!(session));
    }
    if !target.contains_key("task_id")
        && let Some(task) = trigger.task_id()
    {
        target.insert("task_id".into(), json!(task));
    }
    if target.get("agent").is_none_or(|v| v.is_null())
        && let Some(agent) = &ctx.agent
    {
        target.insert("agent".into(), json!(agent));
    }
}

fn compose(trigger: &Trigger, ctx: &Context, prefs: &Prefs) -> Option<Notification> {
    let subject = ctx.subject();
    Some(match trigger {
        Trigger::Status { session_id, seq, state, detail, run_id, .. } => {
            let (kind, title, fallback) = match state.as_str() {
                "done" => (kinds::DONE, format!("Done: {subject}"), "Finished."),
                "failed" => (kinds::FAILED, format!("Failed: {subject}"), "The run stopped with an error."),
                "cancelled" => (kinds::CANCELLED, format!("Cancelled: {subject}"), "The run was cancelled."),
                "stopped" => (kinds::CANCELLED, format!("Stopped: {subject}"), "The agent was stopped."),
                _ => (kinds::BLOCKED, format!("Blocked: {subject}"), "Waiting for you."),
            };
            let body = if detail.trim().is_empty() { fallback.to_owned() } else { detail.trim().to_owned() };
            let key = if kind == kinds::BLOCKED {
                blocked_key(ctx, session_id)
            } else {
                format!("run:{session_id}:{}", run_id.clone().unwrap_or_else(|| seq.to_string()))
            };
            // The place to look: the final answer (done) or the last error (failed), else the status.
            let mut target = session_target("session", ctx, session_id, Some(ctx.focus_seq.unwrap_or(*seq)));
            target["status_seq"] = json!(seq);
            base(kind, title, body, key, ctx, Some(session_id), target)
        }
        Trigger::Approval { session_id, seq, call_id, command, classes, output, tool, pending } => {
            // The approval description: what it would do (the tool and its command) and why it
            // needs a yes (the question or the refusal, and the action classes).
            let what = match (command, tool.as_deref()) {
                (Some(command), _) => Some(command.clone()),
                (None, Some(tool)) => Some(format!("the {tool} tool")),
                (None, None) => None,
            };
            let mut lines: Vec<String> = Vec::new();
            if !output.trim().is_empty() {
                lines.push(output.trim().to_owned());
            } else if let Some(what) = &what {
                lines.push(format!("{what} needs your approval."));
            } else {
                lines.push("A command needs your approval.".to_owned());
            }
            if let Some(command) = command
                && !output.contains(command.as_str())
            {
                lines.push(format!("Runs: {command}"));
            }
            if !classes.is_empty() {
                lines.push(format!("Asks for: {}", classes.join(", ")));
            }
            if !pending {
                lines.push("It was not run; the run went on without it.".to_owned());
            }
            let mut target = session_target("approval", ctx, session_id, Some(*seq));
            target["call_id"] = json!(call_id);
            target["classes"] = json!(classes);
            if let Some(command) = command {
                target["command"] = json!(command);
            }
            if let Some(tool) = tool {
                target["tool"] = json!(tool);
            }
            if *pending {
                // chatflow.approve answers this entry (the notification's Approve action does).
                target["approval_seq"] = json!(seq);
            }
            let title = format!("Approval needed: {subject}");
            let mut n = base(kinds::APPROVAL, title, lines.join("\n"), format!("approval:{session_id}:{call_id}"), ctx, Some(session_id), target);
            if *pending {
                n.actions = vec![APPROVE_ACTION.to_owned()];
            }
            n
        }
        Trigger::Verification { session_id, seq, task_id, ok, checks, merged, agent } => {
            let mut ctx = ctx.clone();
            if ctx.agent.is_none() {
                ctx.agent = agent.clone();
            }
            let subject = ctx.subject();
            let passed = checks.iter().filter(|check| check.ok).count();
            let (kind, title, body) = if *ok {
                let mut body = format!("{passed} of {} checks passed.", checks.len());
                if let Some(sha) = merged {
                    body.push_str(&format!(" Merged {}.", sha.chars().take(10).collect::<String>()));
                }
                (kinds::VERIFIED, format!("Contract verified: {subject}"), body)
            } else {
                let failing: Vec<&Check> = checks.iter().filter(|check| !check.ok).collect();
                let names: Vec<String> = failing
                    .iter()
                    .map(|check| match check.exit_code {
                        Some(code) => format!("{} (exit {code})", check.name),
                        None => check.name.clone(),
                    })
                    .collect();
                let mut body = if names.is_empty() { "The contract does not hold.".to_owned() } else { format!("Failing: {}.", names.join(", ")) };
                if let Some(line) = failing.first().and_then(|check| last_line(&check.output_tail)) {
                    body.push('\n');
                    body.push_str(&line);
                }
                (kinds::VERIFY_FAILED, format!("Contract failed: {subject}"), body)
            };
            let mut target = session_target("task", &ctx, session_id, Some(*seq));
            target["task_id"] = json!(task_id);
            if let Some(check) = checks.iter().find(|check| !check.ok) {
                target["check"] = json!(check.name);
            }
            base(kind, title, body, format!("verify:{task_id}"), &ctx, Some(session_id), target)
        }
        Trigger::Spawned { session_id, seq, agent, agent_session, .. } => {
            let mut ctx = ctx.clone();
            ctx.agent = Some(agent.clone());
            let body = match &ctx.task_title {
                Some(task) => format!("Working on: {task}"),
                None => "Started on a task.".to_owned(),
            };
            let session = agent_session.as_deref().unwrap_or(session_id);
            base(
                kinds::SPAWNED,
                format!("Agent started: {agent}"),
                body,
                format!("spawned:{session}"),
                &ctx,
                Some(session_id),
                session_target("session", &ctx, session, Some(*seq).filter(|_| agent_session.is_none())),
            )
        }
        Trigger::Conflict { session_id, seq, text, files } => {
            let body = if text.trim().is_empty() { "Merging conflicted; the agent is resolving it.".to_owned() } else { text.trim().to_owned() };
            let mut target = session_target("conflict", ctx, session_id, Some(*seq));
            if let Some((path, line)) = files.first() {
                target["path"] = json!(path);
                target["line"] = json!(line);
            }
            target["files"] = json!(files.iter().map(|(path, line)| json!({"path": path, "line": line})).collect::<Vec<_>>());
            base(kinds::CONFLICT, format!("Merge conflict: {subject}"), body, format!("conflict:{session_id}"), ctx, Some(session_id), target)
        }
        Trigger::AgentBlocked { project_id, agent, task_id, session_id } => {
            let mut ctx = ctx.clone();
            ctx.agent = Some(agent.clone());
            ctx.project_id = ctx.project_id.clone().or_else(|| Some(project_id.clone()));
            let body = match &ctx.task_title {
                Some(task) => format!("Waiting for you on: {task}"),
                None => "Waiting for you.".to_owned(),
            };
            let mut target = json!({"open": "agent", "project_id": project_id, "agent": agent, "task_id": task_id});
            if let Some(session) = session_id {
                target["session_id"] = json!(session);
                if let Some(seq) = ctx.focus_seq {
                    target["seq"] = json!(seq);
                }
            }
            let mut n = base(kinds::BLOCKED, format!("Blocked: {agent}"), body, format!("blocked:{project_id}:{agent}"), &ctx, session_id.as_deref(), target);
            n.subtitle = join(&[ctx.project_name.as_deref(), ctx.task_title.as_deref()], " · ");
            n
        }
        Trigger::Swap { result } => {
            let verb = if result.rolled_back { "rolled back" } else { "failed" };
            let failed_step = result.steps.iter().find(|step| !step.ok);
            let body = result
                .error
                .clone()
                .filter(|e| !e.trim().is_empty())
                .or_else(|| failed_step.map(|step| if step.detail.is_empty() { format!("Step {} failed.", step.step) } else { step.detail.clone() }))
                .unwrap_or_else(|| format!("The swap was {verb}."));
            let versions = join(&[Some(result.from_version.as_str()), Some(result.to_version.as_str())], " → ");
            let key = format!("swap:{}:{}", result.category, result.to_version);
            Notification {
                kind: kinds::SWAP_FAILED.into(),
                title: format!("Plugin swap {verb}: {}", result.category),
                body,
                key: Some(key),
                project_id: None,
                session_id: None,
                target: json!({"open": "plugins", "category": result.category}),
                actions: Vec::new(),
                subtitle: versions.map(|v| format!("{} {v}", result.category)),
            }
        }
        Trigger::Crash { plugin_id, category, version, restarts } => Notification {
            kind: kinds::FAILED.into(),
            title: format!("{plugin_id} keeps stopping"),
            body: format!(
                "The {category} service stopped {restarts} times in a row right after starting; CortexCode keeps restarting it. Settings > Plugins has Restart and Roll back."
            ),
            key: Some(format!("crash:{category}")),
            project_id: None,
            session_id: None,
            target: json!({"open": "plugins", "plugin": plugin_id, "category": category, "version": version, "restarts": restarts}),
            actions: vec!["Open Plugins".into()],
            subtitle: Some(format!("{plugin_id} {}", version.split('+').next().unwrap_or(version))),
        },
        Trigger::Job { job_id, session_id, state, exit_code } => {
            let ran = ctx.job_started_ms.map(|started| ctx.now_ms.saturating_sub(started));
            // Only long jobs are worth a notification; the threshold is a user preference.
            match ran {
                Some(ms) if ms >= prefs.long_job_ms => {}
                _ => return None,
            }
            let after = ran.map(|ms| format!(" after {}", duration(ms))).unwrap_or_default();
            let title = match (state.as_str(), exit_code) {
                ("killed", _) => format!("Job killed{after}"),
                (_, Some(0)) => format!("Job finished{after}"),
                (_, Some(code)) => format!("Job failed (exit {code}){after}"),
                (_, None) => format!("Job ended{after}"),
            };
            let body = ctx.job_output_tail.as_deref().and_then(last_line).unwrap_or_else(|| format!("Background job {job_id}."));
            let target = json!({"open": "job", "project_id": ctx.project_id, "session_id": session_id, "job_id": job_id});
            base(kinds::JOB, title, body, format!("job:{job_id}"), ctx, session_id.as_deref(), target)
        }
    })
}

fn blocked_key(ctx: &Context, session_id: &str) -> String {
    match (&ctx.project_id, &ctx.agent) {
        (Some(project), Some(agent)) => format!("blocked:{project}:{agent}"),
        _ => format!("blocked:{session_id}"),
    }
}
