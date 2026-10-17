//! The language plugins (`lang@1`, one per language: `lang-rust`, `lang-go`, ...) as the tools see
//! them: the plugin for a file, its language server's diagnostics after an edit, and the
//! `definition://`, `references://`, `hover://` and `diagnostics://` URIs `read` serves.
//!
//! Plugins are found with `frame.plugins.list` (contract `lang@1`) and asked once per instance what
//! they cover (`describe`); a swapped plugin is a new instance and is asked again, and a plugin whose
//! skills changed ([`SKILLS_CHANGED`]) is asked again too.

use crate::codeskills::CodeHit;
use crate::host::Host;
use cc_protocol::contracts::lang::{Description, Diagnostic, DiagnosticsResult, HoverResult, LocationsResult, SkillInfo, methods};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// How long an edit waits for the language server's diagnostics of the edited file.
pub const EDIT_DIAGNOSTICS_WAIT_MS: u64 = 4000;
/// At most this many diagnostics are listed after an edit (all are counted).
pub const EDIT_DIAGNOSTICS_SHOWN: usize = 20;
/// Said when a language server answered before it had finished.
pub const PARTIAL: &str = " (the checker was still working; the list may be incomplete)";
/// A workspace-wide read whose servers had nothing yet.
const NOTHING_YET: &str = "no errors or warnings known yet (the language servers were still starting or analysing; read diagnostics:// again later)";

/// Published by every plugin that serves skills when its skills changed (`{category}`): its cached
/// description is dropped ([`forget`]), so the next catalog and code index carry the new skills.
pub const SKILLS_CHANGED: &str = cc_protocol::contracts::skills::topics::CHANGED;

/// Drop the cached descriptions of the plugin serving `category`.
pub fn forget(category: &str) {
    if let Ok(mut cache) = cache().lock() {
        cache.retain(|_, (cached, _)| cached != category);
    }
    if let Ok(mut cache) = skills_cache().lock() {
        cache.retain(|_, (cached, _)| cached != category);
    }
}

/// Descriptions by plugin instance.
fn cache() -> &'static Mutex<HashMap<String, (String, Description)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, Description)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The active language plugins: (category, description).
pub async fn plugins(host: &dyn Host) -> Vec<(String, Description)> {
    let Ok(listed) = host.call("frame.plugins.list", json!({})).await else { return Vec::new() };
    let entries = listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut found = Vec::new();
    for entry in entries {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        if field("contract") != cc_protocol::contracts::lang::CONTRACT || field("state") != "active" {
            continue;
        }
        let (category, instance) = (field("category"), field("instance"));
        let cached = cache().lock().ok().and_then(|cache| cache.get(&instance).cloned());
        let description = match cached {
            Some((_, description)) => description,
            None => match host.call(&format!("{category}.{}", methods::DESCRIBE), json!({})).await {
                Ok(value) => match serde_json::from_value::<Description>(value) {
                    Ok(description) => {
                        if let Ok(mut cache) = cache().lock() {
                            cache.insert(instance, (category.clone(), description.clone()));
                        }
                        description
                    }
                    Err(_) => continue,
                },
                Err(_) => continue,
            },
        };
        found.push((category, description));
    }
    found
}

/// Every language plugin's toolchain for commands: PATH directories and variables, in a stable
/// order (by plugin category).
pub async fn toolchain(host: &dyn Host) -> crate::shell::Toolchain {
    let mut plugins = plugins(host).await;
    plugins.sort_by(|a, b| a.0.cmp(&b.0));
    let mut toolchain = crate::shell::Toolchain::default();
    for (_, description) in plugins {
        for dir in description.path {
            if !toolchain.path.contains(&dir) {
                toolchain.path.push(dir);
            }
        }
        toolchain.env.extend(description.env);
    }
    toolchain
}

/// Which plugin covers a file: by exact file name, then file-name pattern, then extension, then the
/// `#!` interpreter.
pub fn pick<'a>(plugins: &'a [(String, Description)], path: &Path, first_line: Option<&str>) -> Option<&'a (String, Description)> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let extension = path.extension().map(|ext| ext.to_string_lossy().to_ascii_lowercase());
    let covers =
        |test: &dyn Fn(&cc_protocol::contracts::lang::LanguageInfo) -> bool| plugins.iter().find(|(_, d)| d.languages.iter().any(test));
    if let Some(found) = covers(&|language| language.filenames.contains(&name)) {
        return Some(found);
    }
    if let Some(found) =
        covers(&|language| language.patterns.iter().any(|pattern| cc_protocol::contracts::lang::name_matches(pattern, &name)))
    {
        return Some(found);
    }
    if let Some(extension) = &extension
        && let Some(found) = covers(&|language| language.extensions.iter().any(|e| e.eq_ignore_ascii_case(extension)))
    {
        return Some(found);
    }
    let interpreter = shebang_interpreter(first_line?)?;
    covers(&|language| language.shebangs.contains(&interpreter))
}

