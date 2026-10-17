//! Java extraction (tree-sitter-java).
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/java.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, LocalFact, ParamFact, RefKind, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashSet;

fn language() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    /// Maven Surefire's default test includes (`Test*.java`, `*Test.java`, `*Tests.java`,
    /// `*TestCase.java`) or a file under `src/test/`.
    test_file: bool,
    /// Framework of annotated tests: `testng` when the file imports TestNG, else `junit`.
    framework: &'static str,
}

#[derive(Clone, Default)]
struct Scope {
    parent: Option<usize>,
    /// Qualified-name prefix (`Outer.Inner.`).
    prefix: String,
    /// The enclosing type's simple name.
    owner: String,
    /// Members of an interface or annotation type are implicitly public.
    in_interface: bool,
    /// A JUnit 3 test class: `test*` methods are tests.
    junit3: bool,
}

/// What a method body is walked with.
#[derive(Default)]
struct Body {
    /// Names declared inside the method (parameters, locals, lambda / catch / for / pattern
    /// variables): reads of these are locals, not fields.
    declared: HashSet<String>,
}

/// JUnit 4/5 and TestNG annotations that make a method a test (API names).
const TEST_ANNOTATIONS: &[&str] = &["Test", "ParameterizedTest", "RepeatedTest", "TestFactory", "TestTemplate"];

pub fn extract(path: &str, src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let stem = file_name.trim_end_matches(".java");
    let test_file = stem.starts_with("Test") || stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("TestCase") || path.contains("src/test/");
    let mut walker = Walker { src, facts, test_file, framework: "junit" };
    let nodes = named_children(root);
    for node in &nodes {
        match node.kind() {
            "package_declaration" => {
                walker.facts.package = named_children(*node).into_iter().find(|c| matches!(c.kind(), "scoped_identifier" | "identifier")).map(|c| text(c, src).to_owned());
            }
            "import_declaration" => walker.import(*node),
            _ => {}
        }
    }
    if walker.facts.imports.iter().any(|i| i.module == "org.testng" || i.module.starts_with("org.testng.")) {
        walker.framework = "testng";
    }
    let scope = Scope::default();
    for node in &nodes {
        walker.member(*node, &scope);
    }
}

/// `a.b.C<T>` / `C[]` -> `C`.
fn base_type_name(node: Node, src: &str) -> String {
    match node.kind() {
        "generic_type" => named_children(node).into_iter().next().map(|t| base_type_name(t, src)).unwrap_or_default(),
        "scoped_type_identifier" => named_children(node).into_iter().last().map(|t| base_type_name(t, src)).unwrap_or_default(),
        "array_type" => field(node, "element").map(|t| base_type_name(t, src)).unwrap_or_default(),
        "annotated_type" => named_children(node).into_iter().last().map(|t| base_type_name(t, src)).unwrap_or_default(),
        "type_identifier" | "identifier" => text(node, src).to_owned(),
        _ => String::new(),
    }
}

/// A type as written at a use site: (name, qualifier) for `a.b.C<T>` -> (`C`, `a.b`).
fn type_ref(node: Node, src: &str) -> (String, Option<String>) {
    match node.kind() {
        "generic_type" => named_children(node).into_iter().next().map(|t| type_ref(t, src)).unwrap_or_default(),
        "scoped_type_identifier" => {
            let raw = text(node, src);
            match raw.rsplit_once('.') {
                Some((q, n)) => (n.trim().to_owned(), Some(q.trim().to_owned())),
                None => (raw.to_owned(), None),
            }
        }
        _ => (base_type_name(node, src), None),
    }
}

