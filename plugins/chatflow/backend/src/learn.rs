//! Learning from finished runs (docs/LEARNING-REVIEW-2026-10-19.md): lessons for work in any
//! project, learned at the end of every run, kept at once, shown with the knowledge note after
//! ROUTE (`recall`), and scored by whether the error they are about came back.
//!
//! - **Episodes, cut by the host** from the run's journal, no model call: a refused tool call and
//!   the next call of that tool that worked; a command failing the same way twice and the
//!   different call that worked; a failing command that passed again after edits; an attempt
//!   that ended done and was reported wrong, fixed by this run; a steer. Each is a short card in
//!   host words (`learn_*` texts): never the conversation.
//! - **One LEARN call per episode** (`prompts/learn.md`, `schemas/learn.json`): useful or not, an
//!   anchor from the episode's own tools and languages, a kind, one sentence. It runs at idle
//!   priority (`model.chat` `idle_only`): only while the model serves nothing else, in a free slot,
//!   leaving nothing behind, so it never slows a turn or an agent.
//! - **Generality is the host's check**: a lesson naming this project (its folder, a file, a type,
//!   a code-shaped symbol) is not kept.
//! - **Stored globally** as `lesson` knowledge items whose name starts with the anchor. A lesson as
//!   similar as `limits.lesson_merge_score` to a stored one is that one learned again (`seen`).
//! - **Scored**: a lesson shown to the run whose error came back gets `hurt`; one whose error did
//!   not, in a run that ended done, gets `helped`; one that hurt more than it helped, after three
//!   such counts, is withdrawn.
//!
//! One worker learns run after run (a run is marked by a `learned` note, so a successor instance
//! skips it); after each it takes up earlier runs no instance learned or whose learning failed (the
//! backlog), newest sessions first, until none is left.

use crate::data::{Data, Dataset};
use crate::host::Host;
use crate::impact::scope_args;
use crate::reads::{self, Scope};
use crate::schemas;
use cc_protocol::RpcError;
use cc_protocol::contracts::chatflow::kinds as chatflow_kinds;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::{KnowledgeItem, KnowledgeSearchResult, knowledge, methods as memory_methods};
use cc_protocol::contracts::model::{ChatMessage, ChatResult, methods as model_methods};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The model conversation every LEARN call runs under (its slot is cleared after each call).
const CACHE_KEY: &str = "learn";
/// Placeholder of LEARN's `anchor` enum (the episode's tools, its languages, `approach`).
const ANCHORS_PLACEHOLDER: &str = "@anchors";
/// Characters of one failure's gist, of the edits shown, and of a message, in a card.
const GIST_CHARS: usize = 600;
const EDITS_CHARS: usize = 1500;
const MESSAGE_CHARS: usize = 900;

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn cut(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

enum Job {
    Run { session_id: String, run_id: String },
    Backlog,
}

/// The learning worker: runs are queued and learned one at a time.
#[derive(Clone)]
pub struct Learner {
    tx: mpsc::UnboundedSender<Job>,
}

impl Learner {
    /// Start the worker; `park` ends it (a call in flight is cancelled; its run stays marked).
    pub fn start(host: Host, data: Arc<Data>, park: CancellationToken) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let again = tx.clone();
        tokio::spawn(async move {
            loop {
                let job = tokio::select! {
                    job = rx.recv() => job,
                    _ = park.cancelled() => None,
                };
                let Some(job) = job else { return };
                let dataset = data.get();
                match job {
                    Job::Run { session_id, run_id } => {
                        if let Err(error) = learn_run(&host, &dataset, &session_id, &run_id, &park).await {
                            tracing::warn!(%error, session = %session_id, run = %run_id, "the run was not learned");
                        }
                    }
                    Job::Backlog => {}
                }
                if park.is_cancelled() {
                    continue;
                }
                match backlog(&host, &dataset, &park).await {
                    // More may wait: the next pass is queued behind any run that ends meanwhile.
                    Ok(learned) if learned > 0 => {
                        let _ = again.send(Job::Backlog);
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "earlier runs were not learned"),
                }
            }
        });
        Self { tx }
    }

    /// Learn this run when the worker gets to it.
    pub fn learn(&self, session_id: &str, run_id: &str) {
        let _ = self.tx.send(Job::Run { session_id: session_id.to_owned(), run_id: run_id.to_owned() });
    }

    /// Take up earlier runs no instance learned.
    pub fn backlog(&self) {
        let _ = self.tx.send(Job::Backlog);
    }
}

