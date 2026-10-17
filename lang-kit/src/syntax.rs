//! Tree-sitter helpers shared by the language extractors, and the facts' hashes.
//!
//! Everything here is structural: node kinds, fields and byte ranges of a syntax tree, quoting
//! rules of string literals and bracket nesting. The extractors decide what a construct is.

use cc_protocol::contracts::lang::{ParamFact, RefFact, RefKind, RefTarget, SymbolFact, TEXT_EXCERPT_CHARS};
use tree_sitter::{Language, Node, Parser, Tree};

/// Parse `text` with `language`. None only when the grammar cannot be loaded (ABI mismatch) or the
/// parse was cancelled.
pub fn parse(language: &Language, text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    parser.parse(text, None)
}

/// Short stable content hash (BLAKE3, 128 bits as hex): the code graph's body and signature hashes.
pub fn hash_hex(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    let mut out = String::with_capacity(32);
    for byte in &hash.as_bytes()[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The first [`TEXT_EXCERPT_CHARS`] characters of `text`.
pub fn excerpt(text: &str) -> String {
    text.chars().take(TEXT_EXCERPT_CHARS).collect()
}

/// Two definitions with the same qualified name in one file (overloads, cfg variants, redefinitions)
/// get a positional suffix so stable ids stay unique. Structural rule.
pub fn dedupe_qnames(symbols: &mut [SymbolFact]) {
    let mut seen: std::collections::HashMap<(String, String), u32> = std::collections::HashMap::new();
    for symbol in symbols.iter_mut() {
        let key = (symbol.kind.clone(), symbol.qname.clone());
        let count = seen.entry(key).or_insert(0);
        *count += 1;
        if *count > 1 {
            symbol.qname = format!("{}~{}", symbol.qname, count);
        }
    }
}

pub fn text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.start_byte()..node.end_byte()).unwrap_or("")
}

pub fn line(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

pub fn end_line(node: Node) -> u32 {
    last_row(node) as u32 + 1
}

/// The 0-based row of a node's last character: a node whose text ends with its newline (a
/// preprocessor line, a line comment in some grammars) ends at column 0 of the next row, so it ends
/// on the row before its end position.
pub fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row { end.row - 1 } else { end.row }
}

pub fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub fn children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

pub fn field<'t>(node: Node<'t>, name: &str) -> Option<Node<'t>> {
    node.child_by_field_name(name)
}

pub fn field_text<'a>(node: Node, name: &str, src: &'a str) -> Option<&'a str> {
    node.child_by_field_name(name).map(|child| text(child, src))
}

/// The first node of `kind` in `node`'s subtree (preorder), `node` included.
pub fn find_first<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    for child in named_children(node) {
        if let Some(found) = find_first(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Collapse runs of whitespace into single spaces (signatures).
pub fn squash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

/// Signature: the declaration text up to (not including) its body, whitespace-collapsed.
pub fn signature(decl: Node, body: Option<Node>, src: &str) -> String {
    let end = body.map(|b| b.start_byte()).unwrap_or(decl.end_byte());
    let raw = src.get(decl.start_byte()..end).unwrap_or("");
    let squashed = squash(raw);
    squashed.trim_end_matches(['{', ':', '=', '>', ' ']).trim().to_owned()
}

/// Signature of a type declaration: its text from byte `start` up to the body, whitespace-collapsed,
/// without a trailing `{` or `;`. Unlike [`signature`] it keeps a trailing `>` (`class Box<T>`,
/// `implements Comparable<T>`), `:` and `=`; `start` skips what precedes the head (annotations,
/// template headers).
pub fn head_signature(decl: Node, start: usize, body: Option<Node>, src: &str) -> String {
    let end = body.map(|b| b.start_byte()).unwrap_or(decl.end_byte());
    squash(src.get(start.min(end)..end).unwrap_or("")).trim_end_matches(['{', ';', ' ']).to_owned()
}

/// A string literal's content without quotes, when the node is a plain (non-interpolated) string.
pub fn string_content(node: Node, src: &str) -> Option<String> {
    let raw = text(node, src);
    match node.kind() {
        "string" | "string_literal" | "raw_string_literal" | "interpreted_string_literal" => {
            // Python f-strings / interpolations are not literals.
            if named_children(node).iter().any(|child| child.kind() == "interpolation") {
                return None;
            }
            Some(strip_quotes(raw))
        }
        "template_string" => {
            if named_children(node).iter().any(|child| child.kind() == "template_substitution") {
                None
            } else {
                Some(strip_quotes(raw))
            }
        }
        _ => None,
    }
}

pub fn strip_quotes(raw: &str) -> String {
    let mut s = raw;
    // String prefixes (Python r/b/u/f, Rust b/r#) only when a quote follows them.
    let unprefixed = s.trim_start_matches(['r', 'b', 'u', 'f', 'R', 'B', 'U', 'F']).trim_start_matches('#');
    if unprefixed.len() < s.len() && unprefixed.starts_with(['"', '\'']) {
        let hashes = s.len() - s.trim_end_matches('#').len();
        s = &unprefixed[..unprefixed.len() - hashes.min(unprefixed.len())];
    }
    for quote in ["\"\"\"", "'''", "\"", "'", "`"] {
        if s.len() >= 2 * quote.len() && s.starts_with(quote) && s.ends_with(quote) {
            return s[quote.len()..s.len() - quote.len()].to_owned();
        }
    }
    s.to_owned()
}

/// Template-aware URL text: `${...}` / `{...}` substitutions become `{}` (for route matching).
pub fn template_pattern(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "template_string" => {
            let mut out = String::new();
            for child in children(node) {
                match child.kind() {
                    "string_fragment" => out.push_str(text(child, src)),
                    "template_substitution" => out.push_str("{}"),
                    _ => {}
                }
            }
            Some(out)
        }
        _ => string_content(node, src),
    }
}

/// Type identifiers mentioned in a type annotation (for OF_TYPE / RETURNS edges).
pub fn type_names(node: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_type_names(node, src, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_type_names(node: Node, src: &str, out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" => out.push(text(node, src).to_owned()),
        // Python annotations are expressions: identifiers are type names.
        "identifier"
            if node.parent().is_some_and(|p| {
                p.kind() == "type" || p.kind() == "generic_type" || p.kind() == "binary_operator" || p.kind() == "subscript"
            }) =>
        {
            out.push(text(node, src).to_owned())
        }
        _ => {}
    }
    for child in named_children(node) {
        collect_type_names(child, src, out);
    }
}

/// Number of ERROR and MISSING nodes in the tree under `node`.
pub fn has_error(node: Node) -> u32 {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.is_error() || current.is_missing() {
            count += 1;
        }
        if current.has_error() {
            for child in current.children(&mut cursor) {
                stack.push(child);
            }
        }
    }
    count
}

/// Start a symbol from its declaration node.
pub fn new_symbol(kind: &str, name: &str, qname: String, decl: Node, body: Option<Node>, src: &str) -> SymbolFact {
    let code = text(decl, src).to_owned();
    let signature = signature(decl, body, src);
    SymbolFact {
        kind: kind.to_owned(),
        name: name.to_owned(),
        qname,
        sig_hash: hash_hex(signature.as_bytes()),
        signature,
        body_hash: body.map(|b| hash_hex(text(b, src).as_bytes())).unwrap_or_default(),
        code,
        start_line: line(decl),
        end_line: end_line(decl),
        start_byte: decl.start_byte() as u32,
        end_byte: decl.end_byte() as u32,
        ..SymbolFact::default()
    }
}

pub fn push_ref(refs: &mut Vec<RefFact>, from: Option<usize>, kind: RefKind, name: &str, qualifier: Option<String>, args: Vec<String>, node: Node) {
    if name.is_empty() {
        return;
    }
    refs.push(RefFact { from, kind, target: RefTarget { name: name.to_owned(), qualifier }, args, line: line(node) });
}

pub fn param(name: &str, position: u32) -> ParamFact {
    ParamFact { name: name.to_owned(), position, ..ParamFact::default() }
}

/// Doc text from comment lines: strips comment markers, keeps the words.
pub fn clean_doc(lines: &[&str]) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in lines {
        for line in raw.lines() {
            let mut l = line.trim();
            for prefix in ["///", "//!", "/**", "/*!", "/*", "*/", "//", "#"] {
                if let Some(rest) = l.strip_prefix(prefix) {
                    l = rest;
                    break;
                }
            }
            let l = l.trim_end_matches("*/").trim();
            let l = l.strip_prefix('*').map(str::trim).unwrap_or(l);
            out.push(l.to_owned());
        }
    }
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() { None } else { Some(out.join("\n")) }
}

