//! Rust extraction (tree-sitter-rust): items with qualified names, signatures, docs, attributes
//! and parameters; calls, instantiations, field and const reads and writes, macro calls (with the
//! calls inside their arguments), imports, `impl Trait for Type`, receiver-typed locals, environment
//! reads, programs run, errors returned, and axum / actix / Tauri routes and commands.
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/rust.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImplFact, ImportFact, LocalFact, RefKind, RefTarget, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};

fn language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
}

#[derive(Clone, Default)]
struct Scope {
    parent: Option<usize>,
    /// Qualified-name prefix (`mod::` or `Type::`).
    prefix: String,
    /// Inside `impl X` / `trait X`: the type (or trait) name.
    owner: Option<String>,
    impl_trait: Option<String>,
    in_trait: bool,
}

pub fn extract(src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let mut walker = Walker { src, facts };
    walker.items(root, &Scope::default());
}

impl<'a> Walker<'a> {
    fn items(&mut self, list: Node, scope: &Scope) {
        let nodes = named_children(list);
        for (index, node) in nodes.iter().enumerate() {
            let preceding = &nodes[..index];
            match node.kind() {
                "function_item" | "function_signature_item" => self.function(*node, preceding, scope),
                "struct_item" | "union_item" => self.structure(*node, preceding, scope),
                "enum_item" => self.enumeration(*node, preceding, scope),
                "trait_item" => self.trait_item(*node, preceding, scope),
                "impl_item" => self.impl_item(*node, scope),
                "type_item" => self.simple(*node, preceding, scope, "type"),
                "const_item" => self.simple(*node, preceding, scope, "const"),
                "static_item" => self.simple(*node, preceding, scope, "static"),
                "mod_item" => self.module(*node, preceding, scope),
                "macro_definition" => self.simple(*node, preceding, scope, "macro"),
                "use_declaration" => self.use_decl(*node, scope),
                "associated_type" => {}
                "expression_statement" | "macro_invocation" => {
                    // Top-level macro invocations (e.g. lazy statics) still reference code.
                    self.body(*node, scope.parent);
                }
                _ => {}
            }
        }
    }