/// `#!/usr/bin/env python3` -> `python3`; `#!/bin/bash -e` -> `bash`.
pub fn shebang_interpreter(line: &str) -> Option<String> {
    let rest = line.strip_prefix("#!")?.trim();
    let mut words = rest.split_whitespace();
    let program = words.next()?;
    let program = if program.ends_with("/env") { words.find(|word| !word.starts_with('-'))? } else { program };
    Some(program.rsplit('/').next().unwrap_or(program).to_owned())
}

/// The language plugin for a workspace file (its category).
pub async fn plugin_for(host: &dyn Host, absolute: &Path) -> Option<String> {
    let plugins = plugins(host).await;
    let first_line = std::fs::read(absolute).ok().and_then(|bytes| {
        let end = bytes.iter().position(|b| *b == b'\n').unwrap_or(bytes.len()).min(256);
        String::from_utf8(bytes[..end].to_vec()).ok()
    });
    pick(&plugins, absolute, first_line.as_deref()).map(|(category, _)| category.clone())
}

/// The language server's view of a file after an edit: its errors and warnings in the file (errors
/// first), whether the server had finished, and the codes of the diagnostics listed (for
/// [`crate::codeskills`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditDiagnostics {
    pub items: Vec<Diagnostic>,
    pub partial: bool,
    pub hits: Vec<CodeHit>,
}

/// After an edit: the language server's errors and warnings in the edited file. None when no
/// language plugin covers the file or it has no server.
pub async fn after_edit(host: &dyn Host, workspace: &Path, absolute: &Path, display: &str) -> Option<EditDiagnostics> {
    let category = plugin_for(host, absolute).await?;
    let params = json!({"root": workspace.display().to_string(), "paths": [display], "wait_ms": EDIT_DIAGNOSTICS_WAIT_MS});
    let value = host.call(&format!("{category}.{}", methods::DIAGNOSTICS), params).await.ok()?;
    let result: DiagnosticsResult = serde_json::from_value(value).ok()?;
    Some(EditDiagnostics {
        items: listed(&result, display).into_iter().cloned().collect(),
        partial: result.partial,
        hits: diagnostic_hits(&result, display, &category, EDIT_DIAGNOSTICS_SHOWN),
    })
}

/// `1 error`, `2 errors, 1 warning` (no zero counts).
pub fn counts(errors: usize, warnings: usize) -> String {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(plural(errors, "error"));
    }
    if warnings > 0 {
        parts.push(plural(warnings, "warning"));
    }
    parts.join(", ")
}

/// A diagnostic's message whole: up to 3 lines, later ones indented under the first.
pub fn message_text(message: &str) -> String {
    message.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()).take(3).collect::<Vec<_>>().join("\n  ")
}

/// The errors and warnings listed for `only` (every file when empty), errors first.
fn listed<'a>(result: &'a DiagnosticsResult, only: &str) -> Vec<&'a Diagnostic> {
    let mut items: Vec<&Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| (only.is_empty() || d.path == only) && matches!(d.severity.as_str(), "error" | "warning"))
        .collect();
    items.sort_by_key(|d| (d.severity != "error", d.path.clone(), d.line, d.column));
    items
}

/// The structured codes of the diagnostics [`diagnostics_text`] lists (the first `shown`), in its
/// order (each once), reported by the language plugin `category`.
pub fn diagnostic_hits(result: &DiagnosticsResult, only: &str, category: &str, shown: usize) -> Vec<CodeHit> {
    let mut hits: Vec<CodeHit> = Vec::new();
    for code in listed(result, only).into_iter().take(shown).filter_map(|d| d.code.as_deref()) {
        let hit = CodeHit { owner: category.to_owned(), code: code.to_owned() };
        if !hits.contains(&hit) {
            hits.push(hit);
        }
    }
    hits
}

