//! Plan mode (docs/IDEATION-WORKFLOW.md, part 6): a plan document and a task graph, written by a
//! constrained PLAN decision with no code changed; edited by later messages (PLAN_EDIT, a new
//! version with its difference to the previous one); approved by the user; implemented by the
//! existing machinery (one task: the foreground DO loop with the task's contract; several: memory
//! tasks with contracts and dependencies, started through `agents.spawn`).
//!
//! Every content change is a new `plan` entry (a version, same `plan_id`); status changes
//! (draft -> approved -> implementing -> done) patch the newest version. The newest version is
//! shown to the model whole, in place; older versions are shown as one line, so the conversation
//! never carries two versions of a plan and is never folded (plans are not tool outputs).

use crate::data::Dataset;
use crate::schemas::{Decision, PlanOutput};
use crate::spec::Spec;
use cc_protocol::contracts::chatflow::{PlanEditParams, kinds as chatflow_kinds, plan_status};
use cc_protocol::contracts::frame::JournalEntry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanDoc {
    /// The work thought through before the plan (written first, inside the constrained call).
    #[serde(default)]
    pub analysis: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub approach: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub risks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanTask {
    /// 1-based position in the plan.
    pub n: u32,
    pub title: String,
    #[serde(default)]
    pub brief: String,
    /// The request's rules this task makes true (a behaviour, a value, an error, a limit), each to
    /// get its own test in the project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// Numbers of earlier tasks.
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
    /// Requirement ids of the spec.
    #[serde(default)]
    pub covers: Vec<String>,
    /// Skills the task chose for its implementation (only these reach its run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The memory task made for it when implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// What a version changed against the previous one (shown by the UI; the model sees the new
/// version whole).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanDiff {
    /// Document fields whose text changed (goal, approach, architecture).
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub steps_added: Vec<String>,
    #[serde(default)]
    pub steps_removed: Vec<String>,
    #[serde(default)]
    pub risks_added: Vec<String>,
    #[serde(default)]
    pub risks_removed: Vec<String>,
    /// Task titles.
    #[serde(default)]
    pub tasks_added: Vec<String>,
    #[serde(default)]
    pub tasks_removed: Vec<String>,
    #[serde(default)]
    pub tasks_changed: Vec<String>,
}

impl PlanDiff {
    pub fn is_empty(&self) -> bool {
        self == &PlanDiff::default()
    }
}

/// A step of the plan the foreground loop has carried out: what the loop reported (the same
/// outcomes as a run: done, failed, stopped, ...), its closing summary and the files it changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StepDone {
    /// 1-based position in `doc.steps`.
    pub n: u32,
    pub outcome: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
}

/// One version of a plan (the data of a `plan` entry, besides `run_id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Plan {
    pub plan_id: String,
    pub version: u32,
    #[serde(default)]
    pub doc: PlanDoc,
    #[serde(default)]
    pub tasks: Vec<PlanTask>,
    /// draft | approved | implementing | done
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<PlanDiff>,
    #[serde(default)]
    pub note: String,
    /// The root memory task when implemented by agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_task: Option<String>,
    /// The steps the foreground loop has carried out, in order (a plan made for a DO decision:
    /// the loop runs `doc.steps` one at a time and records each here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps_done: Vec<StepDone>,
    /// How many times this plan was written again after one of its tasks ended badly.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replans: u32,
    /// `user` for a version the user edited by hand (`chatflow.plan.edit`); empty for the planner's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// Tasks of the session's open background work this plan takes the place of: cancelled (their
    /// agents stopped) when the plan is carried out, then cleared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Plan {
    pub fn entry(&self, run_id: &str) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert("run_id".into(), json!(run_id));
        }
        value
    }

    /// Draft or approved: the plan later messages edit and IMPLEMENT carries out.
    pub fn is_open(&self) -> bool {
        self.status == plan_status::DRAFT || self.status == plan_status::APPROVED
    }
}

