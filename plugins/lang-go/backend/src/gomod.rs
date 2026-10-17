//! go.mod: the module path, the required modules (direct and `// indirect`), their sources in the
//! module cache, `replace` directives (local directories and other modules) and `tool` directives.
//!
//! The format is the one `golang.org/x/mod/modfile` reads: one directive per line (`module`, `go`,
//! `toolchain`, `godebug`, `require`, `replace`, `exclude`, `retract`, `tool`, `ignore`), or a
//! directive followed by a parenthesised block of lines; `//` comments; tokens optionally quoted
//! with `"` or a backquote. A required version is exact (minimal version selection), so the
//! source of `path@version` in the module cache is the source the build uses; it is reported only
//! when that directory exists. Local `replace` targets are resolved only when the extraction context
//! names the repository root.

use cc_lang_kit::ExtractContext;
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts};
use cc_lang_kit::manifest::dir_of;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The module cache (`$GOMODCACHE`, else `$GOPATH/pkg/mod` of GOPATH's first entry, else
/// `~/go/pkg/mod`, Go's documented defaults), read once.
fn module_cache() -> &'static [PathBuf] {
    static CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let dir = var("GOMODCACHE").map(PathBuf::from).or_else(|| var("GOPATH").and_then(|p| std::env::split_paths(&p).next()).map(|p| p.join("pkg").join("mod"))).or_else(|| {
            var("HOME").map(|home| PathBuf::from(home).join("go").join("pkg").join("mod"))
        });
        dir.into_iter().filter(|d| d.is_dir()).collect()
    })
}

pub fn extract(path: &str, text: &str, ctx: &ExtractContext, facts: &mut FileFacts) {
    extract_with(path, text, ctx.root.as_deref(), module_cache(), facts);
}

/// One directive line: its verb (the block's verb inside a block), its tokens, whether it carries
/// an `// indirect` comment, and its 1-based line.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    verb: String,
    tokens: Vec<String>,
    indirect: bool,
    line: u32,
}

const VERBS: &[&str] = &["module", "go", "toolchain", "godebug", "require", "replace", "exclude", "retract", "tool", "ignore"];

/// Split one line into tokens (quotes removed) and its comment.
fn tokenize(line: &str) -> Result<(Vec<String>, String), ()> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        match ch {
            c if c.is_whitespace() => {}
            '/' if line[at..].starts_with("//") => return Ok((tokens, line[at + 2..].trim().to_owned())),
            '"' | '`' => {
                let mut token = String::new();
                let mut closed = false;
                let mut escape = false;
                for (_, c) in chars.by_ref() {
                    if escape {
                        token.push(c);
                        escape = false;
                    } else if c == '\\' && ch == '"' {
                        escape = true;
                    } else if c == ch {
                        closed = true;
                        break;
                    } else {
                        token.push(c);
                    }
                }
                if !closed {
                    return Err(());
                }
                tokens.push(token);
            }
            '(' | ')' => tokens.push(ch.to_string()),
            _ => {
                let mut token = String::from(ch);
                while let Some(&(_, c)) = chars.peek() {
                    if c.is_whitespace() || c == '"' || c == '`' || c == '(' || c == ')' {
                        break;
                    }
                    token.push(c);
                    chars.next();
                }
                tokens.push(token);
            }
        }
    }
    Ok((tokens, String::new()))
}

/// The directive lines of a go.mod, and how many lines did not parse.
fn directives(text: &str) -> (Vec<Line>, u32) {
    let mut out = Vec::new();
    let mut errors = 0;
    let mut block: Option<String> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index as u32 + 1;
        let Ok((tokens, comment)) = tokenize(raw) else {
            errors += 1;
            continue;
        };
        let indirect = comment.split([';', ' ']).any(|w| w == "indirect");
        if tokens.is_empty() {
            continue;
        }
        if let Some(verb) = &block {
            if tokens == [")"] {
                block = None;
            } else {
                out.push(Line { verb: verb.clone(), tokens, indirect, line });
            }
            continue;
        }
        let verb = tokens[0].clone();
        if !VERBS.contains(&verb.as_str()) {
            errors += 1;
            continue;
        }
        if tokens.len() == 2 && tokens[1] == "(" {
            block = Some(verb);
            continue;
        }
        out.push(Line { verb, tokens: tokens[1..].to_vec(), indirect, line });
    }
    if block.is_some() {
        errors += 1;
    }
    (out, errors)
}

