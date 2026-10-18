//! The session's spec (a long or requirement-heavy request kept whole, with its requirements as a
//! checklist) and the decision log (decisions the discussion settled, proposed by the model and
//! accepted by the user). See docs/IDEATION-WORKFLOW.md, parts 1 and 5.
//!
//! - A message becomes the spec when the user marked it or by the router's judgement (`spec` new /
//!   amend); by length (`limits.spec_min_chars`, off by default) only when that limit is set. Its requirements come from one constrained SPEC call; a later message that
//!   changes them is one constrained SPEC_EDIT call (added, dropped and changed ids).
//! - Every change is a new `spec` journal entry (a version); the newest is the spec. The user edits
//!   it through `chatflow.spec.update` (a version `by: user`). A version's number is its place in
//!   the journal's append order (the n-th `spec` entry is version n, [`versions`]), so a user edit
//!   and a model amendment that land together never share a number; the number a writer stores is
//!   corrected to that place right after its append ([`append`]).
//! - The model sees the current spec whole, right after the system prompt, with its requirements
//!   and decisions ([`pinned`]). It is never folded: it is not a tool output, and the fit counts it
//!   like any message. The message the spec was made from is shown as a reference to it, so the
//!   text is not in the prompt twice.
//! - Decisions are `decision` entries (proposed, then accepted or rejected by the user). An accepted
//!   one joins the spec's `decisions` (a new version); without a spec, the accepted entries are the
//!   session's decision list, pinned the same way.

use crate::data::{Dataset, Limits};
use crate::journal::Journal;
use crate::schemas::{SpecEdit, SpecExtract};
use cc_protocol::RpcError;
use cc_protocol::contracts::chatflow::{RequirementEdit, kinds as chatflow_kinds, requirement_status};
use cc_protocol::contracts::frame::JournalEntry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Decision states.
pub const PROPOSED: &str = "proposed";
pub const ACCEPTED: &str = "accepted";
pub const REJECTED: &str = "rejected";

fn open_status() -> String {
    requirement_status::OPEN.to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    #[serde(default = "open_status")]
    pub status: String,
}

/// A settled decision as the spec (or the decision list) keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Decided {
    pub decided: String,
    #[serde(default)]
    pub because: String,
    #[serde(default)]
    pub rejected: String,
    /// The `decision` entry it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// One version of the spec (the data of a `spec` entry, besides `run_id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Spec {
    pub version: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// The request's own worked examples, copied verbatim at SPEC: the check the work runs.
    #[serde(default)]
    pub examples: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<Decided>,
    /// The user message the text came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_seq: Option<u64>,
    /// What this version changed, as the model reads it.
    #[serde(default)]
    pub changes: String,
    /// model | user
    #[serde(default)]
    pub by: String,
}

impl Spec {
    /// Requirements still in force (not dropped), in order.
    pub fn live(&self) -> impl Iterator<Item = &Requirement> {
        self.requirements.iter().filter(|requirement| requirement.status != requirement_status::DROPPED)
    }

    /// Ids of the requirements still in force.
    pub fn live_ids(&self) -> Vec<String> {
        self.live().map(|requirement| requirement.id.clone()).collect()
    }

    /// Ids of the open requirements (not met, not dropped).
    pub fn open_ids(&self) -> Vec<String> {
        self.requirements.iter().filter(|requirement| requirement.status == requirement_status::OPEN).map(|requirement| requirement.id.clone()).collect()
    }

    /// The entry data of this version.
    pub fn entry(&self, run_id: Option<&str>) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let (Some(run_id), Some(map)) = (run_id, value.as_object_mut()) {
            map.insert("run_id".into(), json!(run_id));
        }
        value
    }
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// The version of every `spec` entry by its seq: its place among the `spec` entries in append
/// order (the journal's seq is assigned by the frame, one writer at a time).
pub fn versions(entries: &[JournalEntry]) -> HashMap<u64, u32> {
    let mut seqs: Vec<u64> = entries.iter().filter(|entry| entry.kind == chatflow_kinds::SPEC).map(|entry| entry.seq).collect();
    seqs.sort_unstable();
    seqs.into_iter().zip(1..).collect()
}

