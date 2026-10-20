//! TypeScript / TSX / JavaScript / JSX extraction (tree-sitter-typescript, tree-sitter-javascript):
//! functions, classes, interfaces, type aliases, enums, namespaces and module-level variables with
//! qualified names (`Class.method`), signatures, JSDoc, parameters and types; React components
//! (capitalized functions returning JSX, `React.Component` classes) and their props; imports,
//! `require`, exports and `module.exports`; calls, instantiations, renders, hook uses, `this` field
//! reads and writes, `process.env` / `import.meta.env` reads, thrown errors and receiver-typed
//! locals; jest / vitest / mocha / node:test tests, Express routes, Electron and Tauri IPC, `fetch` /
//! axios requests with literal URLs and child processes run.
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/js.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, IpcCallFact, LocalFact, ParamFact, RefKind, RefTarget, RequestFact, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};

/// The grammar of a file: TypeScript (`.ts`, `.mts`, `.cts`), TSX (`.tsx`) or JavaScript with JSX
/// (`.js`, `.jsx`, `.mjs`, `.cjs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    TypeScript,
    Tsx,
    JavaScript,
}

impl Dialect {
    fn language(self) -> Language {
        match self {
            Dialect::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Dialect::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Dialect::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    /// Pending Tauri `invoke` calls: kept only if `invoke` is imported from `@tauri-apps/api`.
    tauri_calls: Vec<(String, IpcCallFact)>,
    /// describe() titles for nested test names.
    suites: Vec<String>,
}

pub fn extract(dialect: Dialect, src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&dialect.language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let mut walker = Walker { src, facts, tauri_calls: Vec::new(), suites: Vec::new() };
    walker.statements(root, None, "");
    // Keep Tauri invokes whose callee name is imported from the Tauri API package (API-name rule).
    let tauri_locals: Vec<String> = walker
        .facts
        .imports
        .iter()
        .filter(|i| i.module.starts_with("@tauri-apps/api"))
        .map(|i| i.local.clone())
        .collect();
    let calls = std::mem::take(&mut walker.tauri_calls);
    for (local, call) in calls {
        if tauri_locals.contains(&local) {
            walker.facts.ipc_calls.push(call);
        }
    }
    resolve_component_props(walker.facts);
    attach_notes(root, src, walker.facts);
}

impl<'a> Walker<'a> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    /// JSDoc (`/** */`) or contiguous `//` comments directly above `node`.
    fn doc_before(&self, node: Node) -> Option<String> {
        // `export declare function f()`: the comment precedes the outermost wrapper.
        let mut node = node;
        while node.prev_named_sibling().is_none_or(|p| p.kind() != "comment")
            && let Some(parent) = node.parent()
            && matches!(parent.kind(), "export_statement" | "ambient_declaration")
        {
            node = parent;
        }
        let mut docs: Vec<&str> = Vec::new();
        let mut current = node.prev_sibling();
        let mut expected_row = node.start_position().row;
        while let Some(prev) = current {
            if prev.kind() != "comment" {
                break;
            }
            // Must be adjacent (no blank line between).
            if prev.end_position().row + 1 < expected_row {
                break;
            }
            let t = text(prev, self.src);
            if t.starts_with("/**") {
                docs.push(t);
                break;
            }
            if t.starts_with("//") {
                docs.push(t);
                expected_row = prev.start_position().row;
                current = prev.prev_sibling();
                continue;
            }
            break;
        }
        docs.reverse();
        clean_doc(&docs)
    }

    fn statements(&mut self, list: Node, parent: Option<usize>, prefix: &str) {
        for node in named_children(list) {
            self.statement(node, parent, prefix, false);
        }
    }

    fn statement(&mut self, node: Node, parent: Option<usize>, prefix: &str, exported: bool) {
        match node.kind() {
            "import_statement" => self.import(node),
            "export_statement" => self.export(node, parent, prefix),
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                let name = field_text(node, "name", self.src).unwrap_or("default").to_owned();
                self.function(node, &name, prefix, parent, exported, doc_node, node);
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                self.class(node, prefix, parent, exported, doc_node);
            }
            "interface_declaration" => {
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                self.interface(node, prefix, parent, exported, doc_node);
            }
            "type_alias_declaration" => {
                let Some(name) = field_text(node, "name", self.src) else { return };
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                let mut symbol = new_symbol("type", name, format!("{prefix}{name}"), node, None, self.src);
                symbol.doc = self.doc_before(doc_node);
                symbol.exported = exported;
                symbol.parent = parent;
                if let Some(value) = field(node, "value") {
                    symbol.type_text = Some(text(value, self.src).to_owned());
                    symbol.type_names = type_names(value, self.src);
                }
                self.push(symbol);
            }
            "enum_declaration" => {
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                self.enumeration(node, prefix, parent, exported, doc_node);
            }
            "lexical_declaration" | "variable_declaration" => {
                let doc_node = if exported { node.parent().unwrap_or(node) } else { node };
                let is_const = children(node).iter().any(|c| c.kind() == "const");
                for declarator in named_children(node) {
                    if declarator.kind() == "variable_declarator" {
                        self.declarator(declarator, node, doc_node, prefix, parent, exported, is_const);
                    }
                }
            }
            "ambient_declaration" => {
                for child in named_children(node) {
                    self.statement(child, parent, prefix, true);
                }
            }
            "module" | "internal_module" => {
                // `namespace X { ... }` / `declare module 'x' { ... }`.
                let name = field_text(node, "name", self.src).map(strip_quotes).unwrap_or_default();
                if let Some(body) = field(node, "body") {
                    let symbol = new_symbol("module", &name, format!("{prefix}{name}"), node, Some(body), self.src);
                    let index = self.push(symbol);
                    let inner = format!("{prefix}{name}.");
                    self.statements(body, Some(index), &inner);
                }
            }
            "expression_statement" => {
                // Top-level code: routes, IPC handlers, tests, module.exports.
                self.top_expression(node, parent, prefix);
            }
            "statement_block" | "if_statement" | "try_statement" | "for_statement" | "for_in_statement" | "while_statement" => {
                self.body(node, parent);
            }
            _ => {}
        }
    }

