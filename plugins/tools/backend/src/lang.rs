//! Tree-sitter integration: language detection, structural outlines (definitions with line spans,
//! bodies elided) and syntax-error extraction for the post-edit structural check.
//!
//! Everything here is structural: node kinds from each grammar decide what an outline item is.

use std::collections::HashMap;
use std::path::Path;
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Bash,
    Json,
    Css,
    Html,
    Markdown,
    Toml,
    Yaml,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Ruby => "ruby",
            Lang::Bash => "bash",
            Lang::Json => "json",
            Lang::Css => "css",
            Lang::Html => "html",
            Lang::Markdown => "markdown",
            Lang::Toml => "toml",
            Lang::Yaml => "yaml",
        }
    }

    pub fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
            Lang::Css => tree_sitter_css::LANGUAGE.into(),
            Lang::Html => tree_sitter_html::LANGUAGE.into(),
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
            Lang::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Lang::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        }
    }

    /// Detect from the file name, then from a shebang line.
    pub fn detect(path: &Path, text: &str) -> Option<Lang> {
        let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
        let by_name = match name.as_str() {
            "rakefile" | "gemfile" | "podfile" => Some(Lang::Ruby),
            "cargo.lock" | "pyproject.toml" => Some(Lang::Toml),
            ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => Some(Lang::Bash),
            _ => None,
        };
        if by_name.is_some() {
            return by_name;
        }
        let extension = path.extension().map(|ext| ext.to_string_lossy().to_lowercase()).unwrap_or_default();
        let by_extension = match extension.as_str() {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" | "pyw" => Some(Lang::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "go" => Some(Lang::Go),
            "java" => Some(Lang::Java),
            "c" => Some(Lang::C),
            // `.h` is shared by C and C++; the C++ grammar accepts both.
            "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ino" | "mm" => Some(Lang::Cpp),
            "cs" => Some(Lang::CSharp),
            "rb" | "rake" | "gemspec" => Some(Lang::Ruby),
            "sh" | "bash" | "zsh" | "ksh" => Some(Lang::Bash),
            "json" => Some(Lang::Json),
            "css" => Some(Lang::Css),
            "html" | "htm" => Some(Lang::Html),
            "md" | "markdown" => Some(Lang::Markdown),
            "toml" => Some(Lang::Toml),
            "yaml" | "yml" => Some(Lang::Yaml),
            _ => None,
        };
        if by_extension.is_some() {
            return by_extension;
        }
        let first = text.lines().next().unwrap_or("");
        let shebang = first.strip_prefix("#!")?;
        let interpreter = shebang.split_whitespace().flat_map(|part| part.rsplit('/').next()).find(|part| *part != "env")?;
        match interpreter {
            "bash" | "sh" | "zsh" | "ksh" | "dash" => Some(Lang::Bash),
            "node" | "deno" | "bun" => Some(Lang::JavaScript),
            "ruby" => Some(Lang::Ruby),
            name if name.starts_with("python") => Some(Lang::Python),
            _ => None,
        }
    }
}

pub fn parse(lang: Lang, text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    parser.parse(text, None)
}

/// One outline entry (0-based rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: String,
    pub start: usize,
    /// Last row of the header (signature) that is shown.
    pub header_end: usize,
    pub end: usize,
    pub children: Vec<Item>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// An outline item whose nested definitions are listed too.
    Container,
    /// An outline item shown by its header only.
    Leaf,
    /// An item only when it sits at the top level of the file.
    TopLevelLeaf,
}

