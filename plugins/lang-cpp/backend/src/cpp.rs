//! C and C++ extraction (tree-sitter-cpp, which parses C declarations too).
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/cpp.rs),
//! producing `lang@1` facts. C (`.c`) and C++ files share this extractor and the C++ grammar.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, LocalFact, ParamFact, RefKind, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashSet;

fn language() -> Language {
    tree_sitter_cpp::LANGUAGE.into()
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    /// Namespaces this file opens (a qualified definition `ns::f` outside them is not a member).
    namespaces: HashSet<String>,
    /// Out-of-line member definitions (`void T::f() {}`) and the qualified name of their class.
    out_of_line: Vec<(usize, String)>,
    /// Framework of `TEST_CASE` tests: `doctest` when the file includes it, else `catch2`.
    case_framework: &'static str,
}

#[derive(Clone, Default)]
struct Scope {
    parent: Option<usize>,
    /// Qualified-name prefix (`ns::Class::`).
    prefix: String,
    /// Inside a class body: the class name.
    owner: Option<String>,
    /// Inside a class body: the access of the members that follow (`public` / `private` / `protected`).
    access: Option<String>,
    /// Outside an anonymous namespace (names have external linkage).
    linkage: bool,
}

/// GoogleTest / Boost.Test macros whose expansion is a test function: `TEST(Suite, Name) {..}`
/// (API names).
const FUNCTION_TESTS: &[(&str, &str)] = &[
    ("TEST", "gtest"),
    ("TEST_F", "gtest"),
    ("TEST_P", "gtest"),
    ("TYPED_TEST", "gtest"),
    ("TYPED_TEST_P", "gtest"),
    ("BOOST_AUTO_TEST_CASE", "boost"),
    ("BOOST_FIXTURE_TEST_CASE", "boost"),
    ("BOOST_DATA_TEST_CASE", "boost"),
];

/// Catch2 / doctest macros followed by the test body: `TEST_CASE("name", "[tag]") {..}` (API names).
const CASE_TESTS: &[&str] = &["TEST_CASE", "TEST_CASE_METHOD", "SCENARIO", "TEMPLATE_TEST_CASE", "TEMPLATE_PRODUCT_TEST_CASE"];

pub fn extract(src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let mut namespaces = HashSet::new();
    collect_namespaces(root, src, &mut namespaces);
    let mut walker = Walker { src, facts, namespaces, out_of_line: Vec::new(), case_framework: "catch2" };
    walker.items(root, &Scope { linkage: true, ..Scope::default() });
    if walker.facts.imports.iter().any(|i| i.module.contains("doctest")) {
        for symbol in walker.facts.symbols.iter_mut().filter(|s| s.framework.as_deref() == Some("catch2")) {
            symbol.framework = Some("doctest".into());
        }
    }
    walker.attach_out_of_line();
}

fn collect_namespaces(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "namespace_definition"
            && let Some(name) = field(current, "name")
        {
            for segment in text(name, src).split("::") {
                out.insert(segment.trim().to_owned());
            }
        }
        if !matches!(current.kind(), "compound_statement" | "field_declaration_list") {
            stack.extend(named_children(current));
        }
    }
}

/// A name without template arguments: `Account<T>` -> `Account`.
fn strip_template(name: &str) -> &str {
    name.split('<').next().unwrap_or(name).trim()
}

/// Scope segments and final name node of a (possibly nested) qualified identifier.
fn flatten_qualified<'t>(node: Node<'t>, src: &str) -> (Vec<String>, Node<'t>) {
    let mut scopes = Vec::new();
    let mut current = node;
    while current.kind() == "qualified_identifier" {
        if let Some(scope) = field(current, "scope") {
            scopes.push(strip_template(text(scope, src)).to_owned());
        }
        match field(current, "name") {
            Some(name) => current = name,
            None => break,
        }
    }
    (scopes, current)
}

/// The name a declarator declares, looking through pointers, references, arrays and
/// initializers.
fn declarator_name<'t>(node: Node<'t>) -> Option<Node<'t>> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier" | "destructor_name" | "operator_name" | "template_function" => Some(node),
        "init_declarator" | "pointer_declarator" | "reference_declarator" | "array_declarator" | "parenthesized_declarator" | "attributed_declarator" | "function_declarator" => {
            field(node, "declarator").or_else(|| named_children(node).into_iter().find(|c| c.kind().ends_with("declarator") || c.kind().ends_with("identifier"))).and_then(declarator_name)
        }
        _ => None,
    }
}

/// The function declarator of a function declaration (through return-type pointers and
/// references); none for a function pointer variable (`int (*fp)(int)`).
fn function_declarator(node: Node) -> Option<Node> {
    match node.kind() {
        "function_declarator" => {
            let inner = field(node, "declarator")?;
            if matches!(inner.kind(), "parenthesized_declarator") { None } else { Some(node) }
        }
        "pointer_declarator" | "reference_declarator" | "attributed_declarator" => field(node, "declarator").or_else(|| named_children(node).into_iter().last()).and_then(function_declarator),
        _ => None,
    }
}

/// The class a type expression names: `const ns::Store<T>&` -> `Store`; smart pointers
/// (`std::unique_ptr<T>`, `shared_ptr`, `weak_ptr`: std API names) name their element type.
fn base_type_name(node: Node, src: &str) -> String {
    match node.kind() {
        "type_identifier" | "identifier" => text(node, src).to_owned(),
        "qualified_identifier" => {
            let (_, name) = flatten_qualified(node, src);
            base_type_name(name, src)
        }
        "template_type" | "template_function" => {
            let name = field_text(node, "name", src).unwrap_or("");
            if matches!(name, "unique_ptr" | "shared_ptr" | "weak_ptr")
                && let Some(first) = field(node, "arguments").and_then(|a| named_children(a).into_iter().next())
            {
                return base_type_name(first, src);
            }
            name.to_owned()
        }
        "type_descriptor" => field(node, "type").map(|t| base_type_name(t, src)).unwrap_or_default(),
        _ => String::new(),
    }
}