    fn import(&mut self, node: Node) {
        let Some(source) = field(node, "source") else { return };
        let module = strip_quotes(text(source, self.src));
        let line = line(node);
        let clause = named_children(node).into_iter().find(|c| c.kind() == "import_clause");
        let Some(clause) = clause else {
            // Side-effect import.
            self.facts.imports.push(ImportFact { module, name: "*".into(), local: String::new(), line });
            return;
        };
        for part in named_children(clause) {
            match part.kind() {
                "identifier" => self.facts.imports.push(ImportFact { module: module.clone(), name: "default".into(), local: text(part, self.src).to_owned(), line }),
                "namespace_import" => {
                    let local = named_children(part).into_iter().next().map(|n| text(n, self.src).to_owned()).unwrap_or_default();
                    self.facts.imports.push(ImportFact { module: module.clone(), name: "*".into(), local, line });
                }
                "named_imports" => {
                    for spec in named_children(part) {
                        if spec.kind() != "import_specifier" {
                            continue;
                        }
                        let name = field_text(spec, "name", self.src).unwrap_or("").to_owned();
                        let local = field_text(spec, "alias", self.src).map(str::to_owned).unwrap_or_else(|| name.clone());
                        self.facts.imports.push(ImportFact { module: module.clone(), name, local, line });
                    }
                }
                _ => {}
            }
        }
    }