/// The version of the `spec` entry `seq` (its place in append order).
pub fn version_at(entries: &[JournalEntry], seq: u64) -> u32 {
    entries.iter().filter(|entry| entry.kind == chatflow_kinds::SPEC && entry.seq <= seq).count() as u32
}

/// The current spec: the newest `spec` entry, numbered by its place in append order.
pub fn current(entries: &[JournalEntry]) -> Option<(u64, Spec)> {
    let (seq, mut spec) = entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::SPEC)
        .max_by_key(|entry| entry.seq)
        .and_then(|entry| serde_json::from_value::<Spec>(entry.data.clone()).ok().map(|spec| (entry.seq, spec)))?;
    spec.version = version_at(entries, seq);
    Some((seq, spec))
}

/// Append a spec version. Its number is its place in append order: after the append the journal
/// is read up to it (the frame answers an append once every earlier entry is in), and a number that
/// a concurrent writer took is corrected on the entry. No wait, no lock: the frame's seq decides.
/// Returns the entry's seq and version.
pub async fn append(journal: &mut Journal, spec: &Spec, run_id: Option<&str>) -> Result<(u64, u32), RpcError> {
    let seq = journal.append(chatflow_kinds::SPEC, spec.entry(run_id)).await?;
    journal.sync().await?;
    let version = version_at(&journal.entries_vec(), seq);
    if version != spec.version {
        journal.set(seq, "version", json!(version)).await?;
    }
    Ok((seq, version))
}

/// Accepted `decision` entries, in order (the session's decision list).
pub fn accepted(entries: &[JournalEntry]) -> Vec<Decided> {
    entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::DECISION && str_field(&entry.data, "state") == ACCEPTED)
        .map(|entry| Decided {
            decided: str_field(&entry.data, "decided").to_owned(),
            because: str_field(&entry.data, "because").to_owned(),
            rejected: str_field(&entry.data, "rejected").to_owned(),
            seq: Some(entry.seq),
        })
        .collect()
}

/// The decisions the model sees: the spec's when there is one, else the accepted entries.
pub fn decisions(entries: &[JournalEntry]) -> Vec<Decided> {
    match current(entries) {
        Some((_, spec)) => spec.decisions,
        None => accepted(entries),
    }
}

fn decision_lines(ds: &Dataset, decisions: &[Decided]) -> String {
    let mut text = String::new();
    for decision in decisions {
        let because = if decision.because.trim().is_empty() { String::new() } else { ds.text("decision_because", &[("text", decision.because.trim())]) };
        let rejected = if decision.rejected.trim().is_empty() { String::new() } else { ds.text("decision_rejected", &[("text", decision.rejected.trim())]) };
        text.push_str(&ds.text("decision_line", &[("decided", decision.decided.trim()), ("because", &because), ("rejected", &rejected)]));
    }
    text
}

/// The block pinned right after the system prompt: the spec with its requirements and decisions,
/// or (without a spec) the accepted decisions. `None` when there is neither.
pub fn pinned(ds: &Dataset, entries: &[JournalEntry]) -> Option<String> {
    if let Some((_, spec)) = current(entries) {
        return Some(whole(ds, &spec));
    }
    let accepted = accepted(entries);
    if accepted.is_empty() {
        return None;
    }
    Some(format!("{}{}", ds.text("decisions_pinned", &[]), decision_lines(ds, &accepted)))
}

