//! Go extraction (tree-sitter-go): package clause, imports, functions and methods (attached to their
//! receiver type), types with fields and interface methods, consts and vars, `go test` tests,
//! benchmarks, examples and `t.Run` subtests, calls, instantiations, reads and writes of package-level
//! names and receiver members, receiver-typed locals, environment reads, programs run and error values
//! returned.
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/go.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, LocalFact, ParamFact, RefKind, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashSet;

fn language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    /// `go test` collects test functions from `_test.go` files only.
    test_file: bool,
    /// Methods and the receiver type they belong to (attached to a same-file type afterwards).
    methods: Vec<(usize, String)>,
}

/// What a function body is walked with.
#[derive(Clone, Default)]
struct Body {
    /// The method receiver's name: its selectors are the receiver type's members (`self`).
    receiver: Option<String>,
    /// Names declared inside the function (parameters, `:=`, `var`, range variables): reads of
    /// these are locals, not package-level names.
    declared: HashSet<String>,
    /// Inside a test: the `*testing.T` parameter, whose `Run("name", func(..))` is a subtest.
    test_param: Option<String>,
    /// The function's last result is `error`: a returned composite literal there is an error value.
    returns_error: bool,
    /// The function's qualified name (subtests are `Parent/name`).
    qname: String,
}

pub fn extract(path: &str, src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let test_file = path.ends_with("_test.go");
    let mut walker = Walker { src, facts, test_file, methods: Vec::new() };
    let nodes = named_children(root);
    for node in &nodes {
        match node.kind() {
            "package_clause" => {
                walker.facts.package = named_children(*node).into_iter().find(|c| c.kind() == "package_identifier").map(|c| text(c, src).to_owned());
            }
            "import_declaration" => walker.imports(*node),
            _ => {}
        }
    }
    for node in &nodes {
        match node.kind() {
            "function_declaration" => walker.function(*node, None),
            "method_declaration" => walker.function(*node, field(*node, "receiver")),
            "type_declaration" => walker.types(*node),
            "const_declaration" | "var_declaration" => walker.values(*node),
            _ => {}
        }
    }
    walker.attach_methods();
    attach_notes(root, src, walker.facts);
}

/// Go export rule: an identifier is exported when its first character is an upper-case letter.
fn is_exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// go/ast directive comments are not documentation: `//line `, `//extern `, `//export ` and
/// `//[a-z0-9]+:[a-z0-9]` (`//go:generate`, `//nolint:x`).
fn is_directive(comment: &str) -> bool {
    let Some(rest) = comment.strip_prefix("//") else { return false };
    if rest.starts_with("line ") || rest.starts_with("extern ") || rest.starts_with("export ") {
        return true;
    }
    let Some((head, tail)) = rest.split_once(':') else { return false };
    !head.is_empty() && head.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) && tail.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The package name an import path binds by default: its last element, skipping a module
/// major-version element (`/v2`) and a gopkg.in version suffix (`yaml.v3`), per the Go module
/// path conventions.
fn default_import_name(path: &str) -> String {
    let elements: Vec<&str> = path.split('/').filter(|e| !e.is_empty()).collect();
    let is_major = |e: &str| e.len() > 1 && e.starts_with('v') && e[1..].chars().all(|c| c.is_ascii_digit());
    let last = match elements.as_slice() {
        [.., prev, last] if is_major(last) => prev,
        [.., last] => last,
        [] => "",
    };
    match last.rsplit_once(".v") {
        Some((base, version)) if !version.is_empty() && version.chars().all(|c| c.is_ascii_digit()) => base.to_owned(),
        _ => last.to_owned(),
    }
}

/// The named type of a receiver or embedded type: `*T`, `T[K]`, `pkg.T` -> `T`.
fn base_type_name(node: Node, src: &str) -> String {
    match node.kind() {
        "pointer_type" | "parenthesized_type" => named_children(node).into_iter().next().map(|t| base_type_name(t, src)).unwrap_or_default(),
        "generic_type" => field(node, "type").map(|t| base_type_name(t, src)).unwrap_or_default(),
        "qualified_type" => field_text(node, "name", src).unwrap_or("").to_owned(),
        "type_identifier" => text(node, src).to_owned(),
        _ => String::new(),
    }
}