fn role(lang: Lang, kind: &str) -> Option<Role> {
    use Role::*;
    let role = match lang {
        Lang::Rust => match kind {
            "impl_item" | "trait_item" | "mod_item" | "foreign_mod_item" => Container,
            "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "macro_definition"
            | "const_item"
            | "static_item"
            | "type_item" => Leaf,
            _ => return None,
        },
        Lang::Python => match kind {
            "class_definition" => Container,
            "function_definition" | "decorated_definition" => Leaf,
            _ => return None,
        },
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match kind {
            "class_declaration" | "abstract_class_declaration" | "class" | "internal_module" | "module" => Container,
            "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "function_signature"
            | "abstract_method_signature" => Leaf,
            "lexical_declaration" | "variable_declaration" => TopLevelLeaf,
            _ => return None,
        },
        Lang::Go => match kind {
            "function_declaration" | "method_declaration" | "type_declaration" => Leaf,
            "const_declaration" | "var_declaration" => TopLevelLeaf,
            _ => return None,
        },
        Lang::Java => match kind {
            "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "annotation_type_declaration" => {
                Container
            }
            "method_declaration" | "constructor_declaration" => Leaf,
            _ => return None,
        },
        Lang::C => match kind {
            "function_definition" | "type_definition" => Leaf,
            "struct_specifier" | "union_specifier" | "enum_specifier" => TopLevelLeaf,
            _ => return None,
        },
        Lang::Cpp => match kind {
            "class_specifier" | "namespace_definition" => Container,
            "function_definition" | "type_definition" | "alias_declaration" => Leaf,
            "struct_specifier" | "union_specifier" | "enum_specifier" => Container,
            _ => return None,
        },
        Lang::CSharp => match kind {
            "class_declaration" | "struct_declaration" | "interface_declaration" | "record_declaration" | "namespace_declaration" => {
                Container
            }
            "enum_declaration" | "method_declaration" | "constructor_declaration" | "property_declaration" | "delegate_declaration" => Leaf,
            _ => return None,
        },
        Lang::Ruby => match kind {
            "class" | "module" | "singleton_class" => Container,
            "method" | "singleton_method" => Leaf,
            _ => return None,
        },
        Lang::Bash => match kind {
            "function_definition" => Leaf,
            _ => return None,
        },
        Lang::Json => match kind {
            "pair" => Leaf,
            _ => return None,
        },
        Lang::Yaml => match kind {
            "block_mapping_pair" => Leaf,
            _ => return None,
        },
        Lang::Toml => match kind {
            "table" | "table_array_element" => Leaf,
            "pair" => TopLevelLeaf,
            _ => return None,
        },
        Lang::Css => match kind {
            "rule_set" | "media_statement" | "keyframes_statement" | "supports_statement" | "at_rule" | "import_statement" => Leaf,
            _ => return None,
        },
        Lang::Markdown => match kind {
            "section" => Container,
            _ => return None,
        },
        Lang::Html => return None,
    };
    Some(role)
}

/// Structural outline of a file, or `None` when the language has no grammar or no items were found.
pub fn outline(lang: Lang, text: &str) -> Option<Vec<Item>> {
    let tree = parse(lang, text)?;
    let source = text.as_bytes();
    let mut items = Vec::new();
    collect(lang, tree.root_node(), source, 0, &mut items);
    (!items.is_empty()).then_some(items)
}

fn collect(lang: Lang, node: Node, source: &[u8], depth: usize, out: &mut Vec<Item>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match role(lang, child.kind()) {
            Some(Role::TopLevelLeaf) if depth > 0 => collect(lang, child, source, depth, out),
            Some(role) => {
                let mut item = Item {
                    kind: child.kind().to_owned(),
                    start: child.start_position().row,
                    header_end: header_end(child, source),
                    end: end_row(child),
                    children: Vec::new(),
                };
                if role == Role::Container {
                    collect(lang, child, source, depth + 1, &mut item.children);
                }
                out.push(item);
            }
            None => collect(lang, child, source, depth, out),
        }
    }
}

/// Last row of a node, not counting a trailing newline that ends exactly at column 0.
fn end_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row { end.row - 1 } else { end.row }
}

