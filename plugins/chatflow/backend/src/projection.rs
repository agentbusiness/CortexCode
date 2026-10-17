//! The model's view of a session: a deterministic projection of journal entries into chat messages.
//!
//! The projection is a pure function of the materialized journal, so a hot-swapped successor (or a
//! resumed run) renders byte-identical messages and keeps the model's prefix cache warm. Rules:
//! - `user_message` -> user message (its `attachments` rendered after the text: paths, sizes,
//!   folder listings, image dimensions; see [`crate::attachments`]).
//! - One model call's `reasoning`, `assistant_message` and `tool_call` entries (same `run_id` and
//!   `step`) -> one assistant message, immediately followed by its calls' `tool_result`s as tool
//!   messages (the raw call name and argument text the model wrote are replayed verbatim).
//! - Host reads (`tool_call`/`tool_result` with `host: true`) -> one host message per consecutive
//!   group, numbered.
//! - `route` entries (router and decide outputs) -> a short host message stating the decision.
//! - `note` with `host_message: true` -> host message; other notes are transcript-only.
//! - A consumed `steer` -> user message at the place it was consumed (`consumed_at`).
//! - The current spec (or, without one, the accepted decisions) is one host message right after the
//!   system prompt, whole ([`crate::spec::pinned`]); the user message it was made from is shown as
//!   a reference to it, and each `spec` entry in place as one line saying what changed.
//! - A `plan` entry -> host message: the newest version of its plan whole, older ones as one line.
//! - `status`, `usage`, `error`, `task_link`, `agent_spawned`, `verification`, `context`,
//!   `decision` and unconsumed steers are not part of the conversation.
//! - Entries marked `interrupted` (a generation cut off by a hot-swap) are skipped: the step is
//!   generated again.
//! - Compaction marks on the `context` entry ([`crate::fit`] sets them): a tool output at or before
//!   `folded_through` is one line saying it was folded; a finished turn whose terminal status is at
//!   or before `digested_through` keeps its user message, steers, plans and spec changes, and the
//!   rest of it becomes one host note listing its calls plus its final answer, placed where it ended.
//!
//! Host messages are user-role messages starting with `[host]`: chat templates reject system
//! messages after the first, and the system prompt states that `[host]` messages come from the host.