impl<'a> Walker<'a> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    /// Doc comment (go/doc): the comment lines directly above a declaration, directives removed.
    fn doc(&self, node: Node) -> Option<String> {
        let mut docs: Vec<&str> = Vec::new();
        let mut current = node.prev_sibling();
        let mut expected_row = node.start_position().row;
        while let Some(prev) = current {
            if prev.kind() != "comment" || prev.end_position().row + 1 < expected_row {
                break;
            }
            if prev.prev_sibling().is_some_and(|before| last_row(before) == prev.start_position().row) {
                break;
            }
            let t = text(prev, self.src);
            if !is_directive(t) {
                docs.push(t);
            }
            expected_row = prev.start_position().row;
            current = prev.prev_sibling();
        }
        docs.reverse();
        clean_doc(&docs)
    }

    fn imports(&mut self, node: Node) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "import_spec" => {
                    let Some(module) = field(current, "path").and_then(|p| string_content(p, self.src)) else { continue };
                    let (name, local) = match field(current, "name") {
                        Some(n) if n.kind() == "dot" => ("*".to_owned(), "*".to_owned()),
                        Some(n) => ("*".to_owned(), text(n, self.src).to_owned()),
                        None => ("*".to_owned(), default_import_name(&module)),
                    };
                    self.facts.imports.push(ImportFact { module, name, local, line: line(current) });
                }
                _ => {
                    let mut kids = named_children(current);
                    kids.reverse();
                    stack.extend(kids);
                }
            }
        }
    }

    /// Local name of an import path (`os`, `exec`, an alias), when the file imports it.
    fn import_local(&self, path: &str) -> Option<&str> {
        self.facts.imports.iter().find(|i| i.module == path).map(|i| i.local.as_str())
    }

    /// The single parameter's name when a parameter list is exactly `(x *testing.<wanted>)`.
    fn testing_param(&self, params: Option<Node>, wanted: &str) -> Option<String> {
        let decls: Vec<Node> = params.map(named_children).unwrap_or_default().into_iter().filter(|p| p.kind() != "comment").collect();
        let [decl] = decls.as_slice() else { return None };
        if decl.kind() != "parameter_declaration" {
            return None;
        }
        let mut cursor = decl.walk();
        let names: Vec<Node> = decl.children_by_field_name("name", &mut cursor).collect();
        if names.len() > 1 {
            return None;
        }
        let pointer = field(*decl, "type").filter(|t| t.kind() == "pointer_type")?;
        let target = named_children(pointer).into_iter().next()?;
        let matches = match target.kind() {
            "qualified_type" => {
                field_text(target, "name", self.src) == Some(wanted) && self.import_local("testing").is_some_and(|local| field_text(target, "package", self.src) == Some(local))
            }
            // `import . "testing"`.
            "type_identifier" => text(target, self.src) == wanted && self.import_local("testing") == Some("*"),
            _ => false,
        };
        matches.then(|| names.first().map(|n| text(*n, self.src).to_owned()).unwrap_or_default())
    }

    /// `go test` test functions: `TestXxx(*testing.T)`, `BenchmarkXxx(*testing.B)`,
    /// `FuzzXxx(*testing.F)` and `ExampleXxx()`, where Xxx does not start with a lower-case letter.
    fn test_kind(&self, node: Node, name: &str) -> Option<Option<String>> {
        if !self.test_file || node.kind() != "function_declaration" || field(node, "type_parameters").is_some() {
            return None;
        }
        let named = |prefix: &str| name.strip_prefix(prefix).is_some_and(|rest| !rest.chars().next().is_some_and(char::is_lowercase));
        let params = field(node, "parameters");
        if named("Test") {
            return self.testing_param(params, "T").map(Some);
        }
        if named("Benchmark") {
            return self.testing_param(params, "B").map(Some);
        }
        if named("Fuzz") {
            return self.testing_param(params, "F").map(Some);
        }
        let no_params = params.is_none_or(|p| named_children(p).is_empty());
        if named("Example") && no_params && field(node, "result").is_none() {
            return Some(None);
        }
        None
    }

    fn function(&mut self, node: Node, receiver: Option<Node>) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(node, "body");
        let receiver_decl = receiver.and_then(|r| named_children(r).into_iter().find(|c| c.kind() == "parameter_declaration"));
        let owner = receiver_decl.and_then(|d| field(d, "type")).map(|t| base_type_name(t, self.src)).filter(|o| !o.is_empty());
        let test = self.test_kind(node, &name);
        let kind = if test.is_some() {
            "test"
        } else if owner.is_some() {
            "method"
        } else {
            "function"
        };
        let qname = match &owner {
            Some(owner) => format!("{owner}.{name}"),
            None => name.clone(),
        };
        let mut symbol = new_symbol(kind, &name, qname.clone(), node, body, self.src);
        symbol.doc = self.doc(node);
        symbol.exported = is_exported(&name);
        if test.is_some() {
            symbol.framework = Some("go".into());
        }
        if let Some(params) = field(node, "parameters") {
            symbol.params = self.params(params);
        }
        let mut returns_error = false;
        if let Some(result) = field(node, "result") {
            symbol.return_type = Some(text(result, self.src).to_owned());
            symbol.return_type_names = type_names(result, self.src);
            let last = if result.kind() == "parameter_list" { named_children(result).into_iter().rfind(|c| c.kind() != "comment").and_then(|p| field(p, "type")) } else { Some(result) };
            returns_error = last.is_some_and(|t| text(t, self.src) == "error");
        }
        let index = self.push(symbol);
        if let Some(owner) = owner {
            self.methods.push((index, owner));
        }
        if let Some(body) = body {
            let mut ctx = Body {
                receiver: receiver_decl.and_then(|d| field_text(d, "name", self.src)).filter(|n| *n != "_").map(str::to_owned),
                declared: HashSet::new(),
                test_param: test.flatten().filter(|p| !p.is_empty() && p != "_"),
                returns_error,
                qname,
            };
            if let Some(params) = field(node, "parameters") {
                collect_declared(params, self.src, &mut ctx.declared);
            }
            collect_declared(body, self.src, &mut ctx.declared);
            self.body(body, Some(index), &ctx);
        }
    }

    fn params(&self, params: Node) -> Vec<ParamFact> {
        let mut out = Vec::new();
        let mut position = 0;
        for p in named_children(params) {
            if !matches!(p.kind(), "parameter_declaration" | "variadic_parameter_declaration") {
                continue;
            }
            let variadic = p.kind() == "variadic_parameter_declaration";
            let type_node = field(p, "type");
            let mut cursor = p.walk();
            let mut names: Vec<String> = p.children_by_field_name("name", &mut cursor).map(|n| text(n, self.src).to_owned()).collect();
            if names.is_empty() {
                // Unnamed parameters (`func(int, string)`).
                names.push("_".into());
            }
            for name in names {
                let mut fact = param(&name, position);
                fact.variadic = variadic;
                if let Some(t) = type_node {
                    fact.type_text = Some(if variadic { format!("...{}", text(t, self.src)) } else { text(t, self.src).to_owned() });
                    fact.type_names = type_names(t, self.src);
                }
                out.push(fact);
                position += 1;
            }
        }
        out
    }

    fn types(&mut self, decl: Node) {
        let specs: Vec<Node> = named_children(decl).into_iter().filter(|c| matches!(c.kind(), "type_spec" | "type_alias")).collect();
        let single = specs.len() == 1;
        for spec in specs {
            let Some(name) = field_text(spec, "name", self.src) else { continue };
            let name = name.to_owned();
            let type_node = field(spec, "type");
            let kind = match type_node.map(|t| t.kind()) {
                Some("struct_type") => "struct",
                Some("interface_type") => "interface",
                _ => "type",
            };
            let span = if single { decl } else { spec };
            let body = type_node.and_then(|t| named_children(t).into_iter().find(|c| matches!(c.kind(), "field_declaration_list")).or_else(|| (t.kind() == "interface_type").then_some(t)));
            let mut symbol = new_symbol(kind, &name, name.clone(), span, body, self.src);
            if kind == "interface"
                && let Some(t) = type_node
            {
                // The interface type is its own body: the signature ends at its keyword.
                symbol.signature = format!("{} interface", squash(self.src.get(span.start_byte()..t.start_byte()).unwrap_or("")));
                symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
            }
            symbol.doc = self.doc(spec).or_else(|| if single { self.doc(decl) } else { None });
            symbol.exported = is_exported(&name);
            if kind == "type"
                && let Some(t) = type_node
            {
                symbol.type_text = Some(text(t, self.src).to_owned());
                symbol.type_names = type_names(t, self.src);
            }
            let index = self.push(symbol);
            let Some(t) = type_node else { continue };
            match kind {
                "struct" => {
                    if let Some(list) = body {
                        self.struct_fields(list, index, &name);
                    }
                }
                "interface" => self.interface_elems(t, index, &name),
                _ => {}
            }
        }
    }

    fn struct_fields(&mut self, list: Node, parent: usize, owner: &str) {
        for decl in named_children(list) {
            if decl.kind() != "field_declaration" {
                continue;
            }
            let type_node = field(decl, "type");
            let mut cursor = decl.walk();
            let names: Vec<String> = decl.children_by_field_name("name", &mut cursor).map(|n| text(n, self.src).to_owned()).collect();
            let doc = comments_before(decl, self.src, |_| true);
            if names.is_empty() {
                // An embedded field: its methods and fields are promoted to the struct.
                let Some(t) = type_node else { continue };
                let embedded = base_type_name(t, self.src);
                if embedded.is_empty() {
                    continue;
                }
                self.facts.symbols[parent].extends.push(embedded.clone());
                let mut f = new_symbol("field", &embedded, format!("{owner}.{embedded}"), decl, None, self.src);
                f.doc = doc;
                f.parent = Some(parent);
                f.exported = is_exported(&embedded);
                f.type_text = Some(text(t, self.src).to_owned());
                f.type_names = type_names(t, self.src);
                self.push(f);
                continue;
            }
            for fname in names {
                let mut f = new_symbol("field", &fname, format!("{owner}.{fname}"), decl, None, self.src);
                f.doc = doc.clone();
                f.parent = Some(parent);
                f.exported = is_exported(&fname);
                if let Some(t) = type_node {
                    f.type_text = Some(text(t, self.src).to_owned());
                    f.type_names = type_names(t, self.src);
                }
                self.push(f);
            }
        }
    }

    fn interface_elems(&mut self, iface: Node, parent: usize, owner: &str) {
        for elem in named_children(iface) {
            match elem.kind() {
                "method_elem" => {
                    let Some(name) = field_text(elem, "name", self.src) else { continue };
                    let name = name.to_owned();
                    let mut m = new_symbol("method", &name, format!("{owner}.{name}"), elem, None, self.src);
                    m.doc = comments_before(elem, self.src, |_| true);
                    m.parent = Some(parent);
                    m.exported = is_exported(&name);
                    if let Some(params) = field(elem, "parameters") {
                        m.params = self.params(params);
                    }
                    if let Some(result) = field(elem, "result") {
                        m.return_type = Some(text(result, self.src).to_owned());
                        m.return_type_names = type_names(result, self.src);
                    }
                    self.push(m);
                }
                // An embedded interface (a single named type, not a constraint union).
                "type_elem" => {
                    let types = named_children(elem);
                    if let [t] = types.as_slice() {
                        let embedded = base_type_name(*t, self.src);
                        if !embedded.is_empty() {
                            self.facts.symbols[parent].extends.push(embedded);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Package-level `const` / `var` declarations.
    fn values(&mut self, decl: Node) {
        let mut specs: Vec<Node> = Vec::new();
        for child in named_children(decl) {
            match child.kind() {
                "const_spec" | "var_spec" => specs.push(child),
                "var_spec_list" => specs.extend(named_children(child).into_iter().filter(|c| c.kind() == "var_spec")),
                _ => {}
            }
        }
        let single = specs.len() == 1;
        let kind = if decl.kind() == "const_declaration" { "const" } else { "variable" };
        let module_ctx = Body::default();
        for spec in specs {
            let mut cursor = spec.walk();
            let names: Vec<Node> = spec.children_by_field_name("name", &mut cursor).collect();
            let values: Vec<Node> = field(spec, "value").map(named_children).unwrap_or_default().into_iter().filter(|c| c.kind() != "comment").collect();
            let type_node = field(spec, "type");
            let doc = self.doc(spec).or_else(|| if single { self.doc(decl) } else { None });
            let span = if single { decl } else { spec };
            for (i, name_node) in names.iter().enumerate() {
                let name = text(*name_node, self.src).to_owned();
                if name == "_" {
                    if let Some(v) = values.get(i) {
                        self.body(*v, None, &module_ctx);
                    }
                    continue;
                }
                let mut symbol = new_symbol(kind, &name, name.clone(), span, None, self.src);
                symbol.doc = doc.clone();
                symbol.exported = is_exported(&name);
                if let Some(t) = type_node {
                    symbol.type_text = Some(text(t, self.src).to_owned());
                    symbol.type_names = type_names(t, self.src);
                }
                let value = values.get(i).copied();
                if let Some(v) = value {
                    symbol.value = Some(text(v, self.src).to_owned());
                    if type_node.is_none()
                        && let Some(t) = literal_type(v, self.src)
                    {
                        symbol.type_names = vec![t];
                    }
                }
                let index = self.push(symbol);
                if let Some(v) = value {
                    self.body(v, Some(index), &module_ctx);
                }
            }
        }
    }

    /// Methods declared in the same file as their receiver type become its children.
    fn attach_methods(&mut self) {
        for (index, owner) in std::mem::take(&mut self.methods) {
            let parent = self.facts.symbols.iter().position(|s| s.parent.is_none() && s.name == owner && matches!(s.kind.as_str(), "struct" | "interface" | "type"));
            if let Some(parent) = parent {
                self.facts.symbols[index].parent = Some(parent);
            }
        }
    }

    /// The qualifier of a selector operand: the receiver becomes `self`, simple chains keep their
    /// text, anything else is `""` (unknown receiver).
    fn qualifier(&self, operand: Node, ctx: &Body) -> String {
        match operand.kind() {
            "identifier" => {
                let t = text(operand, self.src);
                if ctx.receiver.as_deref() == Some(t) { "self".to_owned() } else { t.to_owned() }
            }
            "selector_expression" => {
                let inner = field(operand, "operand").map(|o| self.qualifier(o, ctx)).unwrap_or_default();
                let f = field_text(operand, "field", self.src).unwrap_or("");
                if inner.is_empty() { String::new() } else { format!("{inner}.{f}") }
            }
            _ => String::new(),
        }
    }

    fn is_receiver(&self, node: Option<Node>, ctx: &Body) -> bool {
        node.is_some_and(|n| n.kind() == "identifier" && ctx.receiver.as_deref() == Some(text(n, self.src)))
    }

    /// References inside a function body or initializer.
    fn body(&mut self, node: Node, from: Option<usize>, ctx: &Body) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "call_expression" => {
                    if self.call(current, from, ctx) {
                        continue;
                    }
                }
                "composite_literal" => {
                    if let Some(t) = field(current, "type") {
                        let (name, qualifier) = type_ref(t, self.src);
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &name, qualifier, Vec::new(), current);
                    }
                }
                "short_var_declaration" | "var_spec" => {
                    if let Some(from) = from {
                        self.locals(current, from);
                    }
                }
                "return_statement" if ctx.returns_error => {
                    // Error results: `return nil, &MyError{..}` / `return MyError{..}` -> THROWS MyError.
                    if let Some(last) = named_children(current).into_iter().next().and_then(|list| named_children(list).into_iter().rfind(|c| c.kind() != "comment"))
                        && let Some(t) = literal_type(last, self.src)
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::Throw, &t, None, Vec::new(), current);
                    }
                }
                "assignment_statement" | "inc_statement" | "dec_statement" => {
                    let lefts: Vec<Node> = match field(current, "left") {
                        Some(list) => named_children(list),
                        None => named_children(current).into_iter().take(1).collect(),
                    };
                    for left in lefts {
                        match left.kind() {
                            "selector_expression" if self.is_receiver(field(left, "operand"), ctx) => {
                                let f = field_text(left, "field", self.src).unwrap_or("");
                                push_ref(&mut self.facts.refs, from, RefKind::Write, f, Some("self".into()), Vec::new(), current);
                            }
                            "identifier" => {
                                let t = text(left, self.src);
                                if t != "_" && !ctx.declared.contains(t) {
                                    push_ref(&mut self.facts.refs, from, RefKind::Write, t, None, Vec::new(), current);
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(right) = field(current, "right") {
                        stack.push(right);
                    }
                    // The left side's own sub-expressions (index operands, selector operands) are reads.
                    for left in field(current, "left").map(named_children).unwrap_or_default() {
                        if !matches!(left.kind(), "identifier" | "selector_expression") {
                            stack.push(left);
                        }
                    }
                    continue;
                }
                "selector_expression" => {
                    let operand = field(current, "operand");
                    let f = field_text(current, "field", self.src).unwrap_or("");
                    if is_callee(current) {
                        // The call is recorded by `call`; only the operand chain is read.
                        if let Some(o) = operand {
                            stack.push(o);
                        }
                        continue;
                    }
                    if self.is_receiver(operand, ctx) {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, f, Some("self".into()), Vec::new(), current);
                        continue;
                    }
                    // `pkg.Name`: a package-level value of an imported package.
                    if let Some(o) = operand
                        && o.kind() == "identifier"
                        && !ctx.declared.contains(text(o, self.src))
                        && self.facts.imports.iter().any(|i| i.local == text(o, self.src))
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, f, Some(text(o, self.src).to_owned()), Vec::new(), current);
                        continue;
                    }
                }
                "identifier" => {
                    let t = text(current, self.src);
                    let is_key = current.parent().is_some_and(|p| p.kind() == "literal_element" && p.parent().is_some_and(|k| k.kind() == "keyed_element" && field(k, "key").is_some_and(|key| key.id() == p.id())));
                    // An imported package's name is only ever a qualifier.
                    let is_package = self.facts.imports.iter().any(|i| i.local == t);
                    if t != "_" && !is_key && !is_callee(current) && !ctx.declared.contains(t) && !is_package {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, t, None, Vec::new(), current);
                    }
                }
                // Declarations' names are not reads; their values and types are walked.
                "parameter_list" | "type_identifier" | "qualified_type" | "field_identifier" | "package_identifier" => continue,
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    /// Locals whose type their declaration names: `x := T{..}`, `x := &T{..}`, `x := new(T)`,
    /// `var x T`.
    fn locals(&mut self, decl: Node, from: usize) {
        if decl.kind() == "var_spec" {
            let mut cursor = decl.walk();
            let names: Vec<Node> = decl.children_by_field_name("name", &mut cursor).collect();
            let declared = field(decl, "type").map(|t| base_type_name(t, self.src)).filter(|t| !t.is_empty());
            let values: Vec<Node> = field(decl, "value").map(named_children).unwrap_or_default();
            for (i, n) in names.iter().enumerate() {
                let inferred = values.get(i).and_then(|v| literal_type(*v, self.src));
                if let Some(type_name) = declared.clone().or(inferred) {
                    self.facts.locals.push(LocalFact { from, name: text(*n, self.src).to_owned(), type_name });
                }
            }
            return;
        }
        let lefts: Vec<Node> = field(decl, "left").map(named_children).unwrap_or_default();
        let rights: Vec<Node> = field(decl, "right").map(named_children).unwrap_or_default();
        if lefts.len() != rights.len() {
            return;
        }
        for (left, right) in lefts.iter().zip(rights.iter()) {
            if left.kind() == "identifier"
                && let Some(type_name) = literal_type(*right, self.src)
            {
                self.facts.locals.push(LocalFact { from, name: text(*left, self.src).to_owned(), type_name });
            }
        }
    }

    /// Records a call. Returns true when the node's children were handled (subtests).
    fn call(&mut self, node: Node, from: Option<usize>, ctx: &Body) -> bool {
        let Some(function) = field(node, "function") else { return false };
        let arg_nodes: Vec<Node> = field(node, "arguments").map(|a| named_children(a).into_iter().filter(|c| c.kind() != "comment").collect()).unwrap_or_default();
        let args: Vec<String> = arg_nodes.iter().map(|a| text(*a, self.src).to_owned()).collect();
        let (name, qualifier) = match function.kind() {
            "identifier" => (text(function, self.src).to_owned(), None),
            "selector_expression" => {
                let name = field_text(function, "field", self.src).unwrap_or("").to_owned();
                let qualifier = field(function, "operand").map(|o| self.qualifier(o, ctx));
                (name, qualifier)
            }
            // Generic instantiation `F[int](x)`.
            "index_expression" => match field(function, "operand") {
                Some(o) if o.kind() == "identifier" => (text(o, self.src).to_owned(), None),
                _ => return false,
            },
            _ => return false,
        };
        if name.is_empty() {
            return false;
        }
        let q = qualifier.as_deref().unwrap_or("");
        // Subtests: `t.Run("name", func(t *testing.T) {..})` on the test's `*testing.T`.
        if name == "Run"
            && ctx.test_param.as_deref().is_some_and(|p| p == q)
            && let [title, func] = arg_nodes.as_slice()
            && func.kind() == "func_literal"
            && let Some(title) = string_content(*title, self.src)
            && let Some(body) = field(*func, "body")
        {
            let qname = format!("{}/{}", ctx.qname, title);
            let mut symbol = new_symbol("test", &title, qname.clone(), node, Some(body), self.src);
            symbol.parent = from;
            symbol.framework = Some("go".into());
            symbol.signature = format!("{}.Run({})", q, text(arg_nodes[0], self.src));
            let index = self.push(symbol);
            let mut inner = ctx.clone();
            inner.test_param = self.testing_param(field(*func, "parameters"), "T").filter(|p| !p.is_empty() && p != "_");
            inner.qname = qname;
            inner.returns_error = false;
            self.body(body, Some(index), &inner);
            return true;
        }
        let is_import = |w: &str| self.import_local(w).is_some_and(|local| local == q && !ctx.declared.contains(q));
        // Environment reads: `os.Getenv("X")`, `os.LookupEnv("X")`, `syscall.Getenv("X")` (API names).
        if matches!(name.as_str(), "Getenv" | "LookupEnv") && (is_import("os") || is_import("syscall")) {
            if let Some(key) = arg_nodes.first().and_then(|a| string_content(*a, self.src)) {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), node);
            }
            return false;
        }
        // Programs run: `exec.Command("x", ..)`, `exec.CommandContext(ctx, "x", ..)`,
        // `syscall.Exec("x", ..)`, `os.StartProcess("x", ..)` (API names).
        if (matches!(name.as_str(), "Command" | "CommandContext") && is_import("os/exec")) || (name == "Exec" && is_import("syscall")) || (name == "StartProcess" && is_import("os")) {
            let mut stack = arg_nodes.clone();
            while let Some(arg) = stack.pop() {
                if let Some(lit) = string_content(arg, self.src) {
                    for word in lit.split_whitespace() {
                        if looks_like_script_path(word) {
                            self.facts.runs.push(RunFact { from, target: word.to_owned(), line: line(node) });
                        }
                    }
                } else if matches!(arg.kind(), "composite_literal" | "literal_value" | "literal_element") {
                    stack.extend(named_children(arg));
                }
            }
        }
        if qualifier.is_none() {
            match name.as_str() {
                // Builtin allocation: `new(T)` instantiates T.
                "new" if !ctx.declared.contains("new") => {
                    if let Some(t) = arg_nodes.first() {
                        let (n, q) = type_ref(*t, self.src);
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &n, q, Vec::new(), node);
                    }
                    return false;
                }
                // `panic(MyError{..})`: a thrown error value.
                "panic" if !ctx.declared.contains("panic") => {
                    if let Some(t) = arg_nodes.first().and_then(|a| literal_type(*a, self.src)) {
                        push_ref(&mut self.facts.refs, from, RefKind::Throw, &t, None, Vec::new(), node);
                    }
                }
                _ => {}
            }
        }
        // A call of a local function value is not a reference to a declaration.
        if qualifier.is_none() && ctx.declared.contains(&name) {
            return false;
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
        false
    }
}

