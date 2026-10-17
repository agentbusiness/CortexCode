//! Ask-mode gate for `run`: finds actions that leave the machine or cannot be undone by parsing the
//! command with tree-sitter-bash and inspecting each simple command structurally (program name,
//! subcommand, literal arguments, redirection targets). No text search over the command string.
//!
//! Classes:
//! - `git_push`: `git push` (and `git send-email`).
//! - `publish`: package/registry publishing and GitHub mutations (`npm publish`, `cargo publish`,
//!   `gh pr merge`, `gh release create`, `gh api`, ...).
//! - `network`: network clients (curl, wget, ssh, scp, rsync to a host, nc, ...) talking to a host
//!   that is not this machine.
//! - `delete_outside`: deleting files outside the session workspace (rm, rmdir, unlink, shred,
//!   `find -delete` / `-exec rm`).
//! - `overwrite_outside`: `>` redirection truncating an existing file outside the workspace.
//! - `unresolved`: a program name or a deletion target that is only known at run time
//!   (variables, command substitution), so it cannot be checked.

use crate::util;
use serde::Serialize;
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Concern {
    pub class: &'static str,
    pub detail: String,
    /// The file or directory outside the workspace that the action would delete or overwrite (the
    /// write sandbox must be widened there for the action to work once approved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<std::path::PathBuf>,
}

pub struct Context<'a> {
    pub cwd: &'a Path,
    pub workspace: &'a Path,
}

pub fn assess(command: &str, context: &Context) -> Vec<Concern> {
    let mut concerns = Vec::new();
    assess_into(command, context, 0, &mut concerns);
    concerns.dedup();
    concerns
}