/// The lessons that fit a turn's work, best first: a search of the global lessons by `work`,
/// those anchored to a language the session does not use left out, ranked by the search score
/// with a small lift for a lesson that helped more than it hurt.
pub async fn recall(host: &Host, data: &Dataset, work: &str, languages: &[String]) -> Vec<KnowledgeItem> {
    let limit = data.limits.lessons_per_turn as usize;
    if limit == 0 || work.trim().is_empty() {
        return Vec::new();
    }
    let languages: HashSet<String> = languages.iter().map(|language| language.to_lowercase()).collect();
    let text = if languages.is_empty() { work.to_owned() } else { format!("{work} ({})", languages.iter().cloned().collect::<Vec<_>>().join(", ")) };
    let Ok(found) = host.get::<KnowledgeSearchResult>(memory_methods::KNOWLEDGE_SEARCH, json!({"text": text, "kind": knowledge::LESSON, "limit": limit * 3})).await else {
        return Vec::new();
    };
    let mut ranked: Vec<(f64, KnowledgeItem)> = found
        .items
        .into_iter()
        .filter(|item| match anchor_of(&item.name).strip_prefix("lang:") {
            Some(language) => languages.contains(&language.to_lowercase()),
            None => true,
        })
        .map(|item| {
            let lift = (item.helped as f64 - item.hurt as f64).clamp(-5.0, 5.0) * 0.03;
            (item.score.unwrap_or(0.0) + lift, item)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().take(limit).map(|(_, item)| item).collect()
}

/// A lesson's anchor: its name up to the first `": "`.
fn anchor_of(name: &str) -> &str {
    name.split(": ").next().unwrap_or("")
}

// ---- the run's steps ------------------------------------------------------------------------

/// One model tool call with its result.
struct Step {
    seq: u64,
    tool: String,
    args: Value,
    ok: bool,
    error_kind: String,
    output: String,
    changed: Vec<String>,
    diff: String,
}

impl Step {
    /// The call as the model made it: its stated intent, then what it asked for (a command, a
    /// path and range, each edit's operation, anchors and first line of text), so two calls can be
    /// told apart by what differs between them.
    fn call(&self, workspace: &str) -> String {
        let field = |name: &str| str_field(&self.args, name).to_owned();
        let intent = field("intent");
        let asked = match self.tool.as_str() {
            "run" => format!("run `{}`", field("command")),
            "read" => {
                let range = field("range");
                if range.is_empty() { format!("read {}", field("path")) } else { format!("read {} {range}", field("path")) }
            }
            "edit" => {
                let mut parts: Vec<String> = Vec::new();
                for edit in self.args.get("edits").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let text = str_field(&edit, "text");
                    let lines = text.lines().count();
                    let first = cut(text.lines().next().unwrap_or(""), 80);
                    let to = str_field(&edit, "to");
                    let span = if to.is_empty() { str_field(&edit, "at").to_owned() } else { format!("{} to {to}", str_field(&edit, "at")) };
                    parts.push(format!("{} at {span}, {lines} line(s) of text starting `{first}`", str_field(&edit, "op")));
                }
                if let Some(content) = self.args.get("content").and_then(Value::as_str).filter(|content| !content.is_empty()) {
                    parts.push(format!("the whole file as `content`, {} line(s)", content.lines().count()));
                }
                format!("edit {}: {}", field("path"), parts.join("; "))
            }
            _ => format!("{} {}", self.tool, self.args),
        };
        let text = if intent.is_empty() { asked } else { format!("{asked} (intent: {intent})") };
        cut(&local(&text, workspace), 500)
    }

    /// A refusal's text whole (the tool says what rule the call broke and what to send instead).
    fn refusal(&self, workspace: &str) -> String {
        let lines: Vec<&str> = self.output.lines().filter(|line| !line.trim().is_empty()).take(10).collect();
        cut(&local(&lines.join("\n"), workspace), 900)
    }

    /// The output's head and tail: where an error says what it is.
    fn gist(&self, workspace: &str) -> String {
        let lines: Vec<&str> = self.output.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()).collect();
        let mut picked: Vec<&str> = lines.iter().take(4).copied().collect();
        for line in lines.iter().skip(4).rev().take(2).rev() {
            if !picked.contains(line) {
                picked.push(line);
            }
        }
        let text: Vec<String> = picked.iter().map(|line| cut(line, 200)).collect();
        cut(&local(&text.join("\n"), workspace), GIST_CHARS)
    }

    /// The first line of the gist: what two failures must share to be the same failure.
    fn signature(&self, workspace: &str) -> String {
        self.gist(workspace).lines().next().unwrap_or("").to_owned()
    }

    fn command(&self) -> &str {
        str_field(&self.args, "command").trim()
    }
}