fn parse(entry: &JournalEntry) -> Option<Plan> {
    (entry.kind == chatflow_kinds::PLAN).then(|| serde_json::from_value::<Plan>(entry.data.clone()).ok()).flatten()
}

/// The newest plan version of the session.
pub fn latest(entries: &[JournalEntry]) -> Option<(u64, Plan)> {
    entries.iter().rev().find_map(|entry| parse(entry).map(|plan| (entry.seq, plan)))
}

/// The newest plan when it is open (draft or approved).
pub fn open(entries: &[JournalEntry]) -> Option<(u64, Plan)> {
    latest(entries).filter(|(_, plan)| plan.is_open())
}

/// The plan entry at `seq`.
pub fn at(entries: &[JournalEntry], seq: u64) -> Option<Plan> {
    entries.iter().find(|entry| entry.seq == seq).and_then(parse)
}

/// The next version of a plan after one of its tasks ended badly: the tasks done so far stay as
/// they are (their records and numbers), the planner's new tasks follow them, renumbered, each
/// after the one before it; the done steps are kept, the bad one dropped, so the Act stage goes
/// on with the first new task.
pub fn replanned(previous: &Plan, doc: PlanDoc, new_tasks: Vec<PlanTask>, note: &str) -> Plan {
    let done: Vec<StepDone> = previous.steps_done.iter().filter(|step| step.outcome == "done").cloned().collect();
    let kept = done.len();
    let mut tasks: Vec<PlanTask> = previous.tasks.iter().take(kept).cloned().collect();
    // A planner that wrote the whole plan again repeats the done tasks: those are not done twice.
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let fresh: Vec<PlanTask> = new_tasks.into_iter().filter(|task| !tasks.iter().any(|kept| same(&kept.title, &task.title))).collect();
    for (index, mut task) in fresh.into_iter().enumerate() {
        let n = (kept + index + 1) as u32;
        task.n = n;
        task.depends_on = task.depends_on.iter().map(|d| d + kept as u32).filter(|d| *d < n).collect();
        if task.depends_on.is_empty() && n > 1 {
            task.depends_on = vec![n - 1];
        }
        task.task_id = None;
        tasks.push(task);
    }
    Plan {
        plan_id: previous.plan_id.clone(),
        version: previous.version + 1,
        doc,
        tasks,
        status: plan_status::IMPLEMENTING.into(),
        diff: None,
        note: note.to_owned(),
        root_task: previous.root_task.clone(),
        steps_done: done,
        replans: previous.replans + 1,
        by: String::new(),
        replaces: Vec::new(),
    }
}

/// The plan a run wrote for its own DO decision (the one its Act stage carries out step by step).
pub fn for_run(entries: &[JournalEntry], run_id: &str) -> Option<(u64, Plan)> {
    entries
        .iter()
        .rev()
        .filter(|entry| entry.data.get("run_id").and_then(Value::as_str) == Some(run_id))
        .find_map(|entry| parse(entry).map(|plan| (entry.seq, plan)))
}

/// The newest version of `plan_id`.
pub fn version_of(entries: &[JournalEntry], plan_id: &str) -> Option<(u64, Plan)> {
    entries.iter().rev().find_map(|entry| parse(entry).filter(|plan| plan.plan_id == plan_id).map(|plan| (entry.seq, plan)))
}

/// Seq of the newest version of every plan.
pub fn newest_versions(entries: &[JournalEntry]) -> HashMap<String, u64> {
    let mut newest = HashMap::new();
    for entry in entries {
        if entry.kind == chatflow_kinds::PLAN
            && let Some(id) = entry.data.get("plan_id").and_then(Value::as_str)
        {
            newest.insert(id.to_owned(), entry.seq);
        }
    }
    newest
}

