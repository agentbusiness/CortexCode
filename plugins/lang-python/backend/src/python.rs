//! Python extraction (tree-sitter-python): modules' functions, classes, methods, fields (class
//! attributes and `self.x` in `__init__`) and variables with qualified names, signatures,
//! docstrings, decorators, parameters and annotations; pytest / unittest tests; FastAPI / Flask
//! routes (with `APIRouter(prefix=..)` / `Blueprint(url_prefix=..)` prefixes); imports, `__all__`
//! exports, calls with argument text, `self` attribute reads and writes, raised exceptions,
//! receiver-typed locals, environment reads and scripts run through `subprocess` / `os.system`.
//!
//! Ported from the memory plugin's extractor (plugins/memory-irongraph/backend/src/lang/python.rs),
//! producing `lang@1` facts.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, LocalFact, ParamFact, RefKind, RefTarget, RunFact, SymbolFact};
use cc_lang_kit::syntax::*;
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashMap;

fn language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

struct Walker<'a> {
    src: &'a str,
    facts: &'a mut FileFacts,
    /// Router variables with a prefix: `router = APIRouter(prefix="/x")`, `bp = Blueprint(.., url_prefix="/x")`.
    router_prefixes: HashMap<String, String>,
    test_file: bool,
}

pub fn extract(path: &str, src: &str, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), src) else { return };
    let root = tree.root_node();
    facts.parse_errors = has_error(root);
    let file_name = path.rsplit('/').next().unwrap_or(path);
    // pytest collection convention: test_*.py, *_test.py, or files under a tests/ directory.
    let test_file = file_name.starts_with("test_") || file_name.ends_with("_test.py") || path.split('/').any(|seg| seg == "tests" || seg == "test");
    let mut walker = Walker { src, facts, router_prefixes: HashMap::new(), test_file };
    walker.block(root, None, "", None);
    attach_notes(root, src, walker.facts);
}