/// Whether `node` is the callee of a call expression.
fn is_callee(node: Node) -> bool {
    node.parent().is_some_and(|p| p.kind() == "call_expression" && field(p, "function").is_some_and(|f| f.id() == node.id()))
}

/// Names declared inside a function: parameters, `:=` / `var` / `const` names, range and
/// type-switch variables, function-literal parameters.
fn collect_declared(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "parameter_declaration" | "variadic_parameter_declaration" | "var_spec" | "const_spec" => {
                let mut cursor = current.walk();
                for n in current.children_by_field_name("name", &mut cursor) {
                    out.insert(text(n, src).to_owned());
                }
            }
            "short_var_declaration" => {
                for n in field(current, "left").map(named_children).unwrap_or_default() {
                    if n.kind() == "identifier" {
                        out.insert(text(n, src).to_owned());
                    }
                }
            }
            "range_clause" | "receive_statement" => {
                if children(current).iter().any(|c| c.kind() == ":=") {
                    for n in field(current, "left").map(named_children).unwrap_or_default() {
                        if n.kind() == "identifier" {
                            out.insert(text(n, src).to_owned());
                        }
                    }
                }
            }
            "type_switch_statement" => {
                for n in field(current, "alias").map(named_children).unwrap_or_default() {
                    out.insert(text(n, src).to_owned());
                }
            }
            _ => {}
        }
        stack.extend(named_children(current));
    }
}

