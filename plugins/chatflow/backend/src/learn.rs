//! Learning from finished runs (docs/LEARNING-REVIEW-2026-10-19.md): lessons for work in any
//! project, learned at the end of every run, kept at once, shown with the knowledge note after
//! ROUTE (`recall`) and right under a failure they were learned from (`at_failure`), and scored by
//! what the run shows about them.
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
//!   a code-shaped symbol) is about this project: it is kept as a project `fact`, labelled with its
//!   anchor, and the knowledge note brings it to this project's turns.
//! - **Stored globally** as `lesson` knowledge items whose name starts with the anchor. A lesson as
//!   similar as `limits.lesson_merge_score` to a stored one is that one learned again (`seen`).
//! - **Found again by its failure**: a lesson from a refusal or a failing command carries that
//!   failure's key as an alias (`failure_key`: the tool and the refusal's kind, or the shape of the
//!   first error line). When a failure with that key comes in a later run, the lesson is the note
//!   right under it, where the next step is decided.
//! - **Scored by verdicts**: `hurt` when its error came back (it was learned again); `helped` only
//!   when the run ended done and the lesson can be credited: shown under its failure, which did not
//!   come again, or the only lesson shown whose anchor the run exercised (its tool used, a file of
//!   its language touched); `confounded` when several such lessons share the credit; `no observed
//!   effect` otherwise. Only `helped` and `hurt` count; one that hurt more than it helped, after
//!   three counts, is withdrawn. The verdicts are kept on the run's `learned` note.
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

/// The learning worker: runs are queued and learned one at a time. With nothing left to learn it
/// writes the day's night report (`night.rs`) and, while no run is going, replays one lesson
/// (`replay.rs`).
#[derive(Clone)]
pub struct Learner {
    tx: mpsc::UnboundedSender<Job>,
}