use crate::attachments;
use crate::data::Dataset;
use crate::{plan, spec};
use cc_protocol::contracts::chatflow::kinds as chatflow_kinds;
use crate::journal::extra_kinds;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::methods as memory_methods;
use cc_protocol::contracts::tools::methods as tools_methods;
use cc_protocol::contracts::model::{ChatMessage, ToolCall};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// A user-role message from the host (marked with the data set's `host_prefix`).
pub fn host_message(ds: &Dataset, text: &str) -> ChatMessage {
    ChatMessage::new("user", format!("{} {text}", ds.text("host_prefix", &[])))
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn bool_field(data: &Value, field: &str) -> bool {
    data.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn is_host(entry: &JournalEntry) -> bool {
    bool_field(&entry.data, "host")
}

fn step_key(entry: &JournalEntry) -> Option<(String, u64)> {
    let in_step = matches!(entry.kind.as_str(), kinds::REASONING | kinds::ASSISTANT_MESSAGE)
        || (entry.kind == kinds::TOOL_CALL && !is_host(entry));
    if !in_step {
        return None;
    }
    let step = entry.data.get("step").and_then(Value::as_u64)?;
    Some((str_field(&entry.data, "run_id").to_owned(), step))
}

/// Text that replaces a folded tool output.
fn folded_text(ds: &Dataset, result: &JournalEntry) -> String {
    let artifact = str_field(&result.data, "artifact");
    let size = str_field(&result.data, "output").len().to_string();
    if artifact.is_empty() {
        ds.text("folded", &[("size", &size)])
    } else {
        ds.text("folded_artifact", &[("size", &size), ("artifact", artifact)])
    }
}

/// One digested turn: the run's work shown as one host note and its final answer.
struct Digest {
    /// Seq of the run's terminal `status` (where the note and the answer are placed).
    end: u64,
    /// The model's own calls, in order: `tool target` and whether it failed.
    calls: Vec<(String, bool)>,
    /// The run's final answer: the text of its last step that made no calls.
    answer: String,
}

/// The turns the context entry's `digested_through` covers: runs whose newest status is terminal
/// and at or before it ([`crate::fit`] chooses the mark; the current run is never covered).
struct Digests(HashMap<String, Digest>);

impl Digests {
    /// The digest `entry` belongs to: its own `run_id`, or (a result) its call's.
    fn of(&self, entry: &JournalEntry, calls: &HashMap<String, &JournalEntry>) -> Option<&Digest> {
        if self.0.is_empty() {
            return None;
        }
        let run_id = match str_field(&entry.data, "run_id") {
            "" if entry.kind == kinds::TOOL_RESULT => calls.get(str_field(&entry.data, "call_id")).map(|call| str_field(&call.data, "run_id")).unwrap_or(""),
            run_id => run_id,
        };
        self.0.get(run_id)
    }
}

/// Seq of every run's terminal status: the runs whose newest `status` is not `running`.
pub fn finished_runs(entries: &[JournalEntry]) -> HashMap<String, u64> {
    let mut newest: HashMap<&str, &JournalEntry> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry.kind == kinds::STATUS) {
        let run_id = str_field(&entry.data, "run_id");
        if !run_id.is_empty() {
            newest.insert(run_id, entry);
        }
    }
    newest
        .into_iter()
        .filter(|(_, status)| str_field(&status.data, "state") != "running")
        .map(|(run_id, status)| (run_id.to_owned(), status.seq))
        .collect()
}

fn digests(entries: &[JournalEntry], calls: &HashMap<String, &JournalEntry>) -> Digests {
    let through = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == extra_kinds::CONTEXT)
        .and_then(|entry| entry.data.get("digested_through").and_then(Value::as_u64))
        .unwrap_or(0);
    if through == 0 {
        return Digests(HashMap::new());
    }
    let mut digests: HashMap<String, Digest> = finished_runs(entries)
        .into_iter()
        .filter(|(_, end)| *end <= through)
        .map(|(run_id, end)| (run_id, Digest { end, calls: Vec::new(), answer: String::new() }))
        .collect();
    let failed: HashSet<&str> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_RESULT && entry.data.get("ok").and_then(Value::as_bool) == Some(false))
        .map(|entry| str_field(&entry.data, "call_id"))
        .collect();
    // Steps that made calls: their text is not a final answer.
    let calling: HashSet<(String, u64)> = calls.values().filter(|call| !is_host(call)).filter_map(|call| step_key(call)).collect();
    let mut answer_step: HashMap<String, u64> = HashMap::new();
    for entry in entries.iter().filter(|entry| !bool_field(&entry.data, "interrupted")) {
        let Some(digest) = digests.get_mut(str_field(&entry.data, "run_id")) else { continue };
        let Some((run_id, step)) = step_key(entry) else { continue };
        if entry.kind == kinds::TOOL_CALL {
            let call = (call_label(entry), failed.contains(str_field(&entry.data, "call_id")));
            if digest.calls.last() != Some(&call) {
                digest.calls.push(call);
            }
        } else if entry.kind == kinds::ASSISTANT_MESSAGE && !calling.contains(&(run_id.clone(), step)) && !str_field(&entry.data, "text").trim().is_empty() {
            // The newest such step; a step's text may be streamed into several entries.
            let newest = answer_step.get(&run_id).copied().unwrap_or(0);
            if step > newest {
                digest.answer.clear();
                answer_step.insert(run_id, step);
            }
            if step >= newest {
                digest.answer.push_str(str_field(&entry.data, "text"));
            }
        }
    }
    Digests(digests)
}

/// Calls shown in one digest note at most; the rest are counted.
const DIGEST_CALLS: usize = 40;

/// A call as `tool target`: its tool and the first argument that names what it worked on.
fn call_label(call: &JournalEntry) -> String {
    let tool = str_field(&call.data, "tool");
    let args = call.data.get("args").cloned().unwrap_or(Value::Null);
    let target = ["path", "file", "command", "cmd", "uri", "url", "query", "pattern", "name"]
        .iter()
        .find_map(|field| args.get(*field).and_then(Value::as_str).filter(|value| !value.trim().is_empty()))
        .map(|value| {
            let line = value.lines().next().unwrap_or("").trim();
            let short: String = line.chars().take(80).collect();
            if short.len() < line.len() || value.lines().nth(1).is_some() { format!("{short}…") } else { short }
        });
    match target {
        Some(target) => format!("{tool} {target}"),
        None => tool.to_owned(),
    }
}