fn clean(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items.iter().map(|item| item.trim()).filter(|item| !item.is_empty()) {
        if !out.iter().any(|known| known == item) {
            out.push(item.to_owned());
        }
    }
    out
}

/// What a PLAN decision may name: the spec's requirement ids, and the session's skill catalog with
/// the cap of skills per task (`limits.skills_per_turn`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Known<'a> {
    pub requirements: &'a [String],
    pub skills: &'a [String],
    pub skills_max: usize,
}

/// The document and task graph from a PLAN decision. Tasks without a title or brief are left
/// out (the numbers the model wrote are mapped to the kept ones); a dependency on a later task,
/// itself or a number that does not exist is dropped, so the graph is acyclic in the order
/// written; at most `max_tasks`; `covers` keeps known requirement ids, `skills` catalog skills.
/// Tasks that touch exactly the same files and symbols stay separate small tasks: each later such
/// task waits for the latest earlier one, so they never run at the same time.
/// A task that touches nothing joins the task before it (its brief, dependencies, checks,
/// requirements and skills are kept), and the rest are renumbered.
pub fn merge_same_touches(tasks: Vec<PlanTask>) -> Vec<PlanTask> {
    let key = |task: &PlanTask| {
        let mut touches: Vec<String> = task.touches.iter().map(|item| item.trim().trim_start_matches("./").to_owned()).filter(|item| !item.is_empty()).collect();
        touches.sort();
        touches.dedup();
        touches
    };
    let mut merged: Vec<PlanTask> = Vec::new();
    let mut renumber: HashMap<u32, u32> = HashMap::new();
    // (task, the earlier task with the same touches it waits for), both in the new numbering.
    let mut waits: Vec<(u32, u32)> = Vec::new();
    for task in tasks {
        let touches = key(&task);
        // A task that touches nothing changes no code (only runs or reviews): it joins the task
        // before it, whose own checks run its cases.
        let target = if touches.is_empty() { merged.len().checked_sub(1) } else { None };
        // Moving this task into an earlier one must not move its work before a prerequisite.
        // A dependency already folded into the target is satisfied inside the combined task.
        let target = target.filter(|index| task.depends_on.iter().all(|dependency| {
            renumber.get(dependency).is_some_and(|n| *n <= merged[*index].n)
        }));
        if let Some(first) = target.and_then(|index| merged.get_mut(index)) {
            renumber.insert(task.n, first.n);
            if !task.brief.trim().is_empty() {
                first.brief = format!("{}\n{}", first.brief.trim_end(), task.brief.trim());
            }
            first.depends_on.extend(task.depends_on);
            for (into, from) in [(&mut first.rules, task.rules), (&mut first.checks, task.checks), (&mut first.covers, task.covers), (&mut first.skills, task.skills)] {
                for item in from {
                    if !into.contains(&item) {
                        into.push(item);
                    }
                }
            }
            continue;
        }
        let n = merged.len() as u32 + 1;
        renumber.insert(task.n, n);
        if let Some(same) = merged.iter().rev().find(|kept| !touches.is_empty() && key(kept) == touches) {
            waits.push((n, same.n));
        }
        merged.push(PlanTask { n, ..task });
    }
    for task in &mut merged {
        let same = waits.iter().filter(|(n, _)| *n == task.n).map(|(_, same)| *same);
        let mut depends: Vec<u32> = task.depends_on.iter().filter_map(|d| renumber.get(d).copied()).chain(same).filter(|d| *d < task.n).collect();
        depends.sort_unstable();
        depends.dedup();
        task.depends_on = depends;
    }
    merged
}