    fn export(&mut self, node: Node, parent: Option<usize>, prefix: &str) {
        if let Some(decl) = field(node, "declaration") {
            let before = self.facts.symbols.len();
            self.statement(decl, parent, prefix, true);
            let is_default = children(node).iter().any(|c| c.kind() == "default");
            for symbol in &self.facts.symbols[before..] {
                if symbol.parent == parent {
                    self.facts.exports.push(symbol.name.clone());
                }
            }
            if is_default && let Some(first) = self.facts.symbols.get(before) {
                let name = first.name.clone();
                self.facts.exports.push(format!("default={name}"));
            }
            return;
        }
        // `export default <expr>` / `export { a, b as c }` / `export * from './x'`.
        let source = field(node, "source").map(|s| strip_quotes(text(s, self.src)));
        for child in named_children(node) {
            match child.kind() {
                "export_clause" => {
                    for spec in named_children(child) {
                        if spec.kind() != "export_specifier" {
                            continue;
                        }
                        let name = field_text(spec, "name", self.src).unwrap_or("").to_owned();
                        let alias = field_text(spec, "alias", self.src).map(str::to_owned).unwrap_or_else(|| name.clone());
                        if let Some(source) = &source {
                            self.facts.imports.push(ImportFact { module: source.clone(), name: name.clone(), local: alias.clone(), line: line(node) });
                        }
                        self.facts.exports.push(if alias == name { name } else { format!("{alias}={name}") });
                    }
                }
                "identifier" => self.facts.exports.push(format!("default={}", text(child, self.src))),
                "arrow_function" | "function_expression" | "function" => {
                    self.function(child, "default", prefix, parent, true, node, child);
                    self.facts.exports.push("default".into());
                }
                "call_expression" => {
                    // `export default memo(Component)` and friends.
                    self.body(child, parent);
                }
                _ => {}
            }
        }
        if source.is_some() && named_children(node).iter().all(|c| c.kind() != "export_clause") {
            // `export * from './x'`.
            if let Some(src) = source {
                self.facts.imports.push(ImportFact { module: src, name: "*".into(), local: "*".into(), line: line(node) });
                self.facts.exports.push("*".into());
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn declarator(&mut self, declarator: Node, decl: Node, doc_node: Node, prefix: &str, parent: Option<usize>, exported: bool, is_const: bool) {
        let Some(name_node) = field(declarator, "name") else { return };
        let value = field(declarator, "value");
        if name_node.kind() != "identifier" {
            // Destructuring at top level: references only.
            if let Some(value) = value {
                self.body(value, parent);
            }
            return;
        }
        let name = text(name_node, self.src).to_owned();
        // `const x = require('y')`.
        if let Some(v) = value
            && v.kind() == "call_expression"
            && field(v, "function").is_some_and(|f| text(f, self.src) == "require")
            && let Some(arg) = field(v, "arguments").and_then(|a| named_children(a).into_iter().next())
            && let Some(module) = string_content(arg, self.src)
        {
            self.facts.imports.push(ImportFact { module, name: "*".into(), local: name, line: line(declarator) });
            return;
        }
        // Function values: arrow functions, function expressions, or a call wrapping one
        // (`memo(() => ..)`, `forwardRef(..)`, `useCallback(..)`). Structural.
        let function = value.and_then(|v| match v.kind() {
            "arrow_function" | "function_expression" | "function" | "generator_function" => Some(v),
            "call_expression" => field(v, "arguments")
                .and_then(|a| named_children(a).into_iter().rev().find(|c| matches!(c.kind(), "arrow_function" | "function_expression" | "function"))),
            _ => None,
        });
        if let Some(function) = function {
            let span = if parent.is_none() { decl } else { declarator };
            let index = self.function(function, &name, prefix, parent, exported, doc_node, span);
            if let Some(v) = value
                && v.kind() == "call_expression"
            {
                // The wrapper call itself (memo, forwardRef) is a reference from the definition (the
                // memory plugin's original took the last symbol pushed, a component's last prop).
                if let Some(f) = field(v, "function") {
                    let (n, q) = member_name(f, self.src);
                    push_ref(&mut self.facts.refs, Some(index), RefKind::Call, &n, q, Vec::new(), v);
                }
            }
            return;
        }
        let kind = if is_const { "const" } else { "variable" };
        // Module-level variables only become symbols; locals inside functions do not.
        let mut symbol = new_symbol(kind, &name, format!("{prefix}{name}"), if parent.is_none() { decl } else { declarator }, None, self.src);
        symbol.doc = self.doc_before(doc_node);
        symbol.exported = exported;
        symbol.parent = parent;
        if let Some(t) = field(declarator, "type") {
            symbol.type_text = Some(text(t, self.src).trim_start_matches(':').trim().to_owned());
            symbol.type_names = type_names(t, self.src);
        }
        if let Some(v) = value {
            symbol.value = Some(text(v, self.src).to_owned());
        }
        let index = self.push(symbol);
        if let Some(v) = value {
            if v.kind() == "object" {
                self.object_methods(v, index, &format!("{prefix}{name}."));
            } else {
                self.body(v, Some(index));
            }
        }
    }

    /// Methods of an object literal (`export const api = { get(id) {..}, list: () => .. }`).
    fn object_methods(&mut self, object: Node, parent: usize, prefix: &str) {
        for member in named_children(object) {
            match member.kind() {
                "method_definition" => {
                    let name = field_text(member, "name", self.src).map(strip_quotes).unwrap_or_default();
                    self.function(member, &name, prefix, Some(parent), false, member, member);
                    if let Some(last) = self.facts.symbols.last_mut() {
                        last.kind = "method".into();
                    }
                }
                "pair" => {
                    let key = field_text(member, "key", self.src).map(strip_quotes).unwrap_or_default();
                    let Some(value) = field(member, "value") else { continue };
                    if matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
                        self.function(value, &key, prefix, Some(parent), false, member, member);
                        if let Some(last) = self.facts.symbols.last_mut() {
                            last.kind = "method".into();
                        }
                    } else {
                        self.body(value, Some(parent));
                    }
                }
                _ => self.body(member, Some(parent)),
            }
        }
    }

    /// A function-like definition. `span` is the node whose text and lines the symbol covers.
    #[allow(clippy::too_many_arguments)]
    fn function(&mut self, node: Node, name: &str, prefix: &str, parent: Option<usize>, exported: bool, doc_node: Node, span: Node) -> usize {
        let body = field(node, "body");
        let mut symbol = new_symbol("function", name, format!("{prefix}{name}"), span, body, self.src);
        // Signature from the definition node itself when the span is a wider declaration.
        symbol.signature = if span.id() == node.id() { symbol.signature } else { signature(span, body, self.src) };
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
        // Start line: skip decorators.
        if let Some(first) = children(span).into_iter().find(|c| c.kind() != "decorator" && c.kind() != "comment") {
            symbol.start_line = line(first);
        }
        symbol.doc = self.doc_before(doc_node);
        symbol.exported = exported;
        symbol.parent = parent;
        symbol.is_async = children(node).iter().any(|c| c.kind() == "async");
        symbol.is_static = children(node).iter().any(|c| c.kind() == "static");
        symbol.decorators = children(span).iter().filter(|c| c.kind() == "decorator").map(|c| text(*c, self.src).to_owned()).collect();
        if let Some(params) = field(node, "parameters").or_else(|| field(node, "parameter")) {
            symbol.params = self.params(params);
        }
        if let Some(ret) = field(node, "return_type") {
            symbol.return_type = Some(text(ret, self.src).trim_start_matches(':').trim().to_owned());
            symbol.return_type_names = type_names(ret, self.src);
        }
        // Structural JSX rule: a capitalized function whose body contains JSX is a React component.
        let capitalized = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        if capitalized && body.is_some_and(|b| contains_jsx(b)) {
            symbol.kind = "component".into();
        }
        let is_component = symbol.kind == "component";
        let first_param = field(node, "parameters").and_then(|p| named_children(p).into_iter().find(|c| !c.kind().contains("comment")));
        let index = self.push(symbol);
        if is_component && let Some(first) = first_param {
            self.component_props(first, index, &format!("{prefix}{name}."));
        }
        if let Some(body) = body {
            self.body(body, Some(index));
        }
        index
    }

    fn params(&self, params: Node) -> Vec<ParamFact> {
        let mut out = Vec::new();
        let list: Vec<Node> = if params.kind() == "formal_parameters" { named_children(params) } else { vec![params] };
        let mut position = 0;
        for p in list {
            if p.kind().contains("comment") || p.kind() == "decorator" {
                continue;
            }
            let (pattern, type_node, value, optional) = match p.kind() {
                "required_parameter" | "optional_parameter" => (field(p, "pattern"), field(p, "type"), field(p, "value"), p.kind() == "optional_parameter"),
                "assignment_pattern" => (field(p, "left"), None, field(p, "right"), false),
                _ => (Some(p), None, None, false),
            };
            let Some(pattern) = pattern else { continue };
            // `this` parameters (TS) are not real parameters.
            if text(pattern, self.src) == "this" {
                continue;
            }
            let (name, variadic) = match pattern.kind() {
                "rest_pattern" => (named_children(pattern).into_iter().next().map(|n| text(n, self.src).to_owned()).unwrap_or_default(), true),
                "assignment_pattern" => (field_text(pattern, "left", self.src).unwrap_or("").to_owned(), false),
                _ => (text(pattern, self.src).to_owned(), false),
            };
            let mut fact = param(&squash(&name), position);
            fact.optional = optional || value.is_some();
            fact.variadic = variadic;
            if let Some(t) = type_node {
                fact.type_text = Some(text(t, self.src).trim_start_matches(':').trim().to_owned());
                fact.type_names = type_names(t, self.src);
            }
            let default = value.or_else(|| if pattern.kind() == "assignment_pattern" { field(pattern, "right") } else { None });
            if let Some(v) = default {
                fact.default_value = Some(text(v, self.src).to_owned());
            }
            out.push(fact);
            position += 1;
        }
        out
    }

    /// React props from the component's first parameter: destructured names (with defaults) and the
    /// fields of an inline object type. Named prop types are resolved after the whole file is read.
    fn component_props(&mut self, first: Node, component: usize, prefix: &str) {
        let (pattern, type_node) = match first.kind() {
            "required_parameter" | "optional_parameter" => (field(first, "pattern"), field(first, "type")),
            _ => (Some(first), None),
        };
        let mut seen: Vec<String> = Vec::new();
        let types: Vec<(String, String, bool)> = type_node
            .and_then(|t| named_children(t).into_iter().find(|c| c.kind() == "object_type"))
            .map(|object| self.object_type_fields(object))
            .unwrap_or_default();
        if let Some(pattern) = pattern
            && pattern.kind() == "object_pattern"
        {
            for prop in named_children(pattern) {
                let (pname, default) = match prop.kind() {
                    "shorthand_property_identifier_pattern" => (text(prop, self.src).to_owned(), None),
                    "object_assignment_pattern" => (field_text(prop, "left", self.src).unwrap_or("").to_owned(), field(prop, "right").map(|r| text(r, self.src).to_owned())),
                    "pair_pattern" => (field_text(prop, "key", self.src).unwrap_or("").to_owned(), field(prop, "value").filter(|v| v.kind() == "assignment_pattern").and_then(|v| field(v, "right")).map(|r| text(r, self.src).to_owned())),
                    "rest_pattern" => continue,
                    _ => continue,
                };
                if pname.is_empty() {
                    continue;
                }
                let type_text = types.iter().find(|(n, _, _)| *n == pname).map(|(_, t, _)| t.clone());
                self.prop_symbol(&pname, default, type_text, component, prefix, prop);
                seen.push(pname);
            }
        }
        for (pname, type_text, _) in types {
            if !seen.contains(&pname) {
                self.prop_symbol(&pname, None, Some(type_text), component, prefix, first);
            }
        }
        // A named props type (`props: Props`) is resolved against the file's interfaces later.
        if let Some(t) = type_node
            && let Some(named) = named_children(t).into_iter().find(|c| c.kind() == "type_identifier")
        {
            let name = text(named, self.src).to_owned();
            if let Some(sym) = self.facts.symbols.get_mut(component) {
                sym.type_names.push(format!("\u{1}props\u{1}{name}"));
            }
        }
    }

    fn prop_symbol(&mut self, name: &str, default: Option<String>, type_text: Option<String>, component: usize, prefix: &str, node: Node) {
        let mut prop = new_symbol("prop", name, format!("{prefix}{name}"), node, None, self.src);
        prop.parent = Some(component);
        prop.value = default.clone();
        prop.type_text = type_text;
        prop.signature = match (&prop.type_text, &default) {
            (Some(t), Some(d)) => format!("{name}: {t} = {d}"),
            (Some(t), None) => format!("{name}: {t}"),
            (None, Some(d)) => format!("{name} = {d}"),
            (None, None) => name.to_owned(),
        };
        self.push(prop);
    }

    fn object_type_fields(&self, object: Node) -> Vec<(String, String, bool)> {
        let mut out = Vec::new();
        for member in named_children(object) {
            if member.kind() == "property_signature" {
                let name = field_text(member, "name", self.src).unwrap_or("").to_owned();
                let t = field(member, "type").map(|t| text(t, self.src).trim_start_matches(':').trim().to_owned()).unwrap_or_default();
                let optional = text(member, self.src).contains('?');
                out.push((name, t, optional));
            }
        }
        out
    }

    fn class(&mut self, node: Node, prefix: &str, parent: Option<usize>, exported: bool, doc_node: Node) {
        let name = field_text(node, "name", self.src).unwrap_or("default").to_owned();
        let body = field(node, "body");
        let mut symbol = new_symbol("class", &name, format!("{prefix}{name}"), node, body, self.src);
        if let Some(first) = children(node).into_iter().find(|c| c.kind() != "decorator" && c.kind() != "comment") {
            symbol.start_line = line(first);
        }
        symbol.doc = self.doc_before(doc_node);
        symbol.exported = exported;
        symbol.parent = parent;
        symbol.decorators = children(node).iter().filter(|c| c.kind() == "decorator").map(|c| text(*c, self.src).to_owned()).collect();
        for heritage in named_children(node).into_iter().filter(|c| c.kind() == "class_heritage") {
            for clause in named_children(heritage) {
                match clause.kind() {
                    "extends_clause" => {
                        for v in named_children(clause) {
                            if v.kind() != "type_arguments" {
                                symbol.extends.push(member_name(v, self.src).0);
                            }
                        }
                    }
                    "implements_clause" => {
                        for t in named_children(clause) {
                            symbol.implements.push(text(t, self.src).split('<').next().unwrap_or("").to_owned());
                        }
                    }
                    // JavaScript grammar: `class A extends B` puts the expression directly.
                    _ => symbol.extends.push(member_name(clause, self.src).0),
                }
            }
        }
        // React class components: extends (React.)Component / PureComponent (API names).
        let is_component = symbol.extends.iter().any(|e| e == "Component" || e == "PureComponent");
        if is_component {
            symbol.kind = "component".into();
        }
        let index = self.push(symbol);
        let Some(body) = body else { return };
        let member_prefix = format!("{prefix}{name}.");
        for member in named_children(body) {
            match member.kind() {
                "method_definition" | "abstract_method_signature" | "method_signature" => {
                    let mname = field_text(member, "name", self.src).map(strip_quotes).unwrap_or_default();
                    let m = self.function(member, &mname, &member_prefix, Some(index), exported, member, member);
                    if let Some(sym) = self.facts.symbols.get_mut(m) {
                        sym.kind = "method".into();
                    }
                }
                "public_field_definition" | "field_definition" => {
                    let fname = field_text(member, "name", self.src).or_else(|| field_text(member, "property", self.src)).map(strip_quotes).unwrap_or_default();
                    let value = field(member, "value");
                    if let Some(v) = value
                        && matches!(v.kind(), "arrow_function" | "function_expression" | "function")
                    {
                        let m = self.function(v, &fname, &member_prefix, Some(index), exported, member, member);
                        if let Some(sym) = self.facts.symbols.get_mut(m) {
                            sym.kind = "method".into();
                            sym.is_static = children(member).iter().any(|c| c.kind() == "static");
                        }
                        continue;
                    }
                    let mut f = new_symbol("field", &fname, format!("{member_prefix}{fname}"), member, None, self.src);
                    f.parent = Some(index);
                    f.doc = self.doc_before(member);
                    f.is_static = children(member).iter().any(|c| c.kind() == "static");
                    if let Some(t) = field(member, "type") {
                        f.type_text = Some(text(t, self.src).trim_start_matches(':').trim().to_owned());
                        f.type_names = type_names(t, self.src);
                    }
                    if let Some(v) = value {
                        f.value = Some(text(v, self.src).to_owned());
                    }
                    let fi = self.push(f);
                    if let Some(v) = value {
                        self.body(v, Some(fi));
                    }
                }
                "class_static_block" => self.body(member, Some(index)),
                _ => {}
            }
        }
    }

    fn interface(&mut self, node: Node, prefix: &str, parent: Option<usize>, exported: bool, doc_node: Node) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(node, "body");
        let mut symbol = new_symbol("interface", &name, format!("{prefix}{name}"), node, body, self.src);
        symbol.doc = self.doc_before(doc_node);
        symbol.exported = exported;
        symbol.parent = parent;
        for clause in named_children(node).into_iter().filter(|c| c.kind() == "extends_type_clause") {
            for t in named_children(clause) {
                symbol.extends.push(text(t, self.src).split('<').next().unwrap_or("").to_owned());
            }
        }
        let index = self.push(symbol);
        let Some(body) = body else { return };
        let member_prefix = format!("{prefix}{name}.");
        for member in named_children(body) {
            match member.kind() {
                "property_signature" => {
                    let fname = field_text(member, "name", self.src).map(strip_quotes).unwrap_or_default();
                    let mut f = new_symbol("field", &fname, format!("{member_prefix}{fname}"), member, None, self.src);
                    f.parent = Some(index);
                    f.doc = self.doc_before(member);
                    if let Some(t) = field(member, "type") {
                        f.type_text = Some(text(t, self.src).trim_start_matches(':').trim().to_owned());
                        f.type_names = type_names(t, self.src);
                    }
                    self.push(f);
                }
                "method_signature" => {
                    let mname = field_text(member, "name", self.src).map(strip_quotes).unwrap_or_default();
                    let m = self.function(member, &mname, &member_prefix, Some(index), exported, member, member);
                    if let Some(sym) = self.facts.symbols.get_mut(m) {
                        sym.kind = "method".into();
                    }
                }
                _ => {}
            }
        }
    }

    fn enumeration(&mut self, node: Node, prefix: &str, parent: Option<usize>, exported: bool, doc_node: Node) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(node, "body");
        let mut symbol = new_symbol("enum", &name, format!("{prefix}{name}"), node, body, self.src);
        symbol.doc = self.doc_before(doc_node);
        symbol.exported = exported;
        symbol.parent = parent;
        let index = self.push(symbol);
        let Some(body) = body else { return };
        for member in named_children(body) {
            let (vname, value) = match member.kind() {
                "enum_assignment" => (field_text(member, "name", self.src).unwrap_or("").to_owned(), field(member, "value").map(|v| text(v, self.src).to_owned())),
                "property_identifier" | "string" => (strip_quotes(text(member, self.src)), None),
                _ => continue,
            };
            let mut v = new_symbol("variant", &vname, format!("{prefix}{name}.{vname}"), member, None, self.src);
            v.parent = Some(index);
            v.value = value;
            self.push(v);
        }
    }

