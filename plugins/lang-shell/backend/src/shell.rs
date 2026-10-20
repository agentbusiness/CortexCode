//! sh / bash / zsh extraction (tree-sitter-bash; zsh, dash and ksh scripts are parsed with the bash
//! grammar): functions (with their comment docs and the positional parameters they use) and
//! top-level variables, commands run (with their arguments), `source` / `.` imports, scripts run
//! through interpreters or by path, and environment reads (unassigned upper-case variables).
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/shell.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, RefKind, RunFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashSet;

fn language() -> Language {
    tree_sitter_bash::LANGUAGE.into()
}

pub fn extract(src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    // Variables assigned anywhere in the script are not environment inputs.
    let mut assigned: HashSet<String> = HashSet::new();
    collect_assigned(root, src, &mut assigned);
    let mut walker = Walker { src, facts, assigned };
    walker.block(root, None);
    attach_notes(root, src, walker.facts);
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    assigned: HashSet<String>,
}

fn collect_assigned(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "variable_assignment" => {
                if let Some(name) = field(current, "name") {
                    out.insert(text(name, src).to_owned());
                }
            }
            "for_statement" | "select_statement" => {
                if let Some(var) = field(current, "variable") {
                    out.insert(text(var, src).to_owned());
                }
            }
            "command" => {
                // `read VAR`, `local VAR`, `declare VAR`, `export VAR`.
                let name = field(current, "name").map(|n| text(n, src)).unwrap_or("");
                if matches!(name, "read" | "local" | "declare" | "typeset" | "export" | "readonly") {
                    let mut cursor = current.walk();
                    for arg in current.children_by_field_name("argument", &mut cursor) {
                        let word = text(arg, src);
                        if !word.starts_with('-') {
                            out.insert(word.split('=').next().unwrap_or(word).to_owned());
                        }
                    }
                }
            }
            _ => {}
        }
        stack.extend(named_children(current));
    }
}

impl<'a> Walker<'a> {
    fn block(&mut self, node: Node, from: Option<usize>) {
        for child in named_children(node) {
            match child.kind() {
                "function_definition" => self.function(child),
                "variable_assignment" if from.is_none() => self.variable(child, child),
                "declaration_command" if from.is_none() => {
                    for assignment in named_children(child).into_iter().filter(|c| c.kind() == "variable_assignment") {
                        self.variable(assignment, child);
                    }
                    self.body(child, from);
                }
                _ => self.body(child, from),
            }
        }
    }

    fn variable(&mut self, assignment: Node, stmt: Node) {
        let Some(name) = field_text(assignment, "name", self.src) else { return };
        let mut symbol = new_symbol("variable", name, name.to_owned(), stmt, None, self.src);
        symbol.value = field(assignment, "value").map(|v| text(v, self.src).to_owned());
        symbol.exported = text(stmt, self.src).starts_with("export");
        self.facts.symbols.push(symbol);
        if let Some(value) = field(assignment, "value") {
            self.body(value, None);
        }
    }

    fn function(&mut self, node: Node) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let body = field(node, "body");
        let mut symbol = new_symbol("shell_function", name, name.to_owned(), node, body, self.src);
        // Comments directly above are the function's doc.
        let mut docs: Vec<&str> = Vec::new();
        let mut current = node.prev_sibling();
        let mut row = node.start_position().row;
        while let Some(prev) = current {
            if prev.kind() == "comment" && prev.end_position().row + 1 >= row && !text(prev, self.src).starts_with("#!") {
                docs.push(text(prev, self.src));
                row = prev.start_position().row;
                current = prev.prev_sibling();
            } else {
                break;
            }
        }
        docs.reverse();
        symbol.doc = clean_doc(&docs);
        // Positional parameters used in the body ($1, $2, ...) are the function's parameters.
        let mut positions: Vec<u32> = Vec::new();
        if let Some(b) = body {
            let mut stack = vec![b];
            while let Some(c) = stack.pop() {
                if matches!(c.kind(), "simple_expansion" | "expansion")
                    && let Some(var) = named_children(c).into_iter().find(|n| n.kind() == "variable_name" || n.kind() == "special_variable_name")
                    && let Ok(n) = text(var, self.src).parse::<u32>()
                    && n > 0
                    && !positions.contains(&n)
                {
                    positions.push(n);
                }
                stack.extend(named_children(c));
            }
        }
        positions.sort_unstable();
        for (index, n) in positions.iter().enumerate() {
            symbol.params.push(param(&format!("${n}"), index as u32));
        }
        self.facts.symbols.push(symbol);
        let index = self.facts.symbols.len() - 1;
        if let Some(b) = body {
            self.block(b, Some(index));
        }
    }

    fn body(&mut self, node: Node, from: Option<usize>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "command" => self.command(current, from),
                "function_definition" => {
                    self.function(current);
                    continue;
                }
                "simple_expansion" | "expansion" => {
                    if let Some(var) = named_children(current).into_iter().find(|n| n.kind() == "variable_name") {
                        let name = text(var, self.src);
                        // Unassigned upper-case variables are environment inputs (shell convention:
                        // environment variables are upper case; assigned ones are script state).
                        if !self.assigned.contains(name) && name.parse::<u32>().is_err() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                            push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, name, Some("env".into()), Vec::new(), current);
                        }
                    }
                }
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn command(&mut self, node: Node, from: Option<usize>) {
        let Some(name_node) = field(node, "name") else { return };
        let name = text(name_node, self.src).to_owned();
        let mut cursor = node.walk();
        let args: Vec<String> = node.children_by_field_name("argument", &mut cursor).map(|a| text(a, self.src).to_owned()).collect();
        let line = line(node);
        match name.as_str() {
            "source" | "." => {
                if let Some(path) = args.first() {
                    let path = strip_quotes(path);
                    self.facts.imports.push(ImportFact { module: path.clone(), name: "*".into(), local: String::new(), line });
                    self.facts.runs.push(RunFact { from, target: path, line });
                }
            }
            // Interpreters given a script path (program names, structural).
            "bash" | "sh" | "zsh" | "node" | "python" | "python3" | "deno" | "tsx" | "ts-node" | "exec" => {
                if let Some(script) = args.iter().map(|a| strip_quotes(a)).find(|a| !a.starts_with('-'))
                    && looks_like_script_path(&script)
                {
                    self.facts.runs.push(RunFact { from, target: script, line });
                }
            }
            _ => {
                if looks_like_script_path(&name) && (name.starts_with("./") || name.starts_with('/') || name.contains('/')) {
                    self.facts.runs.push(RunFact { from, target: name.clone(), line });
                }
            }
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, None, args, node);
    }
}
