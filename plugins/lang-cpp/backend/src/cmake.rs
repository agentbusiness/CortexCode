//! CMakeLists.txt: the project name, the targets it builds (executables and libraries, with the
//! source files they compile), its tests, and the packages it finds or fetches.
//!
//! Structural, after the CMake language reference: a file is command invocations `name(args)` with
//! `#` line comments, `#[[ ]]` bracket comments, quoted arguments (`"..."`, backslash escapes),
//! bracket arguments (`[[...]]`, `[=[...]=]`) and unquoted arguments; command names are
//! case-insensitive. Arguments holding `${...}` variable references are not resolved: a source
//! list entry is reported as a file only when it is a literal path that exists under the
//! repository root. FetchContent sources are reported where CMake puts them
//! (`<root>/build/_deps/<name>-src`), only when that directory exists.

use cc_lang_kit::ExtractContext;
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts};
use cc_lang_kit::manifest::{dir_of, is_repo_file, join_rel};

/// One command invocation: its lower-cased name, arguments and 1-based line.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub name: String,
    pub args: Vec<String>,
    pub line: u32,
}

/// The length of a bracket opening `[=*[` at `chars[i..]`, as its `=` count.
fn bracket_open(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'[') {
        return None;
    }
    let mut j = i + 1;
    while chars.get(j) == Some(&'=') {
        j += 1;
    }
    (chars.get(j) == Some(&'[')).then_some(j - i - 1)
}

/// The index past the bracket close `]=*]` with `level` `=` signs, searching from `from`.
fn bracket_close(chars: &[char], from: usize, level: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == ']' && chars[i + 1..].iter().take(level).all(|c| *c == '=') && chars.get(i + 1 + level) == Some(&']') && i + 1 + level < chars.len() {
            return Some((i, i + level + 2));
        }
        i += 1;
    }
    None
}

/// The command invocations of a CMake file; None when an argument list, string or bracket does not
/// end.
pub fn parse(text: &str) -> Option<Vec<Invocation>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    let count_lines = |from: usize, to: usize| chars[from..to].iter().filter(|c| **c == '\n').count() as u32;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '#' {
            if let Some(level) = bracket_open(&chars, i + 1) {
                let (_, end) = bracket_close(&chars, i + level + 3, level)?;
                line += count_lines(i, end);
                i = end;
            } else {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect::<String>().to_ascii_lowercase();
            let invocation_line = line;
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if chars.get(i) != Some(&'(') {
                return None;
            }
            i += 1;
            let mut args = Vec::new();
            let mut depth = 0;
            loop {
                let &c = chars.get(i)?;
                match c {
                    '\n' => {
                        line += 1;
                        i += 1;
                    }
                    c if c.is_whitespace() => i += 1,
                    '#' => {
                        if let Some(level) = bracket_open(&chars, i + 1) {
                            let (_, end) = bracket_close(&chars, i + level + 3, level)?;
                            line += count_lines(i, end);
                            i = end;
                        } else {
                            while i < chars.len() && chars[i] != '\n' {
                                i += 1;
                            }
                        }
                    }
                    ')' if depth == 0 => {
                        i += 1;
                        break;
                    }
                    '(' | ')' => {
                        // Nested parentheses (`if()` conditions) are arguments of their own.
                        depth += if c == '(' { 1 } else { -1 };
                        args.push(c.to_string());
                        i += 1;
                    }
                    '"' => {
                        let mut value = String::new();
                        i += 1;
                        loop {
                            let &c = chars.get(i)?;
                            match c {
                                '\\' => {
                                    match chars.get(i + 1) {
                                        Some('n') => value.push('\n'),
                                        Some('t') => value.push('\t'),
                                        Some('\n') => line += 1,
                                        Some(&other) => value.push(other),
                                        None => return None,
                                    }
                                    i += 2;
                                }
                                '"' => {
                                    i += 1;
                                    break;
                                }
                                _ => {
                                    if c == '\n' {
                                        line += 1;
                                    }
                                    value.push(c);
                                    i += 1;
                                }
                            }
                        }
                        args.push(value);
                    }
                    '[' if bracket_open(&chars, i).is_some() => {
                        let level = bracket_open(&chars, i).unwrap_or(0);
                        let (close, end) = bracket_close(&chars, i + level + 2, level)?;
                        args.push(chars[i + level + 2..close].iter().collect());
                        line += count_lines(i, end);
                        i = end;
                    }
                    _ => {
                        let start = i;
                        while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '(' | ')' | '"' | '#') {
                            if chars[i] == '\\' {
                                i += 1;
                            }
                            i += 1;
                        }
                        args.push(chars[start..i.min(chars.len())].iter().collect());
                    }
                }
            }
            out.push(Invocation { name, args, line: invocation_line });
        } else {
            return None;
        }
    }
    Some(out)
}