/// Diagnostics as text: errors first, then warnings (hints and infos are not shown after edits).
pub fn diagnostics_text(result: &DiagnosticsResult, only: &str, shown: usize) -> String {
    let items = listed(result, only);
    let errors = items.iter().filter(|d| d.severity == "error").count();
    let warnings = items.len() - errors;
    let partial = if result.partial { PARTIAL } else { "" };
    if items.is_empty() {
        return format!("No errors or warnings{partial}.");
    }
    let mut text = format!("{}{partial}:", counts(errors, warnings));
    for d in items.iter().take(shown) {
        let code = d.code.as_deref().map(|code| format!(" [{code}]")).unwrap_or_default();
        let _ = write!(text, "\n{}:{} {}{code}: {}", d.path, d.line, d.severity, message_text(&d.message));
    }
    if items.len() > shown {
        let _ = write!(text, "\n... {} more (read diagnostics://{only})", items.len() - shown);
    }
    text
}

/// `definition://`, `references://`, `hover://` (path:line:column) and `diagnostics://` (path or
/// empty for every file the server reported).
pub async fn read_uri(host: &dyn Host, workspace: &Path, scheme: &str, target: &str) -> Result<String, String> {
    let root = workspace.display().to_string();
    if scheme == "diagnostics" {
        return read_diagnostics(host, workspace, target, DiagnosticsRead::default()).await.map(|(text, _)| text);
    }
    let (path, line, column) = position(target).ok_or_else(|| format!("`{scheme}://` needs path:line:column, got `{target}`"))?;
    let category = plugin_for(host, &workspace.join(&path)).await.ok_or_else(|| not_covered(&path))?;
    let method = match scheme {
        "definition" => methods::DEFINITION,
        "references" => methods::REFERENCES,
        "hover" => methods::HOVER,
        other => return Err(format!("unknown language URI `{other}://`")),
    };
    let params = json!({"root": root, "path": path, "line": line, "column": column});
    let value = host.call(&format!("{category}.{method}"), params).await.map_err(|e| e.message)?;
    if scheme == "hover" {
        let hover: HoverResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
        // The server's markdown fences carry nothing the model needs.
        let text = hover.text.lines().filter(|line| !line.trim_start().starts_with("```")).collect::<Vec<_>>().join("\n");
        let text = text.trim();
        return Ok(if text.is_empty() { format!("nothing known at {path}:{line}:{column}") } else { text.to_owned() });
    }
    let found: LocationsResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if found.locations.is_empty() {
        return Ok(format!("no {scheme} found for {path}:{line}:{column}"));
    }
    let lines: Vec<String> = found
        .locations
        .iter()
        .map(|location| match location.line_text.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
            Some(code) => format!("{}:{}  {code}", location.path, location.line),
            None => format!("{}:{}", location.path, location.line),
        })
        .collect();
    Ok(lines.join("\n"))
}

/// A file no language plugin covers, and where to look instead.
fn not_covered(path: &str) -> String {
    format!("no language plugin covers {path}; read the file, or search with graph://search/<words>.")
}

/// How a `diagnostics://` read asks the language servers. The model's reads take the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiagnosticsRead {
    /// How long a server may take to settle (None: its plugin's default; 0: what it has now).
    pub wait_ms: Option<u64>,
    /// At most this many diagnostics listed per server, errors first, the rest counted (None: all).
    pub shown: Option<usize>,
}

/// `diagnostics://<path>` (empty: every file the servers reported): the text, and the codes of the
/// diagnostics it lists with the plugin that reported each. A server that cannot answer is a line
/// of the text; a workspace-wide read leaves out servers with nothing yet (one line says so).
/// Err only when no language plugin covers `path`.
pub async fn read_diagnostics(host: &dyn Host, workspace: &Path, target: &str, how: DiagnosticsRead) -> Result<(String, Vec<CodeHit>), String> {
    let root = workspace.display().to_string();
    let shown = how.shown.unwrap_or(usize::MAX);
    let path = target.trim_matches('/');
    let category = if path.is_empty() {
        None
    } else {
        Some(plugin_for(host, &workspace.join(path)).await.ok_or_else(|| not_covered(path))?)
    };
    let categories = match category {
        Some(category) => vec![category],
        None => plugins(host).await.into_iter().filter(|(_, d)| d.server.as_ref().is_some_and(|s| s.installed)).map(|(c, _)| c).collect(),
    };
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    let mut unfinished = 0;
    for category in categories {
        let mut params = json!({"root": root, "paths": if path.is_empty() { json!([]) } else { json!([path]) }});
        if let Some(wait) = how.wait_ms {
            params["wait_ms"] = json!(wait);
        }
        let answer = host.call(&format!("{category}.{}", methods::DIAGNOSTICS), params).await.map_err(|e| e.message);
        match answer.and_then(|value| serde_json::from_value::<DiagnosticsResult>(value).map_err(|e| e.to_string())) {
            Ok(result) if path.is_empty() && result.partial && listed(&result, path).is_empty() => unfinished += 1,
            Ok(result) => {
                lines.push(diagnostics_text(&result, path, shown));
                hits.extend(diagnostic_hits(&result, path, &category, shown));
            }
            Err(message) => lines.push(message),
        }
    }
    if lines.is_empty() {
        lines.push(if unfinished > 0 { NOTHING_YET } else { "no language server is running for this workspace" }.to_owned());
    }
    Ok((lines.join("\n"), hits))
}

