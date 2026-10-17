//! Files a repository declares as not its own code, read from its `.gitattributes` files and
//! `.git/info/attributes`: the `linguist-vendored` and `linguist-generated` attributes (GitHub
//! Linguist's convention for third-party and generated code). The repository map leaves them out;
//! they stay in the code graph.
//!
//! Git's attribute rules, mechanically: a line is a pattern followed by attributes (`attr` or
//! `attr=true` sets, `-attr` or `attr=false` unsets, `!attr` makes unspecified); patterns use
//! gitignore's glob syntax relative to the file's directory and match the path itself (a pattern
//! naming a directory does not reach the files under it); within a file the last matching line
//! wins, a deeper `.gitattributes` wins over a shallower one, and `.git/info/attributes` over all.
//! Negative patterns and `[attr]` macro definitions are not attribute assignments and are skipped.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};

/// The attributes that mark a file as not the repository's own code.
const ATTRIBUTES: &[&str] = &["linguist-vendored", "linguist-generated"];

/// One attributes file: the directory its patterns are relative to, and per attribute a matcher
/// whose "ignore" means set and "whitelist" means unset or unspecified.
struct Source {
    dir: String,
    matchers: Vec<Gitignore>,
}

/// The vendored and generated declarations of one repository.
pub struct Declared {
    /// Lowest precedence first.
    sources: Vec<Source>,
}

/// What a line of an attributes file says about `attribute`: Some(true) set, Some(false) unset or
/// unspecified, None nothing.
fn state(attributes: &[&str], attribute: &str) -> Option<bool> {
    let mut found = None;
    for item in attributes {
        let (name, value) = match item.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (*item, None),
        };
        if let Some(name) = name.strip_prefix('-').or_else(|| name.strip_prefix('!')) {
            if name == attribute && value.is_none() {
                found = Some(false);
            }
        } else if name == attribute {
            found = Some(value.is_none_or(|value| value != "false"));
        }
    }
    found
}

fn source(root: &Path, dir: &str, text: &str) -> Source {
    let base = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    let matchers = ATTRIBUTES
        .iter()
        .map(|attribute| {
            let mut builder = GitignoreBuilder::new(&base);
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with("[attr]") {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let Some(pattern) = parts.next().filter(|pattern| !pattern.starts_with('!')) else { continue };
                let attributes: Vec<&str> = parts.collect();
                match state(&attributes, attribute) {
                    Some(true) => {
                        let _ = builder.add_line(None, pattern);
                    }
                    Some(false) => {
                        let _ = builder.add_line(None, &format!("!{pattern}"));
                    }
                    None => {}
                }
            }
            builder.build().unwrap_or_else(|_| Gitignore::empty())
        })
        .collect();
    Source { dir: dir.to_owned(), matchers }
}

impl Declared {
    /// Read the declarations of the repository at `root`: the `.gitattributes` files among `files`
    /// (repository-relative paths) and `.git/info/attributes`.
    pub fn read(root: &Path, files: impl IntoIterator<Item = String>) -> Self {
        let mut found: Vec<(String, PathBuf)> = files
            .into_iter()
            .filter(|path| path == ".gitattributes" || path.ends_with("/.gitattributes"))
            .map(|path| {
                let dir = path.strip_suffix(".gitattributes").unwrap_or("").trim_end_matches('/').to_owned();
                let file = root.join(&path);
                (dir, file)
            })
            .collect();
        // Shallower first: a deeper file overrides it.
        found.sort_by_key(|(dir, _)| (if dir.is_empty() { 0 } else { dir.split('/').count() }, dir.clone()));
        found.push((String::new(), root.join(".git").join("info").join("attributes")));
        let sources = found
            .into_iter()
            .filter_map(|(dir, file)| std::fs::read_to_string(&file).ok().map(|text| source(root, &dir, &text)))
            .collect();
        Self { sources }
    }

    /// From attribute texts directly: (directory, text), lowest precedence first.
    pub fn from_texts(root: &Path, texts: &[(&str, &str)]) -> Self {
        Self { sources: texts.iter().map(|(dir, text)| source(root, dir, text)).collect() }
    }

    /// Whether the repository declares `rel` (repository-relative) vendored or generated.
    pub fn excluded(&self, root: &Path, rel: &str) -> bool {
        (0..ATTRIBUTES.len()).any(|index| {
            for source in self.sources.iter().rev() {
                let applies = source.dir.is_empty() || rel.starts_with(&format!("{}/", source.dir));
                if !applies {
                    continue;
                }
                let matched = source.matchers[index].matched(root.join(rel), false);
                if matched.is_ignore() {
                    return true;
                }
                if matched.is_whitelist() {
                    return false;
                }
            }
            false
        })
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}