pub fn from_output(output: &PlanOutput, max_tasks: usize, known: Known) -> (PlanDoc, Vec<PlanTask>) {
    let doc = PlanDoc {
        analysis: output.analysis.trim().to_owned(),
        goal: output.goal.trim().to_owned(),
        approach: output.approach.trim().to_owned(),
        // The tasks are the steps (a separate list made the model write the order twice); an
        // older plan's own list is kept.
        steps: if output.steps.is_empty() { output.tasks.iter().map(|task| task.title.trim().to_owned()).filter(|title| !title.is_empty()).collect() } else { clean(&output.steps) },
        architecture: output.architecture.trim().to_owned(),
        risks: clean(&output.risks),
    };
    let mut numbers: HashMap<usize, u32> = HashMap::new();
    let mut tasks: Vec<PlanTask> = Vec::new();
    for (index, task) in output.tasks.iter().enumerate() {
        if tasks.len() >= max_tasks {
            break;
        }
        let (title, brief) = (task.title.trim(), task.brief.trim());
        if title.is_empty() && brief.is_empty() {
            continue;
        }
        let n = tasks.len() as u32 + 1;
        numbers.insert(index + 1, n);
        let mut depends_on: Vec<u32> = Vec::new();
        for written in &task.depends_on {
            if let Some(&target) = written.trim().parse::<usize>().ok().and_then(|written| numbers.get(&written))
                && target < n
                && !depends_on.contains(&target)
            {
                depends_on.push(target);
            }
        }
        tasks.push(PlanTask {
            n,
            title: if title.is_empty() { brief.lines().next().unwrap_or("").to_owned() } else { title.to_owned() },
            brief: brief.to_owned(),
            rules: clean(&task.rules),
            depends_on,
            touches: clean(&task.touches),
            checks: clean(&task.checks),
            covers: clean(&task.covers).into_iter().filter(|id| known.requirements.contains(id)).collect(),
            skills: crate::skillsel::chosen(&task.skills, known.skills, known.skills_max),
            task_id: None,
        });
    }
    (doc, tasks)
}

/// The next version of an open plan after the user's own edit (`chatflow.plan.edit`): the fields
/// given replace the plan's; `tasks`, when given, is the whole new list, numbered in its order,
/// each `depends_on` kept to earlier positions; a task without a title is left out (its
/// dependents lose that dependency). Back to draft: an edited plan is approved again.
pub fn user_edit(current: &Plan, edit: &PlanEditParams) -> Result<Plan, String> {
    if !current.is_open() {
        return Err(format!("the plan is {}", current.status));
    }
    let text = |given: &Option<String>, now: &str| given.as_deref().map(str::trim).unwrap_or(now).to_owned();
    let mut next = current.clone();
    next.version = current.version + 1;
    next.status = plan_status::DRAFT.into();
    next.by = "user".into();
    next.note = String::new();
    next.doc.goal = text(&edit.goal, &current.doc.goal);
    next.doc.approach = text(&edit.approach, &current.doc.approach);
    next.doc.architecture = text(&edit.architecture, &current.doc.architecture);
    if let Some(risks) = &edit.risks {
        next.doc.risks = clean(risks);
    }
    if let Some(tasks) = &edit.tasks {
        // Positions in the edited list -> the numbers of the tasks kept.
        let mut numbers: HashMap<u32, u32> = HashMap::new();
        let mut kept: Vec<PlanTask> = Vec::new();
        for (index, task) in tasks.iter().enumerate() {
            let title = task.title.trim();
            if title.is_empty() {
                continue;
            }
            let n = kept.len() as u32 + 1;
            numbers.insert(index as u32 + 1, n);
            let mut depends_on: Vec<u32> = Vec::new();
            for position in &task.depends_on {
                if let Some(&target) = numbers.get(position)
                    && target < n
                    && !depends_on.contains(&target)
                {
                    depends_on.push(target);
                }
            }
            kept.push(PlanTask {
                n,
                title: title.to_owned(),
                brief: task.brief.trim().to_owned(),
                rules: clean(&task.rules),
                depends_on,
                touches: clean(&task.touches),
                checks: clean(&task.checks),
                covers: clean(&task.covers),
                skills: clean(&task.skills),
                task_id: None,
            });
        }
        if kept.is_empty() {
            return Err("a plan needs at least one task with a title".into());
        }
        next.tasks = kept;
        next.doc.steps = next.tasks.iter().map(|task| task.title.clone()).collect();
    }
    next.diff = Some(diff(current, &next));
    if next.diff.as_ref().is_some_and(PlanDiff::is_empty) {
        return Err("nothing changed".into());
    }
    Ok(next)
}