    /// Top-level expression statements: tests, routes, IPC handlers, module.exports, plain calls.
    fn top_expression(&mut self, node: Node, parent: Option<usize>, prefix: &str) {
        let Some(expr) = named_children(node).into_iter().next() else { return };
        if expr.kind() == "call_expression" && self.special_call(expr, parent, prefix) {
            return;
        }
        if expr.kind() == "assignment_expression"
            && let (Some(left), Some(right)) = (field(expr, "left"), field(expr, "right"))
        {
            let target = text(left, self.src);
            if target == "module.exports" || target.starts_with("exports.") || target.starts_with("module.exports.") {
                let name = target.rsplit('.').next().unwrap_or("default").to_owned();
                if matches!(right.kind(), "arrow_function" | "function_expression" | "function") {
                    self.function(right, if name == "exports" { "default" } else { &name }, prefix, parent, true, node, node);
                    return;
                }
                if right.kind() == "object" {
                    for member in named_children(right) {
                        if let Some(key) = field_text(member, "key", self.src).or_else(|| (member.kind() == "shorthand_property_identifier").then(|| text(member, self.src))) {
                            self.facts.exports.push(key.to_owned());
                        }
                    }
                } else if right.kind() == "identifier" {
                    self.facts.exports.push(format!("default={}", text(right, self.src)));
                }
            }
        }
        self.body(expr, parent);
    }