    /// Doc comments and attributes immediately preceding an item.
    fn leading(&self, preceding: &[Node]) -> (Option<String>, Vec<String>) {
        let mut docs: Vec<&str> = Vec::new();
        let mut attrs: Vec<String> = Vec::new();
        for node in preceding.iter().rev() {
            match node.kind() {
                "attribute_item" => attrs.push(text(*node, self.src).to_owned()),
                "line_comment" | "block_comment" => {
                    let t = text(*node, self.src);
                    if t.starts_with("///") && !t.starts_with("////") || t.starts_with("/**") && !t.starts_with("/***") {
                        docs.push(t);
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        docs.reverse();
        attrs.reverse();
        (clean_doc(&docs), attrs)
    }

    fn visibility(&self, node: Node) -> Option<String> {
        named_children(node).into_iter().find(|c| c.kind() == "visibility_modifier").map(|c| text(c, self.src).to_owned())
    }

    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    fn function(&mut self, node: Node, preceding: &[Node], scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(node, "body");
        let (doc, attrs) = self.leading(preceding);
        // Structural rule: `#[test]` or any attribute path ending in `test` marks a test function.
        let is_test = attrs.iter().any(|a| {
            let inner = a.trim_start_matches("#[").trim_end_matches(']');
            let path = inner.split('(').next().unwrap_or(inner).trim();
            path == "test" || path.ends_with("::test") || path == "rstest" || path == "test_case"
        });
        let kind = if is_test {
            "test"
        } else if scope.owner.is_some() {
            "method"
        } else {
            "function"
        };
        let mut symbol = new_symbol(kind, &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        symbol.doc = doc;
        symbol.decorators = attrs.clone();
        symbol.parent = scope.parent;
        symbol.visibility = self.visibility(node);
        symbol.exported = symbol.visibility.is_some() || scope.impl_trait.is_some() || scope.in_trait;
        symbol.impl_trait = scope.impl_trait.clone();
        if is_test {
            symbol.framework = Some("rust".into());
        }
        if let Some(modifiers) = named_children(node).into_iter().find(|c| c.kind() == "function_modifiers") {
            symbol.is_async = text(modifiers, self.src).contains("async");
        }
        if let Some(params) = field(node, "parameters") {
            let mut position = 0;
            let mut has_self = false;
            for p in named_children(params) {
                match p.kind() {
                    "self_parameter" => has_self = true,
                    "parameter" => {
                        let pname = field_text(p, "pattern", self.src).unwrap_or("_");
                        let mut fact = param(pname.trim_start_matches("mut "), position);
                        if let Some(t) = field(p, "type") {
                            fact.type_text = Some(text(t, self.src).to_owned());
                            fact.type_names = type_names(t, self.src);
                        }
                        symbol.params.push(fact);
                        position += 1;
                    }
                    "variadic_parameter" => {
                        let mut fact = param("...", position);
                        fact.variadic = true;
                        symbol.params.push(fact);
                        position += 1;
                    }
                    _ => {}
                }
            }
            symbol.is_static = scope.owner.is_some() && !has_self;
        }
        if let Some(ret) = field(node, "return_type") {
            symbol.return_type = Some(text(ret, self.src).to_owned());
            symbol.return_type_names = type_names(ret, self.src);
        }
        let routes = self.attr_routes(&attrs);
        let tauri = attrs.iter().any(|a| a.contains("tauri::command"));
        let index = self.push(symbol);
        for (method, path) in routes {
            self.route_symbol(&method, &path, &name, None, node);
        }
        if tauri {
            let channel = attr_rename(&attrs).unwrap_or_else(|| name.clone());
            let mut ipc = new_symbol("ipc_channel", &channel, format!("ipc:{channel}"), node, None, self.src);
            ipc.code = String::new();
            ipc.signature = format!("tauri command {channel}");
            ipc.route_path = Some(channel);
            ipc.handler = Some(RefTarget { name: name.clone(), qualifier: Some(scope.prefix.trim_end_matches("::").to_owned()).filter(|s| !s.is_empty()) });
            self.push(ipc);
        }
        if let Some(body) = body {
            self.body(body, Some(index));
        }
    }

    /// actix-web style route attributes: `#[get("/path")]`, `#[route("/p", method = "GET")]`.
    fn attr_routes(&self, attrs: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for attr in attrs {
            let inner = attr.trim_start_matches("#[").trim_end_matches(']');
            let Some((path, args)) = inner.split_once('(') else { continue };
            let verb = path.rsplit("::").next().unwrap_or(path).trim();
            let args = args.trim_end_matches(')');
            let literal = args.split(',').next().map(|s| strip_quotes(s.trim())).unwrap_or_default();
            if !literal.starts_with('/') {
                continue;
            }
            if HTTP_VERBS.contains(&verb) {
                out.push((verb.to_uppercase(), literal));
            } else if verb == "route" {
                for part in args.split(',') {
                    if let Some((key, value)) = part.split_once('=')
                        && key.trim() == "method"
                    {
                        out.push((strip_quotes(value.trim()).to_uppercase(), literal.clone()));
                    }
                }
            }
        }
        out
    }

    fn route_symbol(&mut self, method: &str, path: &str, handler: &str, qualifier: Option<String>, node: Node) {
        let name = format!("{method} {path}");
        let mut route = new_symbol("route", &name, format!("route:{name}"), node, None, self.src);
        route.signature = name.clone();
        route.code = text(node, self.src).to_owned();
        route.http_method = Some(method.to_owned());
        route.route_path = Some(path.to_owned());
        route.handler = Some(RefTarget { name: handler.to_owned(), qualifier });
        self.push(route);
    }

    fn structure(&mut self, node: Node, preceding: &[Node], scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let (doc, attrs) = self.leading(preceding);
        let body = field(node, "body");
        let mut symbol = new_symbol("struct", &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        symbol.doc = doc;
        symbol.decorators = attrs;
        symbol.parent = scope.parent;
        symbol.visibility = self.visibility(node);
        symbol.exported = symbol.visibility.is_some();
        let index = self.push(symbol);
        if let Some(body) = body {
            self.fields(body, index, &format!("{}{}::", scope.prefix, name));
        }
    }

    fn fields(&mut self, list: Node, parent: usize, prefix: &str) {
        let mut position = 0;
        let nodes = named_children(list);
        for (i, field_node) in nodes.iter().enumerate() {
            let fname = match field_node.kind() {
                "field_declaration" => field_text(*field_node, "name", self.src).map(str::to_owned),
                _ if list.kind() == "ordered_field_declaration_list" && field_node.kind() != "attribute_item" && field_node.kind() != "visibility_modifier" => {
                    let n = position.to_string();
                    position += 1;
                    Some(n)
                }
                _ => None,
            };
            let Some(fname) = fname else { continue };
            let (doc, _) = self.leading(&nodes[..i]);
            let mut symbol = new_symbol("field", &fname, format!("{prefix}{fname}"), *field_node, None, self.src);
            symbol.doc = doc;
            symbol.parent = Some(parent);
            let type_node = field(*field_node, "type").or(if field_node.kind() == "field_declaration" { None } else { Some(*field_node) });
            if let Some(t) = type_node {
                symbol.type_text = Some(text(t, self.src).to_owned());
                symbol.type_names = type_names(t, self.src);
            }
            symbol.visibility = self.visibility(*field_node);
            self.push(symbol);
        }
    }

    fn enumeration(&mut self, node: Node, preceding: &[Node], scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let (doc, attrs) = self.leading(preceding);
        let body = field(node, "body");
        let mut symbol = new_symbol("enum", &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        symbol.doc = doc;
        symbol.decorators = attrs;
        symbol.parent = scope.parent;
        symbol.visibility = self.visibility(node);
        symbol.exported = symbol.visibility.is_some();
        let index = self.push(symbol);
        if let Some(body) = body {
            let variants = named_children(body);
            for (i, variant) in variants.iter().enumerate() {
                if variant.kind() != "enum_variant" {
                    continue;
                }
                let Some(vname) = field_text(*variant, "name", self.src) else { continue };
                let (vdoc, _) = self.leading(&variants[..i]);
                let mut v = new_symbol("variant", vname, format!("{}{}::{}", scope.prefix, name, vname), *variant, None, self.src);
                v.doc = vdoc;
                v.parent = Some(index);
                if let Some(value) = field(*variant, "value") {
                    v.value = Some(text(value, self.src).to_owned());
                }
                if let Some(vbody) = field(*variant, "body") {
                    v.type_names = type_names(vbody, self.src);
                    v.type_text = Some(text(vbody, self.src).to_owned());
                }
                self.push(v);
            }
        }
    }

    fn trait_item(&mut self, node: Node, preceding: &[Node], scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let (doc, attrs) = self.leading(preceding);
        let body = field(node, "body");
        let mut symbol = new_symbol("trait", &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        symbol.doc = doc;
        symbol.decorators = attrs;
        symbol.parent = scope.parent;
        symbol.visibility = self.visibility(node);
        symbol.exported = symbol.visibility.is_some();
        if let Some(bounds) = field(node, "bounds") {
            symbol.extends = type_names(bounds, self.src);
        }
        let index = self.push(symbol);
        if let Some(body) = body {
            let inner = Scope {
                parent: Some(index),
                prefix: format!("{}{}::", scope.prefix, name),
                owner: Some(name.clone()),
                impl_trait: None,
                in_trait: true,
            };
            self.items(body, &inner);
        }
    }

    fn impl_item(&mut self, node: Node, scope: &Scope) {
        let Some(type_node) = field(node, "type") else { return };
        // `impl<T> Foo<T>` -> `Foo`; `impl Foo for Bar` -> owner Bar, trait Foo.
        let owner = base_type_name(type_node, self.src);
        let trait_name = field(node, "trait").map(|t| base_type_name(t, self.src));
        if let Some(trait_name) = &trait_name {
            self.facts.impls.push(ImplFact { type_name: owner.clone(), trait_name: trait_name.clone(), line: line(node) });
        }
        if let Some(body) = field(node, "body") {
            let inner = Scope {
                parent: scope.parent,
                prefix: format!("{}{}::", scope.prefix, owner),
                owner: Some(owner),
                impl_trait: trait_name,
                in_trait: false,
            };
            self.items(body, &inner);
        }
    }

    fn simple(&mut self, node: Node, preceding: &[Node], scope: &Scope, kind: &str) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let (doc, attrs) = self.leading(preceding);
        let mut symbol = new_symbol(kind, &name, format!("{}{}", scope.prefix, name), node, None, self.src);
        if kind == "macro" {
            symbol.exported = attrs.iter().any(|a| a.contains("macro_export"));
        } else {
            symbol.visibility = self.visibility(node);
            symbol.exported = symbol.visibility.is_some();
        }
        symbol.doc = doc;
        symbol.decorators = attrs;
        symbol.parent = scope.parent;
        if let Some(t) = field(node, "type") {
            symbol.type_text = Some(text(t, self.src).to_owned());
            symbol.type_names = type_names(t, self.src);
        }
        let value = field(node, "value");
        if let Some(v) = value {
            symbol.value = Some(text(v, self.src).to_owned());
        }
        let index = self.push(symbol);
        if let Some(v) = value {
            self.body(v, Some(index));
        }
    }

    fn module(&mut self, node: Node, preceding: &[Node], scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(node, "body");
        let (doc, attrs) = self.leading(preceding);
        if body.is_none() {
            self.facts.mod_decls.push(name.clone());
        }
        let mut symbol = new_symbol("module", &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        symbol.doc = doc;
        symbol.decorators = attrs;
        symbol.parent = scope.parent;
        symbol.visibility = self.visibility(node);
        symbol.exported = symbol.visibility.is_some();
        let index = self.push(symbol);
        if let Some(body) = body {
            let inner = Scope { parent: Some(index), prefix: format!("{}{}::", scope.prefix, name), ..Scope::default() };
            self.items(body, &inner);
        }
    }

    fn use_decl(&mut self, node: Node, scope: &Scope) {
        let Some(arg) = field(node, "argument") else { return };
        let line = line(node);
        let mut out = Vec::new();
        use_tree(arg, self.src, "", &mut out);
        // Inside inline modules (`mod tests { use super::*; }`) leading `super`s first climb the
        // inline modules, which live in this same file.
        let depth = scope.prefix.matches("::").count();
        for (path, local) in out {
            let mut segments: Vec<&str> = path.split("::").collect();
            let supers = segments.iter().take_while(|s| **s == "super").count();
            let path = if depth > 0 && supers > 0 {
                let climbed = supers.min(depth);
                segments.drain(..climbed);
                if supers <= depth {
                    segments.insert(0, "self");
                }
                segments.join("::")
            } else {
                path
            };
            let (module, name) = match path.rsplit_once("::") {
                Some((m, n)) => (m.to_owned(), n.to_owned()),
                None => (String::new(), path.clone()),
            };
            self.facts.imports.push(ImportFact { module, name, local, line });
        }
    }

    /// References inside a function body or initializer.
    fn body(&mut self, node: Node, from: Option<usize>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "call_expression" => self.call(current, from),
                "macro_invocation" => self.macro_call(current, from),
                "struct_expression" => {
                    if let Some(name) = field(current, "name") {
                        let (n, q) = split_path(text(name, self.src));
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &n, q, Vec::new(), current);
                    }
                }
                "let_declaration" => {
                    if let (Some(from), Some(pattern)) = (from, field(current, "pattern"))
                        && pattern.kind() == "identifier"
                    {
                        let declared = field(current, "type").and_then(|t| type_names(t, self.src).into_iter().find(|n| n.chars().next().is_some_and(|c| c.is_ascii_uppercase())));
                        let inferred = field(current, "value").and_then(|v| match v.kind() {
                            "struct_expression" => field(v, "name").map(|n| split_path(text(n, self.src)).0),
                            // `Type::new(..)` / `Type::from(..)`: the path's type segment.
                            "call_expression" => field(v, "function").and_then(|f| {
                                let path = text(f, self.src);
                                let segments: Vec<&str> = path.split('<').next().unwrap_or(path).split("::").collect();
                                if segments.len() >= 2 { Some(segments[segments.len() - 2].to_owned()) } else if f.kind() == "identifier" { Some(path.to_owned()) } else { None }
                            }),
                            _ => None,
                        });
                        if let Some(type_name) = declared.or(inferred) {
                            self.facts.locals.push(LocalFact { from, name: text(pattern, self.src).to_owned(), type_name });
                        }
                    }
                }
                "assignment_expression" | "compound_assignment_expr" => {
                    if let Some(left) = field(current, "left") {
                        self.write_target(left, from, current);
                    }
                }
                "field_expression" => {
                    // Reads of `self.field` (not the callee of a method call, handled in `call`).
                    let is_callee = current.parent().is_some_and(|p| p.kind() == "call_expression" && field(p, "function").is_some_and(|f| f.id() == current.id()));
                    let is_write = current.parent().is_some_and(|p| {
                        matches!(p.kind(), "assignment_expression" | "compound_assignment_expr") && field(p, "left").is_some_and(|l| l.id() == current.id())
                    });
                    if !is_callee
                        && !is_write
                        && let (Some(value), Some(f)) = (field(current, "value"), field(current, "field"))
                        && value.kind() == "self"
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, text(f, self.src), Some("self".into()), Vec::new(), current);
                    }
                }
                "identifier" => {
                    // Structural rule: Rust consts and statics are SCREAMING_SNAKE_CASE (rustc lint).
                    let t = text(current, self.src);
                    let upper = t.len() > 1 && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && t.chars().any(|c| c.is_ascii_uppercase());
                    let is_write = current.parent().is_some_and(|p| {
                        matches!(p.kind(), "assignment_expression" | "compound_assignment_expr") && field(p, "left").is_some_and(|l| l.id() == current.id())
                    });
                    if upper && !is_write {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, t, None, Vec::new(), current);
                    }
                }
                "scoped_identifier" => {
                    // `Self::CONST` / `module::CONST` reads.
                    let is_callee = current.parent().is_some_and(|p| p.kind() == "call_expression" && field(p, "function").is_some_and(|f| f.id() == current.id()));
                    if !is_callee && let Some(name) = field(current, "name") {
                        let t = text(name, self.src);
                        let upper = t.len() > 1 && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && t.chars().any(|c| c.is_ascii_uppercase());
                        if upper {
                            let (n, q) = split_path(text(current, self.src));
                            push_ref(&mut self.facts.refs, from, RefKind::Read, &n, q, Vec::new(), current);
                        }
                    }
                    continue;
                }
                // Nested items inside a body are definitions of their own; skip.
                "function_item" | "impl_item" | "struct_item" | "enum_item" | "trait_item" | "mod_item" => continue,
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn write_target(&mut self, left: Node, from: Option<usize>, at: Node) {
        match left.kind() {
            "field_expression" => {
                if let (Some(value), Some(f)) = (field(left, "value"), field(left, "field"))
                    && value.kind() == "self"
                {
                    push_ref(&mut self.facts.refs, from, RefKind::Write, text(f, self.src), Some("self".into()), Vec::new(), at);
                }
            }
            "identifier" => push_ref(&mut self.facts.refs, from, RefKind::Write, text(left, self.src), None, Vec::new(), at),
            "scoped_identifier" => {
                let (n, q) = split_path(text(left, self.src));
                push_ref(&mut self.facts.refs, from, RefKind::Write, &n, q, Vec::new(), at);
            }
            _ => {}
        }
    }

    fn call(&mut self, node: Node, from: Option<usize>) {
        let Some(function) = field(node, "function") else { return };
        let args: Vec<String> = field(node, "arguments").map(|a| named_children(a).into_iter().filter(|c| !c.kind().ends_with("comment")).map(|c| text(c, self.src).to_owned()).collect()).unwrap_or_default();
        let function = if function.kind() == "generic_function" { field(function, "function").unwrap_or(function) } else { function };
        let (name, qualifier) = match function.kind() {
            "identifier" => (text(function, self.src).to_owned(), None),
            "scoped_identifier" => split_path(text(function, self.src)),
            "field_expression" => {
                let name = field_text(function, "field", self.src).unwrap_or("").to_owned();
                let value = field(function, "value");
                let qualifier = value.map(|v| match v.kind() {
                    "self" | "identifier" | "scoped_identifier" | "field_expression" => text(v, self.src).to_owned(),
                    _ => String::new(),
                });
                (name, qualifier)
            }
            _ => return,
        };
        // The synthetic wrapper used to re-parse macro arguments is not a call.
        if name == "__cc_args" {
            return;
        }
        // Environment reads: `std::env::var("X")`, `env::var_os("X")` (API names).
        if matches!(name.as_str(), "var" | "var_os") && qualifier.as_deref().is_some_and(|q| q == "env" || q.ends_with("::env") || q == "dotenvy" || q == "dotenv") {
            if let Some(first) = field(node, "arguments").and_then(|a| named_children(a).into_iter().next())
                && let Some(key) = string_content(first, self.src)
            {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), node);
            }
            return;
        }
        // Errors returned: `Err(Type::Variant(..))` / `Err(Type { .. })` -> THROWS Type.
        if name == "Err" && qualifier.is_none()
            && let Some(first) = field(node, "arguments").and_then(|a| named_children(a).into_iter().next()) {
                let thrown = match first.kind() {
                    "call_expression" => field(first, "function").map(|f| text(f, self.src).to_owned()),
                    "struct_expression" => field(first, "name").map(|f| text(f, self.src).to_owned()),
                    "scoped_identifier" => Some(text(first, self.src).to_owned()),
                    _ => None,
                };
                if let Some(path) = thrown {
                    // `MyError::Bad` names the type `MyError`; `MyError` alone names itself.
                    let segments: Vec<&str> = path.split("::").collect();
                    let type_name = if segments.len() >= 2 { segments[segments.len() - 2] } else { segments[0] };
                    if type_name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        push_ref(&mut self.facts.refs, from, RefKind::Throw, type_name, None, Vec::new(), node);
                    }
                }
            }
        // Programs run: `Command::new("x")`, `.arg("x")`, `.args([..])` (std::process API names).
        if ((name == "new" && qualifier.as_deref().is_some_and(|q| q == "Command" || q.ends_with("::Command"))) || name == "arg" || name == "args")
            && let Some(arguments) = field(node, "arguments") {
                let mut stack = named_children(arguments);
                while let Some(arg) = stack.pop() {
                    if let Some(lit) = string_content(arg, self.src) {
                        if looks_like_script_path(&lit) {
                            self.facts.runs.push(RunFact { from, target: lit, line: line(node) });
                        }
                    } else if matches!(arg.kind(), "array_expression" | "reference_expression") {
                        stack.extend(named_children(arg));
                    }
                }
            }
        // axum / actix routers: `.route("/p", get(h).post(h2))`, `web::resource("/p").route(web::get().to(h))`.
        if name == "route" {
            self.router_route(node);
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
    }

    fn router_route(&mut self, node: Node) {
        let Some(arguments) = field(node, "arguments") else { return };
        let args = named_children(arguments);
        let (path, method_expr) = match args.as_slice() {
            [p, m] => match string_content(*p, self.src) {
                Some(path) => (path, *m),
                None => return,
            },
            [m] => {
                // Path from the receiver chain: `web::resource("/p").route(..)`.
                let Some(receiver) = field(node, "function").and_then(|f| field(f, "value")) else { return };
                let Some(path) = find_resource_path(receiver, self.src) else { return };
                (path, *m)
            }
            _ => return,
        };
        if !path.starts_with('/') {
            return;
        }
        let mut pairs = Vec::new();
        collect_method_handlers(method_expr, self.src, &mut pairs);
        for (method, handler) in pairs {
            let (h, q) = split_path(&handler);
            self.route_symbol(&method, &path, &h, q, node);
        }
    }

    fn macro_call(&mut self, node: Node, from: Option<usize>) {
        let Some(mac) = field(node, "macro") else { return };
        let macro_name = text(mac, self.src);
        let (name, _) = split_path(macro_name);
        let tree = named_children(node).into_iter().find(|c| c.kind() == "token_tree");
        let inner = tree.map(|t| {
            let raw = text(t, self.src);
            raw.get(1..raw.len().saturating_sub(1)).unwrap_or("").to_owned()
        });
        if matches!(name.as_str(), "env" | "option_env") {
            if let Some(inner) = &inner
                && let Some(first) = split_top_level_args(inner).first()
            {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &strip_quotes(first), Some("env".into()), Vec::new(), node);
            }
            return;
        }
        let args = inner.as_deref().map(split_top_level_args).unwrap_or_default();
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, Some("!".into()), args, node);
        // Macro arguments are token trees, not expressions; re-parse them as expressions so calls
        // inside `assert_eq!(f(x), 3)` or `vec![g()]` are seen. Structural.
        if let (Some(tree), Some(inner)) = (tree, inner) {
            let wrapped = format!("fn __cc_macro() {{ __cc_args({inner}); }}");
            if let Some(parsed) = parse(&language(), &wrapped) {
                let base_row = tree.start_position().row as u32;
                let before = self.facts.refs.len();
                let before_runs = self.facts.runs.len();
                let mut sub = Walker { src: &wrapped, facts: self.facts };
                if let Some(body) = find_first(parsed.root_node(), "block") {
                    sub.body(body, from);
                }
                for r in self.facts.refs.iter_mut().skip(before) {
                    r.line += base_row;
                }
                for r in self.facts.runs.iter_mut().skip(before_runs) {
                    r.line += base_row;
                }
            }
        }
    }
}

/// `a::b::name` -> (`name`, Some(`a::b`)).
pub fn split_path(path: &str) -> (String, Option<String>) {
    let path = path.split('<').next().unwrap_or(path);
    match path.rsplit_once("::") {
        Some((q, n)) => (n.to_owned(), Some(q.to_owned())),
        None => (path.to_owned(), None),
    }
}

fn base_type_name(node: Node, src: &str) -> String {
    match node.kind() {
        "generic_type" => field(node, "type").map(|t| base_type_name(t, src)).unwrap_or_default(),
        "scoped_type_identifier" => field_text(node, "name", src).unwrap_or("").to_owned(),
        "reference_type" => field(node, "type").map(|t| base_type_name(t, src)).unwrap_or_default(),
        _ => text(node, src).to_owned(),
    }
}

fn attr_rename(attrs: &[String]) -> Option<String> {
    for attr in attrs {
        if let Some(pos) = attr.find("rename") {
            let rest = &attr[pos..];
            if let Some((_, value)) = rest.split_once('=') {
                let value = value.trim().trim_end_matches([']', ')']).trim();
                return Some(strip_quotes(value));
            }
        }
    }
    None
}

fn find_resource_path(node: Node, src: &str) -> Option<String> {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "call_expression" {
            let function = field(n, "function")?;
            let fname = match function.kind() {
                "scoped_identifier" | "identifier" => split_path(text(function, src)).0,
                "field_expression" => field_text(function, "field", src).unwrap_or("").to_owned(),
                _ => String::new(),
            };
            if matches!(fname.as_str(), "resource" | "scope") {
                let first = field(n, "arguments").and_then(|a| named_children(a).into_iter().next())?;
                return string_content(first, src);
            }
            current = if function.kind() == "field_expression" { field(function, "value") } else { None };
        } else {
            return None;
        }
    }
    None
}