/// The type a composite-literal expression constructs: `T{..}`, `&T{..}`, `pkg.T{..}`, `new(T)`.
fn literal_type(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "composite_literal" => field(node, "type").map(|t| base_type_name(t, src)).filter(|t| !t.is_empty()),
        "unary_expression" if field(node, "operator").is_some_and(|o| text(o, src) == "&") => field(node, "operand").and_then(|o| literal_type(o, src)),
        "call_expression" if field(node, "function").is_some_and(|f| f.kind() == "identifier" && text(f, src) == "new") => {
            field(node, "arguments").and_then(|a| named_children(a).into_iter().next()).map(|t| base_type_name(t, src)).filter(|t| !t.is_empty())
        }
        _ => None,
    }
}

/// A type reference as (name, package qualifier): `pkg.T` -> (`T`, `pkg`), `*T` / `T[K]` -> `T`.
fn type_ref(node: Node, src: &str) -> (String, Option<String>) {
    match node.kind() {
        "qualified_type" => (field_text(node, "name", src).unwrap_or("").to_owned(), field_text(node, "package", src).map(str::to_owned)),
        "generic_type" => field(node, "type").map(|t| type_ref(t, src)).unwrap_or_default(),
        "pointer_type" => named_children(node).into_iter().next().map(|t| type_ref(t, src)).unwrap_or_default(),
        "type_identifier" | "identifier" => (text(node, src).to_owned(), None),
        // `pkg.T` in expression position (`new(pkg.T)`).
        "selector_expression" => (field_text(node, "field", src).unwrap_or("").to_owned(), field(node, "operand").map(|o| text(o, src).to_owned())),
        _ => (String::new(), None),
    }
}
