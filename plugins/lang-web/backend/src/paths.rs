//! Local URLs of pages and stylesheets to repository files.
//!
//! Structural rules, as a static web server and the browser apply them:
//! - a URL with a scheme (`https:`, `data:`, `mailto:`, `javascript:`), a protocol-relative URL
//!   (`//cdn...`), a bare fragment (`#x`) or template syntax (`{{ }}`, `{% %}`, `${ }`, `<% %>`,
//!   `<? ?>`) names no repository file;
//! - the query and fragment are not part of the path; `%xx` escapes are decoded;
//! - a relative URL resolves against the document's directory (the page's `<base href>` when it
//!   has one; a stylesheet's own directory for `url()` and `@import`);
//! - a root-absolute URL (`/css/site.css`) resolves against the site's document root, which is the
//!   page's directory or one of its ancestors: the nearest one under which the file exists wins;
//! - a URL ending in `/` names that directory's `index.html`.
//!
//! A file is claimed only when it exists under the repository root; without a root nothing is.

use cc_lang_kit::manifest::{dir_of, is_repo_file, join_rel};
use std::path::Path;

/// Whether `url` is a local path (no scheme, not protocol-relative, not a bare fragment, no
/// template syntax).
pub fn is_local(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.starts_with('#') || url.starts_with("//") || url.starts_with('?') {
        return false;
    }
    if ["{{", "{%", "${", "<%", "<?", "@{", "#{"].iter().any(|marker| url.contains(marker)) {
        return false;
    }
    // `scheme:` before any `/`, `?` or `#` (RFC 3986: a letter, then letters, digits, + - .).
    let head = url.split(['/', '?', '#']).next().unwrap_or("");
    if let Some((scheme, _)) = head.split_once(':') {
        let mut chars = scheme.chars();
        if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    true
}

/// The path part of a local URL: query and fragment removed, `%xx` decoded.
pub fn url_path(url: &str) -> String {
    let path = url.trim().split(['?', '#']).next().unwrap_or("");
    percent_decode(path)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// Candidate repository paths of the local URL `url` written in the file `from` (repository
/// relative), relative URLs against `base_dir` (default: `from`'s directory), nearest first.
fn candidates(from: &str, base_dir: Option<&str>, url: &str) -> Vec<String> {
    let path = url_path(url);
    if path.is_empty() {
        return Vec::new();
    }
    let dir = dir_of(from);
    let mut out = Vec::new();
    let mut push = |candidate: String| {
        let candidate = if path.ends_with('/') { join_rel(&candidate, "index.html") } else { candidate };
        if !candidate.is_empty() && !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    if let Some(rest) = path.strip_prefix('/') {
        // The document root is the file's directory or one of its ancestors.
        let mut current = Some(dir);
        while let Some(d) = current {
            push(join_rel(d, rest));
            current = if d.is_empty() { None } else { Some(dir_of(d)) };
        }
    } else {
        push(join_rel(base_dir.unwrap_or(dir), &path));
    }
    out
}

/// The repository file a local URL names, when it exists under `root`.
pub fn resolve(root: Option<&Path>, from: &str, base_dir: Option<&str>, url: &str) -> Option<String> {
    if !is_local(url) || root.is_none() {
        return None;
    }
    candidates(from, base_dir, url).into_iter().find(|candidate| is_repo_file(root, candidate))
}

/// The directory relative URLs of a page resolve against, from its `<base href>` (local bases
/// only: an absolute base on another site leaves the page's URLs unresolvable, None + false).
/// Returns (directory, usable).
pub fn base_dir(root: Option<&Path>, from: &str, href: &str) -> (Option<String>, bool) {
    if !is_local(href) {
        return (None, false);
    }
    let path = url_path(href);
    // `<base href="/app/">`: a directory; `<base href="/app/index.html">`: its directory.
    let dir_part = if path.ends_with('/') { path.trim_end_matches('/').to_owned() } else { dir_of(&path).to_owned() };
    if let Some(rest) = dir_part.strip_prefix('/') {
        // Root-absolute: the nearest ancestor of the page that holds that directory.
        let mut current = Some(dir_of(from));
        while let Some(d) = current {
            let candidate = join_rel(d, rest);
            if root.is_some_and(|r| r.join(&candidate).is_dir()) {
                return (Some(candidate), true);
            }
            current = if d.is_empty() { None } else { Some(dir_of(d)) };
        }
        return (Some(rest.to_owned()), true);
    }
    (Some(join_rel(dir_of(from), &dir_part)), true)
}

/// Repository files a Sass `@use` / `@forward` / `@import` names: partials (`_x.scss`), index
/// files (`x/_index.scss`) and plain CSS, in Sass's lookup order. Built-in modules (`sass:math`)
/// and package URLs (`pkg:`, `~pkg`) name no repository file.
pub fn resolve_sass(root: Option<&Path>, from: &str, url: &str) -> Option<String> {
    if !is_local(url) || url.starts_with('~') || root.is_none() {
        return None;
    }
    let path = join_rel(dir_of(from), &url_path(url));
    let (dir, name) = (dir_of(&path).to_owned(), path.rsplit('/').next().unwrap_or(&path).to_owned());
    let mut options = Vec::new();
    if [".scss", ".sass", ".css"].iter().any(|ext| name.ends_with(ext)) {
        options.push(path.clone());
        options.push(join_rel(&dir, &format!("_{name}")));
    } else {
        for ext in ["scss", "sass", "css"] {
            options.push(join_rel(&dir, &format!("{name}.{ext}")));
            options.push(join_rel(&dir, &format!("_{name}.{ext}")));
        }
        for index in ["_index.scss", "index.scss", "_index.sass", "index.sass"] {
            options.push(join_rel(&path, index));
        }
    }
    options.into_iter().find(|candidate| is_repo_file(root, candidate))
}

/// The repository file a Less `@import` names: as written, else with `.less` added.
pub fn resolve_less(root: Option<&Path>, from: &str, url: &str) -> Option<String> {
    if !is_local(url) || url.starts_with('~') || root.is_none() {
        return None;
    }
    let path = join_rel(dir_of(from), &url_path(url));
    [path.clone(), format!("{path}.less")].into_iter().find(|candidate| is_repo_file(root, candidate))
}
