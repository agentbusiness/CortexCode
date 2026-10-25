//! Shared structural normalization of host-run contract commands.

use crate::contracts::memory::{Contract, ContractCheck};
use std::path::{Component, Path, PathBuf};

/// Words of a shell command, with their byte ranges: split at blanks, quotes and shell punctuation
/// (mechanical; quoting is not interpreted, so a quoted `cd` or path is a word too).
fn words(command: &str) -> Vec<(usize, &str)> {
    let separator = |c: char| c.is_whitespace() || "'\"`;&|()<>{}$".contains(c);
    let mut out = Vec::new();
    let mut start = None;
    for (index, c) in command.char_indices() {
        match (separator(c), start) {
            (true, Some(from)) => {
                out.push((from, &command[from..index]));
                start = None;
            }
            (false, None) => start = Some(index),
            _ => {}
        }
    }
    if let Some(from) = start {
        out.push((from, &command[from..]));
    }
    out
}

/// Path-like parts of a word, with their offsets in it: the word, the value of `NAME=value` or
/// `--flag=value`, and each part of a `:`-separated list. URLs are not paths.
fn path_parts(word: &str) -> Vec<(usize, &str)> {
    if word.contains("://") {
        return Vec::new();
    }
    let (offset, value) = match word.find('=') {
        Some(at) => (at + 1, &word[at + 1..]),
        None => (0, word),
    };
    let mut parts = Vec::new();
    let mut from = 0;
    for part in value.split(':') {
        let looks_like_path = part.starts_with('/') || part.starts_with('~') || part == ".." || part.starts_with("../") || part.contains("/../") || part.ends_with("/..");
        if looks_like_path {
            parts.push((offset + from, part));
        }
        from += part.len() + 1;
    }
    parts
}

/// Device files a check may name.
const DEVICES: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/zero"];

/// `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// File identity (device, inode): equal for one directory under every name it has (symlinks,
/// macOS firmlinks such as `/System/Volumes/Data/private/var` for `/private/var`), so no spelling of
/// a path decides where it is.
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// The part of `path` below `root` (empty for the root itself), when `path` is inside it: the
/// nearest existing ancestor of `path` that is the same directory as `root` decides.
pub fn below(path: &Path, root: &Path) -> Option<PathBuf> {
    let path = normalize(path);
    let Some(root_id) = identity(root) else {
        let root = normalize(root);
        return path.strip_prefix(&root).ok().map(Path::to_path_buf);
    };
    let mut ancestor = path.clone();
    loop {
        if identity(&ancestor) == Some(root_id) {
            return path.strip_prefix(&ancestor).ok().map(Path::to_path_buf);
        }
        if !ancestor.pop() {
            return None;
        }
    }
}

fn expand(part: &str, root: &Path) -> PathBuf {
    match part.strip_prefix('~') {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest.trim_start_matches('/')),
        None if part.starts_with('/') => PathBuf::from(part),
        None => root.join(part),
    }
}

/// Why the host refuses to run `command` as a check in `root`, if it does. A check runs with the
/// root as its working directory, so it may not change directory (`cd`, `pushd`) nor name a path
/// outside the root (absolute, `~` or climbing with `..`), whatever its spelling: such a check
/// verifies something other than the work (a worker once moved its check into Homebrew's `bin`
/// and copied its files there to pass). Mechanical: words and file identities, no judgement.
pub fn refusal(command: &str, root: &str) -> Option<String> {
    let root = Path::new(root);
    if chains_alternatives(command) {
        return Some("it chains alternatives with `||` (a failing first command would hide behind the second); a check is one command whose own exit status decides".to_owned());
    }
    for (_, word) in words(command) {
        if matches!(word, "cd" | "pushd" | "popd" | "chdir") {
            return Some(format!("it changes directory (`{word}`); checks run in the workspace root ({}), so write it as a command run from there", root.display()));
        }
        for (_, part) in path_parts(word) {
            if DEVICES.contains(&part) {
                continue;
            }
            if below(&expand(part, root), root).is_none() {
                return Some(format!("it names `{part}`, outside the workspace root ({}); a check may only use paths inside it", root.display()));
            }
        }
    }
    None
}

/// Whether a command chains alternatives with `||` outside quotes (mechanical scan; `||` inside a
/// quoted string, e.g. a Python expression, is the program's business).
pub fn chains_alternatives(command: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut previous = '\0';
    for c in command.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '|' && previous == '|' => return true,
            None => {}
        }
        previous = if quote.is_some() { '\0' } else { c };
    }
    false
}

/// A check command as the root runs it: a leading `cd <dir> &&` (or `;`) dropped and absolute
/// paths rewritten relative. Only a `cd` to an exact root in `dirs` is dropped; a subdirectory
/// change stays for [`refusal`] to reject instead of silently running a different check.
/// Roots are compared by file identity, not by string.
/// A check naming one copy's absolute path would otherwise verify that copy wherever it runs
/// (e.g. after a merge). Anything else is left for [`refusal`] to judge.
pub fn rooted(command: &str, dirs: &[&str]) -> String {
    let dirs: Vec<&Path> = dirs.iter().filter(|dir| !dir.is_empty()).map(Path::new).collect();
    let inside = |part: &str| -> Option<String> {
        let path = Path::new(part.trim_matches(|c| c == '\'' || c == '"'));
        if !path.is_absolute() {
            return None;
        }
        dirs.iter().find_map(|dir| below(path, dir)).map(|rest| if rest.as_os_str().is_empty() { ".".to_owned() } else { rest.display().to_string() })
    };
    let mut command = command.trim().to_owned();
    if let Some(rest) = command.strip_prefix("cd ") {
        let rest = rest.trim_start();
        let end = rest.find("&&").or_else(|| rest.find(';'));
        if let Some(end) = end
            && inside(rest[..end].trim().trim_end_matches('/')).as_deref() == Some(".")
        {
            let after = &rest[end..];
            command = after.strip_prefix("&&").or_else(|| after.strip_prefix(';')).unwrap_or(after).trim().to_owned();
        }
    }
    let mut out = String::with_capacity(command.len());
    let mut copied = 0;
    for (at, word) in words(&command) {
        for (offset, part) in path_parts(word) {
            if let Some(relative) = inside(part) {
                let from = at + offset;
                out.push_str(&command[copied..from]);
                out.push_str(&relative);
                copied = from + part.len();
            }
        }
    }
    out.push_str(&command[copied..]);
    out
}

/// Contract checks from decided check commands (command kind; success = exit status 0).
pub fn contract_from(checks: &[String]) -> Contract {
    let mut seen = std::collections::HashSet::new();
    Contract {
        checks: checks
            .iter()
            // A leading shell prompt marker (`$ `) is never part of a command (mechanical).
            .map(|command| command.trim().strip_prefix("$ ").unwrap_or(command.trim()).trim())
            .filter(|command| !command.is_empty() && seen.insert(command.to_owned()))
            .enumerate()
            .map(|(index, command)| ContractCheck {
                name: format!("check {}", index + 1),
                kind: "command".into(),
                spec: command.to_owned(),
                expect: None,
            })
            .collect(),
    }
}

/// The command checks of a contract.
pub fn commands(contract: &Contract) -> Vec<String> {
    contract.checks.iter().filter(|check| check.kind == "command").map(|check| check.spec.clone()).collect()
}