/// Collect (METHOD, handler path) pairs from a router method expression.
fn collect_method_handlers(node: Node, src: &str, out: &mut Vec<(String, String)>) {
    if node.kind() == "call_expression"
        && let Some(function) = field(node, "function")
    {
        let args: Vec<Node> = field(node, "arguments").map(named_children).unwrap_or_default();
        let (fname, receiver) = match function.kind() {
            "identifier" | "scoped_identifier" => (split_path(text(function, src)).0, None),
            "field_expression" => (field_text(function, "field", src).unwrap_or("").to_owned(), field(function, "value")),
            _ => (String::new(), None),
        };
        if HTTP_VERBS.contains(&fname.as_str()) || fname == "any" {
            if let Some(handler) = args.first()
                && matches!(handler.kind(), "identifier" | "scoped_identifier")
            {
                out.push((fname.to_uppercase(), text(*handler, src).to_owned()));
            }
        } else if fname == "to" {
            // actix: `web::get().to(handler)`.
            if let (Some(handler), Some(receiver)) = (args.first(), receiver)
                && matches!(handler.kind(), "identifier" | "scoped_identifier")
                && receiver.kind() == "call_expression"
                && let Some(rf) = field(receiver, "function")
            {
                let verb = split_path(text(rf, src)).0;
                if HTTP_VERBS.contains(&verb.as_str()) {
                    out.push((verb.to_uppercase(), text(*handler, src).to_owned()));
                }
            }
        }
        if let Some(receiver) = receiver {
            collect_method_handlers(receiver, src, out);
        }
    }
}