/// A next version's tasks keep the dependencies of the tasks they carry over: a task with the
/// title of a task of `previous` that names no dependency gets that task's, mapped by title to the
/// new numbers (earlier tasks only). A version is written whole, and the planner was seen dropping
/// a kept task's dependency the change did not touch.
pub fn keep_dependencies(previous: &Plan, tasks: &mut [PlanTask]) {
    let numbers: HashMap<String, u32> = tasks.iter().map(|task| (task.title.trim().to_lowercase(), task.n)).collect();
    for task in tasks.iter_mut().filter(|task| task.depends_on.is_empty()) {
        let Some(before) = previous.tasks.iter().find(|before| before.title.trim().eq_ignore_ascii_case(task.title.trim())) else { continue };
        task.depends_on = before
            .depends_on
            .iter()
            .filter_map(|n| previous.tasks.iter().find(|other| other.n == *n))
            .filter_map(|other| numbers.get(&other.title.trim().to_lowercase()).copied())
            .filter(|n| *n < task.n)
            .collect();
    }
}

/// What `new` changed against `old`.
pub fn diff(old: &Plan, new: &Plan) -> PlanDiff {
    let mut out = PlanDiff::default();
    for (name, before, after) in [("goal", &old.doc.goal, &new.doc.goal), ("approach", &old.doc.approach, &new.doc.approach), ("architecture", &old.doc.architecture, &new.doc.architecture)] {
        if before != after {
            out.changed.push(name.to_owned());
        }
    }
    let added = |before: &[String], after: &[String]| after.iter().filter(|item| !before.contains(item)).cloned().collect::<Vec<_>>();
    out.steps_added = added(&old.doc.steps, &new.doc.steps);
    out.steps_removed = added(&new.doc.steps, &old.doc.steps);
    out.risks_added = added(&old.doc.risks, &new.doc.risks);
    out.risks_removed = added(&new.doc.risks, &old.doc.risks);
    let titles = |plan: &Plan| plan.tasks.iter().map(|task| task.title.clone()).collect::<Vec<_>>();
    out.tasks_added = added(&titles(old), &titles(new));
    out.tasks_removed = added(&titles(new), &titles(old));
    let titles_of = |plan: &Plan, numbers: &[u32]| numbers.iter().filter_map(|n| plan.tasks.iter().find(|task| task.n == *n)).map(|task| task.title.clone()).collect::<Vec<_>>();
    for task in &new.tasks {
        if let Some(before) = old.tasks.iter().find(|before| before.title == task.title)
            && (before.brief != task.brief
                || before.rules != task.rules
                || before.touches != task.touches
                || before.checks != task.checks
                || before.covers != task.covers
                || before.skills != task.skills
                || titles_of(old, &before.depends_on) != titles_of(new, &task.depends_on))
        {
            out.tasks_changed.push(task.title.clone());
        }
    }
    out
}

/// The plan as the model reads it.
pub fn render(ds: &Dataset, plan: &Plan) -> String {
    render_with(ds, plan, ds.text("plan_heading", &[("plan_id", &plan.plan_id), ("version", &plan.version.to_string()), ("status", &plan.status)]))
}

/// The heading of a plan shown in place: without its status, which changes while the plan is
/// carried out (a changed message the model already read makes it read everything after it again).
fn placed_heading(ds: &Dataset, plan: &Plan) -> String {
    ds.text("plan_heading_placed", &[("plan_id", &plan.plan_id), ("version", &plan.version.to_string())])
}