/// A spec version whole: its text, live requirements and decisions.
fn whole(ds: &Dataset, spec: &Spec) -> String {
    let mut text = ds.text("spec_pinned", &[("version", &spec.version.to_string()), ("title", spec.title.trim()), ("text", spec.text.trim())]);
    let live: Vec<&Requirement> = spec.live().collect();
    if !live.is_empty() {
        text.push_str(&ds.text("spec_requirements", &[]));
        for requirement in live {
            text.push_str(&ds.text("spec_requirement", &[("id", &requirement.id), ("status", &requirement.status), ("text", requirement.text.trim())]));
        }
    }
    if !spec.examples.is_empty() {
        text.push_str(&ds.text("spec_examples", &[]));
        for example in &spec.examples {
            text.push_str(&ds.text("list_item_line", &[("text", example)]));
        }
    }
    if !spec.decisions.is_empty() {
        text.push_str(&ds.text("spec_decisions", &[]));
        text.push_str(&decision_lines(ds, &spec.decisions));
    }
    text
}

/// The `spec` entry `data` (version `version`) whole, as shown where it was made until a
/// compaction pins it.
pub fn whole_in_place(ds: &Dataset, version: u32, data: &Value) -> Option<String> {
    let mut spec = serde_json::from_value::<Spec>(data.clone()).ok()?;
    spec.version = version;
    Some(whole(ds, &spec))
}

/// An accepted `decision` entry, as shown where it was made until a compaction pins it.
pub fn accepted_in_place(ds: &Dataset, data: &Value) -> Option<String> {
    (str_field(data, "state") == ACCEPTED).then(|| {
        let decided = Decided {
            decided: str_field(data, "decided").to_owned(),
            because: str_field(data, "because").to_owned(),
            rejected: str_field(data, "rejected").to_owned(),
            seq: None,
        };
        format!("{}{}", ds.text("decisions_pinned", &[]), decision_lines(ds, &[decided]))
    })
}

/// The text a user message is shown as when the current spec was made from it.
pub fn source_reference(ds: &Dataset, entries: &[JournalEntry], seq: u64) -> Option<String> {
    let (_, spec) = current(entries)?;
    (spec.source_seq == Some(seq)).then(|| ds.text("spec_source", &[("version", &spec.version.to_string())]))
}

/// The host's statement, in place, that the spec changed to `version`.
pub fn changed_text(ds: &Dataset, version: u32, data: &Value) -> String {
    ds.text("spec_changed", &[("version", &version.to_string()), ("changes", str_field(data, "changes"))])
}

/// The live text of a deciding stage while a spec has open requirements.
pub fn live_text(ds: &Dataset, entries: &[JournalEntry], key: &str) -> String {
    match current(entries) {
        Some((_, spec)) if !spec.open_ids().is_empty() => ds.text(key, &[("ids", &spec.open_ids().join(", "))]),
        _ => String::new(),
    }
}

/// What a message does to the spec: `new`, `amend` or `none`. Structural first (the user marked
/// it; a long message while there is no spec), then the router's judgement.
pub fn action(judged: &str, has_spec: bool, marked: bool, text_chars: usize, limits: &Limits) -> &'static str {
    if marked {
        return "new";
    }
    match judged {
        "amend" if has_spec => "amend",
        "new" => "new",
        _ if !has_spec && limits.spec_min_chars > 0 && text_chars > limits.spec_min_chars => "new",
        _ => "none",
    }
}

/// The spec's text from a user message entry: its text, then its pasted-text attachments while
/// they fit `limits.spec_max_chars` (longer ones are referenced by path).
pub fn source_text(ds: &Dataset, limits: &Limits, data: &Value) -> String {
    let mut text = str_field(data, "text").trim().to_owned();
    for attachment in data.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default() {
        if attachment.get("pasted_text").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let (name, path) = (str_field(&attachment, "name"), str_field(&attachment, "path"));
        let pasted = std::fs::read_to_string(path).unwrap_or_default();
        let piece = ds.text("spec_attached", &[("name", name), ("text", pasted.trim())]);
        if !pasted.is_empty() && text.chars().count() + piece.chars().count() <= limits.spec_max_chars {
            text.push_str(&piece);
        } else {
            text.push_str(&ds.text("spec_attached_ref", &[("name", name), ("path", path)]));
        }
    }
    text
}