/// Keywords of add_executable / add_library that are not sources (CMake command reference).
const TARGET_KEYWORDS: &[&str] = &["WIN32", "MACOSX_BUNDLE", "EXCLUDE_FROM_ALL", "STATIC", "SHARED", "MODULE", "OBJECT", "INTERFACE"];

pub fn extract(path: &str, text: &str, ctx: &ExtractContext, facts: &mut FileFacts) {
    let Some(invocations) = parse(text) else {
        facts.parse_errors = 1;
        return;
    };
    let root = ctx.root.as_deref();
    let dir = dir_of(path);
    let mut targets: Vec<String> = Vec::new();
    for call in &invocations {
        let args = &call.args;
        match call.name.as_str() {
            "project" if facts.package.is_none() => facts.package = args.first().cloned(),
            "add_executable" | "add_library" => {
                let Some(name) = args.first() else { continue };
                if args.iter().any(|a| a == "IMPORTED" || a == "ALIAS") {
                    continue;
                }
                let runs_files: Vec<String> = args[1..]
                    .iter()
                    .filter(|a| !TARGET_KEYWORDS.contains(&a.as_str()) && !a.contains("${") && !a.contains("$<"))
                    .map(|a| join_rel(dir, a))
                    .filter(|rel| is_repo_file(root, rel))
                    .collect();
                targets.push(name.clone());
                facts.commands.push(CommandFact {
                    name: format!("target:{name}"),
                    command: format!("cmake --build build --target {name}"),
                    line: call.line,
                    runs_files,
                    runs_commands: Vec::new(),
                    entry_point: None,
                });
            }
            "add_test" => {
                // `add_test(NAME <name> COMMAND <command> ...)` or the old `add_test(<name> <command> ...)`.
                let (name, command) = match args.as_slice() {
                    [keyword, name, rest @ ..] if keyword == "NAME" => (name.clone(), rest.iter().position(|a| a == "COMMAND").and_then(|p| rest.get(p + 1)).cloned()),
                    [name, command, ..] => (name.clone(), Some(command.clone())),
                    _ => continue,
                };
                let runs_commands = command.filter(|c| targets.contains(c)).map(|c| vec![format!("target:{c}")]).unwrap_or_default();
                facts.commands.push(CommandFact {
                    name: format!("test:{name}"),
                    command: format!("ctest --test-dir build -R ^{name}$"),
                    line: call.line,
                    runs_files: Vec::new(),
                    runs_commands,
                    entry_point: None,
                });
            }
            "find_package" => {
                let Some(name) = args.first() else { continue };
                let spec = args.get(1).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())).cloned().unwrap_or_default();
                let section = if args.iter().any(|a| a == "REQUIRED") { "dependencies" } else { "optional" };
                facts.dependencies.push(DependencyFact { name: name.clone(), ecosystem: "cmake".into(), spec, section: section.into(), installed_version: None, source_dir: None });
            }
            "fetchcontent_declare" => {
                let Some(name) = args.first() else { continue };
                let value = |key: &str| args.iter().position(|a| a == key).and_then(|p| args.get(p + 1)).cloned();
                let spec = value("GIT_TAG").or_else(|| value("URL")).or_else(|| value("GIT_REPOSITORY")).unwrap_or_default();
                let source_dir = root.map(|r| r.join("build").join("_deps").join(format!("{}-src", name.to_ascii_lowercase()))).filter(|d| d.is_dir());
                facts.dependencies.push(DependencyFact {
                    name: name.clone(),
                    ecosystem: "cmake".into(),
                    spec,
                    section: "fetched".into(),
                    installed_version: None,
                    source_dir: source_dir.map(|d| d.to_string_lossy().into_owned()),
                });
            }
            _ => {}
        }
    }
}