fn render_with(ds: &Dataset, plan: &Plan, heading: String) -> String {
    let mut text = heading;
    let doc = &plan.doc;
    if !doc.analysis.is_empty() {
        text.push_str(&ds.text("plan_analysis", &[("text", &doc.analysis)]));
    }
    if !doc.goal.is_empty() {
        text.push_str(&ds.text("plan_goal", &[("text", &doc.goal)]));
    }
    if !doc.approach.is_empty() {
        text.push_str(&ds.text("plan_approach", &[("text", &doc.approach)]));
    }
    if !doc.steps.is_empty() {
        text.push_str(&ds.text("plan_steps", &[]));
        for (index, step) in doc.steps.iter().enumerate() {
            text.push_str(&ds.text("plan_step", &[("number", &(index + 1).to_string()), ("text", step)]));
        }
    }
    if !doc.architecture.is_empty() {
        text.push_str(&ds.text("plan_architecture", &[("text", &doc.architecture)]));
    }
    if !doc.risks.is_empty() {
        text.push_str(&ds.text("plan_risks", &[]));
        for risk in &doc.risks {
            text.push_str(&ds.text("plan_risk", &[("text", risk)]));
        }
    }
    if !plan.tasks.is_empty() {
        text.push_str(&ds.text("plan_tasks", &[]));
        for task in &plan.tasks {
            let after = if task.depends_on.is_empty() {
                String::new()
            } else {
                ds.text("plan_task_after", &[("numbers", &task.depends_on.iter().map(u32::to_string).collect::<Vec<_>>().join(", "))])
            };
            text.push_str(&ds.text("plan_task", &[("number", &task.n.to_string()), ("title", &task.title), ("after", &after), ("brief", &task.brief)]));
            for (key, items) in [("plan_task_touches", &task.touches), ("plan_task_checks", &task.checks), ("plan_task_covers", &task.covers), ("plan_task_skills", &task.skills)] {
                if !items.is_empty() {
                    text.push_str(&ds.text(key, &[("items", &items.join(", "))]));
                }
            }
        }
    }
    text
}

/// A plan entry as the model sees it in place, never changed by later progress: whole, or its goal
/// and task titles while it is carried out; one line once a version settled before `settled`
/// replaced it (`newest`: the newest settled version of each plan; see [`crate::projection`]).
pub fn in_place(ds: &Dataset, entry: &JournalEntry, newest: &HashMap<String, u64>, settled: u64) -> Option<String> {
    let plan = parse(entry)?;
    if entry.seq <= settled && newest.get(&plan.plan_id).is_some_and(|&newest| newest > entry.seq) {
        return Some(ds.text("plan_superseded", &[("plan_id", &plan.plan_id), ("version", &plan.version.to_string())]));
    }
    // Set when its work starts, before a work step reads it; its status changes later, so the
    // choice does not follow it.
    let carried_out = entry.data.get("implement_run").and_then(Value::as_str).is_some_and(|run| !run.is_empty()) && plan.root_task.is_some();
    Some(if carried_out { render_compact(ds, &plan) } else { render_with(ds, &plan, placed_heading(ds, &plan)) })
}

/// A plan task as the foreground loop gets it: only the task's own brief and the text of the spec
/// requirements it serves (the whole work is framed once, in one sentence, by the step message).
/// A task as its step states it: its brief, then the requirements it serves, in the user's own
/// words. The only task of a plan is the whole work, so it carries every live
/// requirement; a task among several carries the ones it covers.
pub fn task_focus(ds: &Dataset, task: &PlanTask, spec: Option<&Spec>, whole: bool) -> String {
    let mut text = task.brief.trim().to_owned();
    // The rules, each with its own test: a test written for the code (seen: tests that passed on
    // wrong code) checks less than one written for each thing the request says must hold.
    if !task.rules.is_empty() {
        let items: Vec<String> = task.rules.iter().map(|rule| ds.text("list_item", &[("text", rule)])).collect();
        text.push_str(&ds.text("plan_task_rules", &[("items", &items.join("\n"))]));
    }
    if let Some(spec) = spec {
        let item = |requirement: &crate::spec::Requirement| ds.text("list_item", &[("text", &format!("{}: {}", requirement.id, requirement.text))]);
        let served: Vec<String> = if whole {
            spec.live().map(item).collect()
        } else {
            task.covers.iter().filter_map(|id| spec.requirements.iter().find(|requirement| &requirement.id == id)).map(item).collect()
        };
        if !served.is_empty() {
            text.push_str(&ds.text("plan_task_requirements", &[("items", &served.join("\n"))]));
        }
        text.push_str(&examples_block(ds, spec));
    }
    text
}