fn digest_text(ds: &Dataset, calls: &[(String, bool)]) -> String {
    let failed = ds.text("turn_digest_failed", &[]);
    let shown: Vec<String> = calls.iter().take(DIGEST_CALLS).map(|(label, bad)| if *bad { format!("{label}{failed}") } else { label.clone() }).collect();
    let mut list = shown.join("; ");
    if calls.len() > DIGEST_CALLS {
        list = ds.text("turn_digest_more", &[("calls", &list), ("more", &(calls.len() - DIGEST_CALLS).to_string())]);
    }
    ds.text("turn_digest", &[("calls", &list)])
}

/// Render a router or decide entry as the host's statement of that decision.
pub fn describe_decision(ds: &Dataset, data: &Value) -> String {
    let stage = str_field(data, "stage");
    let route = str_field(data, "route");
    let detail = data.get("detail").cloned().unwrap_or(Value::Null);
    let text = |field: &str| detail.get(field).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let list = |field: &str| -> String {
        detail
            .get(field)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .unwrap_or_default()
    };
    // ROUTE's `work` sentence serves the UI, the memory search and a delegated agent's context; the
    // model is not shown it: a restatement of the conversation outranks the conversation in its
    // eyes (a DO loop once generalised a fix from that sentence to a case the evidence never named).
    let mut out = if stage == "route" {
        ds.text("decision_route", &[("route", route)])
    } else {
        ds.text("decision", &[("route", route)])
    };
    let missing = text("missing");
    if stage != "route" && !missing.is_empty() {
        out.push_str(&ds.text("decision_missing", &[("missing", &missing)]));
    }
    let brief = text("brief");
    if !brief.is_empty() {
        out.push_str(&ds.text("decision_brief", &[("brief", &brief)]));
    }
    if let Some(checks) = detail.get("checks").and_then(Value::as_array).filter(|checks| !checks.is_empty()) {
        out.push_str(&ds.text("decision_checks", &[]));
        for check in checks {
            let command = check.as_str().or_else(|| check.get("command").and_then(Value::as_str)).unwrap_or("");
            out.push_str(&ds.text("decision_check", &[("command", command)]));
        }
    }
    let reads = [("symbols", "symbols"), ("files", "files"), ("queries", "searches")]
        .iter()
        .filter_map(|(field, label)| {
            let items = list(field);
            (!items.is_empty()).then(|| format!("{label}: {items}"))
        })
        .collect::<Vec<_>>();
    if !reads.is_empty() && route != "DO" && route != "DELEGATE" {
        out.push_str(&ds.text("decision_reads", &[("reads", &reads.join("; "))]));
    }
    out
}

/// Project the journal into the conversation the model sees (without the system prompt and
/// without the trailing stage message), for a text-only model.
pub fn project(entries: &[JournalEntry], ds: &Dataset) -> Vec<ChatMessage> {
    project_with(entries, ds, false)
}

