//! What a failing check's error goes through, shown with it; and the same error surviving edits.
//!
//! A compiler error points at a line. When its cause is a type declared elsewhere (a field whose
//! type cannot do what the line asks), the model edits that line again and again: the line number
//! is the strongest cue in the output (seen: a dozen rewrites of one function while the node type
//! kept its element in a `Cell`, which never lends a reference). So the host shows, right under a
//! failing run's error, the declarations of the project's types that the error's line uses (its
//! fields' owners and the types it names), read from the code graph's outline and the file. And it
//! keys each failure by its error lines without numbers, so the same error after several edits is
//! known structurally, whatever the command or the line numbers.

use crate::host::Host;
use crate::impact::{relative, scope_args};
use crate::reads::Scope;
use cc_protocol::contracts::memory::{OutlineResult, SymbolDetail, SymbolRef, methods as memory_methods};
use regex::Regex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Symbol kinds that declare a type.
const TYPE_KINDS: &[&str] = &["struct", "class", "enum", "trait", "interface", "type", "union", "record"];
/// Symbol kinds that declare a member of a type.
const FIELD_KINDS: &[&str] = &["field", "property", "attribute"];
/// Errors looked at per failing run, and declarations shown.
const SITES: usize = 3;
const SHOWN: usize = 3;
/// A longer declaration is shown as its first line and its fields.
const WHOLE_LINES: usize = 25;

