//! Host-driven impact injection: when an edit changes a symbol's signature, the host lists every
//! call site (with hashline anchors and the arguments as written) and the affected tests, so no call
//! site is forgotten.
//!
//! Before/after signatures come from the code graph's outline of each file: the outline seen when
//! the file was read (or first changed) is the "before", the outline after `memory.refresh` is the
//! "after". Symbols are matched by qualified name (mechanical); a changed signature text, or a
//! symbol that disappeared, triggers `memory.impact`.

use crate::data::Dataset;
use crate::host::Host;
use crate::reads::Scope;
use cc_protocol::contracts::memory::{CallSite, ImpactResult, OutlineResult, SymbolRef, methods as memory_methods};
use cc_protocol::contracts::tools::{ReadResult, methods as tools_methods};
use cc_protocol::hashline;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Default)]
pub struct Tracker {
    /// path -> qualified name -> symbol, as last seen.
    outlines: HashMap<String, BTreeMap<String, SymbolRef>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignatureChange {
    pub before: SymbolRef,
    /// None when the symbol no longer exists under that name.
    pub after: Option<SymbolRef>,
}

fn scope_args(scope: &Scope, extra: Value) -> Value {
    let mut value = if scope.workspace == scope.root {
        json!({"project_id": scope.project_id})
    } else {
        json!({"project_id": scope.project_id, "workspace": scope.workspace})
    };
    if let (Some(map), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        for (key, item) in extra {
            map.insert(key.clone(), item.clone());
        }
    }
    value
}

/// Normalize a path reported by a tool to workspace-relative form.
pub fn relative(scope: &Scope, path: &str) -> String {
    let trimmed = path.strip_prefix(&scope.workspace).map(|rest| rest.trim_start_matches('/')).unwrap_or(path);
    trimmed.trim_start_matches("./").to_owned()
}

/// Mechanical diff of two outlines: changed signature text, or a vanished qualified name.
pub fn diff(before: &BTreeMap<String, SymbolRef>, after: &BTreeMap<String, SymbolRef>) -> Vec<SignatureChange> {
    let mut changes = Vec::new();
    for (qname, old) in before {
        // Only callable or type-like symbols have call sites worth listing.
        if matches!(old.kind.as_str(), "file" | "module" | "variable" | "field") {
            continue;
        }
        match after.get(qname) {
            Some(new) if normalize(&new.signature) != normalize(&old.signature) => {
                changes.push(SignatureChange { before: old.clone(), after: Some(new.clone()) })
            }
            Some(_) => {}
            None => changes.push(SignatureChange { before: old.clone(), after: None }),
        }
    }
    changes
}

/// Whitespace-insensitive signature comparison (layout is not a signature change).
fn normalize(signature: &str) -> String {
    signature.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Tracker {
    async fn outline(host: &Host, scope: &Scope, path: &str) -> Option<BTreeMap<String, SymbolRef>> {
        let value = host.call(memory_methods::OUTLINE, scope_args(scope, json!({"path": path}))).await.ok()?;
        let outline: OutlineResult = serde_json::from_value(value).ok()?;
        Some(outline.symbols.into_iter().map(|symbol| (symbol.qname.clone(), symbol)).collect())
    }

    /// Remember a file's current outline (after a read), unless already known.
    pub async fn remember(&mut self, host: &Host, scope: &Scope, path: &str) {
        let path = relative(scope, path);
        if self.outlines.contains_key(&path) {
            return;
        }
        if let Some(outline) = Self::outline(host, scope, &path).await {
            self.outlines.insert(path, outline);
        }
    }

    /// After files changed: refresh the graph, diff outlines, and build the impact message (None
    /// when no signature changed or nothing depends on the changed ones).
    pub async fn after_edit(&mut self, host: &Host, scope: &Scope, changed_files: &[String], data: &Dataset) -> Option<String> {
        let paths: Vec<String> = changed_files.iter().map(|path| relative(scope, path)).collect::<HashSet<_>>().into_iter().collect();
        if paths.is_empty() {
            return None;
        }
        let mut before_all = HashMap::new();
        for path in &paths {
            let before = match self.outlines.get(path) {
                Some(known) => Some(known.clone()),
                // Not read through the tools first: the graph may still hold the pre-edit outline.
                None => Self::outline(host, scope, path).await,
            };
            before_all.insert(path.clone(), before);
        }
        let refreshed = host.call(memory_methods::REFRESH, scope_args(scope, json!({"paths": paths}))).await;
        if let Err(error) = refreshed {
            tracing::warn!(%error, "memory.refresh failed; impact injection skipped for this edit");
            return None;
        }
        let mut changes = Vec::new();
        for path in &paths {
            let Some(after) = Self::outline(host, scope, path).await else { continue };
            if let Some(Some(before)) = before_all.get(path) {
                changes.extend(diff(before, &after));
            }
            self.outlines.insert(path.clone(), after);
        }
        if changes.is_empty() {
            return None;
        }
        let mut sids: Vec<String> = Vec::new();
        for change in &changes {
            sids.push(change.before.sid.clone());
            if let Some(after) = &change.after
                && after.sid != change.before.sid
            {
                sids.push(after.sid.clone());
            }
        }
        sids.sort();
        sids.dedup();
        let impact: ImpactResult = host
            .get(memory_methods::IMPACT, scope_args(scope, json!({"sids": sids, "paths": [], "depth": 3})))
            .await
            .ok()?;
        let sites = sites_outside(&impact.call_sites, &changes);
        if sites.is_empty() && impact.tests.is_empty() {
            return None;
        }
        let mut anchored = Vec::new();
        for site in &sites {
            anchored.push((site.clone(), anchor_line(host, scope, &site.path, site.line).await));
        }
        Some(render(data, &changes, &anchored, &impact.tests))
    }
}

/// Direct call sites (depth 1) that are not inside one of the changed symbols' own bodies.
fn sites_outside(sites: &[CallSite], changes: &[SignatureChange]) -> Vec<CallSite> {
    let mut seen = HashSet::new();
    sites
        .iter()
        .filter(|site| site.depth <= 1)
        .filter(|site| {
            !changes.iter().any(|change| {
                let symbol = change.after.as_ref().unwrap_or(&change.before);
                symbol.path == site.path && site.line >= symbol.start_line && site.line <= symbol.end_line
            })
        })
        .filter(|site| seen.insert((site.path.clone(), site.line)))
        .cloned()
        .collect()
}

/// The call site's current line as `N:hh|text`, read from the workspace.
async fn anchor_line(host: &Host, scope: &Scope, path: &str, line: u32) -> String {
    let read = host
        .call(tools_methods::READ, json!({"workspace": scope.workspace, "project_id": scope.project_id, "target": path, "range": format!("{line}-{line}")}))
        .await
        .ok()
        .and_then(|value| serde_json::from_value::<ReadResult>(value).ok());
    if let Some(read) = read
        && let Some(rendered) = read.text.lines().find(|text| hashline::parse_anchor(text).is_some_and(|anchor| anchor.line == line as usize))
    {
        return rendered.to_owned();
    }
    // Fall back to the file on disk (same tag function as the tools plugin).
    let full = std::path::Path::new(&scope.workspace).join(path);
    match std::fs::read_to_string(full) {
        Ok(text) => hashline::render(&text, line as usize, line as usize).trim_end().to_owned(),
        Err(_) => format!("{line}:??|(could not read the line)"),
    }
}

fn render(data: &Dataset, changes: &[SignatureChange], sites: &[(CallSite, String)], tests: &[SymbolRef]) -> String {
    let mut text = data.text("impact_heading", &[]);
    for change in changes {
        match &change.after {
            Some(after) => text.push_str(&data.text(
                "impact_changed",
                &[("name", &change.before.qname), ("path", &after.path), ("before", change.before.signature.trim()), ("after", after.signature.trim())],
            )),
            None => text.push_str(&data.text(
                "impact_vanished",
                &[("name", &change.before.qname), ("path", &change.before.path), ("before", change.before.signature.trim())],
            )),
        }
    }
    if !sites.is_empty() {
        text.push_str(&data.text("impact_sites", &[]));
        for (site, line) in sites {
            text.push_str(&data.text("impact_site", &[("path", &site.path), ("line", line), ("args", &site.args.join(", "))]));
        }
    }
    if !tests.is_empty() {
        text.push_str(&data.text("impact_tests", &[]));
        for test in tests {
            text.push_str(&data.text("impact_test", &[("name", &test.qname), ("path", &test.path)]));
        }
    }
    text.push_str(&data.text("impact_close", &[]));
    text
}