/// The module cache's directory name of a module path or version: every upper-case letter becomes
/// `!` and its lower-case form (golang.org/x/mod/module.EscapePath).
pub fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        if ch.is_ascii_uppercase() {
            out.push('!');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// `<cache>/<escaped path>@<escaped version>` when it is a directory.
fn cached_source(cache: &[PathBuf], path: &str, version: &str) -> Option<PathBuf> {
    cache.iter().map(|dir| dir.join(format!("{}@{}", escape_path(path), escape_path(version)))).find(|d| d.is_dir())
}

/// A `replace` target that is a directory: a path beginning with `./`, `../` or `/` (modfile's
/// rule for local replacements).
fn is_local(target: &str) -> bool {
    target.starts_with("./") || target.starts_with("../") || target.starts_with('/') || target == "." || target == ".."
}

/// The package name a tool's import path binds: its last element, skipping a major-version element.
fn tool_name(path: &str) -> &str {
    let elements: Vec<&str> = path.split('/').filter(|e| !e.is_empty()).collect();
    let is_major = |e: &str| e.len() > 1 && e.starts_with('v') && e[1..].chars().all(|c| c.is_ascii_digit());
    match elements.as_slice() {
        [.., prev, last] if is_major(last) => prev,
        [.., last] => last,
        [] => path,
    }
}

/// [`extract`] with an explicit module cache.
pub fn extract_with(path: &str, text: &str, root: Option<&Path>, cache: &[PathBuf], facts: &mut FileFacts) {
    let (lines, errors) = directives(text);
    facts.parse_errors = errors;
    let dir = dir_of(path);
    // `replace old [v] => new [v]`: by old path (and version, when given).
    let mut replaces: Vec<(String, Option<String>, String, Option<String>)> = Vec::new();
    for line in lines.iter().filter(|l| l.verb == "replace") {
        let Some(arrow) = line.tokens.iter().position(|t| t == "=>") else {
            facts.parse_errors += 1;
            continue;
        };
        let (old, new) = line.tokens.split_at(arrow);
        let new = &new[1..];
        match (old, new) {
            ([old, rest @ ..], [new, nv @ ..]) if rest.len() <= 1 && nv.len() <= 1 => replaces.push((old.clone(), rest.first().cloned(), new.clone(), nv.first().cloned())),
            _ => facts.parse_errors += 1,
        }
    }
    for line in &lines {
        match line.verb.as_str() {
            "module" => facts.package = line.tokens.first().cloned(),
            "require" => {
                let [module, version] = line.tokens.as_slice() else {
                    facts.parse_errors += 1;
                    continue;
                };
                let replaced = replaces.iter().find(|(old, ov, _, _)| old == module && ov.as_ref().is_none_or(|v| v == version));
                let (installed_version, source_dir) = match replaced {
                    Some((_, _, new, None)) if is_local(new) => {
                        let local = root.map(|root| if dir.is_empty() { root.join(new) } else { root.join(dir).join(new) }).filter(|p| p.is_dir());
                        (None, local)
                    }
                    Some((_, _, new, Some(nv))) => (cached_source(cache, new, nv).map(|_| nv.clone()), cached_source(cache, new, nv)),
                    _ => {
                        let found = cached_source(cache, module, version);
                        (found.as_ref().map(|_| version.clone()), found)
                    }
                };
                facts.dependencies.push(DependencyFact {
                    name: module.clone(),
                    ecosystem: "go".into(),
                    spec: version.clone(),
                    section: if line.indirect { "indirect".into() } else { "dependencies".into() },
                    installed_version,
                    source_dir: source_dir.map(|p| p.to_string_lossy().into_owned()),
                });
            }
            "tool" => {
                let Some(tool) = line.tokens.first() else { continue };
                let name = tool_name(tool);
                facts.commands.push(CommandFact {
                    name: format!("tool:{name}"),
                    command: format!("go tool {name}"),
                    line: line.line,
                    runs_files: Vec::new(),
                    runs_commands: Vec::new(),
                    entry_point: Some(tool.clone()),
                });
            }
            _ => {}
        }
    }
    if facts.package.is_none() && !text.trim().is_empty() {
        // A go.mod without a module directive is not a module (`go` refuses it).
        facts.parse_errors += 1;
    }
}