/// `skill://lang-rust/rust-errors`: one skill of a language plugin; `skill://` lists them all.
pub async fn read_skill(host: &dyn Host, path: &str) -> Result<String, String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        let mut text = String::from("Skills (read skill://<plugin>/<name>):");
        for (category, skills) in skill_sources(host).await {
            for skill in &skills {
                let _ = write!(text, "\n  skill://{category}/{}  {}", skill.name, skill.summary);
            }
        }
        return Ok(text);
    }
    let (category, name) =
        path.split_once('/').ok_or_else(|| format!("`skill://{path}` needs <plugin>/<name>, e.g. skill://lang-rust/rust-errors"))?;
    let value = host.call(&format!("{category}.{}", methods::SKILL), json!({"name": name})).await.map_err(|e| e.message)?;
    let skill: cc_protocol::contracts::lang::SkillResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(skill.text)
}

/// Skills plugins' descriptions by plugin instance, with their category.
fn skills_cache() -> &'static Mutex<HashMap<String, (String, cc_protocol::contracts::skills::Description)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, cc_protocol::contracts::skills::Description)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every plugin that carries skills ([`cc_protocol::contracts::skills::CARRIERS`]): language
/// plugins (`lang@1`), skills plugins (`skills@1`) and the memory plugin's own skills, with their
/// skills, by category. Descriptions are asked once per plugin instance (again after [`forget`]).
pub async fn skill_sources(host: &dyn Host) -> Vec<(String, Vec<SkillInfo>)> {
    let mut sources: Vec<(String, Vec<SkillInfo>)> =
        plugins(host).await.into_iter().map(|(category, description)| (category, description.skills)).collect();
    let Ok(listed) = host.call("frame.plugins.list", json!({})).await else { return sources };
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        let contract = field("contract");
        // Language plugins came with `plugins` above.
        if contract == cc_protocol::contracts::lang::CONTRACT || field("state") != "active" {
            continue;
        }
        let Some(listing) = cc_protocol::contracts::skills::listing_method(&contract) else { continue };
        let (category, instance) = (field("category"), field("instance"));
        let cached = skills_cache().lock().ok().and_then(|cache| cache.get(&instance).map(|(_, description)| description.clone()));
        let description = match cached {
            Some(description) => Some(description),
            None => match host.call(&format!("{category}.{listing}"), json!({})).await {
                Ok(value) => serde_json::from_value::<cc_protocol::contracts::skills::Description>(value).ok().inspect(|description| {
                    if let Ok(mut cache) = skills_cache().lock() {
                        cache.insert(instance, (category.clone(), description.clone()));
                    }
                }),
                Err(_) => None,
            },
        };
        if let Some(description) = description {
            sources.push((category, description.skills));
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

/// URIs whose path ends in a position (`definition://src/a.rs:12:5`), not a line range.
pub fn is_position_uri(target: &str) -> bool {
    ["definition://", "references://", "hover://"].iter().any(|scheme| target.starts_with(scheme))
}

/// `src/a.rs:12:5` -> ("src/a.rs", 12, 5); a missing column is 1.
pub fn position(target: &str) -> Option<(String, u32, u32)> {
    let target = target.trim_matches('/');
    let mut parts = target.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    match (parts.next(), middle.parse::<u32>(), last.parse::<u32>()) {
        (Some(path), Ok(line), Ok(column)) if !path.is_empty() => Some((path.to_owned(), line, column)),
        (None, _, Ok(line)) if !middle.is_empty() => Some((middle.to_owned(), line, 1)),
        (Some(path), Err(_), Ok(line)) => Some((format!("{path}:{middle}"), line, 1)),
        _ => None,
    }
}