impl<'a> Walker<'a> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    /// The declaration's text up to its body, without its annotations (they are the decorators).
    fn set_signature(&self, symbol: &mut SymbolFact, node: Node, body: Option<Node>) {
        let mut start = node.start_byte();
        if let Some(mods) = named_children(node).into_iter().find(|c| c.kind() == "modifiers") {
            start = children(mods)
                .into_iter()
                .find(|c| !matches!(c.kind(), "marker_annotation" | "annotation") && !c.kind().ends_with("comment"))
                .map(|c| c.start_byte())
                .or_else(|| mods.next_sibling().map(|n| n.start_byte()))
                .unwrap_or(start);
        }
        symbol.signature = head_signature(node, start, body, self.src);
        symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
    }

    /// Javadoc (`/** .. */`) directly above a declaration.
    fn doc(&self, node: Node) -> Option<String> {
        comments_before(node, self.src, |t| t.starts_with("/**") && !t.starts_with("/***"))
    }

    fn import(&mut self, node: Node) {
        // `import a.b.C;` binds C from package a.b; `import static a.b.C.m;` binds m from class a.b.C;
        // wildcards bind every name of the package or class.
        let wildcard = named_children(node).iter().any(|c| c.kind() == "asterisk");
        let Some(path) = named_children(node).into_iter().find(|c| matches!(c.kind(), "scoped_identifier" | "identifier")).map(|c| text(c, self.src).to_owned()) else { return };
        let line = line(node);
        let (module, name, local) = if wildcard {
            (path, "*".to_owned(), "*".to_owned())
        } else {
            match path.rsplit_once('.') {
                Some((module, name)) => (module.to_owned(), name.to_owned(), name.to_owned()),
                None => (String::new(), path.clone(), path),
            }
        };
        self.facts.imports.push(ImportFact { module, name, local, line });
    }

    fn member(&mut self, node: Node, scope: &Scope) {
        match node.kind() {
            "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "annotation_type_declaration" => self.type_decl(node, scope),
            "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" | "annotation_type_element_declaration" => self.method(node, scope),
            "field_declaration" | "constant_declaration" => self.field_decl(node, scope),
            // Initializer blocks run as part of the type.
            "static_initializer" | "block" => {
                let mut ctx = Body::default();
                collect_declared(node, self.src, &mut ctx.declared);
                self.body(node, scope.parent, &ctx);
            }
            _ => {}
        }
    }

    fn type_decl(&mut self, node: Node, scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let kind = match node.kind() {
            "interface_declaration" | "annotation_type_declaration" => "interface",
            "enum_declaration" => "enum",
            _ => "class",
        };
        let body = field(node, "body");
        let mods = Modifiers::of(node, self.src);
        let mut symbol = new_symbol(kind, &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        self.set_signature(&mut symbol, node, body);
        symbol.doc = self.doc(node);
        symbol.decorators = mods.annotations.clone();
        symbol.parent = scope.parent;
        symbol.visibility = mods.visibility.clone();
        symbol.is_static = mods.is_static;
        symbol.exported = mods.visibility.as_deref() == Some("public") || (scope.in_interface && mods.visibility.is_none());
        if let Some(superclass) = field(node, "superclass")
            && let Some(t) = named_children(superclass).into_iter().next()
        {
            symbol.extends.push(base_type_name(t, self.src));
        }
        for clause in named_children(node) {
            let into_extends = match clause.kind() {
                "super_interfaces" => false,
                "extends_interfaces" => true,
                _ => continue,
            };
            for list in named_children(clause) {
                for t in named_children(list) {
                    let n = base_type_name(t, self.src);
                    if n.is_empty() {
                        continue;
                    }
                    if into_extends {
                        symbol.extends.push(n);
                    } else {
                        symbol.implements.push(n);
                    }
                }
            }
        }
        // JUnit 3: subclasses of `TestCase`; in test files, any test class with a superclass.
        let junit3 = node.kind() == "class_declaration" && (symbol.extends.iter().any(|e| e.ends_with("TestCase")) || (self.test_file && !symbol.extends.is_empty()));
        let index = self.push(symbol);
        let inner = Scope { parent: Some(index), prefix: format!("{}{}.", scope.prefix, name), owner: name.clone(), in_interface: kind == "interface", junit3 };
        // Record components are the record's fields.
        if node.kind() == "record_declaration"
            && let Some(params) = field(node, "parameters")
        {
            for component in named_children(params) {
                if component.kind() != "formal_parameter" {
                    continue;
                }
                let Some(cname) = field_text(component, "name", self.src) else { continue };
                let mut f = new_symbol("field", cname, format!("{}{}", inner.prefix, cname), component, None, self.src);
                f.parent = Some(index);
                f.exported = true;
                if let Some(t) = field(component, "type") {
                    f.type_text = Some(text(t, self.src).to_owned());
                    f.type_names = type_names(t, self.src);
                }
                self.push(f);
            }
        }
        let Some(body) = body else { return };
        for child in named_children(body) {
            match child.kind() {
                "enum_constant" => self.enum_constant(child, &inner),
                "enum_body_declarations" => {
                    for member in named_children(child) {
                        self.member(member, &inner);
                    }
                }
                _ => self.member(child, &inner),
            }
        }
    }

    fn enum_constant(&mut self, node: Node, scope: &Scope) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let body = field(node, "body");
        let mut v = new_symbol("variant", name, format!("{}{}", scope.prefix, name), node, body, self.src);
        v.doc = self.doc(node);
        v.parent = scope.parent;
        v.exported = scope.parent.is_some_and(|p| self.facts.symbols[p].exported);
        v.is_static = true;
        let arguments = field(node, "arguments");
        if let Some(arguments) = arguments {
            v.value = Some(text(arguments, self.src).to_owned());
        }
        let index = self.push(v);
        let mut ctx = Body::default();
        collect_declared(node, self.src, &mut ctx.declared);
        if let Some(arguments) = arguments {
            self.body(arguments, Some(index), &ctx);
        }
        // A constant-specific class body: its methods override the enum's.
        if let Some(body) = body {
            self.body(body, Some(index), &ctx);
        }
    }

    fn field_decl(&mut self, node: Node, scope: &Scope) {
        let mods = Modifiers::of(node, self.src);
        // Interface fields are implicitly `public static final`.
        let constant = scope.in_interface || (mods.is_static && mods.is_final);
        let kind = if constant { "const" } else { "field" };
        let type_node = field(node, "type");
        let doc = self.doc(node);
        let mut cursor = node.walk();
        let declarators: Vec<Node> = node.children_by_field_name("declarator", &mut cursor).collect();
        for declarator in declarators {
            let Some(name) = field_text(declarator, "name", self.src) else { continue };
            let mut f = new_symbol(kind, name, format!("{}{}", scope.prefix, name), node, None, self.src);
            f.doc = doc.clone();
            f.decorators = mods.annotations.clone();
            f.parent = scope.parent;
            f.visibility = mods.visibility.clone();
            f.is_static = mods.is_static || scope.in_interface;
            f.exported = mods.visibility.as_deref() == Some("public") || scope.in_interface;
            if let Some(t) = type_node {
                f.type_text = Some(text(t, self.src).to_owned());
                f.type_names = type_names(t, self.src);
            }
            let value = field(declarator, "value");
            if let Some(v) = value {
                f.value = Some(text(v, self.src).to_owned());
            }
            let index = self.push(f);
            if let Some(v) = value {
                let mut ctx = Body::default();
                collect_declared(v, self.src, &mut ctx.declared);
                self.body(v, Some(index), &ctx);
            }
        }
    }

    fn method(&mut self, node: Node, scope: &Scope) {
        let constructor = matches!(node.kind(), "constructor_declaration" | "compact_constructor_declaration");
        let name = if constructor { Some(scope.owner.clone()) } else { field_text(node, "name", self.src).map(str::to_owned) };
        let Some(name) = name.filter(|n| !n.is_empty()) else { return };
        let body = field(node, "body");
        let mods = Modifiers::of(node, self.src);
        let params = field(node, "parameters");
        let return_type = field(node, "type");
        // Annotation API names (JUnit 4/5, TestNG), by simple or qualified name.
        let annotated_test = mods.annotation_names.iter().any(|a| TEST_ANNOTATIONS.contains(&a.rsplit('.').next().unwrap_or(a)));
        // JUnit 3: public, no-argument, void `test*` methods of a test class.
        let junit3_test = scope.junit3
            && name.starts_with("test")
            && mods.visibility.as_deref() == Some("public")
            && !mods.is_static
            && return_type.is_some_and(|t| t.kind() == "void_type")
            && params.is_none_or(|p| named_children(p).is_empty());
        let is_test = !constructor && (annotated_test || junit3_test);
        let kind = if is_test { "test" } else { "method" };
        let mut symbol = new_symbol(kind, &name, format!("{}{}", scope.prefix, name), node, body, self.src);
        self.set_signature(&mut symbol, node, body);
        symbol.doc = self.doc(node);
        symbol.decorators = mods.annotations.clone();
        symbol.parent = scope.parent;
        symbol.visibility = mods.visibility.clone();
        symbol.is_static = mods.is_static;
        symbol.exported = mods.visibility.as_deref() == Some("public") || (scope.in_interface && mods.visibility.is_none());
        if is_test {
            symbol.framework = Some(self.framework.into());
        }
        if let Some(params) = params {
            symbol.params = self.params(params);
        }
        if let Some(ret) = return_type
            && ret.kind() != "void_type"
        {
            symbol.return_type = Some(text(ret, self.src).to_owned());
            symbol.return_type_names = type_names(ret, self.src);
        }
        if node.kind() == "annotation_type_element_declaration"
            && let Some(v) = field(node, "value")
        {
            symbol.value = Some(text(v, self.src).to_owned());
        }
        let index = self.push(symbol);
        // `throws X, Y`: the declared exceptions.
        if let Some(throws) = named_children(node).into_iter().find(|c| c.kind() == "throws") {
            for t in named_children(throws) {
                let (n, q) = type_ref(t, self.src);
                push_ref(&mut self.facts.refs, Some(index), RefKind::Throw, &n, q, Vec::new(), t);
            }
        }
        if let Some(body) = body {
            let mut ctx = Body::default();
            if let Some(params) = params {
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
            let (name, type_node, variadic) = match p.kind() {
                "formal_parameter" => (field_text(p, "name", self.src).unwrap_or("_").to_owned(), field(p, "type"), false),
                "spread_parameter" => {
                    let declarator = named_children(p).into_iter().find(|c| c.kind() == "variable_declarator");
                    let name = declarator.and_then(|d| field_text(d, "name", self.src)).unwrap_or("_").to_owned();
                    let type_node = named_children(p).into_iter().find(|c| !matches!(c.kind(), "variable_declarator" | "modifiers"));
                    (name, type_node, true)
                }
                // `receiver_parameter` (`Foo this`) is not an argument.
                _ => continue,
            };
            let mut fact = param(&name, position);
            fact.variadic = variadic;
            if let Some(t) = type_node {
                fact.type_text = Some(if variadic { format!("{}...", text(t, self.src)) } else { text(t, self.src).to_owned() });
                fact.type_names = type_names(t, self.src);
            }
            out.push(fact);
            position += 1;
        }
        out
    }

    /// The qualifier of a member access's object: `this`, `super`, a name or a simple chain;
    /// anything else is `""` (unknown receiver).
    fn qualifier(&self, object: Node) -> String {
        match object.kind() {
            "this" | "super" | "identifier" => text(object, self.src).to_owned(),
            "field_access" => {
                let inner = field(object, "object").map(|o| self.qualifier(o)).unwrap_or_default();
                let f = field_text(object, "field", self.src).unwrap_or("");
                if inner.is_empty() { String::new() } else { format!("{inner}.{f}") }
            }
            "scoped_identifier" => text(object, self.src).to_owned(),
            _ => String::new(),
        }
    }

    /// References inside a method body or initializer.
    fn body(&mut self, node: Node, from: Option<usize>, ctx: &Body) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "method_invocation" => self.call(current, from),
                "object_creation_expression" => {
                    if let Some(t) = field(current, "type") {
                        let (name, qualifier) = type_ref(t, self.src);
                        let args = self.args(field(current, "arguments"));
                        // `new ProcessBuilder("bash", "x.sh")` (java.lang API name).
                        if name == "ProcessBuilder" {
                            self.runs(field(current, "arguments"), from, current);
                        }
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &name, qualifier, args, current);
                    }
                }
                "throw_statement" => {
                    if let Some(expr) = named_children(current).into_iter().next()
                        && expr.kind() == "object_creation_expression"
                        && let Some(t) = field(expr, "type")
                    {
                        let (name, qualifier) = type_ref(t, self.src);
                        push_ref(&mut self.facts.refs, from, RefKind::Throw, &name, qualifier, Vec::new(), current);
                    }
                }
                "local_variable_declaration" => {
                    if let Some(from) = from {
                        self.locals(current, from);
                    }
                }
                "method_reference" => {
                    // `this::m`, `Foo::m`, `Foo::new`.
                    let parts = named_children(current);
                    if let (Some(object), Some(last)) = (parts.first(), parts.last())
                        && parts.len() >= 2
                        && last.kind() == "identifier"
                    {
                        let q = self.qualifier(*object);
                        push_ref(&mut self.facts.refs, from, RefKind::Call, text(*last, self.src), Some(q), Vec::new(), current);
                    } else if let Some(object) = parts.first()
                        && children(current).iter().any(|c| c.kind() == "new")
                    {
                        let (name, qualifier) = type_ref(*object, self.src);
                        push_ref(&mut self.facts.refs, from, RefKind::Instantiate, &name, qualifier, Vec::new(), current);
                    }
                    continue;
                }
                "assignment_expression" | "update_expression" => {
                    let left = field(current, "left").or_else(|| named_children(current).into_iter().next());
                    if let Some(left) = left {
                        self.write_target(left, from, ctx, current);
                        // The target's own sub-expressions (array index, object chain) are reads.
                        if !matches!(left.kind(), "identifier") {
                            if left.kind() == "field_access" {
                                if let Some(o) = field(left, "object") {
                                    stack.push(o);
                                }
                            } else {
                                stack.push(left);
                            }
                        }
                    }
                    if let Some(right) = field(current, "right") {
                        stack.push(right);
                    }
                    continue;
                }
                "field_access" => {
                    if let (Some(object), Some(f)) = (field(current, "object"), field_text(current, "field", self.src)) {
                        let q = self.qualifier(object);
                        if !q.is_empty() {
                            push_ref(&mut self.facts.refs, from, RefKind::Read, f, Some(q), Vec::new(), current);
                        }
                        if !matches!(object.kind(), "this" | "super") {
                            stack.push(object);
                        }
                    }
                    continue;
                }
                "identifier" => {
                    let t = text(current, self.src);
                    if !ctx.declared.contains(t) && is_expression_identifier(current) {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, t, None, Vec::new(), current);
                    }
                }
                // Type positions, annotations and declarations' names are not reads.
                "marker_annotation" | "annotation" | "type_identifier" | "scoped_type_identifier" | "generic_type" | "formal_parameters" | "catch_formal_parameter" | "class_literal" => continue,
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn args(&self, arguments: Option<Node>) -> Vec<String> {
        arguments.map(|a| named_children(a).into_iter().filter(|c| !c.kind().ends_with("comment")).map(|c| text(c, self.src).to_owned()).collect()).unwrap_or_default()
    }

    /// Script or program paths among string-literal arguments (and array / list arguments).
    fn runs(&mut self, arguments: Option<Node>, from: Option<usize>, at: Node) {
        let mut stack = arguments.map(named_children).unwrap_or_default();
        while let Some(arg) = stack.pop() {
            if let Some(lit) = string_content(arg, self.src) {
                for word in lit.split_whitespace() {
                    if looks_like_script_path(word) {
                        self.facts.runs.push(RunFact { from, target: word.to_owned(), line: line(at) });
                    }
                }
            } else if matches!(arg.kind(), "array_creation_expression" | "array_initializer" | "method_invocation" | "argument_list") {
                if arg.kind() == "method_invocation" {
                    stack.extend(field(arg, "arguments").map(named_children).unwrap_or_default());
                } else {
                    stack.extend(named_children(arg));
                }
            }
        }
    }

    fn write_target(&mut self, left: Node, from: Option<usize>, ctx: &Body, at: Node) {
        match left.kind() {
            "field_access" => {
                if let (Some(object), Some(f)) = (field(left, "object"), field_text(left, "field", self.src)) {
                    let q = self.qualifier(object);
                    if !q.is_empty() {
                        push_ref(&mut self.facts.refs, from, RefKind::Write, f, Some(q), Vec::new(), at);
                    }
                }
            }
            // A bare name that is not a local is a field (of this class, an outer class or a base).
            "identifier" => {
                let t = text(left, self.src);
                if !ctx.declared.contains(t) {
                    push_ref(&mut self.facts.refs, from, RefKind::Write, t, None, Vec::new(), at);
                }
            }
            _ => {}
        }
    }

    /// Locals with a declared type (`Store s = ..`) or, for `var`, a constructed one (`var s = new Store()`).
    fn locals(&mut self, decl: Node, from: usize) {
        let declared = field(decl, "type").map(|t| base_type_name(t, self.src)).filter(|t| !t.is_empty() && t != "var");
        let mut cursor = decl.walk();
        let declarators: Vec<Node> = decl.children_by_field_name("declarator", &mut cursor).collect();
        for d in declarators {
            let Some(name) = field_text(d, "name", self.src) else { continue };
            let inferred = field(d, "value").filter(|v| v.kind() == "object_creation_expression").and_then(|v| field(v, "type")).map(|t| base_type_name(t, self.src));
            if let Some(type_name) = declared.clone().or(inferred).filter(|t| !t.is_empty()) {
                self.facts.locals.push(LocalFact { from, name: name.to_owned(), type_name });
            }
        }
    }

    fn call(&mut self, node: Node, from: Option<usize>) {
        let Some(name) = field_text(node, "name", self.src) else { return };
        let name = name.to_owned();
        let object = field(node, "object");
        let qualifier = object.map(|o| self.qualifier(o));
        let arguments = field(node, "arguments");
        let args = self.args(arguments);
        // Environment reads: `System.getenv("X")` (java.lang API name).
        if name == "getenv" && qualifier.as_deref() == Some("System") {
            if let Some(key) = arguments.and_then(|a| named_children(a).into_iter().next()).and_then(|a| string_content(a, self.src)) {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), node);
            }
            return;
        }
        // Programs run: `Runtime.getRuntime().exec(..)`, `processBuilder.command(..)` (API names).
        let on_runtime = object.is_some_and(|o| o.kind() == "method_invocation" && field_text(o, "name", self.src) == Some("getRuntime"));
        if (name == "exec" && on_runtime) || (name == "command" && object.is_some_and(|o| o.kind() == "object_creation_expression" && field(o, "type").is_some_and(|t| base_type_name(t, self.src) == "ProcessBuilder"))) {
            self.runs(arguments, from, node);
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
    }
}