/// A line that starts an error report (compiler, runtime or test runner), in any language.
static HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(^|[\s:\[])(error|fatal|panicked|exception|assertionerror|failed)\b|\b[A-Z]\w*(Error|Exception)\b").expect("valid"));
/// A source location: `path:line[:col]`, `path(line,col)`, `path:[line,col]`, or a Python frame.
static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"File "([^"]+)", line (\d+)|([A-Za-z0-9_./\\-]+\.[A-Za-z0-9]+)(?::(\d+)|\((\d+),\d+\)|:\[(\d+),\d+\])"#).expect("valid")
});
/// A name the error quotes: in backticks or single quotes.
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`|'([^'\s][^']*)'").expect("valid"));
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("valid"));

/// One reported error: where it points, and the names it quotes.
#[derive(Debug, PartialEq)]
struct Site {
    path: String,
    line: usize,
    names: HashSet<String>,
}

fn location(line: &str) -> Option<(String, usize)> {
    let found = LOCATION.captures(line)?;
    let path = found.get(1).or_else(|| found.get(3))?.as_str().to_owned();
    let number = [2, 4, 5, 6].iter().find_map(|index| found.get(*index))?.as_str().parse().ok()?;
    Some((path, number))
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    WORD.find_iter(text).map(|word| word.as_str().to_owned())
}

/// The first errors of `output`: each from its head line to the next head, located at the first
/// location in it. Output without a head line (a traceback ending in its error) is one error at
/// its last location (the innermost frame).
fn sites(output: &str) -> Vec<Site> {
    let lines: Vec<&str> = output.lines().collect();
    let heads: Vec<usize> = lines.iter().enumerate().filter(|(_, line)| HEAD.is_match(line)).map(|(index, _)| index).collect();
    let quoted = |block: &[&str]| -> HashSet<String> {
        block.iter().flat_map(|line| QUOTED.captures_iter(line).filter_map(|found| found.get(1).or_else(|| found.get(2))).flat_map(|name| words(name.as_str()).collect::<Vec<_>>()).collect::<Vec<_>>()).collect()
    };
    let mut out: Vec<Site> = Vec::new();
    for (index, &start) in heads.iter().enumerate() {
        let end = heads.get(index + 1).copied().unwrap_or(lines.len());
        let block = &lines[start..end];
        let Some((path, line)) = block.iter().find_map(|line| location(line)) else { continue };
        if out.iter().any(|site| site.path == path && site.line == line) {
            continue;
        }
        out.push(Site { path, line, names: quoted(block) });
        if out.len() == SITES {
            break;
        }
    }
    if out.is_empty()
        && let Some((path, line)) = lines.iter().rev().find_map(|line| location(line))
    {
        out.push(Site { path, line, names: quoted(&lines) });
    }
    out
}

/// `path` relative to the workspace when it is inside it (macOS names temp folders both as `/var`
/// and `/private/var`, so both spellings count), else as given.
pub fn project_relative(scope: &Scope, path: &str) -> String {
    let plain = |text: &str| text.strip_prefix("/private").unwrap_or(text).to_owned();
    let workspace = plain(&scope.workspace);
    let given = plain(path);
    match given.strip_prefix(&format!("{}/", workspace.trim_end_matches('/'))) {
        Some(rest) => rest.to_owned(),
        None => relative(scope, path),
    }
}

/// The type a member belongs to: its qualified name without the last part.
fn owner(symbol: &SymbolRef) -> Option<&str> {
    symbol.qname.rsplit_once("::").or_else(|| symbol.qname.rsplit_once('.')).map(|(owner, _)| owner)
}

async fn outline(host: &Host, scope: &Scope, path: &str) -> Vec<SymbolRef> {
    let Ok(value) = host.call(memory_methods::OUTLINE, scope_args(scope, json!({"path": path}))).await else { return Vec::new() };
    serde_json::from_value::<OutlineResult>(value).map(|outline| outline.symbols).unwrap_or_default()
}

/// The declarations of the project's types that the first errors of a failing run go through: the
/// owners of the fields their lines use and the types their lines or messages name. `None` when
/// the output points at no project type.
pub async fn declarations(host: &Host, scope: &Scope, output: &str) -> Option<String> {
    let mut files: HashMap<String, (Vec<SymbolRef>, Vec<String>)> = HashMap::new();
    let mut picked: Vec<SymbolRef> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    for site in sites(output) {
        let path = project_relative(scope, &site.path);
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            continue;
        }
        if !files.contains_key(&path) {
            let Ok(text) = std::fs::read_to_string(std::path::Path::new(&scope.workspace).join(&path)) else { continue };
            let symbols = outline(host, scope, &path).await;
            files.insert(path.clone(), (symbols, text.lines().map(str::to_owned).collect()));
        }
        let (symbols, text) = &files[&path];
        let mut used: HashSet<String> = text.get(site.line.saturating_sub(1)).map(|line| words(line).collect()).unwrap_or_default();
        used.extend(site.names.iter().cloned());
        for symbol in symbols {
            let kind = symbol.kind.as_str();
            let ty = if FIELD_KINDS.contains(&kind) && used.contains(&symbol.name) {
                owner(symbol)
            } else if TYPE_KINDS.contains(&kind) && used.contains(&symbol.name) {
                Some(symbol.qname.as_str())
            } else {
                None
            };
            if let Some(ty) = ty
                && let Some(found) = symbols.iter().find(|candidate| TYPE_KINDS.contains(&candidate.kind.as_str()) && candidate.qname == ty)
                && !picked.iter().any(|known| known.sid == found.sid)
            {
                picked.push(found.clone());
            }
        }
        // Types the message names that this file does not declare: looked up in the project.
        named.extend(site.names.iter().filter(|name| name.chars().next().is_some_and(char::is_uppercase) && !symbols.iter().any(|symbol| &symbol.name == *name)).cloned());
    }
    named.sort();
    named.dedup();
    for name in named.into_iter().take(SHOWN) {
        if picked.len() >= SHOWN {
            break;
        }
        let Ok(value) = host.call(memory_methods::SYMBOL, scope_args(scope, json!({"name": name}))).await else { continue };
        let Ok(detail) = serde_json::from_value::<SymbolDetail>(value) else { continue };
        if TYPE_KINDS.contains(&detail.symbol.kind.as_str()) && detail.symbol.name == name && !picked.iter().any(|known| known.sid == detail.symbol.sid) {
            picked.push(detail.symbol);
        }
    }
    picked.truncate(SHOWN);
    let mut items = Vec::new();
    for ty in &picked {
        if !files.contains_key(&ty.path) {
            let Ok(text) = std::fs::read_to_string(std::path::Path::new(&scope.workspace).join(&ty.path)) else { continue };
            let symbols = outline(host, scope, &ty.path).await;
            files.insert(ty.path.clone(), (symbols, text.lines().map(str::to_owned).collect()));
        }
        let (symbols, text) = &files[&ty.path];
        let (start, end) = (ty.start_line as usize, ty.end_line as usize);
        if start == 0 || start > text.len() {
            continue;
        }
        let shown: Vec<&str> = if end + 1 - start <= WHOLE_LINES {
            text[start - 1..end.min(text.len())].iter().map(String::as_str).collect()
        } else {
            // A long declaration: its first line and its members' lines.
            let members = symbols.iter().filter(|symbol| FIELD_KINDS.contains(&symbol.kind.as_str()) && owner(symbol) == Some(ty.qname.as_str()));
            std::iter::once(text[start - 1].as_str()).chain(members.filter_map(|member| text.get(member.start_line as usize - 1).map(String::as_str))).collect()
        };
        items.push(format!("{}:{}\n{}", ty.path, start, shown.join("\n")));
    }
    (!items.is_empty()).then(|| items.join("\n\n"))
}

/// A line that opens an error report at its start (a compiler's `error[E0599]:`, a `Traceback`, a
/// `TypeError:`, a test runner's `FAILED`): what a failing run prints even when a pipe (`| head`,
/// `| sed`) turned its exit status into 0. A search result's lines start with a path instead.
static REPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(error\b|error\[|fatal\b|panicked\b|FAIL\b|FAILED\b|Traceback\b|[A-Z]\w*(Error|Exception)\b)").expect("valid"));

/// Whether a run failed: its exit status, or, when that is 0, an error report its output opens.
pub fn failed(exit: i64, output: &str) -> bool {
    exit != 0 || REPORT.is_match(output)
}

/// The key of a failing run's errors: its first error lines without numbers or spacing, so the
/// same error matches across commands and moved lines. `None` when the output has no error line.
pub fn error_key(output: &str) -> Option<u64> {
    let heads: Vec<String> = output
        .lines()
        .filter(|line| HEAD.is_match(line))
        .take(5)
        .map(|line| line.chars().filter(|c| !c.is_ascii_digit()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    (!heads.is_empty()).then(|| crate::repetition::fingerprint(&heads.join("\n")))
}

