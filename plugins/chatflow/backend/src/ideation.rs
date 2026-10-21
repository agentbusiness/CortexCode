//! Ideation (docs/IDEATION-WORKFLOW.md, part 4): an open design question (route IDEATE) worked
//! through by the host in small steps, so the model never has to hold the whole procedure:
//!
//! 1. grounding (host): a code-graph search for the question and the files it names, what the
//!    project knows, and earlier research notes on it;
//! 2. IDEAS (one constrained call): the question, its constraints and criteria, and one idea per
//!    generating principle; the principles are the schema's field names, so the ideas differ by
//!    construction instead of by an instruction to differ;
//! 3. lookups (host): every idea searched in the code graph and the research notes; what was found
//!    is written under the idea (a web search by an idea's name was tried and dropped: its hits
//!    were unrelated repositories, a cue the model then cited as prior art);
//! 4. OPTIONS (one constrained call): a keep / drop critique of every idea, the two or three
//!    options with their grounding picked from the findings (never a made-up source), one radical
//!    option and a recommendation.
//!
//! The options are the turn's answer (the model reads them as its own message); the user chooses
//! on the card (`chatflow.ideation.choose`: an accepted decision) or in words (DECISIONS).

use crate::agent_loop::RunEnv;
use crate::data::Dataset;
use crate::journal::Journal;
use crate::schemas::{self, IdeasOutput, OptionsOutput};
use cc_protocol::RpcError;
use cc_protocol::contracts::chatflow::kinds as chatflow_kinds;
use cc_protocol::contracts::frame::{JournalEntry, kinds};
use cc_protocol::contracts::memory::{NoteSearchResult, SearchResult, methods as memory_methods};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PHASE_IDEAS: &str = "ideas";
pub const PHASE_OPTIONS: &str = "options";
/// The letters options are shown under, in order.
const LETTERS: [&str; 3] = ["A", "B", "C"];
/// Hits per idea and source: a few strong ones (more is noise the model copies).
const CODE_HITS: u32 = 2;
const NOTE_HITS: u32 = 2;
/// Notes shown for the question before the ideas are written.
const QUESTION_NOTES: u32 = 4;

/// Something the host found for an idea.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Found {
    /// code | note
    pub kind: String,
    /// What an option's `grounding` names: `path:line name` for code, the url for a note.
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Idea {
    pub n: u32,
    pub principle: String,
    pub name: String,
    pub mechanism: String,
    #[serde(default)]
    pub found: Vec<Found>,
}