/// Paths under the workspace written relative to it.
fn local(text: &str, workspace: &str) -> String {
    if workspace.is_empty() {
        return text.to_owned();
    }
    text.replace(&format!("{}/", workspace.trim_end_matches('/')), "").replace(workspace, ".")
}

/// The run's model tool calls (host reads left out), each with its result, in order.
fn steps(entries: &[JournalEntry], run_id: &str) -> Vec<Step> {
    let mut results: HashMap<String, &JournalEntry> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry.kind == kinds::TOOL_RESULT && str_field(&entry.data, "run_id") == run_id) {
        results.insert(str_field(&entry.data, "call_id").to_owned(), entry);
    }
    // A result may carry no run id: match by call id across the journal.
    for entry in entries.iter().filter(|entry| entry.kind == kinds::TOOL_RESULT) {
        results.entry(str_field(&entry.data, "call_id").to_owned()).or_insert(entry);
    }
    entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_CALL && str_field(&entry.data, "run_id") == run_id && entry.data.get("host").and_then(Value::as_bool) != Some(true))
        .filter_map(|call| {
            let result = results.get(str_field(&call.data, "call_id"))?;
            let display = result.data.get("display").cloned().unwrap_or(Value::Null);
            Some(Step {
                seq: call.seq,
                tool: str_field(&call.data, "tool").to_owned(),
                args: call.data.get("args").cloned().unwrap_or(Value::Null),
                ok: result.data.get("ok").and_then(Value::as_bool).unwrap_or(false),
                error_kind: str_field(&result.data, "error_kind").to_owned(),
                output: str_field(&result.data, "output").to_owned(),
                changed: result.data.get("changed_files").and_then(Value::as_array).map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
                diff: str_field(&display, "diff").to_owned(),
            })
        })
        .collect()
}

/// The edits made between two steps, as their diffs (cut short), or the files they changed.
fn edits_between(steps: &[Step], from: usize, to: usize, workspace: &str) -> String {
    let mut out = String::new();
    for step in steps.iter().take(to).skip(from + 1).filter(|step| step.tool == "edit" && step.ok) {
        let piece = if step.diff.is_empty() { format!("edited {}", step.changed.join(", ")) } else { step.diff.clone() };
        out.push_str(&local(&piece, workspace));
        out.push('\n');
        if out.chars().count() > EDITS_CHARS {
            break;
        }
    }
    cut(&out, EDITS_CHARS)
}

/// One episode: its card body and the tools it involves.
struct Episode {
    body: String,
    tools: Vec<String>,
    evidence: u64,
}