/// The header of a definition runs up to where its body starts: the body's own row when the body
/// opens with a bracket on that row (`{`), else the row before (Python-style indented blocks).
fn header_end(node: Node, source: &[u8]) -> usize {
    let start = node.start_position().row;
    let inner = if node.kind() == "decorated_definition" { node.child_by_field_name("definition").unwrap_or(node) } else { node };
    let body = inner
        .child_by_field_name("body")
        .or_else(|| inner.child_by_field_name("value"))
        .or_else(|| inner.child_by_field_name("declarator").and_then(|declarator| declarator.child_by_field_name("value")));
    let Some(body) = body else {
        return start;
    };
    let row = body.start_position().row;
    if row <= start {
        return start;
    }
    let first = source.get(body.start_byte()).copied().unwrap_or(b' ');
    if matches!(first, b'{' | b'(' | b'[' | b':') { row } else { row - 1 }
}

/// Rows (0-based) to show in the outline view.
pub fn shown_rows(items: &[Item], lines: &[String], shown: &mut std::collections::BTreeSet<usize>) {
    for item in items {
        for row in item.start..=item.header_end.min(item.end) {
            shown.insert(row);
        }
        shown_rows(&item.children, lines, shown);
        // Containers with listed members get their closing line (bracket or `end`) so the nesting
        // stays readable; a leaf's body (closing line included) is elided as a whole.
        if !item.children.is_empty()
            && item.end > item.header_end
            && let Some(last) = lines.get(item.end)
        {
            let trimmed = last.trim_start();
            if trimmed.starts_with(['}', ')', ']']) || trimmed == "end" || trimmed.starts_with("end ") {
                shown.insert(item.end);
            }
        }
    }
}