/// Comments directly above `node` (contiguous, no blank line between, each starting its own
/// line), oldest first, for which `keep` holds; collection stops at the first one it rejects and
/// after a block comment. A comment on the row where the node before it ends ([`last_row`], so a
/// preprocessor line that ends with its newline does not claim the next line's comment) is that
/// node's trailing comment and ends the collection. Cleaned with [`clean_doc`].
pub fn comments_before(node: Node, src: &str, keep: impl Fn(&str) -> bool) -> Option<String> {
    let mut docs: Vec<&str> = Vec::new();
    let mut current = node.prev_sibling();
    let mut expected_row = node.start_position().row;
    while let Some(prev) = current {
        if !prev.kind().ends_with("comment") || prev.end_position().row + 1 < expected_row {
            break;
        }
        // A trailing comment of the previous line's code documents that code, not `node`.
        if prev.prev_sibling().is_some_and(|before| last_row(before) == prev.start_position().row) {
            break;
        }
        let t = text(prev, src);
        if !keep(t) {
            break;
        }
        docs.push(t);
        if t.starts_with("/*") {
            break;
        }
        expected_row = prev.start_position().row;
        current = prev.prev_sibling();
    }
    docs.reverse();
    clean_doc(&docs)
}

/// Split a macro / token-tree argument list at top-level commas (structural).
pub fn split_top_level_args(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth: i32 = 0;
    let mut current = String::new();
    let mut in_str: Option<char> = None;
    let mut escape = false;
    for ch in inner.chars() {
        if let Some(quote) = in_str {
            current.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == quote {
                in_str = None;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = Some(ch);
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let arg = current.trim().to_owned();
                if !arg.is_empty() {
                    args.push(arg);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let arg = current.trim().to_owned();
    if !arg.is_empty() {
        args.push(arg);
    }
    args
}

/// Whether a literal names a script or program file (structural: has a path separator or a script
/// extension). The resolver keeps it only when the file exists in the repository.
pub fn looks_like_script_path(literal: &str) -> bool {
    let l = literal.trim();
    if l.is_empty() || l.contains(char::is_whitespace) || l.starts_with('-') || l.contains("://") {
        return false;
    }
    let ext_script = [".sh", ".bash", ".zsh", ".py", ".js", ".mjs", ".cjs", ".ts"].iter().any(|ext| l.ends_with(ext));
    ext_script || (l.contains('/') && !l.ends_with('/'))
}

/// HTTP verbs used by router APIs (axum, actix, FastAPI, Flask, Express): an API-name list, not a
/// meaning heuristic.
pub const HTTP_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];
