//! Helpers shared by manifest extractors (Cargo.toml, package.json, pyproject.toml, go.mod...):
//! key lines, repository-relative path arithmetic and the files and sibling commands a command line
//! names.

use std::path::Path;

/// 1-based line of the first occurrence of `needle` in `text` (manifest keys), or 1.
pub fn line_of(text: &str, needle: &str) -> u32 {
    text.find(needle).map(|pos| text[..pos].matches('\n').count() as u32 + 1).unwrap_or(1)
}

/// Directory part of a repository-relative path (`""` at the root).
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// `rel` resolved against the repository-relative directory `dir` (`.` and `..` folded).
pub fn join_rel(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Whether the repository-relative `rel` is a file under `root` (false without a root: the caller
/// did not say where the repository is, so nothing can be confirmed).
pub fn is_repo_file(root: Option<&Path>, rel: &str) -> bool {
    !rel.is_empty() && root.is_some_and(|root| root.join(rel).is_file())
}

/// Words of a shell command line that name files in the repository (relative to `dir`), and
/// `npm run x` style references to sibling scripts. Structural tokenization.
pub fn command_targets(command: &str, dir: &str, root: Option<&Path>) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut commands = Vec::new();
    for segment in command.split(['&', ';', '|']) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            let w = word.trim_matches(['"', '\'']);
            if crate::syntax::looks_like_script_path(w) || (w.contains('.') && !w.starts_with('-') && !w.contains('=')) {
                let rel = join_rel(dir, w);
                if is_repo_file(root, &rel) && !files.contains(&rel) {
                    files.push(rel);
                }
            }
            // `npm run x`, `yarn x`, `pnpm x`, `pnpm run x`, `bun run x` (package-manager syntax).
            let pm = matches!(*word, "npm" | "yarn" | "pnpm" | "bun");
            if pm {
                let rest: Vec<&str> = words[i + 1..].iter().copied().filter(|w| !w.starts_with('-')).collect();
                match rest.as_slice() {
                    ["run", script, ..] | ["run-script", script, ..] => commands.push((*script).to_owned()),
                    [script, ..] if *word != "npm" && !matches!(*script, "install" | "add" | "exec" | "dlx" | "x") => {
                        commands.push((*script).to_owned())
                    }
                    _ => {}
                }
            }
        }
    }
    (files, commands)
}