    /// Tests, describe blocks, express routes and IPC handlers. Returns true when handled.
    fn special_call(&mut self, call: Node, parent: Option<usize>, prefix: &str) -> bool {
        let Some(function) = field(call, "function") else { return false };
        let args: Vec<Node> = field(call, "arguments").map(|a| named_children(a).into_iter().filter(|c| !c.kind().contains("comment")).collect()).unwrap_or_default();
        let callee = text(function, self.src);
        // Test frameworks (jest, vitest, mocha, node:test): API names `describe`, `test`, `it`.
        let base = callee.split('.').next().unwrap_or(callee);
        let last_fn = args.iter().rev().find(|a| matches!(a.kind(), "arrow_function" | "function_expression" | "function"));
        if matches!(base, "describe" | "test" | "it") && let (Some(title), Some(func)) = (args.first(), last_fn) {
            let title = template_pattern(*title, self.src).unwrap_or_else(|| text(*title, self.src).to_owned());
            if base == "describe" {
                self.suites.push(title);
                if let Some(body) = field(*func, "body") {
                    self.statements_in(body, parent, prefix);
                }
                self.suites.pop();
            } else {
                let mut full = self.suites.clone();
                full.push(title.clone());
                let qname = full.join(" > ");
                let body = field(*func, "body");
                let mut symbol = new_symbol("test", &title, format!("test:{qname}"), call, body, self.src);
                symbol.parent = parent;
                symbol.framework = Some("js".into());
                symbol.signature = format!("{base}({})", text(args[0], self.src));
                let index = self.push(symbol);
                if let Some(body) = body {
                    self.body(body, Some(index));
                }
            }
            return true;
        }
        // Express-style routes: `app.get('/path', handler)` (API shape: verb member call whose first
        // argument is a path literal and whose last argument is a handler).
        if function.kind() == "member_expression"
            && let Some(prop) = field(function, "property")
        {
            let verb = text(prop, self.src);
            let object = field(function, "object").map(|o| text(o, self.src)).unwrap_or("");
            if (HTTP_VERBS.contains(&verb) || verb == "all") && args.len() >= 2
                && let Some(path) = string_content(args[0], self.src)
                && path.starts_with('/')
            {
                let handler = args[args.len() - 1];
                let method = if verb == "all" { "ANY".to_owned() } else { verb.to_uppercase() };
                self.route_or_channel("route", &method, &path, handler, call, parent, prefix);
                return true;
            }
            // Electron main process: `ipcMain.handle('ch', fn)` / `ipcMain.on('ch', fn)`.
            if object.ends_with("ipcMain") && matches!(verb, "handle" | "on" | "handleOnce" | "once") && args.len() >= 2
                && let Some(channel) = string_content(args[0], self.src)
            {
                let handler = args[args.len() - 1];
                self.route_or_channel("ipc_channel", "IPC", &channel, handler, call, parent, prefix);
                return true;
            }
        }
        false
    }