/// The run's episodes, the ones about reported failures and the user's words first, at most
/// `max`.
fn episodes(data: &Dataset, entries: &[JournalEntry], run_id: &str, workspace: &str, max: usize) -> Vec<Episode> {
    let steps = steps(entries, run_id);
    let mut out: Vec<Episode> = Vec::new();
    let run: Vec<&JournalEntry> = entries.iter().filter(|entry| str_field(&entry.data, "run_id") == run_id).collect();

    // An attempt that ended done, reported wrong by the message that started this run, fixed by
    // this run (it ended done after edits).
    let first_message = run.iter().find(|entry| entry.kind == kinds::USER_MESSAGE && entry.data.get("from_steer").is_none());
    if let Some(message) = first_message
        && ended_done(entries, run_id)
        && earlier_attempt_done(entries, message.seq)
        && steps.iter().any(|step| step.tool == "edit" && step.ok)
    {
        let found = run
            .iter()
            .rev()
            .find(|entry| entry.kind == kinds::ASSISTANT_MESSAGE && entry.data.get("streaming").and_then(Value::as_bool) != Some(true) && !str_field(&entry.data, "text").trim().is_empty())
            .map(|entry| cut(str_field(&entry.data, "text"), MESSAGE_CHARS))
            .unwrap_or_default();
        let body = data.text(
            "learn_retry",
            &[
                ("message", &cut(&local(str_field(&message.data, "text"), workspace), MESSAGE_CHARS)),
                ("found", &local(&found, workspace)),
                ("edits", &edits_between(&steps, 0, steps.len(), workspace)),
            ],
        );
        out.push(Episode { body, tools: vec!["edit".into()], evidence: message.seq });
    }

    // A steer: what the user said while the work ran, between the steps around it.
    for steer in run.iter().filter(|entry| entry.kind == kinds::USER_MESSAGE && entry.data.get("from_steer").is_some()) {
        let before: Vec<String> = steps.iter().filter(|step| step.seq < steer.seq).rev().take(2).map(|step| step.call(workspace)).collect();
        let after: Vec<String> = steps.iter().filter(|step| step.seq > steer.seq).take(2).map(|step| step.call(workspace)).collect();
        let body = data.text(
            "learn_steer",
            &[("message", &cut(str_field(&steer.data, "text"), MESSAGE_CHARS)), ("before", &before.join("; ")), ("after", &after.join("; "))],
        );
        out.push(Episode { body, tools: Vec::new(), evidence: steer.seq });
    }

    // A refused call and the next call of the same tool that worked, once per tool and refusal.
    let mut seen_refusals: HashSet<(String, String)> = HashSet::new();
    for (index, step) in steps.iter().enumerate() {
        if step.ok || step.error_kind.is_empty() || step.error_kind == "nonzero_exit" || !seen_refusals.insert((step.tool.clone(), step.error_kind.clone())) {
            continue;
        }
        // The call that did what the refused one meant: the same tool on the same path when there is
        // one (an edit sent again), else the next call of the tool that worked.
        let path = str_field(&step.args, "path");
        let next: Vec<&Step> = steps.iter().skip(index + 1).take(8).filter(|next| next.tool == step.tool && next.ok).collect();
        if let Some(fix) = next.iter().find(|next| !path.is_empty() && str_field(&next.args, "path") == path).or(next.first()) {
            let body = data.text(
                "learn_refused",
                &[("tool", &step.tool), ("error", &step.refusal(workspace)), ("call", &step.call(workspace)), ("fix", &fix.call(workspace))],
            );
            out.push(Episode { body, tools: vec![step.tool.clone()], evidence: step.seq });
        }
    }

    // A command that failed the same way twice, then a different call that worked; and a command
    // that failed and passed again after edits. Once per command.
    let mut seen_commands: HashSet<String> = HashSet::new();
    for (index, step) in steps.iter().enumerate() {
        if step.tool != "run" || step.ok || step.command().is_empty() || seen_commands.contains(step.command()) {
            continue;
        }
        let signature = step.signature(workspace);
        let same: Vec<usize> = steps
            .iter()
            .enumerate()
            .skip(index)
            .filter(|(_, other)| other.tool == "run" && !other.ok && other.command() == step.command() && other.signature(workspace) == signature)
            .map(|(at, _)| at)
            .collect();
        let passed_again = steps.iter().enumerate().skip(index + 1).find(|(_, other)| other.tool == "run" && other.ok && other.command() == step.command()).map(|(at, _)| at);
        if same.len() >= 2 {
            let last = *same.last().unwrap_or(&index);
            if let Some(fix) = steps.iter().skip(last + 1).find(|other| other.tool == "run" && other.ok && other.command() != step.command()) {
                seen_commands.insert(step.command().to_owned());
                let body = data.text(
                    "learn_repeated",
                    &[("times", &same.len().to_string()), ("command", &step.call(workspace)), ("error", &step.gist(workspace)), ("fix", &fix.call(workspace))],
                );
                out.push(Episode { body, tools: vec!["run".into()], evidence: step.seq });
                continue;
            }
        }
        if let Some(at) = passed_again {
            let edits = edits_between(&steps, index, at, workspace);
            if !edits.is_empty() {
                seen_commands.insert(step.command().to_owned());
                let body = data.text("learn_red_green", &[("command", &step.call(workspace)), ("error", &step.gist(workspace)), ("edits", &edits)]);
                out.push(Episode { body, tools: vec!["run".into(), "edit".into()], evidence: step.seq });
            }
        }
    }
    out.truncate(max);
    out
}