fn assess_into(script: &str, context: &Context, depth: usize, out: &mut Vec<Concern>) {
    // Nesting bound for `bash -c '...'` inside `bash -c` (structural recursion guard).
    if depth > 6 {
        out.push(Concern { path: None, class: "unresolved", detail: "deeply nested shell invocations".to_owned() });
        return;
    }
    let Some(tree) = crate::lang::parse(crate::lang::Lang::Bash, script) else {
        out.push(Concern { path: None, class: "unresolved", detail: "the command could not be parsed".to_owned() });
        return;
    };
    let source = script.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "command" => inspect_command(node, source, context, depth, out),
            "file_redirect" => inspect_redirect(node, source, context, out),
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// The words of a script that is exactly one simple command whose program and arguments are all
/// literal (no pipes, lists, redirections, assignments or expansions); None otherwise.
pub fn simple_argv(script: &str) -> Option<Vec<String>> {
    let tree = crate::lang::parse(crate::lang::Lang::Bash, script)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let statements: Vec<Node> = root.named_children(&mut cursor).filter(|node| node.kind() != "comment").collect();
    let [command] = statements.as_slice() else { return None };
    if command.kind() != "command" {
        return None;
    }
    let source = script.as_bytes();
    let mut words = vec![literal(command.child_by_field_name("name")?, source)?];
    let mut cursor = command.walk();
    for child in command.named_children(&mut cursor) {
        match child.kind() {
            "command_name" | "comment" => {}
            "variable_assignment" | "file_redirect" | "herestring_redirect" => return None,
            _ => words.push(literal(child, source)?),
        }
    }
    Some(words)
}

/// Every simple command of a script (in pipelines, lists, subshells, `if`/`for` bodies), as its
/// program and arguments: literal words, None for a word only known at run time. Redirections and
/// variable assignments are not words. Empty when the script does not parse.
pub fn simple_commands(script: &str) -> Vec<Vec<Option<String>>> {
    let Some(tree) = crate::lang::parse(crate::lang::Lang::Bash, script) else { return Vec::new() };
    let source = script.as_bytes();
    let mut commands = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "command"
            && let Some(name) = node.child_by_field_name("name")
        {
            let mut words = vec![literal(name, source)];
            words.extend(arguments(node, source));
            commands.push((node.start_byte(), words));
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    commands.sort_by_key(|(start, _)| *start);
    commands.into_iter().map(|(_, words)| words).collect()
}

/// Literal value of an argument node, or None when it depends on run-time expansion.
fn literal(node: Node, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    match node.kind() {
        "word" | "number" => Some(unescape_word(text)),
        "raw_string" => Some(text.trim_start_matches('\'').trim_end_matches('\'').to_owned()),
        "string" => {
            let mut cursor = node.walk();
            let dynamic = node.named_children(&mut cursor).any(|child| child.kind() != "string_content");
            if dynamic {
                return None;
            }
            let inner = text.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(text);
            Some(unescape_double(inner))
        }
        "concatenation" => {
            let mut cursor = node.walk();
            let mut out = String::new();
            for child in node.children(&mut cursor) {
                out.push_str(&literal(child, source)?);
            }
            Some(out)
        }
        "command_name" => {
            let child = node.named_child(0)?;
            literal(child, source)
        }
        _ => None,
    }
}

fn unescape_word(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_double(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.peek().copied()
            && matches!(next, '"' | '\\' | '$' | '`')
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

/// Arguments of a command node as literals (None entries are dynamic).
fn arguments(node: Node, source: &[u8]) -> Vec<Option<String>> {
    let mut cursor = node.walk();
    node.children_by_field_name("argument", &mut cursor).map(|arg| literal(arg, source)).collect()
}

fn program_name(text: &str) -> &str {
    text.rsplit('/').next().unwrap_or(text)
}

fn inspect_command(node: Node, source: &[u8], context: &Context, depth: usize, out: &mut Vec<Concern>) {
    let Some(name_node) = node.child_by_field_name("name") else { return };
    let Some(name) = literal(name_node, source) else {
        out.push(Concern {
            path: None,
            class: "unresolved",
            detail: format!("the program `{}` is only known at run time", name_node.utf8_text(source).unwrap_or("?")),
        });
        return;
    };
    let mut words: Vec<Option<String>> = vec![Some(name)];
    words.extend(arguments(node, source));
    classify(&words, context, depth, out);
}

/// Wrappers that run another command: skip their own options to find the real program.
fn unwrap(words: &[Option<String>]) -> Option<(usize, bool)> {
    let first = words.first()?.as_deref().map(program_name)?;
    let mut index = 1;
    let skip_options = |index: &mut usize, takes_value: &[&str]| {
        while let Some(Some(word)) = words.get(*index) {
            if word == "--" {
                *index += 1;
                break;
            }
            if !word.starts_with('-') || word == "-" {
                break;
            }
            *index += 1;
            if takes_value.contains(&word.as_str()) {
                *index += 1;
            }
        }
    };
    match first {
        "sudo" | "doas" => skip_options(&mut index, &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U"]),
        "command" | "exec" | "nohup" | "builtin" | "time" => skip_options(&mut index, &["-a"]),
        "nice" => skip_options(&mut index, &["-n"]),
        "env" => {
            skip_options(&mut index, &["-u", "-C", "-S", "-P"]);
            while let Some(Some(word)) = words.get(index) {
                if word.contains('=') && !word.starts_with('=') {
                    index += 1;
                } else {
                    break;
                }
            }
        }
        "timeout" | "gtimeout" => {
            skip_options(&mut index, &["-s", "-k", "--signal", "--kill-after"]);
            index += 1; // the duration
        }
        "xargs" => {
            skip_options(&mut index, &["-I", "-n", "-P", "-L", "-s", "-d", "-E", "-J", "-R", "-S"]);
            return Some((index, true));
        }
        _ => return None,
    }
    Some((index, false))
}

fn classify(words: &[Option<String>], context: &Context, depth: usize, out: &mut Vec<Concern>) {
    let Some(Some(first)) = words.first() else { return };
    if let Some((index, dynamic_args)) = unwrap(words) {
        if index >= words.len() {
            return;
        }
        if words[index].is_none() {
            out.push(Concern { path: None, class: "unresolved", detail: format!("`{first}` runs a program only known at run time") });
            return;
        }
        let mut rest: Vec<Option<String>> = words[index..].to_vec();
        if dynamic_args {
            // xargs appends arguments read from stdin.
            rest.push(None);
        }
        return classify(&rest, context, depth, out);
    }
    let program = program_name(first);
    let args: Vec<Option<String>> = words[1..].to_vec();
    let literal_args = || args.iter().flatten().cloned().collect::<Vec<String>>();
    match program {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            if let Some(position) =
                args.iter().position(|arg| arg.as_deref().is_some_and(|a| a.starts_with('-') && a.contains('c') && !a.starts_with("--")))
            {
                match args.get(position + 1) {
                    Some(Some(script)) => assess_into(script, context, depth + 1, out),
                    Some(None) => out.push(Concern {
                        path: None,
                        class: "unresolved",
                        detail: format!("`{program} -c` with a script only known at run time"),
                    }),
                    None => {}
                }
            }
        }
        "eval" => {
            if args.iter().any(Option::is_none) {
                out.push(Concern { path: None, class: "unresolved", detail: "`eval` of text only known at run time".to_owned() });
            } else {
                assess_into(&literal_args().join(" "), context, depth + 1, out);
            }
        }
        "git" => {
            let sub = git_subcommand(&args);
            match sub.as_deref() {
                Some("push") => out.push(Concern { path: None, class: "git_push", detail: format!("git {}", literal_args().join(" ")) }),
                Some("send-email") => out.push(Concern { path: None, class: "network", detail: "git send-email".to_owned() }),
                _ => {}
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" | "cargo" | "gem" | "twine" | "poetry" | "flit" | "docker" | "podman" | "dotnet" | "mvn"
        | "gradle" => {
            let sub = args.iter().flatten().find(|arg| !arg.starts_with('-')).cloned().unwrap_or_default();
            let publishing = match program {
                "twine" => sub == "upload",
                "docker" | "podman" => sub == "push",
                "dotnet" => sub == "nuget",
                "mvn" | "gradle" => sub == "deploy" || sub == "publish",
                "gem" => sub == "push",
                _ => sub == "publish",
            };
            if publishing {
                out.push(Concern { path: None, class: "publish", detail: format!("{program} {}", literal_args().join(" ")) });
            }
        }
        "gh" => {
            let positional: Vec<String> = args.iter().flatten().filter(|arg| !arg.starts_with('-')).cloned().collect();
            let area = positional.first().map(String::as_str).unwrap_or("");
            let action = positional.get(1).map(String::as_str).unwrap_or("");
            let read_only = matches!(action, "view" | "list" | "status" | "diff" | "checks" | "") && area != "api";
            if !read_only {
                out.push(Concern { path: None, class: "publish", detail: format!("gh {}", literal_args().join(" ")) });
            }
        }
        "curl" | "wget" | "http" | "https" | "xh" | "xhs" | "httpie" => network_urls(program, &args, out),
        "ssh" | "sftp" | "telnet" | "ftp" | "mosh" => {
            match first_positional(
                &args,
                &["-p", "-i", "-l", "-o", "-F", "-J", "-L", "-R", "-D", "-b", "-c", "-E", "-e", "-m", "-O", "-Q", "-S", "-w", "-W", "-P"],
            ) {
                Some(Some(host)) => {
                    let host = host.rsplit('@').next().unwrap_or(&host).to_owned();
                    if !is_local_host(&host) {
                        out.push(Concern { path: None, class: "network", detail: format!("{program} {host}") });
                    }
                }
                Some(None) => {
                    out.push(Concern { path: None, class: "unresolved", detail: format!("{program} to a host only known at run time") })
                }
                None => {}
            }
        }
        "scp" | "rsync" => {
            for arg in &args {
                match arg {
                    Some(arg) if !arg.starts_with('-') => {
                        if let Some((host, _)) = arg.split_once(':')
                            && !host.contains('/')
                        {
                            let host = host.rsplit('@').next().unwrap_or(host);
                            if !host.is_empty() && !is_local_host(host) {
                                out.push(Concern { path: None, class: "network", detail: format!("{program} to {host}") });
                            }
                        }
                    }
                    None => out.push(Concern {
                        path: None,
                        class: "unresolved",
                        detail: format!("{program} with an argument only known at run time"),
                    }),
                    _ => {}
                }
            }
        }
        "nc" | "ncat" | "netcat" | "socat" => {
            if let Some(Some(host)) = first_positional(&args, &["-p", "-s", "-w", "-x", "-X", "-i", "-q"])
                && !is_local_host(&host)
            {
                out.push(Concern { path: None, class: "network", detail: format!("{program} {host}") });
            }
        }
        "rm" | "rmdir" | "unlink" | "shred" | "srm" | "trash" => {
            let mut options_done = false;
            for arg in &args {
                match arg {
                    Some(arg) if !options_done && arg == "--" => options_done = true,
                    Some(arg) if !options_done && arg.starts_with('-') && arg.len() > 1 => {}
                    Some(target) => check_delete_target(program, target, context, out),
                    None => {
                        out.push(Concern { path: None, class: "unresolved", detail: format!("{program} of a path only known at run time") })
                    }
                }
            }
        }
        "find" => {
            let deletes = args.iter().flatten().any(|arg| arg == "-delete")
                || args.windows(2).any(|pair| {
                    matches!(&pair[0], Some(flag) if flag == "-exec" || flag == "-execdir" || flag == "-ok")
                        && matches!(&pair[1], Some(cmd) if matches!(program_name(cmd), "rm" | "rmdir" | "unlink" | "shred"))
                });
            if deletes {
                let mut roots = Vec::new();
                for arg in &args {
                    match arg {
                        Some(arg) if arg.starts_with('-') || arg == "(" || arg == "!" => break,
                        other => roots.push(other.clone()),
                    }
                }
                if roots.is_empty() {
                    roots.push(Some(".".to_owned()));
                }
                for root in roots {
                    match root {
                        Some(root) => check_delete_target("find -delete", &root, context, out),
                        None => out.push(Concern {
                            path: None,
                            class: "unresolved",
                            detail: "find -delete under a path only known at run time".to_owned(),
                        }),
                    }
                }
            }
        }
        "dd" => {
            for arg in args.iter().flatten() {
                if let Some(target) = arg.strip_prefix("of=")
                    && !target.starts_with("/dev/null")
                {
                    let path = util::resolve(context.cwd, target);
                    if !util::is_inside(&path, context.workspace) {
                        out.push(Concern {
                            path: Some(util::canonical_lenient(&path)),
                            class: "overwrite_outside",
                            detail: format!("dd writes {}", path.display()),
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

fn git_subcommand(args: &[Option<String>]) -> Option<String> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let arg = arg.as_deref()?;
        if matches!(arg, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path") {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(arg.to_owned());
    }
    None
}

fn first_positional(args: &[Option<String>], takes_value: &[&str]) -> Option<Option<String>> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg {
            Some(word) if word.starts_with('-') => {
                index += if takes_value.contains(&word.as_str()) { 2 } else { 1 };
            }
            other => return Some(other.clone()),
        }
    }
    None
}

/// Value-taking options of curl/wget/httpie (their CLI grammars), so option values are not
/// mistaken for URLs.
const URL_TOOL_VALUE_OPTIONS: &[&str] = &[
    "-o",
    "--output",
    "-H",
    "--header",
    "-d",
    "--data",
    "--data-raw",
    "--data-binary",
    "--data-urlencode",
    "-X",
    "--request",
    "-u",
    "--user",
    "-A",
    "--user-agent",
    "-e",
    "--referer",
    "-b",
    "--cookie",
    "-c",
    "--cookie-jar",
    "-T",
    "--upload-file",
    "-F",
    "--form",
    "-w",
    "--write-out",
    "-m",
    "--max-time",
    "--connect-timeout",
    "-x",
    "--proxy",
    "-K",
    "--config",
    "-r",
    "--range",
    "-E",
    "--cert",
    "--key",
    "--cacert",
    "--resolve",
    "-O",
    "--output-document",
    "-P",
    "--directory-prefix",
    "-t",
    "--tries",
    "-T",
    "--timeout",
    "-U",
    "-a",
    "--append-output",
    "-i",
    "--input-file",
    "--header",
    "--post-data",
    "--post-file",
    "--body-data",
    "--method",
];

fn network_urls(program: &str, args: &[Option<String>], out: &mut Vec<Concern>) {
    let mut index = 0;
    let mut saw_target = false;
    while index < args.len() {
        match &args[index] {
            Some(word) if word.starts_with('-') => {
                // curl -O / wget -O differ: curl's -O takes no value. Handle the known exception.
                let takes = URL_TOOL_VALUE_OPTIONS.contains(&word.as_str()) && !(program == "curl" && word == "-O") && !word.contains('=');
                index += if takes { 2 } else { 1 };
                continue;
            }
            Some(word) => {
                // httpie: `http POST host` and `key=value` items.
                let is_method =
                    matches!(program, "http" | "https" | "xh" | "xhs" | "httpie") && word.chars().all(|c| c.is_ascii_uppercase());
                let is_item = matches!(program, "http" | "https" | "xh" | "xhs" | "httpie") && saw_target;
                if !is_method && !is_item {
                    saw_target = true;
                    let host = url_host(word);
                    if !is_local_host(&host) {
                        out.push(Concern { path: None, class: "network", detail: format!("{program} {word}") });
                    }
                }
            }
            None => {
                out.push(Concern { path: None, class: "unresolved", detail: format!("{program} with an argument only known at run time") });
                saw_target = true;
            }
        }
        index += 1;
    }
}

/// Host part of a URL or `host[:port][/path]`.
pub fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(stripped) = authority.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(stripped).to_owned();
    }
    authority.split(':').next().unwrap_or(authority).to_lowercase()
}

pub fn is_local_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']').to_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host == "0.0.0.0"
        || host.starts_with("127.")
        || host == "host.docker.internal"
}

fn check_delete_target(program: &str, target: &str, context: &Context, out: &mut Vec<Concern>) {
    // A glob is checked through its fixed directory prefix.
    let (fixed, is_glob) = match target.find(['*', '?', '[']) {
        Some(position) => {
            let prefix = &target[..position];
            match prefix.rfind('/') {
                Some(slash) => (prefix[..=slash].to_owned(), true),
                None => (".".to_owned(), true),
            }
        }
        None => (target.to_owned(), false),
    };
    let path = util::canonical_lenient(&util::resolve(context.cwd, &fixed));
    let root = util::canonical_lenient(context.workspace);
    if !path.starts_with(&root) {
        out.push(Concern { path: Some(path.clone()), class: "delete_outside", detail: format!("{program} {}", path.display()) });
    } else if path == root && !is_glob && program != "find -delete" {
        out.push(Concern {
            path: None,
            class: "delete_outside",
            detail: format!("{program} removes the workspace itself ({})", path.display()),
        });
    }
}

fn inspect_redirect(node: Node, source: &[u8], context: &Context, out: &mut Vec<Concern>) {
    let mut cursor = node.walk();
    let operator = node.children(&mut cursor).find(|child| !child.is_named()).and_then(|child| child.utf8_text(source).ok()).unwrap_or("");
    if !matches!(operator, ">" | ">|" | "&>") {
        return;
    }
    let Some(destination) = node.child_by_field_name("destination") else { return };
    let Some(target) = literal(destination, source) else { return };
    if target.starts_with("/dev/") || target.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    let path = util::resolve(context.cwd, &target);
    if util::is_inside(&path, context.workspace) || !path.exists() {
        return;
    }
    let temp = std::env::temp_dir();
    if util::is_inside(&path, &temp) || util::is_inside(&path, Path::new("/tmp")) || util::is_inside(&path, Path::new("/private/tmp")) {
        return;
    }
    out.push(Concern {
        path: Some(util::canonical_lenient(&path)),
        class: "overwrite_outside",
        detail: format!("`{operator}` truncates {}", path.display()),
    });
}