/// Fallback outline for text without a grammar: lines with no indentation (structural).
pub fn indentation_rows(lines: &[String]) -> std::collections::BTreeSet<usize> {
    lines.iter().enumerate().filter(|(_, line)| !line.trim().is_empty() && !line.starts_with([' ', '\t'])).map(|(row, _)| row).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxIssue {
    /// 1-based line.
    pub line: usize,
    pub column: usize,
    /// 1-based last line of the region the parser could not read (`line` for a named spot).
    pub end_line: usize,
    /// `error` (unparseable text) or `missing` (the parser inserted an expected token).
    pub kind: &'static str,
    pub detail: String,
}

/// All syntax errors of a text (outermost ERROR nodes and MISSING tokens).
pub fn syntax_issues(lang: Lang, text: &str) -> Option<Vec<SyntaxIssue>> {
    let tree = parse(lang, text)?;
    let mut issues = Vec::new();
    let mut stack = vec![tree.root_node()];
    let source = text.as_bytes();
    while let Some(node) = stack.pop() {
        if node.is_missing() {
            issues.push(SyntaxIssue {
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
                end_line: node.start_position().row + 1,
                kind: "missing",
                detail: format!("missing `{}`", node.kind()),
            });
            continue;
        }
        if node.is_error() {
            // A bracket that closes nothing or the wrong opener is named where it is; the parser
            // only knows the region it could not read.
            let (start, end) = (node.start_position(), node.end_position());
            let issue = match bracket_fault(lang, text, (start.row + 1, start.column + 1), (end.row + 1, end.column + 1)) {
                Some(fault) => SyntaxIssue { line: fault.line, column: fault.column, end_line: fault.line, kind: "error", detail: fault.detail },
                None => {
                    let snippet: String = node.utf8_text(source).unwrap_or("").chars().take(40).collect();
                    let snippet = snippet.split('\n').next().unwrap_or("").to_owned();
                    SyntaxIssue {
                        line: start.row + 1,
                        column: start.column + 1,
                        end_line: end.row + 1,
                        kind: "error",
                        detail: format!("cannot parse `{snippet}`"),
                    }
                }
            };
            issues.push(issue);
            continue;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
    }
    issues.sort_by_key(|issue| (issue.line, issue.column));
    Some(issues)
}

/// A bracket fault: where it is and what is wrong, in words.
struct BracketFault {
    line: usize,
    column: usize,
    detail: String,
}

/// Each bracket of `text` outside strings and comments, in order, with its 1-based line and column,
/// up to the position `to`; `visit` returns true to stop. Mechanical: brackets only, no grammar.
fn scan_brackets(lang: Lang, text: &str, to: (usize, usize), mut visit: impl FnMut(char, usize, usize) -> bool) {
    let hash_comments = matches!(lang, Lang::Python | Lang::Bash | Lang::Ruby | Lang::Toml | Lang::Yaml);
    let slash_comments = matches!(lang, Lang::Rust | Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Go | Lang::Java | Lang::C | Lang::Cpp | Lang::CSharp | Lang::Css);
    let chars: Vec<char> = text.chars().collect();
    let (mut line, mut column) = (1usize, 1usize);
    let mut index = 0;
    // Advance over `count` characters, keeping the position.
    let step = |index: &mut usize, line: &mut usize, column: &mut usize, count: usize| {
        for _ in 0..count {
            if *index >= chars.len() {
                return;
            }
            if chars[*index] == '\n' {
                *line += 1;
                *column = 1;
            } else {
                *column += 1;
            }
            *index += 1;
        }
    };
    let starts = |index: usize, pattern: &str| pattern.chars().enumerate().all(|(offset, c)| chars.get(index + offset) == Some(&c));
    while index < chars.len() && (line, column) <= to {
        let c = chars[index];
        // Comments: to the end of the line, or to `*/`.
        if (hash_comments && c == '#') || (slash_comments && starts(index, "//")) {
            while index < chars.len() && chars[index] != '\n' {
                step(&mut index, &mut line, &mut column, 1);
            }
            continue;
        }
        if slash_comments && starts(index, "/*") {
            step(&mut index, &mut line, &mut column, 2);
            while index < chars.len() && !starts(index, "*/") {
                step(&mut index, &mut line, &mut column, 1);
            }
            step(&mut index, &mut line, &mut column, 2);
            continue;
        }
        // Strings: Python's triple quotes and backtick strings may span lines, others end at the line.
        let triple = lang == Lang::Python && (starts(index, "\"\"\"") || starts(index, "\'\'\'"));
        // A Rust `'` is a char literal only when it closes within a character or an escape; else a lifetime.
        let rust_lifetime = lang == Lang::Rust && c == '\'' && chars.get(index + 2) != Some(&'\'') && chars.get(index + 1) != Some(&'\\');
        if triple || c == '`' || c == '"' || (c == '\'' && !rust_lifetime) {
            let quote: String = if triple { chars[index..index + 3].iter().collect() } else { c.to_string() };
            let multiline = triple || c == '`';
            step(&mut index, &mut line, &mut column, quote.chars().count());
            while index < chars.len() && !starts(index, &quote) && (multiline || chars[index] != '\n') {
                let skip = if chars[index] == '\\' { 2 } else { 1 };
                step(&mut index, &mut line, &mut column, skip);
            }
            if starts(index, &quote) {
                step(&mut index, &mut line, &mut column, quote.chars().count());
            }
            continue;
        }
        if matches!(c, '(' | '[' | '{' | ')' | ']' | '}') && visit(c, line, column) {
            return;
        }
        step(&mut index, &mut line, &mut column, 1);
    }
}

/// The opening bracket a closing bracket closes.
fn opener_of(closer: char) -> char {
    match closer {
        ')' => '(',
        ']' => '[',
        _ => '{',
    }
}

/// The first bracket fault inside the region `from`..`to` (1-based line and column) of `text`: a
/// closing bracket that closes nothing, or one that closes another kind of bracket. The whole text
/// up to the region's end is scanned, so brackets opened on earlier lines count. Strings and
/// comments are skipped. Mechanical: brackets only, no grammar; None when it finds no fault there.
fn bracket_fault(lang: Lang, text: &str, from: (usize, usize), to: (usize, usize)) -> Option<BracketFault> {
    let mut stack: Vec<(char, usize, usize)> = Vec::new();
    let mut fault = None;
    scan_brackets(lang, text, to, |c, line, column| {
        let inside = (line, column) >= from;
        match c {
            '(' | '[' | '{' => stack.push((c, line, column)),
            _ => match stack.pop() {
                Some((open, ..)) if open == opener_of(c) => {}
                Some((open, open_line, open_column)) if inside => {
                    let at = if open_line == line { format!("column {open_column}") } else { format!("line {open_line}, column {open_column}") };
                    let detail = format!("`{c}` at column {column} closes the `{open}` opened at {at}: one `{c}` too many, or a closing bracket missing before it");
                    fault = Some(BracketFault { line, column, detail });
                    return true;
                }
                None if inside => {
                    fault = Some(BracketFault { line, column, detail: format!("`{c}` at column {column} closes nothing: one `{c}` too many") });
                    return true;
                }
                // A fault before the region is not this error: take the closer as the stray one.
                Some(open) => stack.push(open),
                None => {}
            },
        }
        false
    });
    fault
}

/// The line (1-based) of the bracket that the first closing bracket on line `line` of `text` closes,
/// when that line has one and it closes a bracket of its kind.
pub fn opening_line(lang: Lang, text: &str, line: usize) -> Option<usize> {
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut found = None;
    scan_brackets(lang, text, (line, usize::MAX), |c, at, _| {
        match c {
            '(' | '[' | '{' => stack.push((c, at)),
            _ => {
                let open = stack.pop();
                if at == line {
                    found = open.filter(|(open, _)| *open == opener_of(c)).map(|(_, open_line)| open_line);
                    return true;
                }
            }
        }
        false
    });
    found
}

/// Syntax issues present after an edit that were not present before, matched by
/// (kind, detail, content of the line) so pre-existing errors that merely moved are not reported.
pub fn introduced_issues(lang: Lang, before: &str, after: &str) -> Option<Vec<SyntaxIssue>> {
    let old = syntax_issues(lang, before)?;
    let new = syntax_issues(lang, after)?;
    let old_lines: Vec<&str> = before.split('\n').collect();
    let new_lines: Vec<&str> = after.split('\n').collect();
    let key = |issue: &SyntaxIssue, lines: &[&str]| {
        (issue.kind, issue.detail.clone(), lines.get(issue.line - 1).map(|line| line.trim().to_owned()).unwrap_or_default())
    };
    let mut budget: HashMap<(&'static str, String, String), usize> = HashMap::new();
    for issue in &old {
        *budget.entry(key(issue, &old_lines)).or_default() += 1;
    }
    // A construct the grammar cannot read that the file already uses is the project's, not the
    // edit's: a macro call that opens a block (`TEST_CASE("x") {`, `TEST(Suite, Name) {`) is a parse
    // error on every such line (seen: each new Catch2 test case refused as "missing `;`" while the
    // file held seven of them).
    let shapes: std::collections::HashSet<(&'static str, String, String)> =
        old.iter().filter_map(|issue| block_call(old_lines.get(issue.line - 1).copied().unwrap_or("")).map(|shape| (issue.kind, issue.detail.clone(), shape))).collect();
    let mut introduced = Vec::new();
    for issue in new {
        let entry = budget.entry(key(&issue, &new_lines)).or_default();
        if *entry > 0 {
            *entry -= 1;
        } else if block_call(new_lines.get(issue.line - 1).copied().unwrap_or("")).is_some_and(|shape| shapes.contains(&(issue.kind, issue.detail.clone(), shape))) {
            continue;
        } else {
            introduced.push(issue);
        }
    }
    Some(introduced)
}

/// The name of a call that opens a block on its line (`NAME(...) {`), the shape of a macro the
/// grammar cannot read; None for any other line.
fn block_call(line: &str) -> Option<String> {
    static CALL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\s*([A-Za-z_][A-Za-z0-9_:]*)\s*\(.*\)\s*\{\s*$").expect("valid"));
    CALL.captures(line).map(|found| found[1].to_owned())
}