impl<'a> Walker<'a> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    /// Statements of a module or class body. `class` is set inside a class body.
    fn block(&mut self, list: Node, parent: Option<usize>, prefix: &str, class: Option<(usize, bool)>) {
        for node in named_children(list) {
            self.statement(node, parent, prefix, class);
        }
    }

    fn statement(&mut self, node: Node, parent: Option<usize>, prefix: &str, class: Option<(usize, bool)>) {
        match node.kind() {
            "function_definition" => {
                self.function(node, node, Vec::new(), parent, prefix, class);
            }
            "class_definition" => self.class(node, node, Vec::new(), parent, prefix),
            "decorated_definition" => {
                let decorators: Vec<Node> = named_children(node).into_iter().filter(|c| c.kind() == "decorator").collect();
                if let Some(def) = field(node, "definition") {
                    match def.kind() {
                        "function_definition" => self.function(def, node, decorators, parent, prefix, class),
                        "class_definition" => self.class(def, node, decorators, parent, prefix),
                        _ => {}
                    }
                }
            }
            "import_statement" | "import_from_statement" | "future_import_statement" => self.import(node),
            "expression_statement" => {
                let Some(expr) = named_children(node).into_iter().next() else { return };
                if expr.kind() == "assignment" && parent.is_none_or(|_| class.is_some()) {
                    self.assignment(expr, node, parent, prefix, class);
                } else if parent.is_none() || class.is_some() {
                    self.body(node, parent);
                }
            }
            // Module-level control flow (`if __name__ == "__main__":`, try/except imports).
            "if_statement" | "try_statement" | "with_statement" | "for_statement" | "while_statement" => {
                if parent.is_none() && class.is_none() {
                    for child in named_children(node) {
                        match child.kind() {
                            "block" => self.block(child, parent, prefix, class),
                            "else_clause" | "elif_clause" | "except_clause" | "finally_clause" => {
                                for inner in named_children(child) {
                                    if inner.kind() == "block" {
                                        self.block(inner, parent, prefix, class);
                                    } else {
                                        self.body(inner, parent);
                                    }
                                }
                            }
                            _ => self.body(child, parent),
                        }
                    }
                } else {
                    self.body(node, parent);
                }
            }
            _ => {
                if parent.is_none() {
                    self.body(node, parent);
                }
            }
        }
    }

    fn import(&mut self, node: Node) {
        let line = line(node);
        match node.kind() {
            "import_statement" => {
                for name in named_children(node) {
                    match name.kind() {
                        "dotted_name" => {
                            let module = text(name, self.src).to_owned();
                            // `import a.b` binds `a`.
                            let local = module.split('.').next().unwrap_or(&module).to_owned();
                            self.facts.imports.push(ImportFact { module, name: "*".into(), local, line });
                        }
                        "aliased_import" => {
                            let module = field_text(name, "name", self.src).unwrap_or("").to_owned();
                            let local = field_text(name, "alias", self.src).unwrap_or("").to_owned();
                            self.facts.imports.push(ImportFact { module, name: "*".into(), local, line });
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                let module = field(node, "module_name").map(|m| text(m, self.src).to_owned()).unwrap_or_default();
                let mut any = false;
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    any = true;
                    match name.kind() {
                        "dotted_name" => {
                            let n = text(name, self.src).to_owned();
                            self.facts.imports.push(ImportFact { module: module.clone(), name: n.clone(), local: n, line });
                        }
                        "aliased_import" => {
                            let n = field_text(name, "name", self.src).unwrap_or("").to_owned();
                            let local = field_text(name, "alias", self.src).unwrap_or("").to_owned();
                            self.facts.imports.push(ImportFact { module: module.clone(), name: n, local, line });
                        }
                        _ => {}
                    }
                }
                if !any && named_children(node).iter().any(|c| c.kind() == "wildcard_import") {
                    self.facts.imports.push(ImportFact { module, name: "*".into(), local: "*".into(), line });
                }
            }
            _ => {}
        }
    }

    fn docstring(&self, body: Option<Node>) -> Option<String> {
        let body = body?;
        let first = named_children(body).into_iter().next()?;
        if first.kind() != "expression_statement" {
            return None;
        }
        let expr = named_children(first).into_iter().next()?;
        if expr.kind() != "string" {
            return None;
        }
        let raw = strip_quotes(text(expr, self.src));
        let cleaned: Vec<&str> = raw.lines().map(str::trim).collect();
        let joined = cleaned.join("\n").trim().to_owned();
        if joined.is_empty() { None } else { Some(joined) }
    }

    fn function(&mut self, def: Node, outer: Node, decorators: Vec<Node>, parent: Option<usize>, prefix: &str, class: Option<(usize, bool)>) {
        let Some(name) = field_text(def, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(def, "body");
        let decorator_texts: Vec<String> = decorators.iter().map(|d| text(*d, self.src).to_owned()).collect();
        let is_static = decorator_texts.iter().any(|d| d.contains("staticmethod"));
        let in_class = class.is_some();
        // pytest / unittest conventions: `test*` functions in test files, `test*` methods of
        // `Test*` classes or TestCase subclasses.
        let is_test = match class {
            Some((_, test_class)) => test_class && name.starts_with("test"),
            None => self.test_file && name.starts_with("test"),
        };
        let kind = if is_test {
            "test"
        } else if in_class {
            "method"
        } else {
            "function"
        };
        let mut symbol = new_symbol(kind, &name, format!("{prefix}{name}"), def, body, self.src);
        symbol.code = text(outer, self.src).to_owned();
        symbol.doc = self.docstring(body);
        symbol.decorators = decorator_texts.clone();
        symbol.parent = parent;
        symbol.is_async = children(def).iter().any(|c| c.kind() == "async");
        symbol.is_static = is_static;
        symbol.exported = !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"));
        if is_test {
            symbol.framework = Some("pytest".into());
        }
        if let Some(params) = field(def, "parameters") {
            let skip_receiver = in_class && !is_static;
            symbol.params = self.params(params, skip_receiver);
        }
        if let Some(ret) = field(def, "return_type") {
            symbol.return_type = Some(text(ret, self.src).to_owned());
            symbol.return_type_names = type_names(ret, self.src);
        }
        let index = self.push(symbol);
        for decorator in &decorators {
            self.decorator_route(*decorator, &name, prefix, outer);
            self.body(*decorator, parent);
        }
        if let Some(body) = body {
            self.body(body, Some(index));
            if in_class && name == "__init__" {
                self.instance_fields(body, class.map(|c| c.0), prefix);
            }
        }
    }

    fn params(&self, params: Node, skip_receiver: bool) -> Vec<ParamFact> {
        let mut out = Vec::new();
        let mut position = 0;
        let mut first = true;
        for p in named_children(params) {
            let (name, type_node, value, variadic) = match p.kind() {
                "identifier" => (text(p, self.src).to_owned(), None, None, false),
                "typed_parameter" => {
                    let n = named_children(p).into_iter().find(|c| matches!(c.kind(), "identifier" | "list_splat_pattern" | "dictionary_splat_pattern"));
                    let variadic = n.is_some_and(|n| n.kind() != "identifier");
                    (n.map(|n| text(n, self.src).trim_start_matches('*').to_owned()).unwrap_or_default(), field(p, "type"), None, variadic)
                }
                "default_parameter" => (field_text(p, "name", self.src).unwrap_or("").to_owned(), None, field(p, "value"), false),
                "typed_default_parameter" => (field_text(p, "name", self.src).unwrap_or("").to_owned(), field(p, "type"), field(p, "value"), false),
                "list_splat_pattern" | "dictionary_splat_pattern" => {
                    (named_children(p).into_iter().next().map(|n| text(n, self.src).to_owned()).unwrap_or_default(), None, None, true)
                }
                // `*` and `/` separators.
                "keyword_separator" | "positional_separator" => continue,
                _ => continue,
            };
            if first && skip_receiver {
                first = false;
                continue;
            }
            first = false;
            let display = match p.kind() {
                "list_splat_pattern" => format!("*{name}"),
                "dictionary_splat_pattern" => format!("**{name}"),
                _ => name,
            };
            let mut fact = param(&display, position);
            fact.variadic = variadic;
            if let Some(t) = type_node {
                fact.type_text = Some(text(t, self.src).to_owned());
                fact.type_names = type_names(t, self.src);
            }
            if let Some(v) = value {
                fact.default_value = Some(text(v, self.src).to_owned());
                fact.optional = true;
            }
            out.push(fact);
            position += 1;
        }
        out
    }

    /// FastAPI `@router.get("/p")`, Flask `@app.route("/p", methods=[..])`, `@x.api_route(..)`.
    fn decorator_route(&mut self, decorator: Node, handler: &str, prefix: &str, outer: Node) {
        let Some(call) = named_children(decorator).into_iter().next() else { return };
        if call.kind() != "call" {
            return;
        }
        let Some(function) = field(call, "function") else { return };
        if function.kind() != "attribute" {
            return;
        }
        let verb = field_text(function, "attribute", self.src).unwrap_or("");
        let object = field_text(function, "object", self.src).unwrap_or("").to_owned();
        let Some(arguments) = field(call, "arguments") else { return };
        let args = named_children(arguments);
        let Some(path) = args.iter().find(|a| a.kind() != "keyword_argument").and_then(|a| string_content(*a, self.src)) else { return };
        let mut methods: Vec<String> = Vec::new();
        if HTTP_VERBS.contains(&verb) {
            methods.push(verb.to_uppercase());
        } else if verb == "route" || verb == "api_route" {
            for arg in &args {
                if arg.kind() == "keyword_argument"
                    && field_text(*arg, "name", self.src) == Some("methods")
                    && let Some(list) = field(*arg, "value")
                {
                    for item in named_children(list) {
                        if let Some(m) = string_content(item, self.src) {
                            methods.push(m.to_uppercase());
                        }
                    }
                }
            }
            if methods.is_empty() {
                methods.push("GET".into());
            }
        } else {
            return;
        }
        let full_path = match self.router_prefixes.get(&object) {
            Some(router_prefix) => format!("{}{}", router_prefix.trim_end_matches('/'), path),
            None => path,
        };
        for method in methods {
            let name = format!("{method} {full_path}");
            let mut route = new_symbol("route", &name, format!("route:{name}"), decorator, None, self.src);
            route.signature = text(decorator, self.src).to_owned();
            route.code = text(outer, self.src).lines().take(3).collect::<Vec<_>>().join("\n");
            route.http_method = Some(method);
            route.route_path = Some(full_path.clone());
            route.handler = Some(RefTarget { name: handler.to_owned(), qualifier: (!prefix.is_empty()).then(|| prefix.trim_end_matches('.').to_owned()) });
            self.push(route);
        }
    }

    fn class(&mut self, def: Node, outer: Node, decorators: Vec<Node>, parent: Option<usize>, prefix: &str) {
        let Some(name) = field_text(def, "name", self.src) else { return };
        let name = name.to_owned();
        let body = field(def, "body");
        let mut symbol = new_symbol("class", &name, format!("{prefix}{name}"), def, body, self.src);
        symbol.code = text(outer, self.src).to_owned();
        symbol.doc = self.docstring(body);
        symbol.decorators = decorators.iter().map(|d| text(*d, self.src).to_owned()).collect();
        symbol.parent = parent;
        symbol.exported = !name.starts_with('_');
        if let Some(supers) = field(def, "superclasses") {
            for base in named_children(supers) {
                if base.kind() != "keyword_argument" {
                    symbol.extends.push(text(base, self.src).rsplit('.').next().unwrap_or("").to_owned());
                }
            }
        }
        // unittest / pytest class conventions.
        let test_class = name.starts_with("Test") || symbol.extends.iter().any(|b| b.ends_with("TestCase"));
        let index = self.push(symbol);
        for decorator in &decorators {
            self.body(*decorator, parent);
        }
        if let Some(body) = body {
            self.block(body, Some(index), &format!("{prefix}{name}."), Some((index, test_class)));
        }
    }

    fn assignment(&mut self, assign: Node, stmt: Node, parent: Option<usize>, prefix: &str, class: Option<(usize, bool)>) {
        let Some(left) = field(assign, "left") else { return };
        let right = field(assign, "right");
        // Router prefixes for route paths (FastAPI APIRouter / Flask Blueprint API names).
        if left.kind() == "identifier"
            && let Some(r) = right
            && r.kind() == "call"
            && let Some(f) = field(r, "function")
        {
            let fname = text(f, self.src).rsplit('.').next().unwrap_or("");
            if matches!(fname, "APIRouter" | "Blueprint" | "FastAPI" | "Flask")
                && let Some(arguments) = field(r, "arguments")
            {
                for arg in named_children(arguments) {
                    if arg.kind() == "keyword_argument"
                        && matches!(field_text(arg, "name", self.src), Some("prefix") | Some("url_prefix") | Some("root_path"))
                        && let Some(value) = field(arg, "value").and_then(|v| string_content(v, self.src))
                    {
                        self.router_prefixes.insert(text(left, self.src).to_owned(), value);
                    }
                }
            }
        }
        if left.kind() == "identifier" {
            let name = text(left, self.src).to_owned();
            if name == "__all__" {
                if let Some(r) = right {
                    for item in named_children(r) {
                        if let Some(n) = string_content(item, self.src) {
                            self.facts.exports.push(n);
                        }
                    }
                }
                return;
            }
            let kind = if class.is_some() { "field" } else { "variable" };
            let mut symbol = new_symbol(kind, &name, format!("{prefix}{name}"), stmt, None, self.src);
            symbol.parent = parent;
            symbol.exported = !name.starts_with('_');
            if let Some(t) = field(assign, "type") {
                symbol.type_text = Some(text(t, self.src).to_owned());
                symbol.type_names = type_names(t, self.src);
            }
            if let Some(r) = right {
                symbol.value = Some(text(r, self.src).to_owned());
            }
            // A following string literal is the attribute's doc (Sphinx convention).
            if let Some(next) = stmt.next_named_sibling()
                && next.kind() == "expression_statement"
                && let Some(s) = named_children(next).into_iter().next()
                && s.kind() == "string"
            {
                symbol.doc = Some(strip_quotes(text(s, self.src)).trim().to_owned());
            }
            let index = self.push(symbol);
            if let Some(r) = right {
                self.body(r, Some(index));
            }
        } else {
            self.body(assign, parent);
        }
    }

    /// `self.x = ...` in `__init__` declares instance fields.
    fn instance_fields(&mut self, body: Node, class: Option<usize>, prefix: &str) {
        let Some(class) = class else { return };
        let existing: Vec<String> = self.facts.symbols.iter().filter(|s| s.parent == Some(class) && s.kind == "field").map(|s| s.name.clone()).collect();
        let mut seen = existing;
        let mut stack = vec![body];
        while let Some(current) = stack.pop() {
            if current.kind() == "assignment"
                && let Some(left) = field(current, "left")
                && left.kind() == "attribute"
                && field_text(left, "object", self.src) == Some("self")
                && let Some(attr) = field_text(left, "attribute", self.src)
                && !seen.iter().any(|s| s == attr)
            {
                let stmt = current.parent().unwrap_or(current);
                let mut f = new_symbol("field", attr, format!("{prefix}{attr}"), stmt, None, self.src);
                f.parent = Some(class);
                if let Some(t) = field(current, "type") {
                    f.type_text = Some(text(t, self.src).to_owned());
                    f.type_names = type_names(t, self.src);
                }
                f.value = field(current, "right").map(|r| text(r, self.src).to_owned());
                seen.push(attr.to_owned());
                self.push(f);
            }
            if matches!(current.kind(), "function_definition" | "class_definition") && current.id() != body.id() {
                continue;
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn body(&mut self, node: Node, from: Option<usize>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "call" => self.call(current, from),
                "raise_statement" => {
                    if let Some(expr) = named_children(current).into_iter().next() {
                        let target = if expr.kind() == "call" { field(expr, "function") } else { Some(expr) };
                        if let Some(t) = target {
                            let (n, q) = dotted(t, self.src);
                            push_ref(&mut self.facts.refs, from, RefKind::Throw, &n, q, Vec::new(), current);
                        }
                    }
                }
                "assignment" | "augmented_assignment" => {
                    if let (Some(from), Some(left)) = (from, field(current, "left"))
                        && left.kind() == "identifier"
                    {
                        let declared = field(current, "type").and_then(|t| type_names(t, self.src).into_iter().next());
                        let inferred = field(current, "right").filter(|r| r.kind() == "call").and_then(|r| field(r, "function")).map(|f| text(f, self.src).rsplit('.').next().unwrap_or("").to_owned());
                        if let Some(type_name) = declared.or(inferred).filter(|t| !t.is_empty()) {
                            self.facts.locals.push(LocalFact { from, name: text(left, self.src).to_owned(), type_name });
                        }
                    }
                    if let Some(left) = field(current, "left") {
                        match left.kind() {
                            "attribute" if field_text(left, "object", self.src) == Some("self") => {
                                let attr = field_text(left, "attribute", self.src).unwrap_or("");
                                push_ref(&mut self.facts.refs, from, RefKind::Write, attr, Some("self".into()), Vec::new(), current);
                            }
                            "identifier" => push_ref(&mut self.facts.refs, from, RefKind::Write, text(left, self.src), None, Vec::new(), current),
                            _ => {}
                        }
                    }
                    if let Some(right) = field(current, "right") {
                        stack.push(right);
                    }
                    continue;
                }
                "attribute" => {
                    let is_callee = current.parent().is_some_and(|p| p.kind() == "call" && field(p, "function").is_some_and(|f| f.id() == current.id()));
                    if !is_callee && field_text(current, "object", self.src) == Some("self") {
                        let attr = field_text(current, "attribute", self.src).unwrap_or("");
                        push_ref(&mut self.facts.refs, from, RefKind::Read, attr, Some("self".into()), Vec::new(), current);
                        continue;
                    }
                }
                "subscript" => {
                    // `os.environ["X"]` (API name).
                    if let (Some(value), Some(sub)) = (field(current, "value"), field(current, "subscript"))
                        && text(value, self.src).ends_with("environ")
                        && let Some(key) = string_content(sub, self.src)
                    {
                        push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), current);
                    }
                }
                "identifier" => {
                    let is_callee = current.parent().is_some_and(|p| p.kind() == "call" && field(p, "function").is_some_and(|f| f.id() == current.id()));
                    let is_kw = current.parent().is_some_and(|p| p.kind() == "keyword_argument" && field(p, "name").is_some_and(|n| n.id() == current.id()));
                    if !is_callee && !is_kw {
                        push_ref(&mut self.facts.refs, from, RefKind::Read, text(current, self.src), None, Vec::new(), current);
                    }
                }
                "function_definition" | "class_definition" | "lambda" if current.id() != node.id() => {
                    // Nested definitions: their references still belong to the enclosing symbol.
                }
                _ => {}
            }
            let mut kids = named_children(current);
            kids.reverse();
            stack.extend(kids);
        }
    }

    fn call(&mut self, node: Node, from: Option<usize>) {
        let Some(function) = field(node, "function") else { return };
        let arg_nodes: Vec<Node> = field(node, "arguments").map(|a| named_children(a).into_iter().filter(|c| c.kind() != "comment").collect()).unwrap_or_default();
        let args: Vec<String> = arg_nodes.iter().map(|a| text(*a, self.src).to_owned()).collect();
        let (name, qualifier) = dotted(function, self.src);
        if name.is_empty() {
            return;
        }
        // Environment reads: `os.environ.get("X")`, `os.getenv("X")` (API names).
        let q = qualifier.as_deref().unwrap_or("");
        if (name == "get" && q.ends_with("environ")) || (name == "getenv" && (q == "os" || q.is_empty())) {
            if let Some(key) = arg_nodes.first().and_then(|a| string_content(*a, self.src)) {
                push_ref(&mut self.facts.refs, from, RefKind::ReadConfig, &key, Some("env".into()), Vec::new(), node);
            }
            return;
        }
        // Subprocesses: `subprocess.run(["bash", "x.sh"])`, `os.system("./x.sh")`.
        if (q == "subprocess" && matches!(name.as_str(), "run" | "call" | "check_call" | "check_output" | "Popen")) || (q == "os" && matches!(name.as_str(), "system" | "execv" | "execvp")) {
            let mut stack = arg_nodes.clone();
            while let Some(arg) = stack.pop() {
                if let Some(lit) = string_content(arg, self.src) {
                    for word in lit.split_whitespace() {
                        if looks_like_script_path(word) {
                            self.facts.runs.push(RunFact { from, target: word.to_owned(), line: line(node) });
                        }
                    }
                } else if matches!(arg.kind(), "list" | "tuple") {
                    stack.extend(named_children(arg));
                }
            }
        }
        push_ref(&mut self.facts.refs, from, RefKind::Call, &name, qualifier, args, node);
    }
}

/// `a.b.c` -> (`c`, Some(`a.b`)); non-simple receivers become `Some("")`.
fn dotted(node: Node, src: &str) -> (String, Option<String>) {
    match node.kind() {
        "identifier" => (text(node, src).to_owned(), None),
        "attribute" => {
            let attr = field_text(node, "attribute", src).unwrap_or("").to_owned();
            let object = field(node, "object");
            let qualifier = object.map(|o| {
                let t = text(o, src);
                if matches!(o.kind(), "identifier" | "attribute") && !t.contains('(') {
                    t.to_owned()
                } else if o.kind() == "call" && field(o, "function").is_some_and(|f| text(f, src) == "super") {
                    // `super().m()`: the base class's member.
                    "super".to_owned()
                } else {
                    String::new()
                }
            });
            (attr, qualifier)
        }
        _ => (String::new(), None),
    }
}