/// Flatten a `use` tree into (full path, local name) pairs.
fn use_tree(node: Node, src: &str, prefix: &str, out: &mut Vec<(String, String)>) {
    let join = |prefix: &str, tail: &str| if prefix.is_empty() { tail.to_owned() } else { format!("{prefix}::{tail}") };
    match node.kind() {
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
            let full = join(prefix, text(node, src));
            let local = full.rsplit("::").next().unwrap_or(&full).to_owned();
            out.push((full, local));
        }
        "use_as_clause" => {
            let path = field(node, "path").map(|p| join(prefix, text(p, src))).unwrap_or_default();
            let alias = field_text(node, "alias", src).unwrap_or("").to_owned();
            out.push((path, alias));
        }
        "scoped_use_list" => {
            let base = field(node, "path").map(|p| join(prefix, text(p, src))).unwrap_or_else(|| prefix.to_owned());
            if let Some(list) = field(node, "list") {
                for child in named_children(list) {
                    use_tree(child, src, &base, out);
                }
            }
        }
        "use_list" => {
            for child in named_children(node) {
                use_tree(child, src, prefix, out);
            }
        }
        "use_wildcard" => {
            let inner = named_children(node).into_iter().next().map(|p| join(prefix, text(p, src))).unwrap_or_else(|| prefix.to_owned());
            out.push((format!("{inner}::*"), "*".into()));
        }
        _ => {}
    }
}