/// [`project`], and `image_input` (the model takes image parts, per `model.info`): messages with
/// image attachments also carry them as content parts.
pub fn project_with(entries: &[JournalEntry], ds: &Dataset, image_input: bool) -> Vec<ChatMessage> {
    // A stuck task's attempt (an `attempt_failed` note's range) is not shown: the note says what
    // did not work, and the next attempt starts from fresh reads, not from the old picture.
    let dropped: Vec<(String, u64, u64)> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == "attempt_failed")
        .filter_map(|entry| Some((str_field(&entry.data, "run_id").to_owned(), entry.data.get("drop_from")?.as_u64()?, entry.data.get("drop_to")?.as_u64()?)))
        .collect();
    let kept: Vec<JournalEntry>;
    let entries: &[JournalEntry] = if dropped.is_empty() {
        entries
    } else {
        let run_of = |entry: &JournalEntry| str_field(&entry.data, "run_id").to_owned();
        let call_runs: HashMap<String, String> = entries
            .iter()
            .filter(|entry| entry.kind == kinds::TOOL_CALL)
            .map(|entry| (str_field(&entry.data, "call_id").to_owned(), run_of(entry)))
            .collect();
        kept = entries
            .iter()
            .filter(|entry| {
                let run = if entry.kind == kinds::TOOL_RESULT { call_runs.get(str_field(&entry.data, "call_id")).cloned().unwrap_or_default() } else { run_of(entry) };
                !dropped.iter().any(|(drop_run, from, to)| *drop_run == run && entry.seq >= *from && entry.seq <= *to)
            })
            .cloned()
            .collect();
        &kept
    };
    let folded_through = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == extra_kinds::CONTEXT)
        .and_then(|entry| entry.data.get("folded_through").and_then(Value::as_u64))
        .unwrap_or(0);
    // The newest tool output is never folded.
    let newest_result = entries.iter().rev().find(|entry| entry.kind == kinds::TOOL_RESULT).map(|entry| entry.seq);

    let calls: HashMap<String, &JournalEntry> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_CALL)
        .map(|entry| (str_field(&entry.data, "call_id").to_owned(), entry))
        .collect();
    let results: HashMap<String, &JournalEntry> = entries
        .iter()
        .filter(|entry| entry.kind == kinds::TOOL_RESULT)
        .map(|entry| (str_field(&entry.data, "call_id").to_owned(), entry))
        .collect();
    let mut steers_at: BTreeMap<u64, Vec<&JournalEntry>> = BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.kind == kinds::STEER) {
        let data = &entry.data;
        // Steers turned into their own turn (`as_turn`) appear as that turn's user message.
        if bool_field(data, "consumed") && str_field(data, "kind") != "followup" && !bool_field(data, "as_turn") {
            let at = data.get("consumed_at").and_then(Value::as_u64).unwrap_or(entry.seq);
            steers_at.entry(at).or_default().push(entry);
        }
    }

    // A skill the run read stays whole: the run is following it (folding it mid-run loses the
    // instructions while their effects continue).
    let is_skill_read = |result: &JournalEntry| calls.get(str_field(&result.data, "call_id")).and_then(|call| crate::skillsel::read_skill_uri(call)).is_some();
    // A read of a file that a later edit changed is an outdated view: shown whole, it competes
    // with the edit result's current view and the model starts doubting the file (seen: "the file
    // on disk may differ from what I'm shown"). It is shown as a one-line pointer instead.
    let file_of = |call: &JournalEntry| -> Option<String> {
        let args = call.data.get("args")?;
        let raw = args.get("path").or_else(|| args.get("target")).and_then(Value::as_str)?;
        if raw.contains("://") {
            return None;
        }
        let base = match raw.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ',')) => head,
            _ => raw,
        };
        Some(base.trim().trim_start_matches("./").to_owned())
    };
    let mut edited_at: HashMap<String, u64> = HashMap::new();
    for result in entries.iter().filter(|entry| entry.kind == kinds::TOOL_RESULT && bool_field(&entry.data, "ok")) {
        if let Some(call) = calls.get(str_field(&result.data, "call_id"))
            && str_field(&call.data, "tool") == "edit"
            && let Some(file) = file_of(call)
        {
            let at = edited_at.entry(file).or_insert(0);
            *at = (*at).max(result.seq);
        }
    }
    let superseded = |result: &JournalEntry| -> Option<String> {
        let call = calls.get(str_field(&result.data, "call_id"))?;
        let tool = str_field(&call.data, "tool");
        if tool != "read" && tool != tools_methods::READ {
            return None;
        }
        let file = file_of(call)?;
        (edited_at.get(&file).is_some_and(|&at| at > result.seq)).then(|| ds.text("read_superseded", &[("path", &file)]))
    };
    let render_output = |result: &JournalEntry| -> String {
        if let Some(pointer) = superseded(result) {
            pointer
        } else if result.seq <= folded_through && Some(result.seq) != newest_result && !is_skill_read(result) {
            folded_text(ds, result)
        } else {
            str_field(&result.data, "output").to_owned()
        }
    };
    let digests = digests(entries, &calls);

    let mut messages: Vec<ChatMessage> = Vec::new();
    // Pinned right after the system prompt: the spec (or the decision list), whole.
    if let Some(pinned) = spec::pinned(ds, entries) {
        messages.push(host_message(ds, &pinned));
    }
    let pins = Pins {
        source: spec::current(entries).and_then(|(_, current)| current.source_seq).and_then(|seq| spec::source_reference(ds, entries, seq).map(|text| (seq, text))),
        newest_plans: plan::newest_versions(entries),
        spec_versions: spec::versions(entries),
    };
    let mut emitted_results: HashSet<String> = HashSet::new();
    let mut host_group: Vec<HostRead> = Vec::new();
    let mut index = 0;

    let flush_host = |group: &mut Vec<HostRead>, messages: &mut Vec<ChatMessage>| {
        if group.is_empty() {
            return;
        }
        let mut text = ds.text("host_reads", &[]);
        for (number, read) in group.drain(..).enumerate() {
            text.push_str("\n\n");
            let number = (number + 1).to_string();
            text.push_str(&match &read.name {
                ReadName::Target(target) => ds.text("host_read", &[("number", &number), ("target", target), ("failed", &read.failed), ("output", &read.output)]),
                ReadName::What(what) => ds.text("host_read_named", &[("number", &number), ("what", what), ("failed", &read.failed), ("output", &read.output)]),
            });
        }
        messages.push(host_message(ds, &text));
    };

    while index < entries.len() {
        let entry = &entries[index];
        if bool_field(&entry.data, "interrupted") {
            index += 1;
            continue;
        }
        if let Some(digest) = digests.of(entry, &calls) {
            // A digested turn: its user message, plans and spec changes stay; everything else of
            // it becomes one host note and its final answer, where the turn ended.
            flush_host(&mut host_group, &mut messages);
            if matches!(entry.kind.as_str(), kinds::USER_MESSAGE | chatflow_kinds::PLAN | chatflow_kinds::SPEC) {
                emit_other(ds, entry, &mut messages, &emitted_results, &calls, &render_output, &mut host_group, image_input, &pins);
            } else if digest.end == entry.seq {
                if !digest.calls.is_empty() {
                    messages.push(host_message(ds, &digest_text(ds, &digest.calls)));
                }
                if !digest.answer.is_empty() {
                    messages.push(ChatMessage::new("assistant", digest.answer.clone()));
                }
            }
            emit_steers(ds, entry.seq, &steers_at, &mut messages, image_input);
            index += 1;
            continue;
        }
        if let Some(key) = step_key(entry) {
            flush_host(&mut host_group, &mut messages);
            // Collect the whole step.
            let mut reasoning = String::new();
            let mut content = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut last = index;
            let mut cursor = index;
            while cursor < entries.len() {
                let candidate = &entries[cursor];
                if bool_field(&candidate.data, "interrupted") {
                    cursor += 1;
                    continue;
                }
                match step_key(candidate) {
                    Some(other) if other == key => {
                        match candidate.kind.as_str() {
                            kinds::REASONING => reasoning.push_str(str_field(&candidate.data, "text")),
                            kinds::ASSISTANT_MESSAGE => content.push_str(str_field(&candidate.data, "text")),
                            _ => tool_calls.push(ToolCall {
                                id: str_field(&candidate.data, "call_id").to_owned(),
                                name: match str_field(&candidate.data, "raw_name") {
                                    "" => str_field(&candidate.data, "tool").to_owned(),
                                    raw => raw.to_owned(),
                                },
                                arguments: match candidate.data.get("raw_arguments").and_then(Value::as_str) {
                                    Some(raw) => raw.to_owned(),
                                    None => candidate.data.get("args").map(Value::to_string).unwrap_or_else(|| "{}".into()),
                                },
                            }),
                        }
                        last = cursor;
                        cursor += 1;
                    }
                    // Tool results, notes and usage of this step may sit between its entries.
                    None if matches!(candidate.kind.as_str(), kinds::TOOL_RESULT | kinds::USAGE) => cursor += 1,
                    _ => break,
                }
            }
            let mut message = ChatMessage::new("assistant", content);
            if !reasoning.is_empty() {
                message.reasoning_content = Some(reasoning);
            }
            message.tool_calls = tool_calls.clone();
            messages.push(message);
            for call in &tool_calls {
                let mut tool = match results.get(&call.id) {
                    Some(result) => ChatMessage::new("tool", render_output(result)),
                    None => ChatMessage::new("tool", ds.text("no_result", &[])),
                };
                tool.tool_call_id = Some(call.id.clone());
                tool.name = Some(call.name.clone());
                emitted_results.insert(call.id.clone());
                messages.push(tool);
            }
            // Entries between the step's first and last entry other than its own were skipped by
            // the scan; process them now in order (notes, steers) except emitted results.
            let mut inner = index + 1;
            while inner <= last {
                let between = &entries[inner];
                if step_key(between).as_ref() != Some(&key) {
                    emit_other(ds, between, &mut messages, &emitted_results, &calls, &render_output, &mut host_group, image_input, &pins);
                }
                emit_steers(ds, between.seq, &steers_at, &mut messages, image_input);
                inner += 1;
            }
            emit_steers(ds, entry.seq, &steers_at, &mut messages, image_input);
            index = last + 1;
            continue;
        }
        let is_host_read = (entry.kind == kinds::TOOL_RESULT || entry.kind == kinds::TOOL_CALL) && is_host(entry);
        if !is_host_read {
            flush_host(&mut host_group, &mut messages);
        }
        emit_other(ds, entry, &mut messages, &emitted_results, &calls, &render_output, &mut host_group, image_input, &pins);
        if steers_at.contains_key(&entry.seq) {
            flush_host(&mut host_group, &mut messages);
        }
        emit_steers(ds, entry.seq, &steers_at, &mut messages, image_input);
        index += 1;
    }
    flush_host(&mut host_group, &mut messages);
    messages
}