/// The request's own examples, right before the work: the check, with its values in view (seen: a
/// task invented 20 cases with values it worked out, several wrong, then spent hours bending
/// correct code to them). Empty when the request gave none.
fn examples_block(ds: &Dataset, spec: &Spec) -> String {
    if spec.examples.is_empty() {
        return String::new();
    }
    let items: Vec<String> = spec.examples.iter().map(|example| ds.text("list_item", &[("text", example)])).collect();
    ds.text("plan_task_examples", &[("items", &items.join("\n"))])
}

/// A plan being carried out by the conversation's own loop, as the model sees it in place: the
/// goal and the task titles, not every task's brief (the briefs of later tasks pulled the first
/// task into doing all the work). No task states: they change at every task's end, and the step
/// message says which task is now and which are done.
pub fn render_compact(ds: &Dataset, plan: &Plan) -> String {
    let mut text = placed_heading(ds, plan);
    if !plan.doc.goal.is_empty() {
        text.push_str(&ds.text("plan_goal", &[("text", &plan.doc.goal)]));
    }
    if !plan.tasks.is_empty() {
        text.push_str(&ds.text("plan_tasks", &[]));
        for task in &plan.tasks {
            text.push_str(&ds.text("plan_task_compact", &[("number", &task.n.to_string()), ("title", &task.title)]));
        }
    }
    text
}

/// The brief of a plan task as an agent (or the foreground loop) gets it: the task's own brief,
/// the plan's goal and approach, and the text of the spec requirements it serves.
pub fn task_brief(ds: &Dataset, plan: &Plan, task: &PlanTask, spec: Option<&Spec>) -> String {
    let mut text = task.brief.clone();
    text.push_str(&ds.text("plan_task_context", &[("plan_id", &plan.plan_id), ("goal", &plan.doc.goal), ("approach", &plan.doc.approach)]));
    if let Some(spec) = spec {
        let served: Vec<String> = task
            .covers
            .iter()
            .filter_map(|id| spec.requirements.iter().find(|requirement| &requirement.id == id))
            .map(|requirement| ds.text("list_item", &[("text", &format!("{}: {}", requirement.id, requirement.text))]))
            .collect();
        if !served.is_empty() {
            text.push_str(&ds.text("plan_task_requirements", &[("items", &served.join("\n"))]));
        }
        text.push_str(&examples_block(ds, spec));
    }
    text
}

/// A plan task's brief for its agent (a memory task): [`task_brief`] and the skills the task chose,
/// never the planning decision's own.
pub fn agent_brief(ds: &Dataset, plan: &Plan, task: &PlanTask, spec: Option<&Spec>) -> String {
    let mut text = task_brief(ds, plan, task, spec);
    if !task.skills.is_empty() {
        text.push_str(&ds.text("delegate_skills", &[("uris", &task.skills.join("\n"))]));
    }
    text
}