/// The run's last status: its state (`done`, `failed`, ...), empty while it runs.
fn final_state(entries: &[JournalEntry], run_id: &str) -> String {
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == kinds::STATUS && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "state") != "running")
        .map(|entry| str_field(&entry.data, "state").to_owned())
        .unwrap_or_default()
}

fn ended_done(entries: &[JournalEntry], run_id: &str) -> bool {
    final_state(entries, run_id) == "done"
}

/// Whether the run before `seq` in the journal did work (an `act_done` note) and ended done.
fn earlier_attempt_done(entries: &[JournalEntry], seq: u64) -> bool {
    let Some(previous) = entries.iter().rev().find(|entry| entry.seq < seq && entry.kind == kinds::STATUS && !str_field(&entry.data, "run_id").is_empty() && str_field(&entry.data, "state") != "running") else {
        return false;
    };
    let run_id = str_field(&previous.data, "run_id");
    str_field(&previous.data, "state") == "done" && entries.iter().any(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "act_done")
}

// ---- generality -------------------------------------------------------------------------------

/// The names that make a sentence about this project: its folder, the names of the files the run
/// read or changed, and those files' types and code-shaped symbol names (with `_`, an inner capital
/// or a digit) from the code graph. Lowercase; names shorter than four characters left out.
async fn project_names(host: &Host, scope: &Scope, steps: &[Step]) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();
    let mut add = |name: &str| {
        let name = name.trim().to_lowercase();
        if name.chars().count() >= 4 {
            names.insert(name);
        }
    };
    for root in [&scope.root, &scope.workspace] {
        if let Some(name) = std::path::Path::new(root).file_name().and_then(|name| name.to_str()) {
            add(name);
            for part in name.split(['-', '_', '.']) {
                add(part);
            }
        }
    }
    // The project's own files: not a URI, not outside the workspace, not the scratch folder
    // (`tmp/`, where probes go and which the host clears after the turn).
    let own = |path: &str| !path.is_empty() && !path.contains("://") && !path.starts_with('/') && !path.starts_with("tmp/");
    let paths: Vec<String> = steps.iter().flat_map(|step| step.changed.iter().cloned().chain(std::iter::once(str_field(&step.args, "path").to_owned()))).filter(|path| own(path)).collect();
    for path in &paths {
        if let Some(stem) = std::path::Path::new(path).file_stem().and_then(|stem| stem.to_str()) {
            add(stem);
        }
    }
    let touched: Vec<String> = paths.iter().cloned().collect::<HashSet<_>>().into_iter().take(8).collect();
    for path in touched {
        let Ok(outline) = host.call(memory_methods::OUTLINE, scope_args(scope, json!({"path": path}))).await else { continue };
        for symbol in outline.get("symbols").and_then(Value::as_array).cloned().unwrap_or_default() {
            let name = str_field(&symbol, "name");
            let kind = str_field(&symbol, "kind");
            if matches!(kind, "class" | "struct" | "enum" | "trait" | "interface" | "type" | "module") || code_shaped(name) {
                add(name);
            }
        }
    }
    names
}

/// A word written like code: with `_`, a capital after the first letter, or a digit.
fn code_shaped(word: &str) -> bool {
    word.contains('_') || word.chars().skip(1).any(char::is_uppercase) || word.chars().any(|c| c.is_ascii_digit())
}

/// The first word of `lesson` that names this project, if any.
fn names_project<'a>(lesson: &'a str, names: &HashSet<String>) -> Option<&'a str> {
    lesson.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|word| !word.is_empty()).find(|word| names.contains(&word.to_lowercase()))
}