impl Learner {
    /// Start the worker; `park` ends it (a call in flight is cancelled; its run stays marked).
    pub fn start(host: Host, data: Arc<Data>, park: CancellationToken, paths: crate::replay::Paths, replayer: Arc<dyn crate::replay::Replayer>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let again = tx.clone();
        tokio::spawn(async move {
            // The kind (INV or DOM) of the lesson replayed last: replays take turns.
            let mut last_dom: Option<bool> = None;
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
                    Ok(_) => {
                        crate::night::report_if_due(&host, &paths).await;
                        if replayer.idle()
                            && let Some(dom) = crate::replay::replay_one(&host, &paths, replayer.as_ref(), last_dom).await
                        {
                            last_dom = Some(dom);
                            let _ = again.send(Job::Backlog);
                        }
                    }
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

/// The key a failure is known by in any project: a refusal by its tool and kind (`refused edit
/// stale_anchor`); a failing command by the shape of its first error line (`failed error[E0382]:
/// borrow of moved value: `_``). None for a call that did not fail, or a failure with no error line.
pub fn failure_key(tool: &str, error_kind: &str, output: &str) -> Option<String> {
    if !error_kind.is_empty() && error_kind != "nonzero_exit" {
        return Some(format!("refused {tool} {error_kind}"));
    }
    // A shape of fewer than three words (`AssertionError: N != N`) is any failure of its kind.
    crate::typeerrors::error_shape(output).filter(|shape| shape.split(|c: char| !c.is_alphabetic()).filter(|word| word.len() > 1 && *word != "N").count() >= 3).map(|shape| format!("failed {shape}"))
}

/// Two keys alike as the memory plugin compares names and aliases: letters and digits, lowercase,
/// any run of other characters one separator.
fn same_key(a: &str, b: &str) -> bool {
    let squash = |text: &str| -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c.is_alphanumeric() {
                out.extend(c.to_lowercase());
            } else if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_end_matches('-').to_owned()
    };
    let (a, b) = (squash(a), squash(b));
    !a.is_empty() && a == b
}

/// What was learned from a failure with this `key`: this project's facts first, then the global
/// lessons, each with whether it is the project's; none the run was shown under a failure already
/// (`shown`), lessons anchored to a language the session does not use left out, the most helpful
/// first, at most `limits.lessons_per_failure`.
pub async fn at_failure(host: &Host, data: &Dataset, project_id: &str, key: &str, languages: &[String], shown: &HashSet<String>) -> Vec<(KnowledgeItem, bool)> {
    let limit = data.limits.lessons_per_failure as usize;
    if limit == 0 || key.trim().is_empty() {
        return Vec::new();
    }
    let languages: HashSet<String> = languages.iter().map(|language| language.to_lowercase()).collect();
    let mut out: Vec<(KnowledgeItem, bool)> = Vec::new();
    for (project, kind) in [(Some(project_id), knowledge::FACT), (None, knowledge::LESSON)] {
        let Ok(found) = host.get::<KnowledgeSearchResult>(memory_methods::KNOWLEDGE_SEARCH, json!({"project_id": project, "text": key, "kind": kind, "limit": limit + 4})).await else { continue };
        let mut items: Vec<KnowledgeItem> = found
            .items
            .into_iter()
            .filter(|item| item.aliases.iter().any(|alias| same_key(alias, key)) && !shown.contains(&item.id))
            .filter(|item| match anchor_of(&item.name).strip_prefix("lang:") {
                Some(language) => languages.contains(&language.to_lowercase()),
                None => true,
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.helped as i64 - item.hurt as i64));
        out.extend(items.into_iter().map(|item| (item, project.is_some())));
    }
    out.truncate(limit);
    out
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

/// One episode: its card body, the tools it involves, and the key of the failure it is about.
struct Episode {
    body: String,
    tools: Vec<String>,
    evidence: u64,
    key: Option<String>,
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
        out.push(Episode { body, tools: vec!["edit".into()], evidence: message.seq, key: None });
    }

    // A steer: what the user said while the work ran, between the steps around it.
    for steer in run.iter().filter(|entry| entry.kind == kinds::USER_MESSAGE && entry.data.get("from_steer").is_some()) {
        let before: Vec<String> = steps.iter().filter(|step| step.seq < steer.seq).rev().take(2).map(|step| step.call(workspace)).collect();
        let after: Vec<String> = steps.iter().filter(|step| step.seq > steer.seq).take(2).map(|step| step.call(workspace)).collect();
        let body = data.text(
            "learn_steer",
            &[("message", &cut(str_field(&steer.data, "text"), MESSAGE_CHARS)), ("before", &before.join("; ")), ("after", &after.join("; "))],
        );
        out.push(Episode { body, tools: Vec::new(), evidence: steer.seq, key: None });
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
            out.push(Episode { body, tools: vec![step.tool.clone()], evidence: step.seq, key: failure_key(&step.tool, &step.error_kind, "") });
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
                out.push(Episode { body, tools: vec!["run".into()], evidence: step.seq, key: failure_key("run", "", &step.output) });
                continue;
            }
        }
        if let Some(at) = passed_again {
            let edits = edits_between(&steps, index, at, workspace);
            if !edits.is_empty() {
                seen_commands.insert(step.command().to_owned());
                let body = data.text("learn_red_green", &[("command", &step.call(workspace)), ("error", &step.gist(workspace)), ("edits", &edits)]);
                out.push(Episode { body, tools: vec!["run".into(), "edit".into()], evidence: step.seq, key: failure_key("run", "", &step.output) });
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

/// One LEARN call: the episode's lesson with its anchor, or None (not useful, unreadable).
async fn distill(host: &Host, data: &Dataset, card: &str, anchors: &[String], cancel: &CancellationToken) -> Result<Option<(String, String)>, RpcError> {
    let Some(shape) = data.shape("LEARN") else { return Ok(None) };
    let shape = shape.filled(ANCHORS_PLACEHOLDER, anchors, None);
    let messages = vec![ChatMessage::new("system", data.prompt("learn").to_owned()), ChatMessage::new("user", card.to_owned())];
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
    Ok(Some((learned.anchor, lesson)))
}

/// Learn one finished run (once: its `learned` note marks it; an attempt that failed flags its
/// note `failed`, and the run is taken up again later).
async fn learn_run(host: &Host, data: &Dataset, session_id: &str, run_id: &str, cancel: &CancellationToken) -> Result<usize, RpcError> {
    let entries = host.journal_read(session_id, 0).await?;
    if learned(&entries, run_id) || final_state(&entries, run_id).is_empty() {
        return Ok(0);
    }
    let mark = host.journal_append(session_id, kinds::NOTE, json!({"text": "", "purpose": "learned", "run_id": run_id, "host_message": false})).await?;
    // A replay's run is evidence about a lesson, not a run to learn from.
    if crate::replay::arm(&entries).is_some() {
        return Ok(0);
    }
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
    let run_steps = steps(&entries, run_id);
    let languages = run_languages(host, &entries, &run_steps).await;
    let mut new = 0usize;
    let mut came_back: HashSet<String> = HashSet::new();
    if !found.is_empty() {
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
            let Some((anchor, lesson)) = distill(host, data, &card, &anchors, cancel).await? else {
                tracing::info!(episode = %cut(&episode.body, 160), "an episode taught nothing");
                continue;
            };
            let evidence = format!("journal://{session_id}/{}", episode.evidence);
            // A lesson that names the project is about this project: a fact of its own.
            let project = names_project(&lesson, &names).map(|word| {
                tracing::info!(%lesson, %word, "a lesson naming the project is kept as a project fact");
                scope.project_id.as_str()
            });
            match store(host, data, project, &anchor, &lesson, episode.key.as_deref(), session_id, &evidence).await {
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
    let verdicts = score(host, &entries, run_id, &run_steps, &languages, &scope.project_id, &came_back, done).await;
    if !verdicts.is_empty() {
        host.journal_set(session_id, mark, "verdicts", json!(verdicts)).await?;
    }
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

/// Keep a lesson: the stored one it repeats learned again, else a new one; global, or with
/// `project` a fact of that project. The failure's `key` goes with it as an alias, so the failure
/// finds it when it comes again. Returns its id and whether it is new.
#[allow(clippy::too_many_arguments)]
async fn store(host: &Host, data: &Dataset, project: Option<&str>, anchor: &str, lesson: &str, key: Option<&str>, by: &str, evidence: &str) -> Result<(String, bool), RpcError> {
    let kind = if project.is_some() { knowledge::FACT } else { knowledge::LESSON };
    let aliases: Vec<&str> = key.into_iter().collect();
    let similar = host.get::<KnowledgeSearchResult>(memory_methods::KNOWLEDGE_SEARCH, json!({"project_id": project, "text": lesson, "kind": kind, "limit": 3})).await?;
    if let Some(existing) = similar.items.iter().find(|item| item.score.unwrap_or(0.0) >= data.limits.lesson_merge_score && anchor_of(&item.name) == anchor) {
        host.call(memory_methods::KNOWLEDGE_UPSERT, json!({"project_id": project, "kind": kind, "name": existing.name, "text": "", "by": by, "evidence": [evidence], "aliases": aliases})).await?;
        host.call(memory_methods::KNOWLEDGE_COUNT, json!({"project_id": project, "id": existing.id, "seen": 1})).await?;
        return Ok((existing.id.clone(), false));
    }
    let head: String = lesson.split(',').next().unwrap_or(lesson).chars().take(80).collect();
    let stored = host
        .call(
            memory_methods::KNOWLEDGE_UPSERT,
            json!({"project_id": project, "kind": kind, "name": format!("{anchor}: {}", head.trim()), "text": lesson, "source": "agent", "confidence": 0.5, "by": by, "evidence": [evidence], "aliases": aliases}),
        )
        .await?;
    let id = str_field(&stored, "id").to_owned();
    // An item with the same anchor and opening words is the same item (its id): learned again.
    let again = stored.get("seen").and_then(Value::as_u64).unwrap_or(0) > 0;
    host.call(memory_methods::KNOWLEDGE_COUNT, json!({"project_id": project, "id": id, "seen": 1})).await?;
    Ok((id, !again))
}

/// Where a run was shown an item: with the knowledge note after ROUTE, or under a failure (with
/// the failure's key and the note's place in the journal).
struct Shown {
    id: String,
    project: bool,
    failure: Option<(String, u64)>,
}

/// The items a run was shown: the knowledge notes' `lessons`, the failure notes' `lessons` and
/// `facts`, a fresh start's `learned`.
fn shown(entries: &[JournalEntry], run_id: &str) -> Vec<Shown> {
    let mut out: Vec<Shown> = Vec::new();
    let ids = |entry: &JournalEntry, field: &str| -> Vec<String> { entry.data.get(field).and_then(Value::as_array).into_iter().flatten().filter_map(|id| id.as_str().map(str::to_owned)).collect() };
    for entry in entries.iter().filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "run_id") == run_id) {
        match str_field(&entry.data, "purpose") {
            "knowledge" => out.extend(ids(entry, "lessons").into_iter().map(|id| Shown { id, project: false, failure: None })),
            "lesson" => {
                let key = str_field(&entry.data, "key").to_owned();
                out.extend(ids(entry, "lessons").into_iter().map(|id| Shown { id, project: false, failure: Some((key.clone(), entry.seq)) }));
                out.extend(ids(entry, "facts").into_iter().map(|id| Shown { id, project: true, failure: Some((key.clone(), entry.seq)) }));
            }
            // A stuck task's fresh start: each item with the failure that found it.
            "attempt_failed" => {
                for learned in entry.data.get("learned").and_then(Value::as_array).into_iter().flatten() {
                    out.push(Shown {
                        id: str_field(learned, "id").to_owned(),
                        project: learned.get("project").and_then(Value::as_bool).unwrap_or(false),
                        failure: Some((str_field(learned, "key").to_owned(), entry.seq)),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// What a run shows about one item it was shown.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Helped,
    Hurt,
    NoObservedEffect,
    Confounded,
}

/// Whether the run exercised an anchor: used the tool (`tool:<name>`), touched a file of the
/// language (`lang:<id>`). An `approach` is not seen in the steps.
fn exercised(anchor: &str, steps: &[Step], languages: &[String]) -> bool {
    if let Some(tool) = anchor.strip_prefix("tool:") {
        return steps.iter().any(|step| step.tool == tool);
    }
    if let Some(language) = anchor.strip_prefix("lang:") {
        return languages.iter().any(|known| known.eq_ignore_ascii_case(language));
    }
    false
}

/// Judge and count the items shown to a run (see the module's doc): `hurt` when the run learned
/// it again; else, in a run that ended done, `helped` for one shown under a failure that did not
/// come again, and for the only one shown with the knowledge note whose anchor the run exercised
/// (`confounded` when several share that); `no observed effect` for the rest. One that hurt more
/// than it helped after three counts is withdrawn. Returns the verdicts by item id.
#[allow(clippy::too_many_arguments)]
async fn score(host: &Host, entries: &[JournalEntry], run_id: &str, steps: &[Step], languages: &[String], project_id: &str, came_back: &HashSet<String>, done: bool) -> HashMap<String, Verdict> {
    let mut verdicts: HashMap<String, Verdict> = HashMap::new();
    let mut projects: HashMap<String, bool> = HashMap::new();
    let mut credited: Vec<String> = Vec::new();
    for item in shown(entries, run_id) {
        projects.insert(item.id.clone(), item.project);
        let verdict = if came_back.contains(&item.id) {
            Verdict::Hurt
        } else if !done {
            Verdict::NoObservedEffect
        } else if let Some((key, seq)) = &item.failure {
            let again = steps.iter().any(|step| step.seq > *seq && !step.ok && failure_key(&step.tool, &step.error_kind, &step.output).is_some_and(|other| same_key(&other, key)));
            if again { Verdict::NoObservedEffect } else { Verdict::Helped }
        } else {
            let anchor = match host.get::<KnowledgeItem>(memory_methods::KNOWLEDGE_GET, json!({"id": item.id})).await {
                Ok(found) => anchor_of(&found.name).to_owned(),
                Err(_) => String::new(),
            };
            if exercised(&anchor, steps, languages) {
                credited.push(item.id.clone());
            }
            Verdict::NoObservedEffect
        };
        // Under a failure is the stronger evidence: it decides over the knowledge note's.
        let keep = match verdicts.get(&item.id) {
            Some(Verdict::Hurt) => false,
            Some(Verdict::Helped) => verdict == Verdict::Hurt,
            _ => true,
        };
        if keep {
            verdicts.insert(item.id, verdict);
        }
    }
    credited.retain(|id| verdicts.get(id) == Some(&Verdict::NoObservedEffect));
    credited.sort();
    credited.dedup();
    let shared = if credited.len() == 1 { Verdict::Helped } else { Verdict::Confounded };
    for id in credited {
        verdicts.insert(id, shared);
    }
    for (id, verdict) in &verdicts {
        let (helped, hurt) = match verdict {
            Verdict::Helped => (1, 0),
            Verdict::Hurt => (0, 1),
            _ => continue,
        };
        let project = projects.get(id).copied().unwrap_or(false).then_some(project_id);
        let counted = match host.get::<KnowledgeItem>(memory_methods::KNOWLEDGE_COUNT, json!({"project_id": project, "id": id, "helped": helped, "hurt": hurt})).await {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!(%error, item = %id, "an item's score was not kept");
                continue;
            }
        };
        if counted.hurt > counted.helped && counted.hurt + counted.helped >= 3 {
            let _ = host.call(memory_methods::KNOWLEDGE_WITHDRAW, json!({"project_id": project, "id": id, "reason": "shown, and the error it is about came back", "by": "chatflow"})).await;
        }
    }
    verdicts
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
