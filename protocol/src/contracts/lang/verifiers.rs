//! The project's own check commands, read from the language plugins' descriptions (`verifiers`):
//! what a decision's `checks` can pick from, so a decision names a real build or test command of
//! this project instead of inventing one. Shared by every consumer that offers checks (the chat
//! flow's DECIDE, CHECKS and PLAN, the agents planner), so they all offer the same commands.
//!
//! A verifier applies when one of its marker files (`when`) exists at the project root (no
//! markers: it always applies), a file of the project matches one of its `files` patterns (none:
//! no such need), and every program it `requires` is found on the PATH its command runs with: the
//! process PATH with the language plugins' toolchain directories (`Description.path`) in front. A
//! lookup only, no process is started. The host fills `{file}` / `{files}` with the files named and
//! leaves a verifier out when it needs files and none were named. A project with no applicable
//! verifier gets no checks: the host then runs nothing and the work is verified by the model's own
//! runs (no check may carry an expected value the model worked out).

use super::{Description, Verifier};
use std::path::{Path, PathBuf};

/// The directories commands search for programs: the toolchain directories of the language plugins
/// (prepended to commands' PATH), then `path_var` (the process PATH).
pub fn search_dirs(descriptions: &[Description], path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = descriptions.iter().flat_map(|description| description.path.iter().map(PathBuf::from)).collect();
    if let Some(path_var) = path_var {
        dirs.extend(std::env::split_paths(path_var));
    }
    dirs
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Whether `program` is found: a path is checked as it is, a name in `dirs`.
pub fn found(program: &str, dirs: &[PathBuf]) -> bool {
    if program.contains('/') {
        return executable(Path::new(program));
    }
    dirs.iter().any(|dir| executable(&dir.join(program)))
}

/// The verifiers that apply to the project at `root`, as (kind, command) lines; `dirs` is where
/// required programs are looked up ([`search_dirs`]).
pub fn applicable(root: &Path, descriptions: &[Description], dirs: &[PathBuf]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for description in descriptions {
        for verifier in &description.verifiers {
            if !applies(root, verifier) || !verifier.requires.iter().all(|program| found(program, dirs)) {
                continue;
            }
            let line = (verifier.kind.clone(), shell_line(&verifier.command));
            if !out.contains(&line) {
                out.push(line);
            }
        }
    }
    out
}

/// A verifier's argv as one shell command line, each word quoted when it needs it: the host runs
/// the line through a shell, so `["sh", "-c", "a && b"]` must stay three words (joined bare, `sh -c`
/// would run `a` alone). `{file}` / `{files}` stay bare for the host to fill.
fn shell_line(argv: &[String]) -> String {
    let plain = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%{}".contains(&b));
    argv.iter().map(|word| if plain(word) { word.clone() } else { format!("'{}'", word.replace('\'', "'\\''")) }).collect::<Vec<_>>().join(" ")
}

fn applies(root: &Path, verifier: &Verifier) -> bool {
    (verifier.when.is_empty() || verifier.when.iter().any(|marker| root.join(marker).exists())) && (verifier.files.is_empty() || has_file(root, &verifier.files))
}

/// Whether a file under `root` matches one of `patterns` (relative paths; `**` any directories,
/// `*` within a name). Hidden, dependency and build directories and symlinked directories are not
/// looked into: a dependency's own tests are not the project's.
fn has_file(root: &Path, patterns: &[String]) -> bool {
    const SKIP: &[&str] = &["node_modules", "target", "build", "dist", "vendor", "__pycache__"];
    let patterns: Vec<Vec<&str>> = patterns.iter().map(|pattern| pattern.split('/').collect()).collect();
    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(root.to_path_buf(), Vec::new())];
    let mut seen = 0usize;
    while let Some((dir, parts)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            seen += 1;
            if seen > 50_000 {
                return false;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let Ok(kind) = item.file_type() else { continue };
            let mut path = parts.clone();
            path.push(name.clone());
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                    stack.push((item.path(), path));
                }
            } else if kind.is_file() {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                if patterns.iter().any(|pattern| path_matches(pattern, &path)) {
                    return true;
                }
            }
        }
    }
    false
}

/// A relative path (its components) against a pattern's components: `**` matches any number of
/// components, any other component matches one by [`glob`].
fn path_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| path_matches(rest, &path[skip..])) || (rest.is_empty() && !path.is_empty()),
        Some((head, rest)) => path.split_first().is_some_and(|(first, tail)| glob(head, first) && path_matches(rest, tail)),
    }
}

/// A verifier command with its placeholders filled: `{file}` with the first of `files`, `{files}`
/// with all of them. None when it needs files and there are none (a verifier that checks named
/// files cannot check nothing).
pub fn fill(command: &str, files: &[String]) -> Option<String> {
    if !command.contains("{file}") && !command.contains("{files}") {
        return Some(command.to_owned());
    }
    let first = files.first()?;
    Some(command.replace("{files}", &files.join(" ")).replace("{file}", first))
}

/// One candidate check: the verifier's kind (`test`, `build`, ...) and its filled command.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub kind: String,
    pub command: String,
}

/// The project's candidate checks at `root`: the applicable verifiers of `descriptions` (the active
/// language plugins'), filled with `files` (the files named so far); empty when nothing applies.
/// `path_var` is the process PATH the commands run with.
pub fn candidates(root: &Path, descriptions: &[Description], path_var: Option<&std::ffi::OsStr>, files: &[String]) -> Vec<Candidate> {
    let dirs = search_dirs(descriptions, path_var);
    let mut out: Vec<Candidate> = Vec::new();
    for description in descriptions {
        // A verifier that takes files checks files of its own language only (a shell syntax check
        // is never filled with a Python file).
        let own = files_of(description, files);
        for (kind, command) in applicable(root, std::slice::from_ref(description), &dirs) {
            if let Some(command) = fill(&command, &own)
                && !out.iter().any(|known| known.command == command)
            {
                out.push(Candidate { kind, command });
            }
        }
    }
    out
}

/// The commands a decision may pick from.
pub fn commands(candidates: &[Candidate]) -> Vec<String> {
    candidates.iter().map(|candidate| candidate.command.clone()).collect()
}

/// The files among `files` that a language plugin's languages cover: by extension, exact file name
/// or file-name pattern (`*` matches any run of characters), as the plugin declares them.
pub fn files_of(description: &Description, files: &[String]) -> Vec<String> {
    let matches = |file: &str| -> bool {
        let name = file.rsplit('/').next().unwrap_or(file);
        let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        description.languages.iter().any(|language| {
            (!extension.is_empty() && language.extensions.iter().any(|known| known == extension))
                || language.filenames.iter().any(|known| known == name)
                || language.patterns.iter().any(|pattern| glob(pattern, name))
        })
    };
    files.iter().filter(|file| matches(file)).cloned().collect()
}

/// `*` matches any run of characters (none included); everything else matches itself.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => name.starts_with(head) && name.len() >= head.len() && glob_rest(tail, &name[head.len()..]),
    }
}

fn glob_rest(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => name.ends_with(pattern),
        Some((head, tail)) => name.find(head).is_some_and(|at| glob_rest(tail, &name[at + head.len()..])),
    }
}