// ---- learning a run -------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct Learned {
    useful: String,
    #[serde(default)]
    anchor: String,
    #[serde(default)]
    lesson: String,
}

/// The languages of the files the run read or changed (by the language plugins' extensions and
/// file names), else the session's.
async fn run_languages(host: &Host, entries: &[JournalEntry], steps: &[Step]) -> Vec<String> {
    let declared = reads::declared_languages(host).await;
    let mut found: Vec<String> = Vec::new();
    for path in steps.iter().flat_map(|step| step.changed.iter().cloned().chain(std::iter::once(str_field(&step.args, "path").to_owned()))) {
        let file = std::path::Path::new(&path);
        let name = file.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let extension = file.extension().and_then(|extension| extension.to_str()).unwrap_or("");
        if let Some(language) = declared.iter().find(|language| language.filenames.iter().any(|known| known == name) || (!extension.is_empty() && language.extensions.iter().any(|known| known == extension)))
            && !found.contains(&language.name)
        {
            found.push(language.name.clone());
        }
    }
    if found.is_empty() {
        let names: HashSet<&str> = declared.iter().map(|language| language.name.as_str()).collect();
        if let Some(context) = entries.iter().rev().find(|entry| entry.kind == crate::journal::extra_kinds::CONTEXT) {
            for language in context.data.get("languages").and_then(Value::as_array).cloned().unwrap_or_default() {
                if let Some(language) = language.as_str()
                    && names.contains(language)
                    && !found.iter().any(|known| known == language)
                {
                    found.push(language.to_owned());
                }
            }
        }
    }
    found
}

/// One LEARN call: the episode's lesson with its anchor and the model's raw answer, or None (not
/// useful, unreadable). `after` continues the exchange (the answer before, the host's note on it).
async fn distill(host: &Host, data: &Dataset, card: &str, after: &[ChatMessage], anchors: &[String], cancel: &CancellationToken) -> Result<Option<(String, String, String)>, RpcError> {
    let Some(shape) = data.shape("LEARN") else { return Ok(None) };
    let shape = shape.filled(ANCHORS_PLACEHOLDER, anchors, None);
    let mut messages = vec![ChatMessage::new("system", data.prompt("learn").to_owned()), ChatMessage::new("user", card.to_owned())];
    messages.extend(after.iter().cloned());
    let params = json!({
        "messages": messages,
        "tools": [],
        "grammar": schemas::grammar(&shape),
        "sampling": data.sampling("LEARN", false),
        "thinking": crate::llm::thinking("none"),
        "priority": "background",
        "idle_only": true,
        "cache_key": CACHE_KEY,
        "max_tokens": 400,
    });
    let value = host.call_with(model_methods::CHAT, params, crate::host::Effect::Idempotent, Some(cancel)).await?;
    let result: ChatResult = cc_protocol::parse_params(value)?;
    let Ok(learned) = schemas::parse_json::<Learned>(&result.content) else { return Ok(None) };
    let lesson = learned.lesson.split_whitespace().collect::<Vec<_>>().join(" ");
    if learned.useful != "yes" || lesson.is_empty() || !anchors.contains(&learned.anchor) {
        return Ok(None);
    }
    Ok(Some((learned.anchor, lesson, result.content)))
}

/// Learn one finished run (once: its `learned` note marks it; an attempt that failed flags its
/// note `failed`, and the run is taken up again later).
async fn learn_run(host: &Host, data: &Dataset, session_id: &str, run_id: &str, cancel: &CancellationToken) -> Result<usize, RpcError> {
    let entries = host.journal_read(session_id, 0).await?;
    if learned(&entries, run_id) || final_state(&entries, run_id).is_empty() {
        return Ok(0);
    }
    let mark = host.journal_append(session_id, kinds::NOTE, json!({"text": "", "purpose": "learned", "run_id": run_id, "host_message": false})).await?;
    let learnt = learn_marked(host, data, session_id, run_id, &entries, mark, cancel).await;
    if learnt.is_err() || cancel.is_cancelled() {
        let _ = host.journal_set(session_id, mark, "failed", json!(true)).await;
    }
    learnt
}

