//! Shared pieces of Dockerfile and Compose facts: image references, `$VAR` / `${VAR...}`
//! expansions, container path arithmetic, the words of a command line and the repository files a
//! container path came from (COPY sources and bind mounts).

use cc_lang_kit::manifest::{is_repo_file, join_rel};
use cc_lang_kit::syntax::looks_like_script_path;
use std::collections::HashMap;
use std::path::Path;

/// `name[:tag][@digest]` -> (name, tag or digest as written; empty when neither). A `:` before the
/// last `/` is a registry port (`localhost:5000/app`), not a tag.
pub fn split_image(reference: &str) -> (String, String) {
    let reference = reference.trim();
    let (named, digest) = match reference.split_once('@') {
        Some((named, digest)) => (named, Some(digest)),
        None => (reference, None),
    };
    let slash = named.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (name, tag) = match named[slash..].rfind(':') {
        Some(colon) => (&named[..slash + colon], Some(&named[slash + colon + 1..])),
        None => (named, None),
    };
    let spec = match (tag, digest) {
        (Some(tag), Some(digest)) => format!("{tag}@{digest}"),
        (Some(tag), None) => tag.to_owned(),
        (None, Some(digest)) => digest.to_owned(),
        (None, None) => String::new(),
    };
    (name.to_owned(), spec)
}

/// One expansion in a text: the variable's name and its byte range (`$X`, `${X}`, `${X:-d}`...).
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    pub name: String,
    pub start: usize,
    pub end: usize,
    /// The operator and word after the name (`:-default`, `?err`, `:+alt`), empty for none.
    pub modifier: String,
}

fn name_char(c: char, first: bool) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!first && c.is_ascii_digit())
}

/// The `$NAME` / `${NAME...}` expansions of `text`, in order. `$$` is an escaped dollar (Compose).
pub fn expansions(text: &str) -> Vec<Expansion> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'$') {
            i += 2;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'{') {
            let Some(close) = text[i + 2..].find('}') else { break };
            let inner = &text[i + 2..i + 2 + close];
            let name_len = inner.char_indices().take_while(|(k, c)| name_char(*c, *k == 0)).count();
            if name_len > 0 {
                out.push(Expansion { name: inner[..name_len].to_owned(), start: i, end: i + 3 + close, modifier: inner[name_len..].to_owned() });
            }
            i += 3 + close;
            continue;
        }
        let rest = &text[i + 1..];
        let name_len = rest.char_indices().take_while(|(k, c)| name_char(*c, *k == 0)).count();
        if name_len > 0 {
            out.push(Expansion { name: rest[..name_len].to_owned(), start: i, end: i + 1 + name_len, modifier: String::new() });
        }
        i += 1 + name_len.max(0);
    }
    out
}

/// `text` with the expansions whose variables `values` knows replaced (`${X:-d}` takes `d` when
/// `X` is unknown or empty, `${X:+a}` takes `a` when `X` is set). None when an expansion stays
/// unresolved: the value is not knowable from the file.
pub fn substitute(text: &str, values: &HashMap<String, String>) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for expansion in expansions(text) {
        out.push_str(&text[at..expansion.start]);
        at = expansion.end;
        let value = values.get(&expansion.name).filter(|v| !v.is_empty());
        let modifier = expansion.modifier.as_str();
        let resolved = if let Some(word) = modifier.strip_prefix(":-").or_else(|| modifier.strip_prefix('-')) {
            Some(value.cloned().unwrap_or_else(|| word.to_owned()))
        } else if let Some(word) = modifier.strip_prefix(":+").or_else(|| modifier.strip_prefix('+')) {
            Some(if value.is_some() { word.to_owned() } else { String::new() })
        } else {
            value.cloned()
        };
        out.push_str(&resolved?);
    }
    out.push_str(&text[at..]);
    Some(out.replace("$$", "$"))
}

/// An absolute container path: `path` taken relative to the absolute directory `base` (`.` and
/// `..` folded).
pub fn container_join(base: &str, path: &str) -> String {
    let joined = if path.starts_with('/') { join_rel("", path) } else { join_rel(base.trim_start_matches('/'), path) };
    format!("/{joined}")
}

/// The words of a command line (shell or exec form): split at whitespace and shell operators,
/// quotes stripped.
pub fn command_words(command: &str) -> Vec<String> {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`'))
        .map(|w| w.trim_matches(['"', '\'']))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Words that name a script or program file (a path or a script extension), not options,
/// assignments, URLs or expansions.
pub fn script_words(command: &str) -> Vec<String> {
    command_words(command).into_iter().filter(|w| !w.contains('=') && !w.contains('$') && looks_like_script_path(w)).collect()
}

/// Where a container path came from: a COPY / ADD into the image, or a bind mount.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    /// Absolute container path the source was placed at.
    pub container: String,
    /// The source, relative to the build context (Dockerfile) or the Compose file's directory.
    pub source: String,
    /// The container path is a directory receiving the source (the source's contents when the
    /// source is a directory, the source itself by its name when it is a file).
    pub into_dir: bool,
}

/// Source-relative paths an absolute container path may have come from, latest mapping first.
pub fn sources_of(container_path: &str, mappings: &[Mapping]) -> Vec<String> {
    let mut out = Vec::new();
    for mapping in mappings.iter().rev() {
        if mapping.source.contains(['*', '?', '[']) {
            continue;
        }
        let source = mapping.source.trim_start_matches("./");
        if mapping.into_dir {
            let prefix = if mapping.container == "/" { "/".to_owned() } else { format!("{}/", mapping.container) };
            if let Some(rest) = container_path.strip_prefix(&prefix) {
                out.push(join_rel("", &format!("{source}/{rest}")));
                let base = source.trim_end_matches('/').rsplit('/').next().unwrap_or(source);
                if rest == base {
                    out.push(join_rel("", source));
                }
            }
        } else if mapping.container == container_path {
            out.push(join_rel("", source));
        }
    }
    out.dedup();
    out
}

/// The repository file a container path names: each source candidate resolved against each of
/// `contexts` (repository-relative directories, in order), kept when the file exists under `root`.
pub fn repo_file(container_path: &str, mappings: &[Mapping], contexts: &[&str], root: Option<&Path>) -> Option<String> {
    root?;
    for source in sources_of(container_path, mappings) {
        for context in contexts {
            let rel = join_rel(context, &source);
            if is_repo_file(root, &rel) {
                return Some(rel);
            }
        }
    }
    None
}