/// The `ideas` phase entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Ideas {
    pub ideation_id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub ideas: Vec<Idea>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Critique {
    pub idea: u32,
    pub verdict: String,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Choice {
    pub letter: String,
    pub idea: u32,
    pub name: String,
    pub mechanism: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub grounding: Vec<Found>,
    #[serde(default)]
    pub prior_art: String,
    #[serde(default)]
    pub cost: String,
    #[serde(default)]
    pub risk: String,
    #[serde(default)]
    pub settle_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Radical {
    pub name: String,
    #[serde(default)]
    pub drops: String,
    #[serde(default)]
    pub buys: String,
    #[serde(default)]
    pub wins_if: String,
}

/// The `options` phase entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Options {
    pub ideation_id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub critique: Vec<Critique>,
    #[serde(default)]
    pub options: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radical: Option<Radical>,
    /// The letter of the recommended option (empty: none).
    #[serde(default)]
    pub recommend: String,
    #[serde(default)]
    pub because: String,
    #[serde(default)]
    pub would_change_if: String,
    /// The option the user chose (a letter or `radical`), with the decision entry it made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_seq: Option<u64>,
}

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_list(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items.iter().map(|item| clean(item)).filter(|item| !item.is_empty()) {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// The phase of the newest `ideation` entry of a run after `seq`.
pub fn phase_after(run: &[&JournalEntry], seq: u64) -> Option<String> {
    run.iter().rev().find(|entry| entry.seq > seq && entry.kind == chatflow_kinds::IDEATION).map(|entry| str_field(&entry.data, "phase").to_owned())
}

/// The run's `ideas` entry.
fn ideas_of_run(entries: &[JournalEntry], run_id: &str) -> Option<Ideas> {
    entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == chatflow_kinds::IDEATION && str_field(&entry.data, "run_id") == run_id && str_field(&entry.data, "phase") == PHASE_IDEAS)
        .find_map(|entry| serde_json::from_value(entry.data.clone()).ok())
}

/// The options entry at `seq`.
pub fn options_at(entry: &JournalEntry) -> Option<Options> {
    (entry.kind == chatflow_kinds::IDEATION && str_field(&entry.data, "phase") == PHASE_OPTIONS).then(|| serde_json::from_value(entry.data.clone()).ok()).flatten()
}

fn list(ds: &Dataset, items: &[String]) -> String {
    items.iter().map(|item| ds.text("list_item", &[("text", item)])).collect::<Vec<_>>().join("\n")
}

/// The ideas and what the host found for each, as the model reads them (a host message).
pub fn render_ideas(ds: &Dataset, data: &Value) -> Option<String> {
    let ideas: Ideas = serde_json::from_value(data.clone()).ok()?;
    let mut text = ds.text("ideas_heading", &[("question", &ideas.question)]);
    if !ideas.constraints.is_empty() {
        text.push_str(&ds.text("ideas_constraints", &[("items", &list(ds, &ideas.constraints))]));
    }
    if !ideas.criteria.is_empty() {
        text.push_str(&ds.text("ideas_criteria", &[("items", &list(ds, &ideas.criteria))]));
    }
    // A finding is shown under the first idea that found it: the same hits under every idea are
    // noise the model would copy.
    let mut shown: Vec<&str> = Vec::new();
    for idea in &ideas.ideas {
        let principle = idea.principle.replace('_', " ");
        text.push_str(&ds.text("ideas_idea", &[("number", &idea.n.to_string()), ("name", &idea.name), ("principle", &principle), ("mechanism", &idea.mechanism)]));
        if idea.found.is_empty() {
            text.push_str(&ds.text("ideas_found_none", &[]));
        }
        for found in &idea.found {
            if shown.contains(&found.reference.as_str()) {
                continue;
            }
            shown.push(&found.reference);
            let key = if found.kind == "note" { "ideas_found_note" } else { "ideas_found_code" };
            text.push_str(&ds.text(key, &[("ref", &found.reference), ("text", &found.text)]));
        }
    }
    Some(text)
}

/// The options as the turn's answer: what the model reads as its own message, and the card's text.
pub fn render_options(ds: &Dataset, data: &Value) -> Option<String> {
    let options: Options = serde_json::from_value(data.clone()).ok()?;
    let mut text = ds.text("options_heading", &[("question", &options.question)]);
    if !options.criteria.is_empty() {
        text.push_str(&ds.text("options_criteria", &[("items", &options.criteria.join("; "))]));
    }
    if options.options.is_empty() {
        let why: Vec<String> = options.critique.iter().map(|item| format!("{}: {}", item.idea, item.why)).collect();
        text.push_str(&ds.text("options_none", &[("items", &list(ds, &why))]));
    }
    for choice in &options.options {
        text.push_str(&ds.text("options_option", &[("letter", &choice.letter), ("name", &choice.name), ("mechanism", &choice.mechanism), ("why", &choice.why)]));
        if !choice.grounding.is_empty() {
            let refs: Vec<String> = choice.grounding.iter().map(|found| found.reference.clone()).collect();
            text.push_str(&ds.text("options_grounding", &[("items", &refs.join(", "))]));
        }
        for (key, value) in [("options_prior_art", &choice.prior_art), ("options_cost", &choice.cost), ("options_risk", &choice.risk), ("options_settle", &choice.settle_by)] {
            if !value.is_empty() {
                text.push_str(&ds.text(key, &[("text", value)]));
            }
        }
    }
    if let Some(radical) = &options.radical {
        text.push_str(&ds.text("options_radical", &[("name", &radical.name), ("drops", &radical.drops), ("buys", &radical.buys), ("wins_if", &radical.wins_if)]));
    }
    if !options.recommend.is_empty() {
        text.push_str(&ds.text("options_recommend", &[("letter", &options.recommend), ("because", &options.because), ("would_change_if", &options.would_change_if)]));
    }
    text.push_str(&ds.text("options_choose", &[]));
    Some(text)
}

/// The decision a chosen option records: `(decided, because, rejected)`; None for an unknown option.
pub fn chosen_decision(options: &Options, option: &str) -> Option<(String, String, String)> {
    let others = |skip: Option<&str>| -> String {
        let mut names: Vec<String> = options.options.iter().filter(|choice| Some(choice.letter.as_str()) != skip).map(|choice| choice.name.clone()).collect();
        if skip.is_some()
            && let Some(radical) = &options.radical
        {
            names.push(radical.name.clone());
        }
        names.join("; ")
    };
    if option.eq_ignore_ascii_case("radical") {
        let radical = options.radical.as_ref()?;
        let decided = format!("{} (drops: {})", radical.name, radical.drops.trim_end_matches('.'));
        return Some((decided, radical.buys.clone(), others(None)));
    }
    let choice = options.options.iter().find(|choice| choice.letter.eq_ignore_ascii_case(option))?;
    Some((format!("{}: {}", choice.name, choice.mechanism), choice.why.clone(), others(Some(&choice.letter))))
}

fn new_id() -> String {
    format!("idea-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// Earlier research notes on the question, as one host note before the ideas (nothing when there
/// are none or memory is away).
pub async fn notes_note(env: &RunEnv, journal: &mut Journal, work: &str) -> Result<(), RpcError> {
    if work.trim().is_empty() {
        return Ok(());
    }
    let Ok(found) = env.host.get::<NoteSearchResult>(memory_methods::NOTE_SEARCH, json!({"text": work, "limit": QUESTION_NOTES})).await else { return Ok(()) };
    let lines: Vec<String> = found
        .notes
        .iter()
        .filter(|note| !note.claim.trim().is_empty())
        .map(|note| env.data.text("list_item", &[("text", &format!("{} ({})", clean(&note.claim), note.url))]))
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    journal.note(&env.run_id, "ideation_notes", &env.data.text("ideation_notes", &[("items", &lines.join("\n"))]), true).await.map(|_| ())
}

/// What the host finds for one idea: code in the project and earlier research notes. Every lookup
/// is best effort: one that fails adds nothing.
async fn look_up(env: &RunEnv, name: &str, mechanism: &str) -> Vec<Found> {
    let query = format!("{name}: {mechanism}");
    let code = env.host.get::<SearchResult>(memory_methods::SEARCH, env.scope.with(json!({"text": query, "limit": CODE_HITS})));
    let notes = env.host.get::<NoteSearchResult>(memory_methods::NOTE_SEARCH, json!({"text": query, "limit": NOTE_HITS}));
    let (code, notes) = tokio::join!(code, notes);
    let mut found = Vec::new();
    for hit in code.map(|result| result.hits).unwrap_or_default().into_iter().take(CODE_HITS as usize) {
        let symbol = hit.symbol;
        let about = symbol.description.as_deref().and_then(|text| text.lines().next()).map(clean).unwrap_or_default();
        let about: String = about.chars().take(120).collect();
        found.push(Found { kind: "code".into(), reference: format!("{}:{} {}", symbol.path, symbol.start_line, symbol.qname), text: about });
    }
    for note in notes.map(|result| result.notes).unwrap_or_default().into_iter().take(NOTE_HITS as usize) {
        found.push(Found { kind: "note".into(), reference: note.url, text: clean(&note.claim) });
    }
    found
}

/// IDEAS, then the host's lookups of every idea: one `ideas` entry.
pub async fn run_ideas(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let shape = crate::flow::stage_shape(env, "IDEAS")?;
    let live = crate::spec::live_text(&env.data, &journal.entries_vec(), "spec_live");
    let (output, _) = crate::flow::decide_json::<IdeasOutput>(env, journal, "IDEAS", &shape, &live).await?;
    let written: Vec<(&str, String, String)> = output
        .ideas
        .in_order()
        .into_iter()
        .map(|(principle, idea)| (principle, clean(&idea.name), clean(&idea.mechanism)))
        .filter(|(_, name, mechanism)| !name.is_empty() || !mechanism.is_empty())
        .collect();
    let lookups = written.iter().map(|(_, name, mechanism)| look_up(env, name, mechanism));
    let found = futures::future::join_all(lookups).await;
    let ideas: Vec<Idea> = written
        .into_iter()
        .zip(found)
        .enumerate()
        .map(|(index, ((principle, name, mechanism), found))| Idea { n: index as u32 + 1, principle: principle.to_owned(), name, mechanism, found })
        .collect();
    let entry = Ideas { ideation_id: new_id(), question: clean(&output.question), constraints: clean_list(&output.constraints), criteria: clean_list(&output.criteria), ideas };
    let mut data = serde_json::to_value(&entry).unwrap_or(Value::Null);
    data["phase"] = json!(PHASE_IDEAS);
    data["run_id"] = json!(env.run_id);
    journal.append(chatflow_kinds::IDEATION, data).await.map(|_| ())
}

/// OPTIONS over the run's ideas: one `options` entry. The idea numbers and the findings are the
/// grammar's only choices for `idea`, `recommend` and `grounding`.
pub async fn run_options(env: &RunEnv, journal: &mut Journal) -> Result<(), RpcError> {
    let entries = journal.entries_vec();
    let Some(ideas) = ideas_of_run(&entries, &env.run_id) else { return Err(RpcError::internal("the run's ideas could not be read back")) };
    let numbers: Vec<String> = ideas.ideas.iter().map(|idea| idea.n.to_string()).collect();
    let findings: Vec<Found> = {
        let mut all: Vec<Found> = Vec::new();
        for found in ideas.ideas.iter().flat_map(|idea| idea.found.iter()) {
            if !all.iter().any(|known| known.reference == found.reference) {
                all.push(found.clone());
            }
        }
        all
    };
    let refs: Vec<String> = findings.iter().map(|found| found.reference.clone()).collect();
    let shape = crate::flow::stage_shape(env, "OPTIONS")?
        .filled(schemas::IDEAS_PLACEHOLDER, &numbers, None)
        .filled(schemas::GROUNDING_PLACEHOLDER, &refs, None);
    let (output, _) = crate::flow::decide_json::<OptionsOutput>(env, journal, "OPTIONS", &shape, "").await?;
    let made = options_from(&ideas, &findings, output);
    let mut data = serde_json::to_value(&made).unwrap_or(Value::Null);
    data["phase"] = json!(PHASE_OPTIONS);
    data["run_id"] = json!(env.run_id);
    journal.append(chatflow_kinds::IDEATION, data).await.map(|_| ())
}

/// The options entry from the OPTIONS output: at most three options, one per idea, lettered in
/// the order written; the recommendation mapped from its idea number to its letter (none when it
/// names no option).
fn options_from(ideas: &Ideas, findings: &[Found], output: OptionsOutput) -> Options {
    let number = |text: &str| text.trim().parse::<u32>().ok().filter(|n| ideas.ideas.iter().any(|idea| idea.n == *n));
    let critique: Vec<Critique> = output
        .critique
        .iter()
        .filter_map(|item| number(&item.idea).map(|idea| Critique { idea, verdict: item.verdict.clone(), why: clean(&item.why) }))
        .collect();
    let mut options: Vec<Choice> = Vec::new();
    for option in output.options {
        let Some(idea) = number(&option.idea) else { continue };
        if options.len() >= LETTERS.len() || options.iter().any(|choice| choice.idea == idea) {
            continue;
        }
        let source = ideas.ideas.iter().find(|item| item.n == idea);
        let pick = |text: &str, fallback: Option<&String>| {
            let text = clean(text);
            if text.is_empty() { fallback.cloned().unwrap_or_default() } else { text }
        };
        let grounding: Vec<Found> = option.grounding.iter().filter_map(|reference| findings.iter().find(|found| &found.reference == reference)).cloned().collect();
        options.push(Choice {
            letter: LETTERS[options.len()].to_owned(),
            idea,
            name: pick(&option.name, source.map(|idea| &idea.name)),
            mechanism: pick(&option.mechanism, source.map(|idea| &idea.mechanism)),
            why: clean(&option.why),
            grounding,
            prior_art: clean(&option.prior_art),
            cost: clean(&option.cost),
            risk: clean(&option.risk),
            settle_by: clean(&option.settle_by),
        });
    }
    let radical = Some(Radical { name: clean(&output.radical.name), drops: clean(&output.radical.drops), buys: clean(&output.radical.buys), wins_if: clean(&output.radical.wins_if) })
        .filter(|radical| !radical.name.is_empty());
    let recommend = number(&output.recommend).and_then(|idea| options.iter().find(|choice| choice.idea == idea)).map(|choice| choice.letter.clone()).unwrap_or_default();
    Options {
        ideation_id: ideas.ideation_id.clone(),
        question: ideas.question.clone(),
        criteria: ideas.criteria.clone(),
        critique,
        options,
        radical,
        because: if recommend.is_empty() { String::new() } else { clean(&output.because) },
        would_change_if: if recommend.is_empty() { String::new() } else { clean(&output.would_change_if) },
        recommend,
        chosen: None,
        decision_seq: None,
    }
}

/// Whether an entry is an ideation phase the model reads: `ideas` as a host message, `options` as
/// its own answer.
pub fn is_answer(entry: &JournalEntry) -> bool {
    entry.kind == chatflow_kinds::IDEATION && str_field(&entry.data, "phase") == PHASE_OPTIONS
}

/// The note that marks the grounding stage done.
pub const GROUNDED: &str = "ideate_grounded";

/// Whether the run grounded its question after `seq`.
pub fn grounded(run: &[&JournalEntry], seq: u64) -> bool {
    run.iter().any(|entry| entry.seq > seq && entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == GROUNDED)
}