async fn learn_marked(host: &Host, data: &Dataset, session_id: &str, run_id: &str, entries: &[JournalEntry], mark: u64, cancel: &CancellationToken) -> Result<usize, RpcError> {
    let entries = entries.to_vec();
    let session = host.session_get(session_id).await?;
    let root = host.project_get(&session.project_id).await.map(|project| project.root).unwrap_or_else(|_| session.workspace.clone());
    let scope = Scope { project_id: session.project_id.clone(), workspace: session.workspace.clone(), root };
    let workspace = session.workspace.trim_end_matches('/').to_owned();
    let found = episodes(data, &entries, run_id, &workspace, data.limits.learn_episodes_per_run);
    tracing::info!(session = %session_id, run = %run_id, episodes = found.len(), "learning a run");
    let done = ended_done(&entries, run_id);
    let shown: Vec<String> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "knowledge")
        .flat_map(|entry| entry.data.get("lessons").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect();
    let mut new = 0usize;
    let mut came_back: HashSet<String> = HashSet::new();
    if !found.is_empty() {
        let run_steps = steps(&entries, run_id);
        let languages = run_languages(host, &entries, &run_steps).await;
        let names = project_names(host, &scope, &run_steps).await;
        let outcome = final_state(&entries, run_id);
        for episode in &found {
            if cancel.is_cancelled() {
                break;
            }
            let mut anchors: Vec<String> = episode.tools.iter().map(|tool| format!("tool:{tool}")).collect();
            anchors.extend(languages.iter().map(|language| format!("lang:{language}")));
            anchors.push("approach".to_owned());
            let card = data.text("learn_card", &[("languages", &if languages.is_empty() { "unknown".to_owned() } else { languages.join(", ") }), ("outcome", &outcome), ("body", &episode.body)]);
            let Some((mut anchor, mut lesson, raw)) = distill(host, data, &card, &[], &anchors, cancel).await? else {
                tracing::info!(episode = %cut(&episode.body, 160), "an episode taught nothing");
                continue;
            };
            // A lesson that names the project goes back once, to be written for any project.
            if let Some(word) = names_project(&lesson, &names).map(str::to_owned) {
                let note = data.text("learn_rename", &[("word", &word)]);
                let after = [ChatMessage::new("assistant", raw), ChatMessage::new("user", note)];
                match distill(host, data, &card, &after, &anchors, cancel).await? {
                    Some((again_anchor, again, _)) if names_project(&again, &names).is_none() => {
                        tracing::info!(before = %lesson, after = %again, "a lesson naming the project was written again");
                        (anchor, lesson) = (again_anchor, again);
                    }
                    _ => {
                        tracing::info!(%lesson, %word, "a lesson naming the project was not kept");
                        continue;
                    }
                }
            }
            let evidence = format!("journal://{session_id}/{}", episode.evidence);
            match store(host, data, &anchor, &lesson, session_id, &evidence).await {
                Ok((id, true)) => {
                    tracing::info!(%anchor, %lesson, "learned a lesson");
                    new += 1;
                    came_back.insert(id);
                }
                Ok((id, false)) => {
                    came_back.insert(id);
                }
                Err(error) => tracing::warn!(%error, "a lesson was not stored"),
            }
        }
    }
    score(host, &shown, &came_back, done).await;
    let kept = entries.iter().filter(|entry| entry.kind == chatflow_kinds::KNOWLEDGE && str_field(&entry.data, "run_id") == run_id && !str_field(&entry.data, "knowledge_id").is_empty()).count();
    let count = new + kept;
    if count > 0 {
        let text = data.text("learned_note", &[("count", &count.to_string()), ("items", if count == 1 { "item" } else { "items" })]);
        host.journal_set(session_id, mark, "text", json!(text)).await?;
        host.journal_set(session_id, mark, "count", json!(count)).await?;
    }
    Ok(count)
}

fn learned(entries: &[JournalEntry], run_id: &str) -> bool {
    entries.iter().any(|entry| {
        entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "purpose") == "learned" && entry.data.get("failed").and_then(Value::as_bool) != Some(true)
    })
}