    fn statements_in(&mut self, body: Node, parent: Option<usize>, prefix: &str) {
        for statement in named_children(body) {
            if statement.kind() == "expression_statement" {
                self.top_expression(statement, parent, prefix);
            } else {
                self.body(statement, parent);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn route_or_channel(&mut self, kind: &str, method: &str, path: &str, handler: Node, call: Node, parent: Option<usize>, prefix: &str) {
        let (name, qname) = if kind == "route" { (format!("{method} {path}"), format!("route:{method} {path}")) } else { (path.to_owned(), format!("ipc:{path}")) };
        // Inline handlers become their own function so HANDLES has a target.
        let handler_ref = if matches!(handler.kind(), "arrow_function" | "function_expression" | "function") {
            let fname = format!("{name} handler");
            self.function(handler, &fname, prefix, parent, false, call, handler);
            Some(RefTarget { name: fname, qualifier: None })
        } else {
            let (n, q) = member_name(handler, self.src);
            Some(RefTarget { name: n, qualifier: q })
        };
        let mut symbol = new_symbol(kind, &name, qname, call, None, self.src);
        symbol.signature = text(call, self.src).lines().next().unwrap_or("").to_owned();
        symbol.http_method = (kind == "route").then(|| method.to_owned());
        symbol.route_path = Some(path.to_owned());
        symbol.handler = handler_ref;
        symbol.parent = parent;
        self.push(symbol);
    }

    /// References inside a body.
    fn body(&mut self, node: Node, from: Option<usize>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "call_expression" => self.call(current, from),
                "new_expression" => {
                    if let Some(ctor) = field(current, "constructor") {
                        let (n, q) = member_name(ctor, self.src);
                        let args = field(current, "arguments").map(|a| self.arg_texts(a)).unwrap_or_default();
                        let thrown = current.parent().is_some_and(|p| p.kind() == "throw_statement");
                        push_ref(&mut self.facts.refs, from, if thrown { RefKind::Throw } else { RefKind::Instantiate }, &n, q.clone(), args.clone(), current);
                        if thrown {
                            push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &n, q, args, current);
                        }
                    }
                }
                "jsx_opening_element" | "jsx_self_closing_element" => {
                    if let Some(name) = field(current, "name") {
                        let tag = text(name, self.src);
                        // JSX rule: lowercase tags are intrinsic DOM elements, capitalized ones are components.
                        if tag.chars().next().is_some_and(|c| c.is_ascii_uppercase()) || tag.contains('.') {
                            let attrs: Vec<String> = named_children(current).into_iter().filter(|c| matches!(c.kind(), "jsx_attribute" | "jsx_expression")).map(|c| text(c, self.src).to_owned()).collect();
                            let (n, q) = match tag.rsplit_once('.') {
                                Some((q, n)) => (n.to_owned(), Some(q.to_owned())),
                                None => (tag.to_owned(), None),
                            };
                            push_ref(&mut self.facts.refs, from, RefKind::Render, &n, q, attrs, current);
                        }
                    }
                }
                "member_expression" => {
                    // Environment reads: `process.env.X`, `import.meta.env.X` (API names).
                    let t = text(current, self.src);
                    if let Some(key) = t.strip_prefix("process.env.").or_else(|| t.strip_prefix("import.meta.env."))
                        && !key.contains('.')
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, key, Some("env".into()), Vec::new(), current);
                        continue;
                    }
                    if let (Some(object), Some(prop)) = (field(current, "object"), field(current, "property"))
                        && object.kind() == "this"
                    {
                        let is_callee = current.parent().is_some_and(|p| p.kind() == "call_expression" && field(p, "function").is_some_and(|f| f.id() == current.id()));
                        let is_write = current.parent().is_some_and(|p| matches!(p.kind(), "assignment_expression" | "augmented_assignment_expression") && field(p, "left").is_some_and(|l| l.id() == current.id()));
                        if is_write {
                            push_ref(&mut self.facts.refs, from, RefKind::Write, text(prop, self.src), Some("this".into()), Vec::new(), current);
                        } else if !is_callee {
                            push_ref(&mut self.facts.refs, from, RefKind::Read, text(prop, self.src), Some("this".into()), Vec::new(), current);
                        }
                    }
                }
                "variable_declarator" => {
                    if let (Some(from), Some(name)) = (from, field(current, "name"))
                        && name.kind() == "identifier"
                    {
                        let declared = field(current, "type").and_then(|t| type_names(t, self.src).into_iter().next());
                        let inferred = field(current, "value").filter(|v| v.kind() == "new_expression").and_then(|v| field(v, "constructor")).map(|c| member_name(c, self.src).0);
                        if let Some(type_name) = declared.or(inferred).filter(|t| !t.is_empty()) {
                            self.facts.locals.push(LocalFact { from, name: text(name, self.src).to_owned(), type_name });
                        }
                    }
                }
                "subscript_expression" => {
                    if let (Some(object), Some(index)) = (field(current, "object"), field(current, "index"))
                        && text(object, self.src) == "process.env"
                        && let Some(key) = string_content(index, self.src)
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), current);
                    }
                }
                "assignment_expression" | "augmented_assignment_expression" => {
                    if let Some(left) = field(current, "left")
                        && left.kind() == "identifier"
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::Write, text(left, self.src), None, Vec::new(), current);
                    }
                }
                "identifier" => {
                    let is_callee = current.parent().is_some_and(|p| matches!(p.kind(), "call_expression" | "new_expression") && (field(p, "function").or_else(|| field(p, "constructor"))).is_some_and(|f| f.id() == current.id()));
                    let is_write = current.parent().is_some_and(|p| matches!(p.kind(), "assignment_expression" | "augmented_assignment_expression") && field(p, "left").is_some_and(|l| l.id() == current.id()));
                    let is_decl = current.parent().is_some_and(|p| p.kind() == "variable_declarator" && field(p, "name").is_some_and(|l| l.id() == current.id()));
                    if !is_callee && !is_write && !is_decl {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, text(current, self.src), None, Vec::new(), current);
                    }
                }
                // Nested named functions / classes inside bodies stay part of the enclosing body.
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn arg_texts(&self, arguments: Node) -> Vec<String> {
        named_children(arguments).into_iter().filter(|c| !c.kind().contains("comment")).map(|c| text(c, self.src).to_owned()).collect()
    }

    fn call(&mut self, node: Node, from: Option<usize>) {
        let Some(function) = field(node, "function") else { return };
        let arguments = field(node, "arguments");
        let arg_nodes: Vec<Node> = arguments.map(|a| named_children(a).into_iter().filter(|c| !c.kind().contains("comment")).collect()).unwrap_or_default();
        let args: Vec<String> = arg_nodes.iter().map(|c| text(*c, self.src).to_owned()).collect();
        let (name, qualifier) = member_name(function, self.src);
        if name.is_empty() {
            return;
        }
        // HTTP requests with literal URLs: `fetch(url, {method})`, `axios.get(url)`, `axios({url, method})`.
        if name == "fetch" && qualifier.as_deref().is_none_or(|q| q == "window" || q == "globalThis") {
            if let Some(url) = arg_nodes.first().and_then(|a| template_pattern(*a, self.src)) {
                let method = arg_nodes.get(1).and_then(|o| object_string_field(*o, "method", self.src)).unwrap_or_else(|| "GET".into());
                self.facts.requests.push(RequestFact { from, method: method.to_uppercase(), url, line: line(node), args: args.clone() });
            }
        } else if qualifier.as_deref() == Some("axios") && (HTTP_VERBS.contains(&name.as_str()) || name == "request") {
            if let Some(url) = arg_nodes.first().and_then(|a| template_pattern(*a, self.src)) {
                self.facts.requests.push(RequestFact { from, method: name.to_uppercase(), url, line: line(node), args: args.clone() });
            }
        } else if name == "axios" && qualifier.is_none()
            && let Some(config) = arg_nodes.first()
                && let Some(url) = object_string_field(*config, "url", self.src)
            {
                let method = object_string_field(*config, "method", self.src).unwrap_or_else(|| "GET".into());
                self.facts.requests.push(RequestFact { from, method: method.to_uppercase(), url, line: line(node), args: args.clone() });
            }
        // Electron renderer IPC: `ipcRenderer.invoke('ch', ..)` (API names).
        if qualifier.as_deref().is_some_and(|q| q.ends_with("ipcRenderer")) && matches!(name.as_str(), "invoke" | "send" | "sendSync")
            && let Some(channel) = arg_nodes.first().and_then(|a| string_content(*a, self.src)) {
                self.facts.ipc_calls.push(IpcCallFact { from, channel, line: line(node), args: args[1..].to_vec() });
            }
        // Tauri: `invoke('command', {..})` — confirmed against imports after the walk.
        if qualifier.is_none()
            && let Some(channel) = arg_nodes.first().and_then(|a| string_content(*a, self.src))
        {
            self.tauri_calls.push((name.clone(), IpcCallFact { from, channel, line: line(node), args: args.get(1..).map(<[String]>::to_vec).unwrap_or_default() }));
        }
        // Child processes: `spawn('bash', ['x.sh'])`, `execFile('./x.sh')`, `exec('bash x.sh')`.
        if matches!(name.as_str(), "spawn" | "spawnSync" | "exec" | "execSync" | "execFile" | "execFileSync" | "fork") {
            let mut stack = arg_nodes.clone();
            while let Some(arg) = stack.pop() {
                if let Some(lit) = string_content(arg, self.src) {
                    for word in lit.split_whitespace() {
                        if looks_like_script_path(word) {
                            self.facts.runs.push(RunFact { from, target: word.to_owned(), line: line(node) });
                        }
                    }
                } else if arg.kind() == "array" {
                    stack.extend(named_children(arg));
                }
            }
        }
        // React rule of hooks: functions named `use[A-Z]...` are hooks (naming convention of the API).
        let is_hook = name.len() > 3 && name.starts_with("use") && name[3..].chars().next().is_some_and(|c| c.is_ascii_uppercase());
        if is_hook {
            push_ref(&mut self.facts.refs, from, RefKind::UseHook, &name, qualifier.clone(), args.clone(), node);
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
    }
}