/// The DO decision the foreground loop carries out for a plan of at most one task: its brief,
/// files, checks and the task's own skills (the planning decision's skills stay with the plan).
pub fn foreground_decision(ds: &Dataset, plan: &Plan, spec: Option<&Spec>, effort: &str) -> Decision {
    let task = plan.tasks.first().cloned().unwrap_or_default();
    let brief = if plan.tasks.is_empty() { plan.doc.goal.clone() } else { task_brief(ds, plan, &task, spec) };
    Decision {
        missing: String::new(),
        route: "DO".into(),
        effort: effort.to_owned(),
        symbols: Vec::new(),
        files: task.touches,
        queries: Vec::new(),
        brief,
        checks: task.checks,
        skills: task.skills,
        note: String::new(),
    }
}

/// A comment on a plan the user has not sent yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub seq: u64,
    pub task: Option<u32>,
    pub task_title: String,
    pub text: String,
}

/// The open comments on `plan_id`, oldest first.
pub fn open_comments(entries: &[JournalEntry], plan_id: &str) -> Vec<Comment> {
    let field = |entry: &JournalEntry, name: &str| entry.data.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    entries
        .iter()
        .filter(|entry| entry.kind == chatflow_kinds::PLAN_COMMENT && field(entry, "plan_id") == plan_id && field(entry, "state") == "open")
        .map(|entry| Comment {
            seq: entry.seq,
            task: entry.data.get("task").and_then(Value::as_u64).map(|n| n as u32),
            task_title: field(entry, "task_title"),
            text: field(entry, "text"),
        })
        .collect()
}

/// The change request the comments make: the comments on the plan first, then each task's in the
/// plan's order, each under the task's current number and title (a comment on a task removed
/// since names the title it was made on).
pub fn revise_request(ds: &Dataset, plan: &Plan, comments: &[Comment]) -> String {
    let mut lines: Vec<String> = comments.iter().filter(|comment| comment.task.is_none()).map(|comment| ds.text("plan_revise_on_plan", &[("text", comment.text.trim())])).collect();
    let mut on_tasks: Vec<(u32, String, &Comment)> = comments
        .iter()
        .filter(|comment| comment.task.is_some())
        .map(|comment| match plan.tasks.iter().find(|task| task.title == comment.task_title).or_else(|| plan.tasks.iter().find(|task| Some(task.n) == comment.task)) {
            Some(task) => (task.n, task.title.clone(), comment),
            None => (u32::MAX, comment.task_title.clone(), comment),
        })
        .collect();
    on_tasks.sort_by_key(|(n, _, comment)| (*n, comment.seq));
    for (n, title, comment) in on_tasks {
        let number = if n == u32::MAX { "-".to_owned() } else { n.to_string() };
        lines.push(ds.text("plan_revise_on_task", &[("number", &number), ("title", &title), ("text", comment.text.trim())]));
    }
    ds.text("plan_revise_request", &[("items", &lines.join("\n"))])
}

/// The task numbers the PLAN grammar offers for `depends_on`.
pub fn task_numbers(max_tasks: usize) -> Vec<String> {
    (1..=max_tasks).map(|n| n.to_string()).collect()
}

/// A new plan id.
/// Whether two of the tasks could run at the same time: neither depends, directly or through
/// others, on the other. A plan that is one chain has none.
pub fn has_parallel(tasks: &[PlanTask]) -> bool {
    let closure = |task: &PlanTask| {
        let mut all: HashSet<u32> = HashSet::new();
        let mut open: Vec<u32> = task.depends_on.clone();
        while let Some(n) = open.pop() {
            if all.insert(n)
                && let Some(dependency) = tasks.iter().find(|other| other.n == n)
            {
                open.extend(dependency.depends_on.iter().copied());
            }
        }
        all
    };
    let closures: Vec<HashSet<u32>> = tasks.iter().map(closure).collect();
    tasks.iter().enumerate().any(|(i, a)| tasks.iter().enumerate().skip(i + 1).any(|(j, b)| !closures[j].contains(&a.n) && !closures[i].contains(&b.n)))
}

pub fn new_id() -> String {
    format!("plan-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}