/// The next free requirement id (`R<n>`, one past the highest number used, dropped ones included).
pub fn next_id(requirements: &[Requirement]) -> String {
    let highest = requirements.iter().filter_map(|requirement| requirement.id.strip_prefix('R').and_then(|n| n.parse::<u32>().ok())).max().unwrap_or(0);
    format!("R{}", highest + 1)
}

/// A new spec from a message and its extracted requirements. The previous spec's decisions (or
/// the accepted decision list) carry over.
pub fn created(ds: &Dataset, previous: Option<&Spec>, accepted: Vec<Decided>, text: String, extract: &SpecExtract, source_seq: u64) -> Spec {
    let mut requirements: Vec<Requirement> = Vec::new();
    for statement in extract.requirements.iter().map(|text| text.trim()).filter(|text| !text.is_empty()) {
        if requirements.iter().any(|requirement| requirement.text == statement) {
            continue;
        }
        let id = next_id(&requirements);
        requirements.push(Requirement { id, text: statement.to_owned(), status: open_status() });
    }
    Spec {
        version: previous.map(|spec| spec.version + 1).unwrap_or(1),
        title: extract.title.trim().to_owned(),
        text,
        changes: ds.text("spec_created", &[("count", &requirements.len().to_string())]),
        requirements,
        examples: extract.examples.iter().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty()).collect(),
        decisions: previous.map(|spec| spec.decisions.clone()).unwrap_or(accepted),
        source_seq: Some(source_seq),
        by: "model".into(),
    }
}

/// The spec after a SPEC_EDIT decision, or `None` when it changes nothing. Ids that are unknown
/// or already dropped are ignored; a changed requirement is open again.
pub fn edited(ds: &Dataset, spec: &Spec, edit: &SpecEdit) -> Option<Spec> {
    let mut next = spec.clone();
    let (mut added, mut dropped, mut modified) = (Vec::new(), Vec::new(), Vec::new());
    for id in &edit.drop {
        if let Some(requirement) = next.requirements.iter_mut().find(|requirement| &requirement.id == id && requirement.status != requirement_status::DROPPED) {
            requirement.status = requirement_status::DROPPED.into();
            dropped.push(id.clone());
        }
    }
    for change in &edit.modify {
        let text = change.text.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(requirement) = next.requirements.iter_mut().find(|requirement| requirement.id == change.id && requirement.status != requirement_status::DROPPED)
            && requirement.text != text
        {
            requirement.text = text.to_owned();
            requirement.status = open_status();
            modified.push(change.id.clone());
        }
    }
    for text in edit.add.iter().map(|text| text.trim()).filter(|text| !text.is_empty()) {
        if next.live().any(|requirement| requirement.text == text) {
            continue;
        }
        let id = next_id(&next.requirements);
        next.requirements.push(Requirement { id: id.clone(), text: text.to_owned(), status: open_status() });
        added.push(id);
    }
    let mut changes = Vec::new();
    for (key, ids) in [("spec_added", &added), ("spec_dropped", &dropped), ("spec_modified", &modified)] {
        if !ids.is_empty() {
            changes.push(ds.text(key, &[("ids", &ids.join(", "))]));
        }
    }
    if changes.is_empty() {
        return None;
    }
    next.version = spec.version + 1;
    next.changes = changes.join("; ");
    next.by = "model".into();
    Some(next)
}

/// Refuse an edit made on an older version than the current one (`base` given and not current):
/// `conflict`, with the current version in `data`. No spec yet is version 0.
pub fn check_base(spec: Option<&Spec>, base: Option<u32>) -> Result<(), RpcError> {
    let current = spec.map_or(0, |spec| spec.version);
    match base {
        Some(base) if base != current => Err(RpcError::new(
            cc_protocol::codes::CONFLICT,
            format!("the spec changed since version {base}: it is version {current} now, and this edit was not applied"),
        )
        .with_data(json!({"version": current}))),
        _ => Ok(()),
    }
}