/// Modifiers of a declaration (the `modifiers` child).
struct Modifiers {
    /// Annotation texts as written (`@Test`, `@ValueSource(ints = {1})`).
    annotations: Vec<String>,
    /// Annotation names (`Test`, `org.junit.Test`).
    annotation_names: Vec<String>,
    visibility: Option<String>,
    is_static: bool,
    is_final: bool,
}

impl Modifiers {
    fn of(node: Node, src: &str) -> Self {
        let mut out = Modifiers { annotations: Vec::new(), annotation_names: Vec::new(), visibility: None, is_static: false, is_final: false };
        let Some(mods) = named_children(node).into_iter().find(|c| c.kind() == "modifiers") else { return out };
        for m in children(mods) {
            match m.kind() {
                "marker_annotation" | "annotation" => {
                    out.annotations.push(text(m, src).to_owned());
                    if let Some(n) = field_text(m, "name", src) {
                        out.annotation_names.push(n.to_owned());
                    }
                }
                "public" | "private" | "protected" => out.visibility = Some(m.kind().to_owned()),
                "static" => out.is_static = true,
                "final" => out.is_final = true,
                _ => {}
            }
        }
        out
    }
}

/// An identifier in expression position (a read of a name), not a declaration's name, a method
/// name, a member name after `.`, a label or an annotation key.
fn is_expression_identifier(node: Node) -> bool {
    let Some(parent) = node.parent() else { return false };
    let is_field = |name: &str| field(parent, name).is_some_and(|f| f.id() == node.id());
    match parent.kind() {
        "method_invocation" => !is_field("name"),
        "field_access" => !is_field("field"),
        "variable_declarator" | "formal_parameter" | "catch_formal_parameter" | "enhanced_for_statement" | "resource" | "instanceof_expression" => !is_field("name"),
        "element_value_pair" => !is_field("key"),
        "labeled_statement" | "break_statement" | "continue_statement" | "scoped_identifier" | "method_reference" | "lambda_expression" | "inferred_parameters" | "type_pattern" | "enum_constant" | "module_declaration" => false,
        _ => true,
    }
}

/// Names declared inside a method: parameters, locals, lambda / catch / for / resource / pattern
/// variables.
fn collect_declared(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "variable_declarator" | "formal_parameter" | "catch_formal_parameter" | "enhanced_for_statement" | "resource" | "instanceof_expression" => {
                if let Some(n) = field(current, "name") {
                    out.insert(text(n, src).to_owned());
                }
            }
            "lambda_expression" => {
                if let Some(params) = field(current, "parameters") {
                    if params.kind() == "identifier" {
                        out.insert(text(params, src).to_owned());
                    } else if params.kind() == "inferred_parameters" {
                        for p in named_children(params) {
                            out.insert(text(p, src).to_owned());
                        }
                    }
                }
            }
            "type_pattern" => {
                if let Some(n) = named_children(current).into_iter().rfind(|c| c.kind() == "identifier") {
                    out.insert(text(n, src).to_owned());
                }
            }
            _ => {}
        }
        stack.extend(named_children(current));
    }
}