/// What the spec and plans change about entries in place.
struct Pins {
    /// The user message the current spec was made from, and the reference it is shown as.
    source: Option<(u64, String)>,
    /// Seq of the newest version of each plan.
    newest_plans: HashMap<String, u64>,
    /// The version of each `spec` entry (its place in append order).
    spec_versions: HashMap<u64, u32>,
}

/// How a host read is named to the model: the target its own `read` would take, or what it is.
enum ReadName {
    Target(String),
    What(String),
}

/// One host read of a group, rendered by the data set's `host_read` or `host_read_named` text.
struct HostRead {
    name: ReadName,
    failed: String,
    output: String,
}

fn emit_steers(ds: &Dataset, seq: u64, steers_at: &BTreeMap<u64, Vec<&JournalEntry>>, messages: &mut Vec<ChatMessage>, image_input: bool) {
    if let Some(steers) = steers_at.get(&seq) {
        for steer in steers {
            let attached = attachments::of_entry(&steer.data);
            let text = attachments::message_text(ds, str_field(&steer.data, "text"), &attached, image_input);
            messages.push(attachments::user_message(ds.text("steer", &[("text", &text)]), &attached, image_input));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_other(
    ds: &Dataset,
    entry: &JournalEntry,
    messages: &mut Vec<ChatMessage>,
    emitted_results: &HashSet<String>,
    calls: &HashMap<String, &JournalEntry>,
    render_output: &dyn Fn(&JournalEntry) -> String,
    host_group: &mut Vec<HostRead>,
    image_input: bool,
    pins: &Pins,
) {
    let data = &entry.data;
    match entry.kind.as_str() {
        kinds::USER_MESSAGE => {
            let attached = attachments::of_entry(data);
            // The spec's own message: its text is pinned at the start, so here only a reference.
            let own = match &pins.source {
                Some((seq, reference)) if *seq == entry.seq => reference.as_str(),
                _ => str_field(data, "text"),
            };
            let text = attachments::message_text(ds, own, &attached, image_input);
            messages.push(attachments::user_message(text, &attached, image_input))
        }
        chatflow_kinds::SPEC => messages.push(host_message(ds, &spec::changed_text(ds, pins.spec_versions.get(&entry.seq).copied().unwrap_or(0), data))),
        chatflow_kinds::PLAN => {
            if let Some(text) = plan::in_place(ds, entry, &pins.newest_plans) {
                messages.push(host_message(ds, &text));
            }
        }
        kinds::ROUTE => messages.push(host_message(ds, &describe_decision(ds, data))),
        kinds::NOTE if bool_field(data, "host_message") => messages.push(host_message(ds, str_field(data, "text"))),
        kinds::TOOL_RESULT if is_host(entry) => {
            let call_id = str_field(data, "call_id");
            let call = calls.get(call_id);
            let tool = call.map(|call| str_field(&call.data, "tool")).unwrap_or("");
            let args = call.and_then(|call| call.data.get("args")).cloned().unwrap_or(Value::Null);
            // Named in the model's own read vocabulary, never by internal method and arguments.
            let name = match crate::reads::read_target(tool, &args) {
                Some(target) => ReadName::Target(target),
                None if tool == memory_methods::REPO_MAP => {
                    let focus: Vec<&str> = args.get("focus").and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    ReadName::What(ds.text("read_map", &[("focus", &focus.join(", "))]))
                }
                None => ReadName::What(call.map(|call| str_field(&call.data, "intent")).filter(|intent| !intent.is_empty()).unwrap_or(tool).to_owned()),
            };
            let failed = if bool_field(data, "ok") { String::new() } else { ds.text("read_failed", &[]) };
            host_group.push(HostRead { name, failed, output: render_output(entry) });
        }
        kinds::TOOL_RESULT if !emitted_results.contains(str_field(data, "call_id")) => {
            // A result whose call entry is missing from the projection (should not happen): keep it
            // visible as host text rather than producing an orphan tool message.
            messages.push(host_message(ds, &ds.text("orphan_result", &[("output", &render_output(entry))])));
        }
        _ => {}
    }
}