/// The spec after the user's edit (`chatflow.spec.update`). Without a current spec, the edit makes
/// version 1 (its text is required then). A requirement left out of a given list is dropped.
pub fn user_update(ds: &Dataset, spec: Option<&Spec>, title: Option<&str>, text: Option<&str>, requirements: Option<&[RequirementEdit]>, accepted: Vec<Decided>) -> Result<Spec, String> {
    let mut next = match spec {
        Some(spec) => spec.clone(),
        None => {
            if text.is_none_or(|text| text.trim().is_empty()) {
                return Err("there is no spec yet: give its text".into());
            }
            Spec { version: 0, decisions: accepted, ..Spec::default() }
        }
    };
    if let Some(title) = title {
        next.title = title.trim().to_owned();
    }
    if let Some(text) = text {
        next.text = text.trim().to_owned();
    }
    if let Some(given) = requirements {
        let mut kept: Vec<Requirement> = Vec::new();
        for edit in given {
            let status = if edit.status.is_empty() { requirement_status::OPEN } else { edit.status.as_str() };
            if !requirement_status::ALL.contains(&status) {
                return Err(format!("unknown requirement status `{status}`"));
            }
            let text = edit.text.trim();
            if text.is_empty() {
                continue;
            }
            let known = edit.id.as_ref().filter(|id| next.requirements.iter().any(|requirement| &&requirement.id == id) && !kept.iter().any(|requirement| &&requirement.id == id));
            let id = match known {
                Some(id) => id.clone(),
                None => {
                    let mut all = next.requirements.clone();
                    all.extend(kept.iter().cloned());
                    next_id(&all)
                }
            };
            kept.push(Requirement { id, text: text.to_owned(), status: status.to_owned() });
        }
        // Left out: dropped (the id stays taken).
        for requirement in &next.requirements {
            if !kept.iter().any(|kept| kept.id == requirement.id) {
                kept.push(Requirement { status: requirement_status::DROPPED.into(), ..requirement.clone() });
            }
        }
        next.requirements = kept;
    }
    next.version += 1;
    next.changes = ds.text("spec_edited", &[]);
    next.by = "user".into();
    Ok(next)
}

/// The spec with an accepted decision added (a new version).
pub fn with_decision(ds: &Dataset, spec: &Spec, decided: Decided) -> Spec {
    let mut next = spec.clone();
    next.version += 1;
    next.changes = ds.text("spec_decided", &[("decided", decided.decided.trim())]);
    next.decisions.push(decided);
    next.by = "user".into();
    next
}

/// A file name for the spec (mechanical: lower-case letters and digits, dashes between).
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            out.push(character);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() { "spec".to_owned() } else { out }
}

/// The spec as a Markdown document for the repository (`docs/specs/<slug>.md`).
pub fn markdown(ds: &Dataset, spec: &Spec) -> String {
    let mut text = ds.text("spec_md", &[("title", spec.title.trim()), ("version", &spec.version.to_string()), ("text", spec.text.trim())]);
    if !spec.requirements.is_empty() {
        text.push_str(&ds.text("spec_md_requirements", &[]));
        for requirement in &spec.requirements {
            text.push_str(&ds.text("spec_md_requirement", &[("id", &requirement.id), ("status", &requirement.status), ("text", requirement.text.trim())]));
        }
    }
    if !spec.decisions.is_empty() {
        text.push_str(&ds.text("spec_md_decisions", &[]));
        text.push_str(&decision_lines(ds, &spec.decisions));
    }
    text.push('\n');
    text
}

/// A proposed decision's entry data.
pub fn proposed_entry(decided: &str, because: &str, rejected: &str, run_id: &str) -> Value {
    json!({"decided": decided.trim(), "because": because.trim(), "rejected": rejected.trim(), "state": PROPOSED, "run_id": run_id})
}