/// A type expression as (name, scope qualifier with its trailing `::`).
fn type_ref(node: Node, src: &str) -> (String, Option<String>) {
    match node.kind() {
        "qualified_identifier" => {
            let (scopes, name) = flatten_qualified(node, src);
            let qualifier = (!scopes.is_empty()).then(|| format!("{}::", scopes.join("::")));
            (base_type_name(name, src), qualifier)
        }
        "type_descriptor" => field(node, "type").map(|t| type_ref(t, src)).unwrap_or_default(),
        _ => (base_type_name(node, src), None),
    }
}

impl<'a> Walker<'a> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    /// Doc comment: the comment block directly above (Doxygen `///`, `/** */`, or plain `//`).
    fn doc(&self, node: Node) -> Option<String> {
        comments_before(node, self.src, |_| true)
    }

    /// Declarations of a translation unit, namespace, linkage block or class body.
    fn items(&mut self, list: Node, scope: &Scope) {
        let nodes = named_children(list);
        let mut scope = scope.clone();
        let mut skip_next = false;
        for (i, node) in nodes.iter().enumerate() {
            if std::mem::take(&mut skip_next) {
                continue;
            }
            match node.kind() {
                "access_specifier" => scope.access = Some(text(*node, self.src).trim().to_owned()),
                "preproc_include" => self.include(*node),
                "preproc_def" | "preproc_function_def" => self.define(*node, &scope),
                // Conditional compilation, `extern "C" {..}` and unparsable stretches (a header's
                // `extern "C" {` split across `#ifdef __cplusplus`) still contain declarations.
                "preproc_ifdef" | "preproc_if" | "preproc_else" | "preproc_elif" | "preproc_elifdef" | "ERROR" => self.items(*node, &scope),
                "linkage_specification" => {
                    if let Some(body) = field(*node, "body") {
                        if body.kind() == "declaration_list" { self.items(body, &scope) } else { self.declaration(body, body, &scope) }
                    }
                }
                "namespace_definition" => self.namespace(*node, &scope),
                "template_declaration" => self.template(*node, &scope),
                "expression_statement" => {
                    // Catch2 / doctest: `TEST_CASE("name") { .. }` parses as a call followed by a block.
                    if let Some(block) = nodes.get(i + 1).filter(|n| n.kind() == "compound_statement")
                        && self.case_test(*node, *block, &scope)
                    {
                        skip_next = true;
                    } else {
                        self.body(*node, scope.parent, &HashSet::new());
                    }
                }
                "compound_statement" => self.body(*node, scope.parent, &HashSet::new()),
                _ => self.declaration(*node, *node, &scope),
            }
        }
    }

    /// A declaration; `outer` is its span (the enclosing `template <..>` when there is one).
    fn declaration(&mut self, node: Node, outer: Node, scope: &Scope) {
        match node.kind() {
            "function_definition" => self.function(node, outer, scope),
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
                self.type_specifier(node, outer, scope);
            }
            "declaration" | "field_declaration" => self.variables_or_prototypes(node, outer, scope),
            "type_definition" => self.typedef(node, outer, scope),
            "alias_declaration" => {
                let Some(name) = field_text(node, "name", self.src) else { return };
                let mut symbol = new_symbol("type", name, format!("{}{}", scope.prefix, name), outer, None, self.src);
                symbol.doc = self.doc(outer);
                symbol.parent = scope.parent;
                symbol.exported = self.exported(scope, false);
                symbol.visibility = scope.access.clone();
                if let Some(t) = field(node, "type") {
                    symbol.type_text = Some(text(t, self.src).to_owned());
                    symbol.type_names = type_names(t, self.src);
                }
                self.push(symbol);
            }
            "template_declaration" => self.template(node, scope),
            _ => {}
        }
    }

    fn template(&mut self, node: Node, scope: &Scope) {
        for child in named_children(node) {
            if child.kind() != "template_parameter_list" {
                self.declaration(child, node, scope);
            }
        }
    }

    /// Whether a declaration is visible outside its file / class: public members, names with
    /// external linkage (not `static`, not in an anonymous namespace).
    fn exported(&self, scope: &Scope, is_static: bool) -> bool {
        match &scope.owner {
            Some(_) => scope.access.as_deref() == Some("public"),
            None => scope.linkage && !is_static,
        }
    }

    fn include(&mut self, node: Node) {
        let Some(path) = field(node, "path") else { return };
        // Quoted includes are kept as their path, system includes with their angle brackets.
        let module = match path.kind() {
            "string_literal" => string_content(path, self.src).unwrap_or_default(),
            _ => text(path, self.src).trim().to_owned(),
        };
        if !module.is_empty() {
            self.facts.imports.push(ImportFact { module, name: "*".into(), local: "*".into(), line: line(node) });
        }
    }

    /// `#define NAME value` is a constant; `#define NAME(args) ..` is a macro. A flag without a
    /// value (include guards, feature switches) is not a definition anything reads.
    fn define(&mut self, node: Node, scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let value = field(node, "value").map(|v| text(v, self.src).trim().to_owned()).filter(|v| !v.is_empty());
        let function_like = node.kind() == "preproc_function_def";
        if !function_like && value.is_none() {
            return;
        }
        let kind = if function_like { "macro" } else { "const" };
        let mut symbol = new_symbol(kind, name, format!("{}{}", scope.prefix, name), node, None, self.src);
        symbol.signature = squash(text(node, self.src).lines().next().unwrap_or("")).trim_end_matches('\\').trim().to_owned();
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
        symbol.doc = self.doc(node);
        symbol.parent = scope.parent;
        symbol.exported = true;
        symbol.value = value;
        if let Some(params) = field(node, "parameters") {
            let mut position = 0;
            for p in children(params) {
                let pname = match p.kind() {
                    "identifier" => text(p, self.src).to_owned(),
                    "..." => "...".to_owned(),
                    _ => continue,
                };
                let mut fact = param(&pname, position);
                fact.variadic = pname == "...";
                symbol.params.push(fact);
                position += 1;
            }
        }
        self.push(symbol);
    }

    fn namespace(&mut self, node: Node, scope: &Scope) {
        let Some(body) = field(node, "body") else { return };
        let Some(name) = field(node, "name").map(|n| squash(text(n, self.src))) else {
            // An anonymous namespace: its names have internal linkage, in the enclosing scope.
            let inner = Scope { linkage: false, ..scope.clone() };
            self.items(body, &inner);
            return;
        };
        let short = name.rsplit("::").next().unwrap_or(&name).trim().to_owned();
        let mut symbol = new_symbol("module", &short, format!("{}{}", scope.prefix, name), node, Some(body), self.src);
        symbol.doc = self.doc(node);
        symbol.parent = scope.parent;
        symbol.exported = scope.linkage;
        let index = self.push(symbol);
        let inner = Scope { parent: Some(index), prefix: format!("{}{}::", scope.prefix, name), owner: None, access: None, linkage: scope.linkage };
        self.items(body, &inner);
    }

    /// A class / struct / union / enum with a body. Returns its index.
    fn type_specifier(&mut self, node: Node, outer: Node, scope: &Scope) -> Option<usize> {
        let body = field(node, "body")?;
        let name_node = field(node, "name")?;
        let name = base_type_name(name_node, self.src);
        if name.is_empty() {
            return None;
        }
        self.named_type(node, outer, scope, &name, body)
    }

    fn named_type(&mut self, node: Node, outer: Node, scope: &Scope, name: &str, body: Node) -> Option<usize> {
        let kind = match node.kind() {
            "class_specifier" => "class",
            "enum_specifier" => "enum",
            _ => "struct",
        };
        let mut symbol = new_symbol(kind, name, format!("{}{}", scope.prefix, name), outer, Some(body), self.src);
        symbol.signature = head_signature(outer, outer.start_byte(), Some(body), self.src);
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
        symbol.doc = self.doc(outer);
        symbol.parent = scope.parent;
        symbol.visibility = scope.access.clone();
        symbol.exported = self.exported(scope, false);
        symbol.decorators = self.attributes(node);
        for clause in named_children(node).into_iter().filter(|c| c.kind() == "base_class_clause") {
            for base in named_children(clause) {
                let n = base_type_name(base, self.src);
                if !n.is_empty() {
                    symbol.extends.push(n);
                }
            }
        }
        if kind == "enum"
            && let Some(t) = field(node, "base")
        {
            symbol.type_text = Some(text(t, self.src).to_owned());
        }
        let index = self.push(symbol);
        let prefix = format!("{}{}::", scope.prefix, name);
        if kind == "enum" {
            // `enum class` enumerators are scoped; plain (C) enumerators are constants of the
            // enclosing scope.
            let scoped = children(node).iter().any(|c| matches!(c.kind(), "class" | "struct"));
            for enumerator in named_children(body) {
                if enumerator.kind() != "enumerator" {
                    continue;
                }
                let Some(vname) = field_text(enumerator, "name", self.src) else { continue };
                let (vkind, qname) = if scoped { ("variant", format!("{prefix}{vname}")) } else { ("const", format!("{}{}", scope.prefix, vname)) };
                let mut v = new_symbol(vkind, vname, qname, enumerator, None, self.src);
                v.doc = self.doc(enumerator);
                v.parent = Some(index);
                v.exported = self.exported(scope, false);
                v.value = field(enumerator, "value").map(|value| text(value, self.src).to_owned());
                let vindex = self.push(v);
                if let Some(value) = field(enumerator, "value") {
                    self.body(value, Some(vindex), &HashSet::new());
                }
            }
            return Some(index);
        }
        let default_access = if kind == "class" { "private" } else { "public" };
        let inner = Scope { parent: Some(index), prefix, owner: Some(name.to_owned()), access: Some(default_access.to_owned()), linkage: scope.linkage };
        self.items(body, &inner);
        Some(index)
    }

    /// `[[nodiscard]]`-style attributes and `virtual` / `override` / `final` specifiers.
    fn attributes(&self, node: Node) -> Vec<String> {
        let mut out = Vec::new();
        for child in children(node) {
            match child.kind() {
                "attribute_specifier" | "attribute_declaration" | "virtual" => out.push(text(child, self.src).to_owned()),
                "virtual_specifier" => out.push(text(child, self.src).to_owned()),
                "function_declarator" => {
                    for inner in children(child) {
                        if inner.kind() == "virtual_specifier" {
                            out.push(text(inner, self.src).to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn storage(&self, node: Node, keyword: &str) -> bool {
        children(node).iter().any(|c| c.kind() == "storage_class_specifier" && text(*c, self.src) == keyword)
    }

    fn function(&mut self, node: Node, outer: Node, scope: &Scope) {
        let Some(declarator) = field(node, "declarator").and_then(function_declarator) else {
            // `struct S {..} s;`-style or malformed: nothing callable.
            return;
        };
        self.function_symbol(node, outer, declarator, field(node, "body"), scope);
    }

    fn function_symbol(&mut self, node: Node, outer: Node, declarator: Node, body: Option<Node>, scope: &Scope) {
        let Some(name_node) = field(declarator, "declarator") else { return };
        let (scopes, last) = flatten_qualified(name_node, self.src);
        let name = match last.kind() {
            "template_function" => field_text(last, "name", self.src).unwrap_or("").to_owned(),
            _ => squash(text(last, self.src)),
        };
        if name.is_empty() {
            return;
        }
        // GoogleTest / Boost.Test: `TEST(Suite, Name) {..}` has no return type.
        if field(node, "type").is_none()
            && scopes.is_empty()
            && let Some((_, framework)) = FUNCTION_TESTS.iter().find(|(m, _)| *m == name)
            && let Some(body) = body
        {
            let args: Vec<String> = field(declarator, "parameters").map(named_children).unwrap_or_default().iter().map(|p| squash(text(*p, self.src))).collect();
            let title = if *framework == "gtest" { args.iter().take(2).cloned().collect::<Vec<_>>().join(".") } else { args.first().cloned().unwrap_or_default() };
            let mut symbol = new_symbol("test", &title, format!("test:{}{}", scope.prefix, title), outer, Some(body), self.src);
            symbol.signature = squash(&text(outer, self.src)[..(declarator.end_byte() - outer.start_byte())]);
            symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
            symbol.doc = self.doc(outer);
            symbol.parent = scope.parent;
            symbol.framework = Some((*framework).into());
            let index = self.push(symbol);
            self.walk_function(declarator, body, index);
            return;
        }
        // An out-of-line member definition `T::f`, unless `T` is a namespace this file opens and the
        // definition is outside it (`void ns::f() {}`); inside `namespace X {..}`, `X::f` names a
        // member of the class X declared there.
        let inside = |s: &str| scope.prefix.split("::").any(|p| p == s);
        let owner_scope = scopes.last().filter(|s| !self.namespaces.contains(s.as_str()) || inside(s)).cloned();
        let member = scope.owner.is_some() || owner_scope.is_some();
        let kind = if member { "method" } else { "function" };
        let path = if scopes.is_empty() { name.clone() } else { format!("{}::{}", scopes.join("::"), name) };
        let qname = format!("{}{}", scope.prefix, path);
        let mut symbol = new_symbol(kind, &name, qname.clone(), outer, body, self.src);
        let sig_end = declarator.end_byte();
        symbol.signature = squash(self.src.get(outer.start_byte()..sig_end).unwrap_or(""));
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
        symbol.doc = self.doc(outer);
        symbol.parent = scope.parent;
        symbol.visibility = if scope.owner.is_some() { scope.access.clone() } else { None };
        let is_static = self.storage(node, "static");
        symbol.is_static = scope.owner.is_some() && is_static;
        // An out-of-line definition's visibility is its in-class declaration's.
        symbol.exported = if scope.owner.is_none() && owner_scope.is_some() { false } else { self.exported(scope, is_static) };
        symbol.decorators = self.attributes(node);
        if let Some(params) = field(declarator, "parameters") {
            symbol.params = self.params(params);
        }
        if let Some(t) = field(node, "type") {
            // The type and the declarator's pointer / reference marks before the name.
            let ret = squash(self.src.get(t.start_byte()..declarator.start_byte()).unwrap_or(""));
            symbol.return_type = Some(if ret.is_empty() { text(t, self.src).to_owned() } else { ret });
            symbol.return_type_names = type_names(t, self.src);
        } else if let Some(trailing) = named_children(declarator).into_iter().find(|c| c.kind() == "trailing_return_type") {
            symbol.return_type = Some(text(trailing, self.src).trim_start_matches("->").trim().to_owned());
            symbol.return_type_names = type_names(trailing, self.src);
        }
        let index = self.push(symbol);
        if scope.owner.is_none()
            && owner_scope.is_some()
        {
            let class_qname = format!("{}{}", scope.prefix, scopes.join("::"));
            self.out_of_line.push((index, class_qname));
        }
        if let Some(body) = body {
            // Member initializers (`: balance_(initial)`) write the class's fields.
            for init in named_children(node).into_iter().filter(|c| c.kind() == "field_initializer_list") {
                for fi in named_children(init) {
                    if let Some(f) = named_children(fi).into_iter().find(|c| c.kind() == "field_identifier") {
                        push_ref(&mut self.facts.refs, Some(index), RefKind::Write, text(f, self.src), Some("this".into()), Vec::new(), fi);
                    }
                    for arg in named_children(fi).into_iter().filter(|c| matches!(c.kind(), "argument_list" | "initializer_list")) {
                        let mut declared = HashSet::new();
                        if let Some(params) = field(declarator, "parameters") {
                            collect_declared(params, self.src, &mut declared);
                        }
                        self.body(arg, Some(index), &declared);
                    }
                }
            }
            self.walk_function(declarator, body, index);
        }
    }

    fn walk_function(&mut self, declarator: Node, body: Node, index: usize) {
        let mut declared = HashSet::new();
        if let Some(params) = field(declarator, "parameters") {
            collect_declared(params, self.src, &mut declared);
        }
        collect_declared(body, self.src, &mut declared);
        self.body(body, Some(index), &declared);
    }

    fn params(&self, params: Node) -> Vec<ParamFact> {
        let mut out = Vec::new();
        let mut position = 0;
        for p in children(params) {
            let variadic = matches!(p.kind(), "variadic_parameter_declaration" | "...");
            if !matches!(p.kind(), "parameter_declaration" | "optional_parameter_declaration" | "variadic_parameter_declaration" | "...") {
                continue;
            }
            if p.kind() == "..." {
                let mut fact = param("...", position);
                fact.variadic = true;
                out.push(fact);
                position += 1;
                continue;
            }
            let declarator = field(p, "declarator");
            let name_node = declarator.and_then(declarator_name);
            // `void` alone declares no parameters (C).
            if name_node.is_none() && declarator.is_none() && text(p, self.src).trim() == "void" {
                continue;
            }
            let name = name_node.map(|n| text(n, self.src).to_owned()).unwrap_or_else(|| "_".into());
            let decl_end = declarator.map(|d| d.end_byte()).or_else(|| field(p, "type").map(|t| t.end_byte())).unwrap_or(p.end_byte());
            let type_text = match name_node {
                Some(n) => format!("{}{}", self.src.get(p.start_byte()..n.start_byte()).unwrap_or(""), self.src.get(n.end_byte()..decl_end).unwrap_or("")),
                None => self.src.get(p.start_byte()..decl_end).unwrap_or("").to_owned(),
            };
            let mut fact = param(&name, position);
            fact.variadic = variadic;
            fact.type_text = Some(squash(&type_text).trim_end_matches("...").trim().to_owned()).filter(|t| !t.is_empty());
            if let Some(t) = field(p, "type") {
                fact.type_names = type_names(t, self.src);
            }
            if let Some(default) = field(p, "default_value") {
                fact.default_value = Some(text(default, self.src).to_owned());
                fact.optional = true;
            }
            out.push(fact);
            position += 1;
        }
        out
    }

    /// A `declaration` / `field_declaration`: function prototypes, variables, constants, fields,
    /// and the types they define inline (`struct S {..} s;`).
    fn variables_or_prototypes(&mut self, node: Node, outer: Node, scope: &Scope) {
        let type_node = field(node, "type");
        if let Some(t) = type_node
            && matches!(t.kind(), "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier")
        {
            self.type_specifier(t, if outer.id() == node.id() { node } else { outer }, scope);
        }
        let mut cursor = node.walk();
        let declarators: Vec<Node> = node.children_by_field_name("declarator", &mut cursor).collect();
        let is_static = self.storage(node, "static");
        let constexpr = children(node).iter().any(|c| c.kind() == "type_qualifier" && text(*c, self.src) == "constexpr");
        let const_qualified = children(node).iter().any(|c| c.kind() == "type_qualifier" && text(*c, self.src) == "const");
        let doc = self.doc(outer);
        for declarator in declarators {
            if let Some(f) = function_declarator(declarator) {
                // A prototype: the declaration of a function or member function.
                self.function_symbol(node, outer, f, None, scope);
                continue;
            }
            let Some(name_node) = declarator_name(declarator) else { continue };
            if name_node.kind() == "qualified_identifier" {
                // `int T::count = 0;`: the definition of a static member declared in the class.
                continue;
            }
            let name = text(name_node, self.src).to_owned();
            // A constant: `constexpr`, or `const` on the object itself (not on a pointee).
            let pointer = matches!(declarator.kind(), "pointer_declarator") || field(declarator, "declarator").is_some_and(|d| d.kind() == "pointer_declarator");
            let const_pointer = {
                let mut d = Some(declarator);
                let mut found = false;
                while let Some(current) = d {
                    if current.kind() == "pointer_declarator" && named_children(current).iter().any(|c| c.kind() == "type_qualifier" && text(*c, self.src) == "const") {
                        found = true;
                    }
                    d = field(current, "declarator");
                }
                found
            };
            let constant = constexpr || (const_qualified && !pointer) || const_pointer;
            let kind = match &scope.owner {
                Some(_) if is_static && constant => "const",
                Some(_) => "field",
                None if constant => "const",
                None => "variable",
            };
            let mut symbol = new_symbol(kind, &name, format!("{}{}", scope.prefix, name), outer, None, self.src);
            symbol.doc = doc.clone();
            symbol.parent = scope.parent;
            symbol.visibility = if scope.owner.is_some() { scope.access.clone() } else { None };
            symbol.is_static = is_static;
            symbol.exported = self.exported(scope, is_static);
            if let Some(t) = type_node {
                // Qualifiers and the type, then this declarator's pointer / reference / array parts.
                let inner = if declarator.kind() == "init_declarator" { field(declarator, "declarator").unwrap_or(declarator) } else { declarator };
                let mut parts: Vec<&str> = children(node).into_iter().filter(|c| c.kind() == "type_qualifier" || c.id() == t.id()).map(|c| text(c, self.src)).collect();
                parts.push(self.src.get(inner.start_byte()..name_node.start_byte()).unwrap_or(""));
                parts.push(self.src.get(name_node.end_byte()..inner.end_byte()).unwrap_or(""));
                symbol.type_text = Some(squash(&parts.join(" ")));
                symbol.type_names = type_names(t, self.src);
                if let Some(index) = self.facts.symbols.iter().rposition(|s| s.parent == scope.parent && matches!(s.kind.as_str(), "struct" | "class" | "enum") && s.start_byte == t.start_byte() as u32) {
                    let tname = self.facts.symbols[index].name.clone();
                    if !symbol.type_names.contains(&tname) {
                        symbol.type_names.push(tname);
                    }
                }
            }
            let value = field(declarator, "value").or_else(|| field(node, "default_value"));
            if let Some(v) = value {
                symbol.value = Some(text(v, self.src).to_owned());
            }
            let index = self.push(symbol);
            if let Some(v) = value {
                self.body(v, Some(index), &HashSet::new());
            }
        }
    }

    fn typedef(&mut self, node: Node, outer: Node, scope: &Scope) {
        let type_node = field(node, "type");
        let mut cursor = node.walk();
        let declarators: Vec<Node> = node.children_by_field_name("declarator", &mut cursor).collect();
        let names: Vec<String> = declarators.iter().filter_map(|d| declarator_name(*d)).map(|n| text(n, self.src).to_owned()).collect();
        let mut defined: Option<String> = None;
        if let Some(t) = type_node
            && matches!(t.kind(), "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier")
            && let Some(body) = field(t, "body")
        {
            match field(t, "name") {
                Some(_) => defined = self.type_specifier(t, outer, scope).map(|i| self.facts.symbols[i].name.clone()),
                // `typedef struct {..} point_t;`: the typedef names the anonymous type.
                None => {
                    if let Some(first) = names.first() {
                        self.named_type(t, outer, scope, first, body);
                        if names.len() == 1 {
                            return;
                        }
                    }
                }
            }
        }
        let doc = self.doc(outer);
        for name in names {
            if defined.as_deref() == Some(name.as_str()) {
                continue;
            }
            let mut symbol = new_symbol("type", &name, format!("{}{}", scope.prefix, name), outer, None, self.src);
            symbol.doc = doc.clone();
            symbol.parent = scope.parent;
            symbol.exported = self.exported(scope, false);
            symbol.visibility = scope.access.clone();
            if let Some(t) = type_node {
                // An inline definition is named by its head (`struct point`), not its body.
                let type_end = field(t, "body").map(|b| b.start_byte()).unwrap_or(node.end_byte());
                let head = squash(self.src.get(t.start_byte()..type_end).unwrap_or(""));
                let rest = if type_end < node.end_byte() && field(t, "body").is_some() { self.src.get(t.end_byte()..node.end_byte()).unwrap_or("") } else { "" };
                symbol.type_text = Some(squash(&format!("{head} {rest}")).trim_end_matches(';').replacen(&name, "", 1).trim().to_owned());
                symbol.type_names = type_names(t, self.src);
                if let Some(d) = &defined
                    && !symbol.type_names.contains(d)
                {
                    symbol.type_names.push(d.clone());
                }
            }
            self.push(symbol);
        }
    }

    /// `TEST_CASE("name", "[tag]") {..}` (Catch2 / doctest). Returns true when it was one.
    fn case_test(&mut self, statement: Node, block: Node, scope: &Scope) -> bool {
        let Some(call) = named_children(statement).into_iter().next().filter(|c| c.kind() == "call_expression") else { return false };
        let Some(function) = field(call, "function").filter(|f| f.kind() == "identifier") else { return false };
        let macro_name = text(function, self.src);
        if !CASE_TESTS.contains(&macro_name) {
            return false;
        }
        let args: Vec<Node> = field(call, "arguments").map(named_children).unwrap_or_default();
        let Some(title) = args.iter().find_map(|a| string_content(*a, self.src)) else { return false };
        let mut symbol = new_symbol("test", &title, format!("test:{}{}", scope.prefix, title), statement, Some(block), self.src);
        symbol.code = self.src.get(statement.start_byte()..block.end_byte()).unwrap_or("").to_owned();
        symbol.end_line = end_line(block);
        symbol.end_byte = block.end_byte() as u32;
        symbol.signature = squash(text(call, self.src));
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
        symbol.doc = self.doc(statement);
        symbol.parent = scope.parent;
        symbol.framework = Some(self.case_framework.into());
        let index = self.push(symbol);
        let mut declared = HashSet::new();
        collect_declared(block, self.src, &mut declared);
        self.body(block, Some(index), &declared);
        true
    }

    /// Out-of-line member definitions in the same file as their class become its children.
    fn attach_out_of_line(&mut self) {
        for (index, class_qname) in std::mem::take(&mut self.out_of_line) {
            if let Some(parent) = self.facts.symbols.iter().position(|s| matches!(s.kind.as_str(), "class" | "struct") && s.qname == class_qname) {
                self.facts.symbols[index].parent = Some(parent);
            }
        }
    }

    /// The qualifier of a member access's object: `this`, a name, or a simple chain written with
    /// `.` (`this->repo` -> `this.repo`); anything else is `""` (unknown receiver).
    fn qualifier(&self, object: Node) -> String {
        match object.kind() {
            "this" | "identifier" => text(object, self.src).to_owned(),
            "field_expression" => {
                let inner = field(object, "argument").map(|o| self.qualifier(o)).unwrap_or_default();
                let f = field_text(object, "field", self.src).unwrap_or("");
                if inner.is_empty() { String::new() } else { format!("{inner}.{f}") }
            }
            "parenthesized_expression" | "pointer_expression" => named_children(object).into_iter().last().map(|o| self.qualifier(o)).unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// References inside a function body or initializer.
    fn body(&mut self, node: Node, from: Option<usize>, declared: &HashSet<String>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "call_expression" => {
                    self.call(current, from, declared);
                    // The callee's own sub-expressions (object chains) are still walked.
                    if let Some(function) = field(current, "function") {
                        match function.kind() {
                            "field_expression" => {
                                if let Some(o) = field(function, "argument") {
                                    stack.push(o);
                                }
                            }
                            "identifier" | "qualified_identifier" | "template_function" => {}
                            _ => stack.push(function),
                        }
                    }
                    if let Some(arguments) = field(current, "arguments") {
                        stack.push(arguments);
                    }
                    continue;
                }
                "new_expression" => {
                    if let Some(t) = field(current, "type") {
                        let (name, qualifier) = type_ref(t, self.src);
                        let args = field(current, "arguments").map(|a| self.args(a)).unwrap_or_default();
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &name, qualifier, args, current);
                    }
                    if let Some(arguments) = field(current, "arguments") {
                        stack.push(arguments);
                    }
                    continue;
                }
                "compound_literal_expression" => {
                    if let Some(t) = field(current, "type") {
                        let (name, qualifier) = type_ref(t, self.src);
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &name, qualifier, Vec::new(), current);
                    }
                }
                "throw_statement" | "throw_expression" => {
                    if let Some(expr) = named_children(current).into_iter().next() {
                        let thrown = match expr.kind() {
                            "call_expression" => field(expr, "function").filter(|f| matches!(f.kind(), "identifier" | "qualified_identifier" | "template_function")),
                            "compound_literal_expression" => field(expr, "type"),
                            _ => None,
                        };
                        if let Some(t) = thrown {
                            let (name, qualifier) = type_ref(t, self.src);
                            push_ref(&mut self.facts.refs, from, RefKind::Throw, &name, qualifier, Vec::new(), current);
                        }
                    }
                }
                "declaration" => {
                    if let Some(from) = from {
                        self.locals(current, from);
                    }
                    // The declared type is not a read; initializers are.
                    let mut cursor = current.walk();
                    let declarators: Vec<Node> = current.children_by_field_name("declarator", &mut cursor).collect();
                    stack.extend(declarators.into_iter().rev());
                    continue;
                }
                "assignment_expression" | "update_expression" => {
                    let left = field(current, "left").or_else(|| field(current, "argument"));
                    if let Some(left) = left {
                        self.write_target(left, from, declared, current);
                        match left.kind() {
                            "identifier" | "qualified_identifier" => {}
                            "field_expression" => {
                                if let Some(o) = field(left, "argument") {
                                    stack.push(o);
                                }
                            }
                            _ => stack.push(left),
                        }
                    }
                    if let Some(right) = field(current, "right") {
                        stack.push(right);
                    }
                    continue;
                }
                "field_expression" => {
                    if let (Some(object), Some(f)) = (field(current, "argument"), field_text(current, "field", self.src)) {
                        let q = self.qualifier(object);
                        if !q.is_empty() {
                            push_ref(&mut self.facts.refs, from, RefKind::Read, f, Some(q), Vec::new(), current);
                        }
                        stack.push(object);
                    }
                    continue;
                }
                "qualified_identifier" => {
                    // `Color::Red`, `ns::kLimit`, `T::count`.
                    let (name, qualifier) = type_ref(current, self.src);
                    push_ref(&mut self.facts.refs, from, RefKind::Read, &name, qualifier, Vec::new(), current);
                    continue;
                }
                "identifier" => {
                    let t = text(current, self.src);
                    if !declared.contains(t) && is_expression_identifier(current) {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, t, None, Vec::new(), current);
                    }
                }
                // Types, declarators' names and template arguments are not reads.
                "type_identifier" | "primitive_type" | "type_descriptor" | "template_argument_list" | "parameter_list" | "statement_identifier" | "field_identifier" | "namespace_identifier" => continue,
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn args(&self, arguments: Node) -> Vec<String> {
        named_children(arguments).into_iter().filter(|c| c.kind() != "comment").map(|c| text(c, self.src).to_owned()).collect()
    }

    fn write_target(&mut self, left: Node, from: Option<usize>, declared: &HashSet<String>, at: Node) {
        match left.kind() {
            "field_expression" => {
                if let (Some(object), Some(f)) = (field(left, "argument"), field_text(left, "field", self.src)) {
                    let q = self.qualifier(object);
                    if !q.is_empty() {
                        push_ref(&mut self.facts.refs, from, RefKind::Write, f, Some(q), Vec::new(), at);
                    }
                }
            }
            // A bare name that is not a local is a member (implicit `this`) or a global.
            "identifier" => {
                let t = text(left, self.src);
                if !declared.contains(t) {
                    push_ref(&mut self.facts.refs, from, RefKind::Write, t, None, Vec::new(), at);
                }
            }
            "qualified_identifier" => {
                let (name, qualifier) = type_ref(left, self.src);
                push_ref(&mut self.facts.refs, from, RefKind::Write, &name, qualifier, Vec::new(), at);
            }
            _ => {}
        }
    }

    /// Locals whose type the declaration names (`Store s(1);`, `Store* p = ..`,
    /// `std::unique_ptr<Store> p`) or, for `auto`, constructs (`auto s = Store(1)`,
    /// `auto p = new Store`, `auto p = std::make_unique<Store>()`).
    fn locals(&mut self, decl: Node, from: usize) {
        let Some(type_node) = field(decl, "type") else { return };
        let auto = type_node.kind() == "placeholder_type_specifier";
        let declared = base_type_name(type_node, self.src);
        let mut cursor = decl.walk();
        let declarators: Vec<Node> = decl.children_by_field_name("declarator", &mut cursor).collect();
        for d in declarators {
            if function_declarator(d).is_some() {
                continue;
            }
            let Some(name) = declarator_name(d).filter(|n| n.kind() == "identifier") else { continue };
            // A declared type is the type; only `auto` takes the initializer's.
            let type_name = if auto { field(d, "value").and_then(|v| self.constructed_type(v)) } else { Some(declared.clone()) };
            if let Some(type_name) = type_name.filter(|t| !t.is_empty()) {
                self.facts.locals.push(LocalFact { from, name: text(name, self.src).to_owned(), type_name });
            }
        }
    }

    fn constructed_type(&self, value: Node) -> Option<String> {
        match value.kind() {
            "new_expression" | "compound_literal_expression" => field(value, "type").map(|t| base_type_name(t, self.src)),
            "call_expression" => {
                let function = field(value, "function")?;
                match function.kind() {
                    "identifier" => Some(text(function, self.src).to_owned()),
                    "qualified_identifier" => {
                        let (_, name) = flatten_qualified(function, self.src);
                        if name.kind() == "template_function" && matches!(field_text(name, "name", self.src), Some("make_unique" | "make_shared")) {
                            field(name, "arguments").and_then(|a| named_children(a).into_iter().next()).map(|t| base_type_name(t, self.src))
                        } else {
                            None
                        }
                    }
                    "template_function" if matches!(field_text(function, "name", self.src), Some("make_unique" | "make_shared")) => {
                        field(function, "arguments").and_then(|a| named_children(a).into_iter().next()).map(|t| base_type_name(t, self.src))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn call(&mut self, node: Node, from: Option<usize>, declared: &HashSet<String>) {
        let Some(function) = field(node, "function") else { return };
        let arguments = field(node, "arguments");
        let arg_nodes: Vec<Node> = arguments.map(named_children).unwrap_or_default().into_iter().filter(|c| c.kind() != "comment").collect();
        let args: Vec<String> = arg_nodes.iter().map(|a| text(*a, self.src).to_owned()).collect();
        let (name, qualifier, template) = match function.kind() {
            "identifier" => (text(function, self.src).to_owned(), None, None),
            "template_function" => (field_text(function, "name", self.src).unwrap_or("").to_owned(), None, Some(function)),
            "qualified_identifier" => {
                let (scopes, last) = flatten_qualified(function, self.src);
                let name = match last.kind() {
                    "template_function" => field_text(last, "name", self.src).unwrap_or("").to_owned(),
                    _ => text(last, self.src).to_owned(),
                };
                let qualifier = (!scopes.is_empty()).then(|| format!("{}::", scopes.join("::")));
                (name, qualifier, (last.kind() == "template_function").then_some(last))
            }
            "field_expression" => {
                let name = field(function, "field").map(|f| match f.kind() {
                    "template_method" => field_text(f, "name", self.src).unwrap_or("").to_owned(),
                    _ => text(f, self.src).to_owned(),
                });
                let qualifier = field(function, "argument").map(|o| self.qualifier(o));
                (name.unwrap_or_default(), qualifier, None)
            }
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        // A call of a local (a function pointer, a lambda) is not a reference to a declaration.
        if qualifier.is_none() && declared.contains(&name) {
            return;
        }
        let std_or_global = qualifier.as_deref().is_none_or(|q| q == "std::");
        // Environment reads: `getenv("X")`, `std::getenv("X")`, `secure_getenv("X")` (libc API names).
        if std_or_global && matches!(name.as_str(), "getenv" | "secure_getenv") {
            if let Some(key) = arg_nodes.first().and_then(|a| string_content(*a, self.src)) {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), node);
            }
            return;
        }
        // Programs run: `system("x")`, `popen("x", "r")`, `exec*("x", ..)`, `posix_spawn(.., "x", ..)`.
        if std_or_global && matches!(name.as_str(), "system" | "popen" | "execl" | "execlp" | "execle" | "execv" | "execvp" | "execve" | "execvpe" | "posix_spawn" | "posix_spawnp") {
            for arg in &arg_nodes {
                if let Some(lit) = string_content(*arg, self.src) {
                    for word in lit.split_whitespace() {
                        if looks_like_script_path(word) {
                            self.facts.runs.push(RunFact { from, target: word.to_owned(), line: line(node) });
                        }
                    }
                }
            }
        }
        // `std::make_unique<T>(..)` / `std::make_shared<T>(..)` construct a T (std API names).
        if matches!(name.as_str(), "make_unique" | "make_shared")
            && let Some(t) = template.and_then(|t| field(t, "arguments")).and_then(|a| named_children(a).into_iter().next())
        {
            let (tname, tq) = type_ref(t, self.src);
            push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &tname, tq, args, node);
            return;
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
    }
}

/// An identifier in expression position (a read of a name), not a declarator's name, a label or
/// a designated initializer's field.
fn is_expression_identifier(node: Node) -> bool {
    let Some(parent) = node.parent() else { return false };
    let is_field = |name: &str| field(parent, name).is_some_and(|f| f.id() == node.id());
    match parent.kind() {
        "call_expression" => !is_field("function"),
        "init_declarator" | "pointer_declarator" | "reference_declarator" | "array_declarator" | "function_declarator" | "parameter_declaration" | "optional_parameter_declaration" => !is_field("declarator"),
        "goto_statement" | "labeled_statement" | "field_designator" | "preproc_defined" | "lambda_capture_specifier" | "destructor_name" => false,
        _ => true,
    }
}

/// Names declared inside a function: parameters, locals, loop / catch / lambda / structured
/// binding variables.
fn collect_declared(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "declaration" | "parameter_declaration" | "optional_parameter_declaration" | "variadic_parameter_declaration" | "for_range_loop" => {
                let mut cursor = current.walk();
                for d in current.children_by_field_name("declarator", &mut cursor) {
                    if function_declarator(d).is_some() {
                        continue;
                    }
                    if let Some(n) = declarator_name(d) {
                        out.insert(text(n, src).to_owned());
                    }
                }
            }
            "structured_binding_declarator" => {
                for n in named_children(current) {
                    if n.kind() == "identifier" {
                        out.insert(text(n, src).to_owned());
                    }
                }
            }
            _ => {}
        }
        stack.extend(named_children(current));
    }
}