/// Keep a lesson: the stored lesson it repeats learned again, else a new one. Returns its id and
/// whether it is new.
async fn store(host: &Host, data: &Dataset, anchor: &str, lesson: &str, by: &str, evidence: &str) -> Result<(String, bool), RpcError> {
    let similar = host.get::<KnowledgeSearchResult>(memory_methods::KNOWLEDGE_SEARCH, json!({"text": lesson, "kind": knowledge::LESSON, "limit": 3})).await?;
    if let Some(existing) = similar.items.iter().find(|item| item.score.unwrap_or(0.0) >= data.limits.lesson_merge_score && anchor_of(&item.name) == anchor) {
        host.call(memory_methods::KNOWLEDGE_UPSERT, json!({"kind": knowledge::LESSON, "name": existing.name, "text": "", "by": by, "evidence": [evidence]})).await?;
        host.call(memory_methods::KNOWLEDGE_COUNT, json!({"id": existing.id, "seen": 1})).await?;
        return Ok((existing.id.clone(), false));
    }
    let head: String = lesson.split(',').next().unwrap_or(lesson).chars().take(80).collect();
    let stored = host
        .call(
            memory_methods::KNOWLEDGE_UPSERT,
            json!({"kind": knowledge::LESSON, "name": format!("{anchor}: {}", head.trim()), "text": lesson, "source": "agent", "confidence": 0.5, "by": by, "evidence": [evidence]}),
        )
        .await?;
    let id = str_field(&stored, "id").to_owned();
    // A lesson with the same anchor and opening words is the same item (its id): learned again.
    let again = stored.get("seen").and_then(Value::as_u64).unwrap_or(0) > 0;
    host.call(memory_methods::KNOWLEDGE_COUNT, json!({"id": id, "seen": 1})).await?;
    Ok((id, !again))
}

/// Score the lessons shown to a run: the error came back (`hurt`), or it did not in a run that
/// ended done (`helped`). One that hurt more than it helped after three counts is withdrawn.
async fn score(host: &Host, shown: &[String], came_back: &HashSet<String>, done: bool) {
    for id in shown.iter().collect::<HashSet<_>>() {
        let (helped, hurt) = if came_back.contains(id) { (0, 1) } else if done { (1, 0) } else { continue };
        let counted = match host.get::<KnowledgeItem>(memory_methods::KNOWLEDGE_COUNT, json!({"id": id, "helped": helped, "hurt": hurt})).await {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!(%error, lesson = %id, "a lesson's score was not kept");
                continue;
            }
        };
        if counted.hurt > counted.helped && counted.hurt + counted.helped >= 3 {
            let _ = host.call(memory_methods::KNOWLEDGE_WITHDRAW, json!({"id": id, "reason": "shown, and the error it is about came back", "by": "chatflow"})).await;
        }
    }
}

/// Earlier runs no instance learned, newest sessions first, at most `limits.learn_backlog_runs`.
/// Returns how many runs were taken up.
async fn backlog(host: &Host, data: &Dataset, cancel: &CancellationToken) -> Result<usize, RpcError> {
    let max = data.limits.learn_backlog_runs;
    if max == 0 {
        return Ok(0);
    }
    let mut sessions = host.session_list(None).await?;
    sessions.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    let mut taken = 0;
    for session in sessions {
        if taken >= max || cancel.is_cancelled() {
            break;
        }
        let Ok(entries) = host.journal_read(&session.id, 0).await else { continue };
        let mut runs: Vec<String> = Vec::new();
        for entry in entries.iter().rev().filter(|entry| entry.kind == kinds::STATUS && str_field(&entry.data, "state") != "running") {
            let run_id = str_field(&entry.data, "run_id");
            if !run_id.is_empty() && !runs.iter().any(|known| known == run_id) && !learned(&entries, run_id) {
                runs.push(run_id.to_owned());
            }
        }
        for run_id in runs {
            if taken >= max || cancel.is_cancelled() {
                break;
            }
            taken += 1;
            if let Err(error) = learn_run(host, data, &session.id, &run_id, cancel).await {
                tracing::warn!(%error, session = %session.id, run = %run_id, "an earlier run was not learned");
            }
        }
    }
    Ok(taken)
}