/// `a.b.c` -> (`c`, Some(`a.b`)); `f` -> (`f`, None). Non-simple receivers become `Some("")`.
fn member_name(node: Node, src: &str) -> (String, Option<String>) {
    match node.kind() {
        "identifier" | "property_identifier" | "type_identifier" => (text(node, src).to_owned(), None),
        "member_expression" | "nested_identifier" | "nested_type_identifier" => {
            let prop = field(node, "property").or_else(|| field(node, "name")).map(|p| text(p, src).to_owned()).unwrap_or_default();
            let object = field(node, "object").or_else(|| field(node, "module"));
            let qualifier = object.map(|o| match o.kind() {
                "identifier" | "this" | "member_expression" | "nested_identifier" | "super" => {
                    let t = text(o, src);
                    if t.contains('(') { String::new() } else { t.to_owned() }
                }
                _ => String::new(),
            });
            (prop, qualifier)
        }
        "generic_type" => named_children(node).into_iter().next().map(|n| member_name(n, src)).unwrap_or_default(),
        _ => (String::new(), None),
    }
}

fn object_string_field(object: Node, key: &str, src: &str) -> Option<String> {
    if object.kind() != "object" {
        return None;
    }
    for pair in named_children(object) {
        if pair.kind() == "pair"
            && field_text(pair, "key", src).map(strip_quotes).as_deref() == Some(key)
            && let Some(value) = field(pair, "value")
        {
            return template_pattern(value, src);
        }
    }
    None
}

