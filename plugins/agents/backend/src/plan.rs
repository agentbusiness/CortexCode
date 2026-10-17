//! Delegation planning: the model splits a goal into a task tree over graph context, and the host
//! makes sure tasks that run in parallel have disjoint impact sets.
//!
//! The cut is mechanical once each task's impact set is known (set intersection over files): two
//! tasks without a dependency path between them may run concurrently only when their sets are
//! disjoint. Overlaps go back to the model once; whatever still overlaps is serialized with a
//! dependency edge so it can never run in parallel.

use crate::schemas::PlannedTask;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Problems that make a plan unusable as written (structural validation).
pub fn validate(tasks: &[PlannedTask]) -> Vec<String> {
    let mut problems = Vec::new();
    if tasks.is_empty() {
        problems.push("the plan has no tasks".to_owned());
    }
    let mut titles = BTreeSet::new();
    for task in tasks {
        let title = task.title.trim();
        if title.is_empty() {
            problems.push("a task has an empty title".to_owned());
        } else if !titles.insert(title.to_owned()) {
            problems.push(format!("two tasks are titled \"{title}\""));
        }
        if !task.checks.iter().any(|check| !check.trim().is_empty()) {
            problems.push(format!("task \"{title}\" has no check command"));
        }
    }
    for task in tasks {
        for dependency in &task.depends_on {
            if !titles.contains(dependency.trim()) {
                problems.push(format!("task \"{}\" depends on unknown task \"{dependency}\"", task.title.trim()));
            }
        }
    }
    if problems.is_empty() && order(tasks).is_none() {
        problems.push("the dependencies form a cycle".to_owned());
    }
    problems
}

/// Task indexes in dependency order, or None for a cycle.
pub fn order(tasks: &[PlannedTask]) -> Option<Vec<usize>> {
    let index: HashMap<&str, usize> = tasks.iter().enumerate().map(|(i, task)| (task.title.trim(), i)).collect();
    let mut state = vec![0u8; tasks.len()];
    let mut out = Vec::new();
    fn visit(i: usize, tasks: &[PlannedTask], index: &HashMap<&str, usize>, state: &mut [u8], out: &mut Vec<usize>) -> bool {
        match state[i] {
            1 => return false,
            2 => return true,
            _ => {}
        }
        state[i] = 1;
        for dependency in &tasks[i].depends_on {
            if let Some(&j) = index.get(dependency.trim())
                && !visit(j, tasks, index, state, out)
            {
                return false;
            }
        }
        state[i] = 2;
        out.push(i);
        true
    }
    for i in 0..tasks.len() {
        if !visit(i, tasks, &index, &mut state, &mut out) {
            return None;
        }
    }
    Some(out)
}

/// Whether `a` depends (directly or transitively) on `b`.
fn reaches(tasks: &[PlannedTask], a: usize, b: usize) -> bool {
    let index: HashMap<&str, usize> = tasks.iter().enumerate().map(|(i, task)| (task.title.trim(), i)).collect();
    let mut stack = vec![a];
    let mut seen = BTreeSet::new();
    while let Some(current) = stack.pop() {
        if current == b && current != a {
            return true;
        }
        if !seen.insert(current) {
            continue;
        }
        for dependency in &tasks[current].depends_on {
            if let Some(&next) = index.get(dependency.trim()) {
                if next == b {
                    return true;
                }
                stack.push(next);
            }
        }
    }
    false
}

/// Pairs of tasks that could run in parallel but share part of their impact sets, with the shared
/// items.
pub fn overlaps(tasks: &[PlannedTask], impact: &[BTreeSet<String>]) -> Vec<(usize, usize, Vec<String>)> {
    let mut found = Vec::new();
    for a in 0..tasks.len() {
        for b in (a + 1)..tasks.len() {
            if reaches(tasks, a, b) || reaches(tasks, b, a) {
                continue;
            }
            let shared: Vec<String> = impact[a].intersection(&impact[b]).cloned().collect();
            if !shared.is_empty() {
                found.push((a, b, shared));
            }
        }
    }
    found
}

/// Serialize every remaining overlap: the later task (in plan order) waits for the earlier one.
pub fn serialize(tasks: &mut [PlannedTask], impact: &[BTreeSet<String>]) -> Vec<String> {
    let mut added = Vec::new();
    loop {
        let pending = overlaps(tasks, impact);
        let Some((a, b, shared)) = pending.into_iter().next() else { break };
        let title = tasks[a].title.trim().to_owned();
        tasks[b].depends_on.push(title.clone());
        added.push(format!("\"{}\" now waits for \"{title}\" (both affect {})", tasks[b].title.trim(), shared.join(", ")));
    }
    added
}

/// Describe overlaps for the model's second attempt.
pub fn describe_overlaps(data: &crate::data::Dataset, tasks: &[PlannedTask], found: &[(usize, usize, Vec<String>)]) -> String {
    let mut text = data.text("overlaps", &[]);
    for (a, b, shared) in found {
        text.push_str(&data.text("overlap", &[("a", tasks[*a].title.trim()), ("b", tasks[*b].title.trim()), ("shared", &shared.join(", "))]));
    }
    text
}

/// Group impact set items by task for reporting.
pub fn impact_summary(tasks: &[PlannedTask], impact: &[BTreeSet<String>]) -> BTreeMap<String, Vec<String>> {
    tasks.iter().zip(impact).map(|(task, set)| (task.title.trim().to_owned(), set.iter().cloned().collect())).collect()
}