fn contains_jsx(node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "jsx_element" | "jsx_self_closing_element" | "jsx_fragment") {
            return true;
        }
        // Nested function definitions have their own JSX.
        if current.id() != node.id() && matches!(current.kind(), "function_declaration" | "class_declaration") {
            continue;
        }
        stack.extend(named_children(current));
    }
    false
}

/// Props of components typed with a named props interface / type literal in the same file.
fn resolve_component_props(facts: &mut FileFacts) {
    let mut additions: Vec<SymbolFact> = Vec::new();
    let mut type_fills: Vec<(usize, String, String)> = Vec::new();
    for (index, symbol) in facts.symbols.iter().enumerate() {
        let Some(props_type) = symbol.type_names.iter().find_map(|t| t.strip_prefix("\u{1}props\u{1}")) else { continue };
        let existing: Vec<&str> = facts.symbols.iter().filter(|s| s.kind == "prop" && s.parent == Some(index)).map(|s| s.name.as_str()).collect();
        let Some(type_index) = facts.symbols.iter().position(|s| (s.kind == "interface" || s.kind == "type") && s.name == props_type) else { continue };
        for field_sym in facts.symbols.iter().filter(|s| s.kind == "field" && s.parent == Some(type_index)) {
            if existing.contains(&field_sym.name.as_str()) {
                if let Some(t) = &field_sym.type_text {
                    type_fills.push((index, field_sym.name.clone(), t.clone()));
                }
                continue;
            }
            let mut prop = field_sym.clone();
            prop.kind = "prop".into();
            prop.parent = Some(index);
            prop.qname = format!("{}.{}", symbol.qname, field_sym.name);
            prop.signature = match &prop.type_text {
                Some(t) => format!("{}: {t}", prop.name),
                None => prop.name.clone(),
            };
            additions.push(prop);
        }
    }
    for symbol in &mut facts.symbols {
        symbol.type_names.retain(|t| !t.starts_with('\u{1}'));
    }
    for (component, name, type_text) in type_fills {
        if let Some(prop) = facts.symbols.iter_mut().find(|s| s.kind == "prop" && s.parent == Some(component) && s.name == name && s.type_text.is_none()) {
            prop.signature = match &prop.value {
                Some(d) => format!("{name}: {type_text} = {d}"),
                None => format!("{name}: {type_text}"),
            };
            prop.type_text = Some(type_text);
        }
    }
    // Props declared through the named type keep the defaults written in the destructuring.
    for prop in additions {
        facts.symbols.push(prop);
    }
}
