//! The in-memory code model of one indexed tree (a project root or a workspace clone): parsed facts
//! per file, the lookup indexes built from them, and the graph rows (nodes and edges) they produce.
//!
//! The graph in IronGraph is the source of truth for every query. This model is the resolver's
//! working set: it is rebuilt from the files on open (parsing is fast and parallel) and updated per
//! change, so cross-file references can be re-resolved without re-reading the graph.

use crate::facts::{FileFacts, Origin, RefKind, SymbolFact};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Kinds that are "symbols" (outline / search / repo map entries) and get the `Sym` label.
pub const SYMBOL_KINDS: &[&str] = &[
    "function", "method", "class", "struct", "trait", "interface", "enum", "type", "const", "static", "field", "variant", "variable",
    "component", "prop", "module", "macro", "test", "route", "command", "shell_function", "ipc_channel",
];

/// Graph label for a kind.
pub fn label_of(kind: &str) -> &'static str {
    match kind {
        "function" => "Function",
        "method" => "Method",
        "class" => "Class",
        "struct" => "Struct",
        "trait" => "Trait",
        "interface" => "Interface",
        "enum" => "Enum",
        "type" => "TypeAlias",
        "const" => "Const",
        "static" => "Static",
        "field" => "Field",
        "variant" => "Variant",
        "variable" => "Variable",
        "component" => "Component",
        "prop" => "Prop",
        "module" => "Module",
        "macro" => "Macro",
        "test" => "Test",
        "route" => "Route",
        "command" => "Command",
        "shell_function" => "ShellFunction",
        "ipc_channel" => "IpcChannel",
        "file" => "File",
        "directory" => "Directory",
        "repo" => "Repo",
        "package" => "Package",
        "dependency" => "Dependency",
        "external_symbol" => "ExternalSymbol",
        "env_var" => "EnvVar",
        "commit" => "Commit",
        "section" => "Section",
        _ => "Symbol",
    }
}

fn is_callable(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "component" | "macro" | "shell_function" | "test")
}

fn is_type(kind: &str) -> bool {
    matches!(kind, "class" | "struct" | "trait" | "interface" | "enum" | "type" | "component")
}

fn is_value(kind: &str) -> bool {
    matches!(kind, "const" | "static" | "variable" | "field")
}

/// HTML pages and CSS / SCSS / Less stylesheets (`lang-web`): one language family for name
/// resolution, resolved by [`Index::resolve_web`].
/// The package an import of a `language` file names, as that language writes packages: Python and
/// Rust the first segment (`os` of `os.path`, `serde` of `serde::Deserialize`), JavaScript and
/// TypeScript the npm name (`react`, `@scope/pkg` of `@scope/pkg/sub`), Go and Java the import path
/// or package as written (`database/sql`, `org.neo4j.driver`), C and C++ the header
/// (`sqlite3.h` of `<sqlite3.h>`). None for a relative import, the crate's own paths
/// (`crate::`, `self::`, `super::`) and languages whose imports name files (pages, stylesheets,
/// scripts, compose files).
pub fn imported_package(language: &str, module: &str) -> Option<String> {
    let module = module.trim();
    let package = match language {
        "python" => {
            if module.starts_with('.') {
                return None;
            }
            module.split('.').next().unwrap_or(module)
        }
        "rust" => {
            let first = module.split("::").next().unwrap_or(module);
            if matches!(first, "crate" | "self" | "super" | "Self") {
                return None;
            }
            first
        }
        "typescript" | "tsx" | "javascript" => {
            if module.starts_with('.') || module.starts_with('/') {
                return None;
            }
            match module.find('/') {
                Some(slash) if module.starts_with('@') => module[slash + 1..].find('/').map_or(module, |end| &module[..slash + 1 + end]),
                Some(slash) => &module[..slash],
                None => module,
            }
        }
        "go" | "java" => module,
        "c" | "cpp" => module.trim_start_matches('<').trim_end_matches('>'),
        _ => return None,
    };
    if package.is_empty() { None } else { Some(package.to_owned()) }
}

pub fn is_web(language: &str) -> bool {
    matches!(language, "html" | "css" | "scss" | "less")
}

fn is_stylesheet(language: &str) -> bool {
    matches!(language, "css" | "scss" | "less")
}

/// Imports that bring a stylesheet's rules, variables, mixins and keyframes into a page or another
/// stylesheet (`lang-web` import names: `<link href>`, `@import`, `@use`, `@forward`). `url()`,
/// `<a href>`, `<script src>`, images and the like import a file, never its names.
const STYLESHEET_IMPORTS: &[&str] = &["link.href", "@import", "@use", "@forward"];

/// When a page or stylesheet name found nothing in the file and its stylesheet chain.
#[derive(Clone, Copy)]
enum ProjectWide {
    /// Linked when every candidate is in one file.
    OneFile,
    /// The candidates nearest by directory.
    Nearest,
}

/// Whether the reference made by `from` in a page comes from CSS (a `<style>` block's rules,
/// constants and keyframes: `lang-web` places them under the block's `module` symbol, `type_text`
/// `style`) rather than from an HTML attribute.
fn in_style_block(file: &FileFacts, from: Option<usize>) -> bool {
    let mut current = from;
    while let Some(index) = current {
        let Some(symbol) = file.symbols.get(index) else { return false };
        if symbol.kind == "module" && symbol.type_text.as_deref() == Some("style") && symbol.qname.starts_with("<style>") {
            return true;
        }
        current = symbol.parent;
    }
    false
}

/// A graph node row.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRow {
    pub sid: String,
    pub kind: String,
    /// The file that defines it (empty for shared nodes).
    pub file: String,
    pub props: Map<String, Value>,
}

/// A graph edge row.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRow {
    pub rel: &'static str,
    pub from: String,
    pub to: String,
    pub props: Map<String, Value>,
}

/// One indexed tree.
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub root: PathBuf,
    pub files: BTreeMap<String, Arc<FileFacts>>,
    /// Public API of installed dependencies: `dep:<eco>:<name>` -> external symbols.
    pub externals: BTreeMap<String, Arc<ExternalApi>>,
    /// Git history: commits, and for each path the commits that modified it.
    pub commits: Vec<CommitFact>,
    pub history: HashMap<String, Vec<String>>,
}

/// One commit of the repository's history.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitFact {
    pub sha: String,
    pub author: String,
    pub time_ms: u64,
    pub subject: String,
}

pub fn commit_sid(sha: &str) -> String {
    format!("commit:{sha}")
}

/// Commit nodes (shared: written once, never owned by a file).
pub fn commit_nodes(model: &Model) -> Vec<NodeRow> {
    model
        .commits
        .iter()
        .map(|c| {
            let mut p = Map::new();
            p.insert("sid".into(), json!(commit_sid(&c.sha)));
            p.insert("kind".into(), json!("commit"));
            p.insert("name".into(), json!(c.subject));
            p.insert("qname".into(), json!(c.sha));
            p.insert("author".into(), json!(c.author));
            p.insert("time_ms".into(), json!(c.time_ms));
            NodeRow { sid: commit_sid(&c.sha), kind: "commit".into(), file: String::new(), props: p }
        })
        .collect()
}

/// Package node of a manifest (`package.json` name, `[package]` of Cargo.toml, `[project]` of
/// pyproject.toml).
pub fn package_sid(file: &FileFacts) -> Option<String> {
    let package = file.package.as_ref()?;
    let eco = match file.language.as_str() {
        "package.json" => "npm",
        "cargo.toml" => "cargo",
        "pyproject.toml" => "pypi",
        _ => return None,
    };
    Some(format!("pkg:{eco}:{}#{package}", file.path))
}

/// Exported API of one installed dependency (the offline docs layer).
#[derive(Debug, Clone, Default)]
pub struct ExternalApi {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub source_dir: String,
    pub symbols: Vec<ExternalSymbolFact>,
}

#[derive(Debug, Clone, Default)]
pub struct ExternalSymbolFact {
    pub kind: String,
    pub name: String,
    pub qname: String,
    pub signature: String,
    pub doc: Option<String>,
    /// Source file inside the package, relative to its source dir.
    pub path: String,
    pub line: u32,
}

pub fn file_sid(path: &str) -> String {
    format!("file:{path}")
}

pub fn dir_sid(path: &str) -> String {
    if path.is_empty() { "repo:".to_owned() } else { format!("dir:{path}") }
}

/// Stable id of a definition: `<prefix>:<path>#<qname>`, the prefix being its language's (from
/// the language plugin's `describe`: `rs`, `py`, `js`, ...).
pub fn symbol_sid(prefix: &str, path: &str, qname: &str) -> String {
    format!("{prefix}:{path}#{qname}")
}

/// The symbol-id prefix of a file's definitions.
pub fn sid_prefix(file: &FileFacts) -> &str {
    if file.sid_prefix.is_empty() { &file.language } else { &file.sid_prefix }
}

pub fn dep_sid(ecosystem: &str, name: &str) -> String {
    format!("dep:{ecosystem}:{name}")
}

pub fn env_sid(name: &str) -> String {
    format!("env:{name}")
}

pub fn ext_sid(ecosystem: &str, dep: &str, qname: &str) -> String {
    format!("ext:{ecosystem}:{dep}:{qname}")
}

/// Location of a symbol inside the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymRef<'a> {
    pub file: &'a str,
    pub index: usize,
}

/// Lookup indexes over a model, rebuilt per change (cheap: hash maps over names).
pub struct Index<'a> {
    model: &'a Model,
    by_name: HashMap<&'a str, Vec<SymRef<'a>>>,
    /// Owner type name -> member symbols (`Type::m`, `Class.m`).
    members: HashMap<(&'a str, &'a str), Vec<SymRef<'a>>>,
    /// Python dotted module suffix -> files.
    py_modules: HashMap<String, Vec<&'a str>>,
    /// Rust crate name (underscored) -> (crate dir, src dir).
    rust_crates: HashMap<String, String>,
    /// File -> (crate src dir, module path segments).
    rust_modules: HashMap<&'a str, (String, Vec<String>)>,
    /// (crate src dir, module path) -> file.
    rust_module_files: HashMap<(String, Vec<String>), &'a str>,
    /// Dependency name -> ecosystem (from manifests).
    deps: HashMap<String, String>,
    /// TS path aliases: (prefix, target dir prefix) from tsconfig `paths`.
    ts_aliases: Vec<(String, String)>,
    /// Go: package directory -> its non-test files.
    go_packages: HashMap<String, Vec<&'a str>>,
    /// Go: a package directory's last element -> the package directories with it.
    go_dirs: HashMap<String, Vec<String>>,
    /// Go: the module path of the root `go.mod`.
    go_module: Option<String>,
    /// Java: declared package -> files.
    java_packages: HashMap<String, Vec<&'a str>>,
    /// C / C++: file name -> files (include resolution).
    c_files: HashMap<&'a str, Vec<&'a str>>,
    routes: Vec<SymRef<'a>>,
    channels: HashMap<&'a str, Vec<SymRef<'a>>>,
    ext_by_name: HashMap<(&'a str, &'a str), Vec<String>>,
}

impl Model {
    pub fn new(root: PathBuf) -> Self {
        Self { root, files: BTreeMap::new(), externals: BTreeMap::new(), commits: Vec::new(), history: HashMap::new() }
    }

    pub fn symbol(&self, r: SymRef) -> Option<&SymbolFact> {
        self.files.get(r.file).and_then(|f| f.symbols.get(r.index))
    }

    pub fn sid_of(&self, r: SymRef) -> Option<String> {
        let file = self.files.get(r.file)?;
        let symbol = file.symbols.get(r.index)?;
        Some(symbol_sid(sid_prefix(file), &file.path, &symbol.qname))
    }
}

/// Owner (container) name of a symbol: class/impl type for members.
fn owner_of<'a>(file: &'a FileFacts, symbol: &'a SymbolFact) -> Option<&'a str> {
    if let Some(parent) = symbol.parent.and_then(|p| file.symbols.get(p))
        && is_type(&parent.kind)
    {
        return Some(parent.name.as_str());
    }
    // Rust impl methods (`Type::method`), C++ out-of-line members (`T::f`), Go methods
    // (`Type.Method`) declared apart from their type.
    let separator = if matches!(file.language.as_str(), "rust" | "c" | "cpp") { "::" } else { "." };
    let (owner, _) = symbol.qname.rsplit_once(separator)?;
    Some(owner.rsplit(separator).next().unwrap_or(owner))
}

impl<'a> Index<'a> {
    pub fn build(model: &'a Model) -> Self {
        let mut by_name: HashMap<&'a str, Vec<SymRef<'a>>> = HashMap::new();
        let mut members: HashMap<(&'a str, &'a str), Vec<SymRef<'a>>> = HashMap::new();
        let mut py_modules: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut routes = Vec::new();
        let mut channels: HashMap<&'a str, Vec<SymRef<'a>>> = HashMap::new();
        let mut deps: HashMap<String, String> = HashMap::new();
        let mut rust_crates: HashMap<String, String> = HashMap::new();
        for (path, file) in &model.files {
            for (index, symbol) in file.symbols.iter().enumerate() {
                let r = SymRef { file: path.as_str(), index };
                by_name.entry(symbol.name.as_str()).or_default().push(r);
                // Pages and stylesheets have no members (their names resolve by `resolve_web`).
                if !is_web(&file.language)
                    && matches!(symbol.kind.as_str(), "method" | "field" | "variant" | "function" | "const" | "prop")
                    && let Some(owner) = owner_of(file, symbol)
                {
                    members.entry((owner, symbol.name.as_str())).or_default().push(r);
                }
                match symbol.kind.as_str() {
                    "route" => routes.push(r),
                    "ipc_channel" => channels.entry(symbol.name.as_str()).or_default().push(r),
                    _ => {}
                }
            }
            if file.language == "python" {
                let module = path.trim_end_matches(".pyi").trim_end_matches(".py").trim_end_matches("/__init__");
                let segments: Vec<&str> = module.split('/').collect();
                for start in 0..segments.len() {
                    py_modules.entry(segments[start..].join(".")).or_default().push(path.as_str());
                }
            }
            for dep in &file.dependencies {
                deps.insert(dep.name.clone(), dep.ecosystem.clone());
                if dep.ecosystem == "cargo" {
                    deps.insert(dep.name.replace('-', "_"), dep.ecosystem.clone());
                }
            }
            if file.language == "cargo.toml"
                && let Some(package) = &file.package
            {
                let dir = path.rsplit_once('/').map(|(d, _)| d.to_owned()).unwrap_or_default();
                rust_crates.insert(package.replace('-', "_"), dir);
            }
        }
        // Rust module paths: files under `<crate>/src`.
        let mut rust_modules = HashMap::new();
        let mut rust_module_files = HashMap::new();
        for (path, file) in &model.files {
            if file.language != "rust" {
                continue;
            }
            if let Some((src_dir, module)) = rust_module_path(path) {
                rust_module_files.insert((src_dir.clone(), module.clone()), path.as_str());
                rust_modules.insert(path.as_str(), (src_dir, module));
            }
        }
        let mut ext_by_name: HashMap<(&'a str, &'a str), Vec<String>> = HashMap::new();
        for api in model.externals.values() {
            for symbol in &api.symbols {
                ext_by_name.entry((api.name.as_str(), symbol.name.as_str())).or_default().push(ext_sid(&api.ecosystem, &api.name, &symbol.qname));
            }
        }
        let ts_aliases = load_ts_aliases(&model.root);
        let mut go_packages: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut java_packages: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut c_files: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for (path, file) in &model.files {
            match file.language.as_str() {
                "go" if !path.ends_with("_test.go") => go_packages.entry(dir_of(path).to_owned()).or_default().push(path.as_str()),
                "java" => java_packages.entry(file.package.clone().unwrap_or_default()).or_default().push(path.as_str()),
                "c" | "cpp" => c_files.entry(path.rsplit('/').next().unwrap_or(path)).or_default().push(path.as_str()),
                _ => {}
            }
        }
        let go_module = load_go_module(&model.root);
        let mut go_dirs: HashMap<String, Vec<String>> = HashMap::new();
        for dir in go_packages.keys() {
            go_dirs.entry(dir.rsplit('/').next().unwrap_or(dir).to_owned()).or_default().push(dir.clone());
        }
        Index { model, by_name, members, py_modules, rust_crates, rust_modules, rust_module_files, deps, ts_aliases, go_packages, go_dirs, go_module, java_packages, c_files, routes, channels, ext_by_name }
    }
}

/// `<crate dir>/src/a/b.rs` -> (`<crate dir>/src`, [a, b]); lib.rs / main.rs / mod.rs map to their dir.
pub fn rust_module_path(path: &str) -> Option<(String, Vec<String>)> {
    let segments: Vec<&str> = path.split('/').collect();
    let src_index = segments.iter().rposition(|s| *s == "src")?;
    let src_dir = segments[..=src_index].join("/");
    let mut module: Vec<String> = segments[src_index + 1..].iter().map(|s| s.to_string()).collect();
    let last = module.pop()?;
    let stem = last.trim_end_matches(".rs");
    if !(module.is_empty() && (stem == "lib" || stem == "main")) && stem != "mod" {
        module.push(stem.to_owned());
    }
    if module.first().is_some_and(|m| m == "bin") {
        return None;
    }
    Some((src_dir, module))
}

/// The `module` path declared by the root go.mod.
fn load_go_module(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join("go.mod")).ok()?;
    raw.lines().find_map(|l| l.trim().strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_owned()).filter(|m| !m.is_empty())
}

/// Read `compilerOptions.paths` from the root tsconfig.json (JSON with comments).
fn load_ts_aliases(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in ["tsconfig.json", "jsconfig.json"] {
        let Ok(raw) = std::fs::read_to_string(root.join(name)) else { continue };
        let Ok(json) = serde_json::from_str::<Value>(&strip_json_comments(&raw)) else { continue };
        let options = &json["compilerOptions"];
        let base = options["baseUrl"].as_str().unwrap_or(".").trim_start_matches("./").trim_end_matches('/').to_owned();
        if let Some(paths) = options["paths"].as_object() {
            for (alias, targets) in paths {
                if let Some(target) = targets.as_array().and_then(|t| t.first()).and_then(|t| t.as_str()) {
                    let prefix = alias.trim_end_matches('*').to_owned();
                    let mut target = target.trim_end_matches('*').trim_start_matches("./").to_owned();
                    if !base.is_empty() && base != "." {
                        target = format!("{base}/{target}");
                    }
                    out.push((prefix, target));
                }
            }
        }
    }
    out
}

/// Remove `//` and `/* */` comments and trailing commas from JSONC (structural).
pub fn strip_json_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == ',' {
            // Drop trailing commas before } or ].
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Result of resolving one name.
#[derive(Debug, Clone, PartialEq)]
pub enum Target<'a> {
    Sym(SymRef<'a>),
    External(String),
}

pub struct Resolved<'a> {
    pub targets: Vec<Target<'a>>,
    /// heuristic | ambiguous
    pub confidence: &'static str,
}

impl<'a> Index<'a> {
    fn file(&self, path: &str) -> Option<&'a FileFacts> {
        self.model.files.get(path).map(|f| f.as_ref())
    }

    fn sym(&self, r: SymRef<'a>) -> &'a SymbolFact {
        &self.model.files[r.file].symbols[r.index]
    }

    /// Resolve a module specifier from `from_file` to repository files.
    pub fn module_files(&self, from_file: &FileFacts, module: &str) -> Vec<&'a str> {
        let lang = from_file.language.as_str();
        let dir = dir_of(&from_file.path);
        match lang {
            "typescript" | "tsx" | "javascript" | "shell" => {
                let mut bases = Vec::new();
                if lang == "shell" {
                    // Scripts are run from their own directory or from the repository root.
                    bases.push(normalize(&format!("{dir}/{module}")));
                    bases.push(normalize(module));
                } else if module.starts_with('.') {
                    bases.push(normalize(&format!("{dir}/{module}")));
                } else if module.starts_with('/') {
                    bases.push(normalize(module));
                } else {
                    for (alias, target) in &self.ts_aliases {
                        if let Some(rest) = module.strip_prefix(alias.as_str()) {
                            bases.push(normalize(&format!("{target}{rest}")));
                        }
                    }
                }
                let mut out = Vec::new();
                for base in bases {
                    let candidates = [
                        base.clone(),
                        format!("{base}.ts"),
                        format!("{base}.tsx"),
                        format!("{base}.d.ts"),
                        format!("{base}.js"),
                        format!("{base}.jsx"),
                        format!("{base}.mjs"),
                        format!("{base}.cjs"),
                        format!("{base}/index.ts"),
                        format!("{base}/index.tsx"),
                        format!("{base}/index.js"),
                        format!("{base}/index.jsx"),
                    ];
                    // `./x.js` written for a `./x.ts` source (TS ESM convention).
                    let stripped = base.strip_suffix(".js").map(|b| vec![format!("{b}.ts"), format!("{b}.tsx")]).unwrap_or_default();
                    for candidate in candidates.iter().chain(stripped.iter()) {
                        if let Some((key, _)) = self.model.files.get_key_value(candidate.as_str()) {
                            out.push(key.as_str());
                            return out;
                        }
                    }
                }
                out
            }
            "python" => {
                let dots = module.chars().take_while(|c| *c == '.').count();
                if dots > 0 {
                    let mut base: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
                    for _ in 1..dots {
                        base.pop();
                    }
                    let rest = &module[dots..];
                    let mut path = base.join("/");
                    if !rest.is_empty() {
                        if !path.is_empty() {
                            path.push('/');
                        }
                        path.push_str(&rest.replace('.', "/"));
                    }
                    for candidate in [format!("{path}.py"), format!("{path}/__init__.py"), format!("{path}.pyi")] {
                        if let Some((key, _)) = self.model.files.get_key_value(candidate.trim_start_matches('/')) {
                            return vec![key.as_str()];
                        }
                    }
                    return Vec::new();
                }
                match self.py_modules.get(module) {
                    Some(files) if files.len() == 1 => files.clone(),
                    Some(files) => {
                        // Prefer the candidate sharing the longest directory prefix with the importer.
                        let best = files.iter().max_by_key(|f| common_prefix(f, &from_file.path)).copied();
                        best.into_iter().collect()
                    }
                    None => Vec::new(),
                }
            }
            "rust" => {
                let Some((src_dir, current)) = self.rust_modules.get(from_file.path.as_str()) else { return Vec::new() };
                let segments: Vec<&str> = module.split("::").filter(|s| !s.is_empty()).collect();
                let (crate_src, mut path): (String, Vec<String>) = match segments.first() {
                    Some(&"crate") => (src_dir.clone(), Vec::new()),
                    Some(&"self") => (src_dir.clone(), current.clone()),
                    Some(&"super") => {
                        let mut p = current.clone();
                        p.pop();
                        (src_dir.clone(), p)
                    }
                    Some(first) => match self.rust_crates.get(*first) {
                        Some(crate_dir) => (if crate_dir.is_empty() { "src".to_owned() } else { format!("{crate_dir}/src") }, Vec::new()),
                        // A sibling module of the current one (`use db::x` in 2018 edition with `mod db`).
                        None => (src_dir.clone(), {
                            let mut p = current.clone();
                            p.push(first.to_string());
                            p
                        }),
                    },
                    None => return Vec::new(),
                };
                for seg in segments.iter().skip(1) {
                    match *seg {
                        "super" => {
                            path.pop();
                        }
                        "self" => {}
                        s => path.push(s.to_owned()),
                    }
                }
                // Longest module prefix that is a file (inline modules live inside it).
                let mut probe = path.clone();
                loop {
                    if let Some(file) = self.rust_module_files.get(&(crate_src.clone(), probe.clone())) {
                        return vec![*file];
                    }
                    if probe.pop().is_none() {
                        return Vec::new();
                    }
                }
            }
            "go" => {
                // An import path names a package directory: below the root go.mod's module path,
                // or else the longest repository directory the path ends with.
                if let Some(rest) = self.go_module.as_deref().and_then(|m| if module == m { Some("") } else { module.strip_prefix(m).and_then(|r| r.strip_prefix('/')) }) {
                    return self.go_packages.get(rest).cloned().unwrap_or_default();
                }
                let last = module.rsplit('/').next().unwrap_or(module);
                let best = self
                    .go_dirs
                    .get(last)
                    .and_then(|dirs| dirs.iter().filter(|d| !d.is_empty() && (module == d.as_str() || (module.ends_with(d.as_str()) && module[..module.len() - d.len()].ends_with('/')))).max_by_key(|d| d.len()));
                best.and_then(|d| self.go_packages.get(d)).cloned().unwrap_or_default()
            }
            "java" => {
                // A package, or a class of a package (`a.b.C` of a static or nested import), by the
                // Java rule that a top-level public class `C` lives in `C.java`.
                if let Some(files) = self.java_packages.get(module) {
                    return files.clone();
                }
                let Some((package, class)) = module.rsplit_once('.') else { return Vec::new() };
                let class_file = format!("{class}.java");
                self.java_packages.get(package).map(|files| files.iter().copied().filter(|f| f.rsplit('/').next() == Some(class_file.as_str())).collect()).unwrap_or_default()
            }
            "c" | "cpp" => {
                // `#include "x.h"` searches the including file's directory first; then (both forms)
                // the repository path or the files whose path ends with it (include directories).
                let (path, quoted) = match module.strip_prefix('<').and_then(|m| m.strip_suffix('>')) {
                    Some(p) => (p, false),
                    None => (module, true),
                };
                if quoted
                    && let Some((key, _)) = self.model.files.get_key_value(normalize(&format!("{dir}/{path}")).as_str())
                {
                    return vec![key.as_str()];
                }
                if let Some((key, _)) = self.model.files.get_key_value(normalize(path).as_str()) {
                    return vec![key.as_str()];
                }
                if path.split('/').any(|s| s == "..") {
                    return Vec::new();
                }
                let name = path.rsplit('/').next().unwrap_or(path);
                let suffix = format!("/{path}");
                let candidates: Vec<&'a str> = self.c_files.get(name).map(|v| v.iter().copied().filter(|f| f.ends_with(&suffix)).collect()).unwrap_or_default();
                let best = candidates.iter().max_by_key(|f| common_prefix(f, &from_file.path)).copied();
                best.into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Top-level (or inline-module) definitions named `name` in `file`, following re-exports.
    fn defined_in(&self, file: &'a str, name: &str, depth: u32) -> Vec<SymRef<'a>> {
        let Some(facts) = self.file(file) else { return Vec::new() };
        let mut out: Vec<SymRef<'a>> = facts
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name == name && (s.parent.is_none() || facts.symbols.get(s.parent.unwrap_or(0)).is_some_and(|p| p.kind == "module")) && s.kind != "route" && s.kind != "test")
            .map(|(i, _)| SymRef { file, index: i })
            .collect();
        // Go and Java imports never re-export.
        if out.is_empty() && depth < 6 && !matches!(facts.language.as_str(), "go" | "java") {
            // `export { x } from './y'`, `export * from './y'`, `from .y import x` re-exports,
            // Rust `pub use` re-exports, C / C++ declarations reached through `#include`.
            for import in &facts.imports {
                if import.local == name || import.name == "*" && import.local == "*" {
                    let wanted = if import.name == "*" { name } else { import.name.as_str() };
                    for target in self.module_files(facts, &import.module) {
                        if target != file {
                            out.extend(self.defined_in(target, wanted, depth + 1));
                        }
                    }
                    if !out.is_empty() {
                        break;
                    }
                }
            }
            // `export default X` / `default=X`.
            if out.is_empty() && name == "default" {
                for export in &facts.exports {
                    if let Some(real) = export.strip_prefix("default=") {
                        out.extend(self.defined_in(file, real, depth + 1));
                    }
                }
            }
        }
        out
    }

    /// Packages the code imports from outside the repository ([`imported_package`] of every
    /// import no repository file answers) and the dependencies the manifests declare: what the
    /// project status reports as its `packages`.
    pub fn packages(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for file in self.model.files.values() {
            out.extend(file.dependencies.iter().map(|dep| dep.name.clone()));
            for import in &file.imports {
                if let Some(package) = imported_package(&file.language, &import.module)
                    && !out.contains(&package)
                    && !self.in_repository(file, &package, &import.module)
                {
                    out.insert(package);
                }
            }
        }
        out
    }

    /// Whether an import of `file` (naming `package`) is answered by the repository. A Rust path
    /// resolves to its longest module prefix that is a file, the crate root at worst, so there
    /// the package itself must be a workspace crate or a module file beside the importing one.
    fn in_repository(&self, file: &FileFacts, package: &str, module: &str) -> bool {
        if file.language != "rust" {
            return !self.module_files(file, module).is_empty();
        }
        if self.rust_crates.contains_key(package) {
            return true;
        }
        let Some((src_dir, current)) = self.rust_modules.get(file.path.as_str()) else { return false };
        let mut sibling = current.clone();
        sibling.push(package.to_owned());
        self.rust_module_files.contains_key(&(src_dir.clone(), sibling))
    }

    /// External package that a module specifier names, when it is a declared dependency.
    fn external_package(&self, lang: &str, module: &str) -> Option<String> {
        let package = match lang {
            "rust" => module.split("::").next().unwrap_or(module).to_owned(),
            "python" => module.split('.').next().unwrap_or(module).to_owned(),
            _ => {
                if module.starts_with('.') || module.starts_with('/') {
                    return None;
                }
                let mut parts = module.split('/');
                let first = parts.next().unwrap_or("");
                if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or("")) } else { first.to_owned() }
            }
        };
        if self.deps.contains_key(&package) {
            return Some(package);
        }
        // Python import names often differ only by `-`/`_`.
        let alt = package.replace('_', "-");
        if self.deps.contains_key(&alt) { Some(alt) } else { None }
    }

    fn external_symbol(&self, lang: &str, module: &str, name: &str) -> Option<String> {
        let package = self.external_package(lang, module)?;
        let candidates = [package.clone(), package.replace('-', "_")];
        for p in candidates {
            if let Some(sids) = self.ext_by_name.get(&(p.as_str(), name)) {
                return sids.first().cloned();
            }
        }
        None
    }

    /// Resolve a reference made in `file` by the symbol `from`.
    pub fn resolve(&self, file: &'a FileFacts, from: Option<usize>, kind: RefKind, name: &str, qualifier: Option<&str>) -> Resolved<'a> {
        self.resolve_with(file, from, kind, name, qualifier, None)
    }

    /// Methods named `name` of the type(s) a type name resolves to (and their bases).
    fn members_of_type(&self, file: &'a FileFacts, from: Option<usize>, type_name: &str, name: &str) -> Vec<SymRef<'a>> {
        let mut owners: Vec<String> = vec![type_name.to_owned()];
        for t in self.resolve(file, from, RefKind::Instantiate, type_name, None).targets {
            if let Target::Sym(r) = t {
                owners.push(self.sym(r).name.clone());
            }
        }
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(owner) = owners.pop() {
            if !visited.insert(owner.clone()) {
                continue;
            }
            if let Some(found) = self.members.get(&(owner.as_str(), name))
                && !found.is_empty()
            {
                return found.clone();
            }
            if let Some(types) = self.by_name.get(owner.as_str()) {
                for t in types {
                    let s = self.sym(*t);
                    if is_type(&s.kind) {
                        owners.extend(s.extends.iter().chain(s.implements.iter()).cloned());
                    }
                }
            }
        }
        Vec::new()
    }

    /// [`Index::resolve`] with the argument count of a call (arity filter for untyped receivers).
    pub fn resolve_with(&self, file: &'a FileFacts, from: Option<usize>, kind: RefKind, name: &str, qualifier: Option<&str>, argc: Option<usize>) -> Resolved<'a> {
        if is_web(&file.language) {
            return self.resolve_web(file, from, kind, name, qualifier);
        }
        let none = Resolved { targets: Vec::new(), confidence: "heuristic" };
        let accept = |s: &SymbolFact| -> bool {
            match kind {
                RefKind::Call => is_callable(&s.kind) || (matches!(s.kind.as_str(), "class" | "struct")),
                RefKind::UseHook => is_callable(&s.kind),
                RefKind::Instantiate | RefKind::Throw => is_type(&s.kind) || s.kind == "function",
                RefKind::Render => matches!(s.kind.as_str(), "component" | "function" | "class"),
                RefKind::Read | RefKind::Write => is_value(&s.kind) || s.kind == "prop",
                RefKind::ReadConfig => false,
                RefKind::Mention => true,
            }
        };
        if kind == RefKind::Mention {
            return self.resolve_mention(name);
        }
        let lang = file.language.as_str();
        let c_family = matches!(lang, "c" | "cpp");
        // Rust macros are invoked with `!`; a C / C++ function-like macro is invoked like a function.
        let macro_call = qualifier == Some("!");
        let accept_kind = |s: &SymbolFact| if macro_call { s.kind == "macro" } else { (s.kind != "macro" || c_family) && accept(s) };
        let from_symbol = from.and_then(|i| file.symbols.get(i));
        let one = |targets: Vec<SymRef<'a>>| -> Resolved<'a> {
            let targets = self.prefer_definitions(targets);
            let confidence = if targets.len() > 1 { "ambiguous" } else { "heuristic" };
            Resolved { targets: targets.into_iter().map(Target::Sym).collect(), confidence }
        };
        let path: &'a str = self.model.files.get_key_value(file.path.as_str()).map(|(k, _)| k.as_str()).unwrap_or("");

        // 1. Receiver is the current object: members of the enclosing type (and its bases).
        if let Some(q) = qualifier
            && matches!(q, "self" | "this" | "Self" | "cls" | "super")
        {
            let mut owners: Vec<&str> = Vec::new();
            if let Some(sym) = from_symbol {
                if let Some(o) = owner_of(file, sym) {
                    owners.push(o);
                }
                // Fields / props written from a method belong to the method's owner.
                if let Some(parent) = sym.parent.and_then(|p| file.symbols.get(p))
                    && let Some(o) = owner_of(file, parent)
                {
                    owners.push(o);
                }
            }
            let mut visited: HashSet<&str> = HashSet::new();
            // `super`: the enclosing type's own members are skipped; its bases are searched.
            let skip: HashSet<&str> = if q == "super" { owners.iter().copied().collect() } else { HashSet::new() };
            while let Some(owner) = owners.pop() {
                if !visited.insert(owner) {
                    continue;
                }
                if !skip.contains(owner)
                    && let Some(found) = self.members.get(&(owner, name))
                {
                    let found: Vec<SymRef> = found.iter().copied().filter(|r| accept_kind(self.sym(*r)) || matches!(kind, RefKind::Read | RefKind::Write)).collect();
                    // Prefer the same file (impl blocks / class bodies are usually local).
                    let local: Vec<SymRef> = found.iter().copied().filter(|r| r.file == path).collect();
                    let chosen = if local.is_empty() { found } else { local };
                    if !chosen.is_empty() {
                        return one(chosen);
                    }
                }
                // Walk base classes by name.
                if let Some(types) = self.by_name.get(owner) {
                    for t in types {
                        let s = self.sym(*t);
                        if is_type(&s.kind) {
                            for base in s.extends.iter().chain(s.implements.iter()) {
                                owners.push(base.as_str());
                            }
                        }
                    }
                }
            }
            return none;
        }

        // 2. Qualified by an imported module / namespace or a type path.
        if let Some(q) = qualifier.filter(|q| !q.is_empty() && *q != "!") {
            // C / C++ `ns::f`, `T::f`, `E::V`: members of the innermost scope (a class, a namespace
            // or an enum: their members are keyed by owner name). A scope never names an object.
            if c_family && let Some(scope) = q.strip_suffix("::") {
                let last = scope.rsplit("::").next().unwrap_or(scope);
                let found: Vec<SymRef> = self.members.get(&(last, name)).map(|v| v.iter().copied().filter(|r| accept_kind(self.sym(*r)) || kind == RefKind::Read).collect()).unwrap_or_default();
                if found.is_empty() {
                    return none;
                }
                let local: Vec<SymRef> = found.iter().copied().filter(|r| r.file == path).collect();
                return one(if local.is_empty() { found } else { local });
            }
            let head = q.split(['.', ':']).next().unwrap_or(q);
            // A namespace or module import (`import * as api`, `import os`, `use crate::db`).
            for import in file.imports.iter().filter(|i| i.local == head || i.local == q) {
                let module = if import.name == "*" || file.language == "rust" && import.local == head {
                    if file.language == "rust" && import.name != "*" { format!("{}::{}", import.module, import.name) } else { import.module.clone() }
                } else {
                    continue;
                };
                let rest = q.strip_prefix(head).unwrap_or("").trim_start_matches(['.', ':']);
                let module = if rest.is_empty() {
                    module
                } else if file.language == "rust" {
                    format!("{module}::{}", rest.replace('.', "::"))
                } else {
                    format!("{module}.{rest}")
                };
                let files = self.module_files(file, &module);
                let found: Vec<SymRef> = files.iter().flat_map(|f| self.defined_in(f, name, 0)).filter(|r| accept_kind(self.sym(*r))).collect();
                if !found.is_empty() {
                    return one(found);
                }
                if let Some(ext) = self.external_symbol(&file.language, &module, name) {
                    return Resolved { targets: vec![Target::External(ext)], confidence: "heuristic" };
                }
            }
            // Rust paths: `crate::a::f`, `super::f`, `Type::method`, `module::f`.
            if file.language == "rust" {
                let segments: Vec<&str> = q.split("::").collect();
                let last = segments.last().copied().unwrap_or(q);
                // Type-qualified: members of a type named `last`.
                if last.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && let Some(found) = self.members.get(&(last, name))
                {
                    let found: Vec<SymRef> = found.iter().copied().filter(|r| accept_kind(self.sym(*r)) || kind == RefKind::Read).collect();
                    if !found.is_empty() {
                        let local: Vec<SymRef> = found.iter().copied().filter(|r| r.file == path).collect();
                        return one(if local.is_empty() { found } else { local });
                    }
                }
                {
                    // `crate::a`, `super::a`, a workspace crate, or a child / sibling module path.
                    let files = self.module_files(file, q);
                    let found: Vec<SymRef> = files.iter().flat_map(|f| self.defined_in(f, name, 0)).filter(|r| accept_kind(self.sym(*r))).collect();
                    if !found.is_empty() {
                        return one(found);
                    }
                }
                if let Some(ext) = self.external_symbol("rust", q, name) {
                    return Resolved { targets: vec![Target::External(ext)], confidence: "heuristic" };
                }
            }
            // `self.field.m()` / `this.field.m()`: the field's declared type.
            if let Some(rest) = q.strip_prefix("self.").or_else(|| q.strip_prefix("this."))
                && !rest.contains('.')
                && let Some(sym) = from_symbol
                && let Some(owner) = owner_of(file, sym)
                && let Some(fields) = self.members.get(&(owner, rest))
            {
                for f in fields.clone() {
                    for type_name in self.sym(f).type_names.clone() {
                        let found: Vec<SymRef> = self.members_of_type(file, from, &type_name, name).into_iter().filter(|r| accept_kind(self.sym(*r))).collect();
                        if !found.is_empty() {
                            return one(found);
                        }
                    }
                }
            }
            // A local whose type its declaration names (`let s = Store::new(); s.save()`), of the
            // definition or of the one enclosing it (a closure-bodied subtest sees its test's locals).
            if let Some(local) = local_of(file, from, head) {
                let found: Vec<SymRef> = self.members_of_type(file, from, &local.type_name, name).into_iter().filter(|r| accept_kind(self.sym(*r))).collect();
                if !found.is_empty() {
                    return one(found);
                }
            }
            // A receiver typed by a parameter of the enclosing function (`fn f(s: &Store) { s.save() }`).
            if let Some(sym) = from_symbol
                && let Some(param) = sym.params.iter().find(|p| p.name == head)
            {
                for type_name in &param.type_names {
                    if let Some(found) = self.members.get(&(type_name.as_str(), name)) {
                        let found: Vec<SymRef> = found.iter().copied().filter(|r| accept_kind(self.sym(*r))).collect();
                        if !found.is_empty() {
                            return one(found);
                        }
                    }
                }
            }
            // Java / C++: a field of the enclosing type named without `this` (`repo.save()`).
            if matches!(lang, "java" | "c" | "cpp")
                && !q.contains('.')
                && let Some(sym) = from_symbol
            {
                for f in self.enclosing_members(file, sym, head, &|s: &SymbolFact| s.kind == "field") {
                    for type_name in self.sym(f).type_names.clone() {
                        let found: Vec<SymRef> = self.members_of_type(file, from, &type_name, name).into_iter().filter(|r| accept_kind(self.sym(*r))).collect();
                        if !found.is_empty() {
                            return one(found);
                        }
                    }
                }
            }
            // A class / type name used as a qualifier (`Store.create()`, `Foo::new`).
            if head.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && let Some(found) = self.members.get(&(head, name))
            {
                let found: Vec<SymRef> = found.iter().copied().filter(|r| accept_kind(self.sym(*r))).collect();
                if !found.is_empty() {
                    return one(found);
                }
            }
            // Unknown receiver: only a project method that is the single one with this name and an
            // arity accepting the call's argument count is linked, marked `ambiguous` (the receiver's
            // type is unknown). Several candidates link none: name matching cannot choose.
            // In Go and Java a name bound by an import is a package or a class, never an object; in
            // statically typed languages a parameter or local with a declared type is not an
            // unknown receiver.
            let bound_by_import = matches!(lang, "go" | "java") && file.imports.iter().any(|i| i.local == head);
            let typed_receiver = matches!(lang, "go" | "java" | "c" | "cpp")
                && (from_symbol.is_some_and(|sym| sym.params.iter().any(|p| p.name == head && !p.type_names.is_empty())) || local_of(file, from, head).is_some());
            if matches!(kind, RefKind::Call) && !self.imported_external_head(file, head) && !bound_by_import && !typed_receiver {
                let candidates: Vec<SymRef> = self
                    .by_name
                    .get(name)
                    .map(|v| v.iter().copied().filter(|r| self.sym(*r).kind == "method" && argc.is_none_or(|n| arity_accepts(self.sym(*r), n))).collect())
                    .unwrap_or_default();
                if candidates.len() == 1 {
                    return Resolved { targets: vec![Target::Sym(candidates[0])], confidence: "ambiguous" };
                }
            }
            return none;
        }

        // 2b. Java / C++ bare names inside a type: its members and its bases' (implicit `this`, and
        // the class scope that C++ name lookup searches before the namespace).
        if matches!(lang, "java" | "c" | "cpp")
            && let Some(sym) = from_symbol
        {
            let found = self.enclosing_members(file, sym, name, &accept_kind);
            if !found.is_empty() {
                let local: Vec<SymRef> = found.iter().copied().filter(|r| r.file == path).collect();
                return one(if local.is_empty() { found } else { local });
            }
        }

        // 3. Bare names: imported bindings.
        if let Some(import) = file.imports.iter().find(|i| i.local == name) {
            let files = self.module_files(file, &import.module);
            let wanted = if import.name == "*" { name } else { import.name.as_str() };
            let found: Vec<SymRef> = files.iter().flat_map(|f| self.defined_in(f, wanted, 0)).filter(|r| accept_kind(self.sym(*r))).collect();
            if !found.is_empty() {
                return one(found);
            }
            // Java `import static a.b.C.m;`: a static member of class C.
            if lang == "java" {
                let class = import.module.rsplit('.').next().unwrap_or(&import.module);
                let found: Vec<SymRef> = self.members.get(&(class, wanted)).map(|v| v.iter().copied().filter(|r| files.contains(&r.file) && accept_kind(self.sym(*r))).collect()).unwrap_or_default();
                if !found.is_empty() {
                    return one(found);
                }
            }
            if let Some(ext) = self.external_symbol(&file.language, &import.module, wanted) {
                return Resolved { targets: vec![Target::External(ext)], confidence: "heuristic" };
            }
            if !files.is_empty() || self.external_package(&file.language, &import.module).is_some() {
                return none;
            }
        }
        // 4. Same file: innermost first (nested functions, then top level).
        let local: Vec<SymRef> = file
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name == name && accept_kind(s) && !matches!(s.kind.as_str(), "route" | "test"))
            .filter(|(_, s)| !matches!(kind, RefKind::Read | RefKind::Write) || s.parent.is_none() || file.symbols.get(s.parent.unwrap_or(0)).is_some_and(|p| p.kind == "module" || (p.kind == "enum" && s.kind == "const")))
            // A bare name is never a Go method (methods are always selected through a value).
            .filter(|(_, s)| s.kind != "method" || (s.parent.is_none() && lang != "go"))
            .map(|(i, _)| SymRef { file: path, index: i })
            .collect();
        if !local.is_empty() {
            return one(local);
        }
        // Shell: functions of sourced scripts.
        if file.language == "shell" {
            for import in &file.imports {
                for target in self.module_files(file, &import.module) {
                    let found = self.defined_in(target, name, 0);
                    if !found.is_empty() {
                        return one(found);
                    }
                }
            }
            return none;
        }
        // 5. Project-wide by name. Bare variable reads only resolve through imports or the same
        // file (a global name match on reads would mostly hit locals).
        // Rust, C / C++ and Go globals are read by bare name across files (Go within the package).
        if matches!(kind, RefKind::Read | RefKind::Write) && !matches!(lang, "rust" | "c" | "cpp" | "go") {
            return none;
        }
        // Go: a bare name is declared in the same package (directory) or a dot-imported one.
        let go_dirs: Option<Vec<&str>> = (lang == "go" && matches!(kind, RefKind::Call | RefKind::Read | RefKind::Write)).then(|| {
            let mut dirs = vec![dir_of(&file.path)];
            for import in file.imports.iter().filter(|i| i.local == "*") {
                dirs.extend(self.module_files(file, &import.module).into_iter().map(dir_of));
            }
            dirs
        });
        let global: Vec<SymRef> = self
            .by_name
            .get(name)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|r| {
                        let s = self.sym(*r);
                        accept_kind(s) && !matches!(s.kind.as_str(), "method" | "field" | "prop" | "route" | "test" | "variant")
                    })
                    .filter(|r| same_language_family(&self.model.files[r.file].language, &file.language))
                    .filter(|r| go_dirs.as_ref().is_none_or(|dirs| dirs.contains(&dir_of(r.file))))
                    .collect()
            })
            .unwrap_or_default();
        if global.len() > 1 {
            // Prefer candidates in the importer's directory tree.
            let best = global.iter().map(|r| common_prefix(r.file, &file.path)).max().unwrap_or(0);
            let near: Vec<SymRef> = global.iter().copied().filter(|r| common_prefix(r.file, &file.path) == best).collect();
            return one(near);
        }
        one(global)
    }

    /// The stylesheets a page or stylesheet brings in ([`STYLESHEET_IMPORTS`] of repository
    /// stylesheets, by the imports' `local` path), and the ones those bring in, nearest first.
    fn stylesheet_chain(&self, file: &'a FileFacts) -> Vec<&'a str> {
        let mut out: Vec<&'a str> = Vec::new();
        let mut visited: HashSet<&str> = HashSet::from([file.path.as_str()]);
        let mut queue: std::collections::VecDeque<&'a FileFacts> = std::collections::VecDeque::from([file]);
        while let Some(current) = queue.pop_front() {
            for import in current.imports.iter().filter(|i| STYLESHEET_IMPORTS.contains(&i.name.as_str())) {
                if let Some((path, target)) = self.model.files.get_key_value(import.local.as_str())
                    && is_stylesheet(&target.language)
                    && visited.insert(path.as_str())
                {
                    out.push(path.as_str());
                    queue.push_back(target.as_ref());
                }
            }
        }
        out
    }

    /// Resolve a name used in a page or a stylesheet (`lang-web` facts; README "Linking HTML to
    /// CSS"). A reference's name is exactly the name of the symbol it means, so resolution chooses
    /// among same-named definitions by where they are:
    ///
    /// - a class / id used in a page (`Instantiate` `.btn`, `#main`) and an `@extend` target are
    ///   the rule keys (kind `class`) named so in the file itself (its `<style>` blocks) and in
    ///   the stylesheets it brings in ([`Index::stylesheet_chain`]), all of them (every such rule
    ///   applies); when there are none, project-wide only when one file defines the key (several
    ///   files: the page brings none of them in, so nothing says which applies). An element name
    ///   is never resolved: element rules (`button`, kind `type`) are not linked from the page;
    /// - `--x`, `$x`, `@x` and keyframes names (read in a stylesheet or a `<style>` block) are the
    ///   constants of that name (keyframes: qname `@keyframes <name>`): in the file and its
    ///   stylesheet chain, else project-wide (nearest by directory, as Rust, Go and C globals are);
    /// - an id read by a page attribute (`for`, `href="#x"`, `aria-*`) is the page's own element
    ///   with that id;
    /// - a mixin call (`@include m`, Less `.m();`) is a mixin / function (or a Less rule used as a
    ///   mixin) of that name: in the file and its chain, else project-wide, nearest.
    pub fn resolve_web(&self, file: &'a FileFacts, from: Option<usize>, kind: RefKind, name: &str, qualifier: Option<&str>) -> Resolved<'a> {
        let none = Resolved { targets: Vec::new(), confidence: "heuristic" };
        if qualifier.is_some_and(|q| !q.is_empty()) || name.is_empty() {
            return none;
        }
        let path: &'a str = self.model.files.get_key_value(file.path.as_str()).map(|(k, _)| k.as_str()).unwrap_or("");
        let named = |test: &dyn Fn(&'a str, &SymbolFact) -> bool| -> Vec<SymRef<'a>> {
            self.by_name.get(name).map(|v| v.iter().copied().filter(|r| test(r.file, self.sym(*r))).collect()).unwrap_or_default()
        };
        // Same file and its stylesheet chain first; then every page and stylesheet of the project.
        let tiers = |test: &dyn Fn(&SymbolFact) -> bool, project_wide: ProjectWide| -> Resolved<'a> {
            let mut near: Vec<&'a str> = vec![path];
            near.extend(self.stylesheet_chain(file));
            let found = named(&|f, s| near.contains(&f) && test(s));
            if !found.is_empty() {
                return Resolved { targets: found.into_iter().map(Target::Sym).collect(), confidence: "heuristic" };
            }
            let global = named(&|f, s| is_web(&self.model.files[f].language) && test(s));
            let mut files: Vec<&str> = global.iter().map(|r| r.file).collect();
            files.sort_unstable();
            files.dedup();
            let chosen: Vec<SymRef<'a>> = match project_wide {
                ProjectWide::OneFile if files.len() == 1 => global,
                ProjectWide::OneFile => Vec::new(),
                ProjectWide::Nearest => {
                    let best = global.iter().map(|r| common_prefix(r.file, path)).max().unwrap_or(0);
                    global.into_iter().filter(|r| common_prefix(r.file, path) == best).collect()
                }
            };
            let mut chosen_files: Vec<&str> = chosen.iter().map(|r| r.file).collect();
            chosen_files.sort_unstable();
            chosen_files.dedup();
            let confidence = if chosen_files.len() > 1 { "ambiguous" } else { "heuristic" };
            Resolved { targets: chosen.into_iter().map(Target::Sym).collect(), confidence }
        };
        match kind {
            RefKind::Instantiate if name.starts_with(['.', '#', '%']) => tiers(&|s| s.kind == "class", ProjectWide::OneFile),
            RefKind::Read | RefKind::Write => {
                if name.starts_with("--") || name.starts_with('$') || name.starts_with('@') {
                    return tiers(&|s| s.kind == "const", ProjectWide::Nearest);
                }
                if is_stylesheet(&file.language) || in_style_block(file, from) {
                    let keyframes = format!("@keyframes {name}");
                    let nested = format!("/{keyframes}");
                    return tiers(&|s| s.kind == "const" && (s.qname == keyframes || s.qname.ends_with(&nested)), ProjectWide::Nearest);
                }
                // A page attribute naming an id of the same page.
                let id = format!("#{name}");
                Resolved { targets: named(&|f, s| f == path && s.kind == "variable" && s.qname == id).into_iter().map(Target::Sym).collect(), confidence: "heuristic" }
            }
            RefKind::Call => tiers(&|s| is_callable(&s.kind) || s.kind == "class", ProjectWide::Nearest),
            _ => none,
        }
    }

    /// A prose mention (`RefKind::Mention`): the one symbol of the project with that name, else the
    /// one with that qualified name (`Type::method`, `Class.method`); several or none link nothing.
    fn resolve_mention(&self, name: &str) -> Resolved<'a> {
        let none = Resolved { targets: Vec::new(), confidence: "heuristic" };
        if name.is_empty() {
            return none;
        }
        if let Some(found) = self.by_name.get(name) {
            return match found.as_slice() {
                [one] => Resolved { targets: vec![Target::Sym(*one)], confidence: "heuristic" },
                _ => none,
            };
        }
        let mut qualified: Vec<SymRef<'a>> = Vec::new();
        for (path, file) in &self.model.files {
            for (i, symbol) in file.symbols.iter().enumerate() {
                if symbol.qname == name {
                    qualified.push(SymRef { file: path.as_str(), index: i });
                }
            }
        }
        match qualified.as_slice() {
            [one] => Resolved { targets: vec![Target::Sym(*one)], confidence: "heuristic" },
            _ => none,
        }
    }

    /// Members named `name` of the type(s) enclosing `sym` and of their bases.
    fn enclosing_members(&self, file: &'a FileFacts, sym: &'a SymbolFact, name: &str, accept: &dyn Fn(&SymbolFact) -> bool) -> Vec<SymRef<'a>> {
        let mut owners: Vec<&str> = Vec::new();
        if is_type(&sym.kind) {
            owners.push(sym.name.as_str());
        }
        if let Some(o) = owner_of(file, sym) {
            owners.push(o);
        }
        // A nested type's members also see the outer type's.
        let mut parent = sym.parent.and_then(|p| file.symbols.get(p));
        while let Some(p) = parent {
            if let Some(o) = owner_of(file, p) {
                owners.push(o);
            }
            parent = p.parent.and_then(|pp| file.symbols.get(pp));
        }
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = owners.into_iter().collect();
        while let Some(owner) = queue.pop_front() {
            if !visited.insert(owner) {
                continue;
            }
            if let Some(found) = self.members.get(&(owner, name)) {
                let found: Vec<SymRef> = found.iter().copied().filter(|r| accept(self.sym(*r))).collect();
                if !found.is_empty() {
                    return found;
                }
            }
            if let Some(types) = self.by_name.get(owner) {
                for t in types {
                    let s = self.sym(*t);
                    if is_type(&s.kind) {
                        queue.extend(s.extends.iter().chain(s.implements.iter()).map(String::as_str));
                    }
                }
            }
        }
        Vec::new()
    }

    /// C / C++: a function declared in a header and defined in a source file is one function; when
    /// both resolve, the definitions (with a body) are kept.
    fn prefer_definitions(&self, targets: Vec<SymRef<'a>>) -> Vec<SymRef<'a>> {
        if targets.len() < 2 || !targets.iter().all(|r| matches!(self.model.files[r.file].language.as_str(), "c" | "cpp")) {
            return targets;
        }
        let defined: Vec<SymRef<'a>> = targets.iter().copied().filter(|r| !self.sym(*r).body_hash.is_empty()).collect();
        if defined.is_empty() || defined.len() == targets.len() { targets } else { defined }
    }

    fn imported_external_head(&self, file: &FileFacts, head: &str) -> bool {
        file.imports.iter().any(|i| i.local == head && self.external_package(&file.language, &i.module).is_some())
    }
}

/// The typed local `name` of the definition `from`, or of a definition enclosing it.
fn local_of<'f>(file: &'f FileFacts, from: Option<usize>, name: &str) -> Option<&'f crate::facts::LocalFact> {
    let mut current = from;
    while let Some(f) = current {
        if let Some(local) = file.locals.iter().rev().find(|l| l.from == f && l.name == name) {
            return Some(local);
        }
        current = file.symbols.get(f).and_then(|s| s.parent);
    }
    None
}

/// Whether a definition accepts `argc` positional / keyword arguments (structural arity).
fn arity_accepts(symbol: &SymbolFact, argc: usize) -> bool {
    if symbol.params.iter().any(|p| p.variadic) {
        return argc >= symbol.params.iter().filter(|p| !p.optional && !p.variadic).count();
    }
    let required = symbol.params.iter().filter(|p| !p.optional && p.default_value.is_none()).count();
    argc >= required && argc <= symbol.params.len()
}

fn same_language_family(a: &str, b: &str) -> bool {
    fn family(l: &str) -> &str {
        match l {
            "typescript" | "tsx" | "javascript" => "js",
            "c" | "cpp" => "c",
            "html" | "css" | "scss" | "less" => "web",
            other => other,
        }
    }
    family(a) == family(b)
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
}

// ------------------------------------------------------------------------------------------------
// Graph rows.

fn opt(value: &Option<String>) -> Value {
    value.as_ref().map(|v| Value::String(v.clone())).unwrap_or(Value::Null)
}

/// Node rows defined by one file: the File node and its symbols (parameters are on them).
pub fn file_nodes(file: &FileFacts) -> Vec<NodeRow> {
    let mut rows = Vec::new();
    let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
    let mut props = Map::new();
    props.insert("sid".into(), json!(file_sid(&file.path)));
    props.insert("kind".into(), json!("file"));
    props.insert("name".into(), json!(name));
    props.insert("qname".into(), json!(file.path));
    props.insert("path".into(), json!(file.path));
    props.insert("language".into(), json!(file.language));
    props.insert("content_hash".into(), json!(file.content_hash));
    props.insert("size".into(), json!(file.size));
    props.insert("start_line".into(), json!(1));
    props.insert("end_line".into(), json!(file.lines.max(1)));
    props.insert("package".into(), opt(&file.package));
    props.insert("parse_errors".into(), json!(file.parse_errors));
    // Which language plugin extracted the file, or why none did.
    let (extractor, version, note) = match &file.origin {
        Origin::Plugin { category, version } => (Some(category.as_str()), Some(version.as_str()), None),
        Origin::Failed { error, .. } => (None, None, Some(error.as_str())),
        Origin::Kept { reason, .. } => (None, None, Some(reason.as_str())),
        Origin::Uncovered => (None, None, None),
    };
    props.insert("extractor".into(), json!(extractor));
    props.insert("extractor_version".into(), json!(version));
    props.insert("note".into(), json!(note));
    if let Some(text) = &file.text {
        // The leading text of a file no parser covers: embedded by IronGraph, so search finds it.
        props.insert("text".into(), json!(text));
    }
    rows.push(NodeRow { sid: file_sid(&file.path), kind: "file".into(), file: file.path.clone(), props });
    for symbol in &file.symbols {
        let sid = symbol_sid(sid_prefix(file), &file.path, &symbol.qname);
        let mut p = Map::new();
        p.insert("sid".into(), json!(sid));
        p.insert("kind".into(), json!(symbol.kind));
        p.insert("name".into(), json!(symbol.name));
        p.insert("qname".into(), json!(symbol.qname));
        p.insert("path".into(), json!(file.path));
        p.insert("language".into(), json!(file.language));
        p.insert("signature".into(), json!(symbol.signature));
        p.insert("doc".into(), opt(&symbol.doc));
        p.insert("start_line".into(), json!(symbol.start_line));
        p.insert("end_line".into(), json!(symbol.end_line));
        p.insert("body_hash".into(), json!(symbol.body_hash));
        p.insert("return_type".into(), opt(&symbol.return_type));
        p.insert("type_text".into(), opt(&symbol.type_text));
        p.insert("exported".into(), json!(symbol.exported));
        p.insert("http_method".into(), opt(&symbol.http_method));
        p.insert("route_path".into(), opt(&symbol.route_path));
        p.insert("framework".into(), opt(&symbol.framework));
        // Parameters in order, one list per column ("" where a parameter has no type or default).
        let mut params: Vec<&crate::facts::ParamFact> = symbol.params.iter().collect();
        params.sort_by_key(|param| param.position);
        p.insert("param_names".into(), json!(params.iter().map(|param| param.name.as_str()).collect::<Vec<_>>()));
        p.insert("param_types".into(), json!(params.iter().map(|param| param.type_text.as_deref().unwrap_or("")).collect::<Vec<_>>()));
        p.insert("param_defaults".into(), json!(params.iter().map(|param| param.default_value.as_deref().unwrap_or("")).collect::<Vec<_>>()));
        rows.push(NodeRow { sid: sid.clone(), kind: symbol.kind.clone(), file: file.path.clone(), props: p });
    }
    for command in &file.commands {
        let sid = command_sid(file, &command.name);
        let mut p = Map::new();
        p.insert("sid".into(), json!(sid));
        p.insert("kind".into(), json!("command"));
        p.insert("name".into(), json!(command.name));
        p.insert("qname".into(), json!(format!("{}:{}", file.package.clone().unwrap_or_default(), command.name)));
        p.insert("path".into(), json!(file.path));
        p.insert("language".into(), json!(file.language));
        p.insert("signature".into(), json!(format!("{}: {}", command.name, command.command)));
        p.insert("start_line".into(), json!(command.line));
        p.insert("end_line".into(), json!(command.line));
        p.insert("entry_point".into(), opt(&command.entry_point));
        rows.push(NodeRow { sid, kind: "command".into(), file: file.path.clone(), props: p });
    }
    if let Some(sid) = package_sid(file) {
        let mut p = Map::new();
        p.insert("sid".into(), json!(sid));
        p.insert("kind".into(), json!("package"));
        p.insert("name".into(), json!(file.package));
        p.insert("qname".into(), json!(file.package));
        p.insert("path".into(), json!(file.path));
        p.insert("language".into(), json!(file.language));
        p.insert("start_line".into(), json!(1));
        p.insert("end_line".into(), json!(file.lines.max(1)));
        rows.push(NodeRow { sid, kind: "package".into(), file: file.path.clone(), props: p });
    }
    rows
}

/// Stable id of a manifest's command: `<prefix>:<path>#<name>` (`npm:package.json#build`).
pub fn command_sid(file: &FileFacts, name: &str) -> String {
    let prefix = if file.sid_prefix.is_empty() { "cmd" } else { &file.sid_prefix };
    format!("{prefix}:{}#{name}", file.path)
}

/// Shared nodes referenced by a file (dependencies, environment variables).
pub fn shared_nodes(file: &FileFacts) -> Vec<NodeRow> {
    let mut rows = Vec::new();
    for dep in &file.dependencies {
        let sid = dep_sid(&dep.ecosystem, &dep.name);
        let mut p = Map::new();
        p.insert("sid".into(), json!(sid));
        p.insert("kind".into(), json!("dependency"));
        p.insert("name".into(), json!(dep.name));
        p.insert("qname".into(), json!(format!("{}:{}", dep.ecosystem, dep.name)));
        p.insert("ecosystem".into(), json!(dep.ecosystem));
        p.insert("installed_version".into(), opt(&dep.installed_version));
        p.insert("source_dir".into(), opt(&dep.source_dir));
        rows.push(NodeRow { sid, kind: "dependency".into(), file: String::new(), props: p });
    }
    let mut seen = HashSet::new();
    for r in &file.refs {
        if r.kind == RefKind::ReadConfig && seen.insert(r.target.name.clone()) {
            let sid = env_sid(&r.target.name);
            let mut p = Map::new();
            p.insert("sid".into(), json!(sid));
            p.insert("kind".into(), json!("env_var"));
            p.insert("name".into(), json!(r.target.name));
            p.insert("qname".into(), json!(r.target.name));
            rows.push(NodeRow { sid, kind: "env_var".into(), file: String::new(), props: p });
        }
    }
    rows
}

/// Every directory on the way to each file, plus the repo node.
pub fn directory_nodes(model: &Model) -> (Vec<NodeRow>, Vec<EdgeRow>) {
    let mut dirs: BTreeMap<String, ()> = BTreeMap::new();
    let mut edges = Vec::new();
    for path in model.files.keys() {
        let mut dir = dir_of(path).to_owned();
        edges.push(EdgeRow { rel: "CONTAINS", from: dir_sid(&dir), to: file_sid(path), props: Map::new() });
        loop {
            if dirs.insert(dir.clone(), ()).is_some() {
                break;
            }
            if dir.is_empty() {
                break;
            }
            let parent = dir_of(&dir).to_owned();
            edges.push(EdgeRow { rel: "CONTAINS", from: dir_sid(&parent), to: dir_sid(&dir), props: Map::new() });
            dir = parent;
        }
    }
    let repo_name = model.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nodes = dirs
        .keys()
        .map(|dir| {
            let mut p = Map::new();
            let kind = if dir.is_empty() { "repo" } else { "directory" };
            p.insert("sid".into(), json!(dir_sid(dir)));
            p.insert("kind".into(), json!(kind));
            p.insert("name".into(), json!(if dir.is_empty() { repo_name.clone() } else { dir.rsplit('/').next().unwrap_or(dir).to_owned() }));
            p.insert("qname".into(), json!(dir));
            p.insert("path".into(), json!(dir));
            NodeRow { sid: dir_sid(dir), kind: kind.into(), file: String::new(), props: p }
        })
        .collect();
    (nodes, edges)
}

/// Every edge whose source node is defined by `file` (the file's outgoing edge set).
pub fn file_edges(index: &Index, file: &FileFacts) -> Vec<EdgeRow> {
    let mut edges: Vec<EdgeRow> = Vec::new();
    let path = file.path.as_str();
    let fsid = file_sid(path);
    let sid_of = |i: usize| symbol_sid(sid_prefix(file), path, &file.symbols[i].qname);
    let target_sid = |t: &Target| -> Option<String> {
        match t {
            Target::Sym(r) => index.model.sid_of(*r),
            Target::External(sid) => Some(sid.clone()),
        }
    };
    let props = |pairs: &[(&str, Value)]| -> Map<String, Value> { pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect() };

    for (i, symbol) in file.symbols.iter().enumerate() {
        let sid = sid_of(i);
        edges.push(EdgeRow { rel: "DEFINES", from: fsid.clone(), to: sid.clone(), props: Map::new() });
        if let Some(parent) = symbol.parent {
            edges.push(EdgeRow { rel: "CONTAINS", from: sid_of(parent), to: sid.clone(), props: Map::new() });
        }
        // TAKES: the types its parameters name (one edge per type; the sort and dedup below).
        for param in &symbol.params {
            for type_name in &param.type_names {
                for t in index.resolve(file, Some(i), RefKind::Instantiate, type_name, None).targets {
                    if let Some(to) = target_sid(&t)
                        && to != sid
                    {
                        edges.push(EdgeRow { rel: "TAKES", from: sid.clone(), to, props: Map::new() });
                    }
                }
            }
        }
        for type_name in &symbol.type_names {
            for t in index.resolve(file, Some(i), RefKind::Instantiate, type_name, None).targets {
                if let Some(to) = target_sid(&t)
                    && to != sid
                {
                    edges.push(EdgeRow { rel: "OF_TYPE", from: sid.clone(), to, props: Map::new() });
                }
            }
        }
        for type_name in &symbol.return_type_names {
            for t in index.resolve(file, Some(i), RefKind::Instantiate, type_name, None).targets {
                if let Some(to) = target_sid(&t) {
                    edges.push(EdgeRow { rel: "RETURNS", from: sid.clone(), to, props: Map::new() });
                }
            }
        }
        for (rel, names) in [("EXTENDS", &symbol.extends), ("IMPLEMENTS", &symbol.implements)] {
            for base in names {
                for t in index.resolve(file, Some(i), RefKind::Instantiate, base, None).targets {
                    if let Some(to) = target_sid(&t)
                        && to != sid
                    {
                        edges.push(EdgeRow { rel, from: sid.clone(), to, props: Map::new() });
                    }
                }
            }
        }
        if let Some(handler) = &symbol.handler {
            let resolved = index.resolve(file, symbol.parent, RefKind::Call, &handler.name, handler.qualifier.as_deref());
            for t in resolved.targets {
                if let Some(to) = target_sid(&t) {
                    edges.push(EdgeRow { rel: "HANDLES", from: sid.clone(), to, props: props(&[("confidence", json!(resolved.confidence))]) });
                }
            }
        }
    }
    // OVERRIDES: methods of a class with a same-named method in a base class / implemented trait.
    for (i, symbol) in file.symbols.iter().enumerate() {
        if symbol.kind != "method" {
            continue;
        }
        let mut bases: Vec<String> = Vec::new();
        if let Some(trait_name) = &symbol.impl_trait {
            bases.push(trait_name.clone());
        }
        if let Some(parent) = symbol.parent.and_then(|p| file.symbols.get(p)) {
            bases.extend(parent.extends.iter().cloned());
            bases.extend(parent.implements.iter().cloned());
        }
        for base in bases {
            if let Some(found) = index.members.get(&(base.as_str(), symbol.name.as_str())) {
                for r in found {
                    if let Some(to) = index.model.sid_of(*r) {
                        edges.push(EdgeRow { rel: "OVERRIDES", from: sid_of(i), to, props: Map::new() });
                    }
                }
            }
        }
    }
    // Rust `impl Trait for Type`.
    for imp in &file.impls {
        let types = index.resolve(file, None, RefKind::Instantiate, &imp.type_name, None).targets;
        let traits = index.resolve(file, None, RefKind::Instantiate, &imp.trait_name, None).targets;
        for t in &types {
            for tr in &traits {
                if let (Some(from), Some(to)) = (target_sid(t), target_sid(tr)) {
                    edges.push(EdgeRow { rel: "IMPLEMENTS", from, to, props: props(&[("line", json!(imp.line))]) });
                }
            }
        }
    }
    // References.
    let mut seen_reads: HashSet<(Option<usize>, RefKind, String, String)> = HashSet::new();
    for r in &file.refs {
        let from = r.from.map(sid_of).unwrap_or_else(|| fsid.clone());
        if r.kind == RefKind::ReadConfig {
            edges.push(EdgeRow { rel: "READS_CONFIG", from, to: env_sid(&r.target.name), props: props(&[("line", json!(r.line))]) });
            continue;
        }
        if matches!(r.kind, RefKind::Read | RefKind::Write | RefKind::Throw | RefKind::Mention) {
            // One edge per (source, target) for data-flow edges and mentions; the first line is kept.
            let key = (r.from, r.kind, r.target.name.clone(), r.target.qualifier.clone().unwrap_or_default());
            if !seen_reads.insert(key) {
                continue;
            }
        }
        let argc = matches!(r.kind, RefKind::Call | RefKind::Instantiate).then_some(r.args.len());
        let resolved = index.resolve_with(file, r.from, r.kind, &r.target.name, r.target.qualifier.as_deref(), argc);
        for t in &resolved.targets {
            let Some(to) = target_sid(t) else { continue };
            if to == from {
                // Direct recursion is kept for calls, dropped for data edges.
                if r.kind != RefKind::Call {
                    continue;
                }
            }
            let target_kind = match t {
                Target::Sym(sr) => index.sym(*sr).kind.as_str(),
                Target::External(_) => "external_symbol",
            };
            let rel = match r.kind {
                RefKind::Call if matches!(target_kind, "class" | "struct") => "INSTANTIATES",
                RefKind::Call => "CALLS",
                RefKind::Instantiate => "INSTANTIATES",
                RefKind::Render => "RENDERS",
                RefKind::UseHook => "USES_HOOK",
                RefKind::Read => "READS",
                RefKind::Write => "WRITES",
                RefKind::Throw => "THROWS",
                RefKind::ReadConfig => "READS_CONFIG",
                RefKind::Mention => "MENTIONS",
            };
            let mut p = props(&[("line", json!(r.line)), ("confidence", json!(resolved.confidence))]);
            match r.kind {
                RefKind::Call | RefKind::Instantiate | RefKind::UseHook => {
                    p.insert("args".into(), json!(r.args));
                }
                RefKind::Render => {
                    p.insert("props".into(), json!(r.args));
                }
                _ => {}
            }
            edges.push(EdgeRow { rel, from: from.clone(), to: to.clone(), props: p });
            // TESTS: a test's direct references to project code.
            if let (Some(fi), Target::Sym(_)) = (r.from, t)
                && resolved.confidence != "ambiguous"
                && file.symbols[fi].kind == "test"
                && target_kind != "test"
                && matches!(r.kind, RefKind::Call | RefKind::Instantiate | RefKind::Render)
            {
                edges.push(EdgeRow { rel: "TESTS", from: from.clone(), to, props: Map::new() });
            }
        }
    }
    // Imports: file -> file / dependency.
    let mut imported: HashSet<String> = HashSet::new();
    for import in &file.imports {
        // Pages and stylesheets name the repository file they import (`local`, resolved by the
        // plugin as a browser and a static server would); never a package.
        if is_web(&file.language) {
            if let Some((target, _)) = index.model.files.get_key_value(import.local.as_str())
                && target != path
                && imported.insert(file_sid(target))
            {
                edges.push(EdgeRow { rel: "IMPORTS", from: fsid.clone(), to: file_sid(target), props: props(&[("line", json!(import.line)), ("module", json!(import.module))]) });
            }
            continue;
        }
        let targets = index.module_files(file, &import.module);
        if targets.is_empty()
            && let Some(package) = index.external_package(&file.language, &import.module) {
                let eco = index.deps.get(&package).cloned().unwrap_or_default();
                let to = dep_sid(&eco, &package);
                if imported.insert(to.clone()) {
                    edges.push(EdgeRow { rel: "IMPORTS", from: fsid.clone(), to, props: props(&[("line", json!(import.line))]) });
                }
            }
        for target in targets {
            let to = file_sid(target);
            if target != path && imported.insert(to.clone()) {
                edges.push(EdgeRow { rel: "IMPORTS", from: fsid.clone(), to, props: props(&[("line", json!(import.line)), ("module", json!(import.module))]) });
            }
        }
    }
    // Rust `mod x;` -> the module's file.
    if file.language == "rust" {
        for name in &file.mod_decls {
            for target in index.module_files(file, &format!("self::{name}")) {
                if target != path && imported.insert(file_sid(target)) {
                    edges.push(EdgeRow { rel: "IMPORTS", from: fsid.clone(), to: file_sid(target), props: props(&[("module", json!(name))]) });
                }
            }
        }
    }
    // History: the commits that modified this file.
    for sha in index.model.history.get(path).map(Vec::as_slice).unwrap_or(&[]) {
        edges.push(EdgeRow { rel: "MODIFIED_IN", from: fsid.clone(), to: commit_sid(sha), props: Map::new() });
    }
    // Manifests.
    if let Some(psid) = package_sid(file) {
        edges.push(EdgeRow { rel: "DEFINES", from: fsid.clone(), to: psid.clone(), props: Map::new() });
        for dep in &file.dependencies {
            edges.push(EdgeRow { rel: "DEPENDS_ON", from: psid.clone(), to: dep_sid(&dep.ecosystem, &dep.name), props: Map::new() });
        }
    }
    for dep in &file.dependencies {
        edges.push(EdgeRow { rel: "DEPENDS_ON", from: fsid.clone(), to: dep_sid(&dep.ecosystem, &dep.name), props: props(&[("spec", json!(dep.spec)), ("section", json!(dep.section))]) });
    }
    for command in &file.commands {
        let csid = command_sid(file, &command.name);
        edges.push(EdgeRow { rel: "DEFINES", from: fsid.clone(), to: csid.clone(), props: Map::new() });
        for target in &command.runs_files {
            if index.model.files.contains_key(target) {
                edges.push(EdgeRow { rel: "RUNS", from: csid.clone(), to: file_sid(target), props: Map::new() });
            }
        }
        for other in &command.runs_commands {
            if file.commands.iter().any(|c| &c.name == other) {
                edges.push(EdgeRow { rel: "RUNS", from: csid.clone(), to: command_sid(file, other), props: Map::new() });
            }
        }
        // `pkg.module:function` entry points.
        if let Some(entry) = &command.entry_point
            && let Some((module, function)) = entry.split_once(':')
        {
            let function = function.split('.').next().unwrap_or(function).trim();
            for target in index.py_modules.get(module.trim()).cloned().unwrap_or_default() {
                for r in index.defined_in(target, function, 0) {
                    if let Some(to) = index.model.sid_of(r) {
                        edges.push(EdgeRow { rel: "RUNS", from: csid.clone(), to, props: Map::new() });
                    }
                }
            }
        }
    }
    // Scripts and programs run by code.
    for run in &file.runs {
        let from = run.from.map(sid_of).unwrap_or_else(|| fsid.clone());
        let dir = dir_of(path);
        let candidates = [normalize(&format!("{dir}/{}", run.target)), normalize(&run.target)];
        if let Some(target) = candidates.iter().find(|c| index.model.files.contains_key(c.as_str())) {
            edges.push(EdgeRow { rel: "RUNS", from, to: file_sid(target), props: props(&[("line", json!(run.line))]) });
        }
    }
    // Cross-language: HTTP requests -> routes.
    for request in &file.requests {
        let from = request.from.map(sid_of).unwrap_or_else(|| fsid.clone());
        for route in match_routes(index, &request.method, &request.url) {
            if let Some(to) = index.model.sid_of(route) {
                edges.push(EdgeRow {
                    rel: "REQUESTS",
                    from: from.clone(),
                    to,
                    props: props(&[("method", json!(request.method)), ("url", json!(request.url)), ("line", json!(request.line)), ("args", json!(request.args)), ("confidence", json!("heuristic"))]),
                });
            }
        }
    }
    // Cross-language: IPC invocations -> channels.
    for call in &file.ipc_calls {
        let from = call.from.map(sid_of).unwrap_or_else(|| fsid.clone());
        for channel in index.channels.get(call.channel.as_str()).cloned().unwrap_or_default() {
            if let Some(to) = index.model.sid_of(channel) {
                edges.push(EdgeRow {
                    rel: "INVOKES",
                    from: from.clone(),
                    to,
                    props: props(&[("channel", json!(call.channel)), ("line", json!(call.line)), ("args", json!(call.args)), ("confidence", json!("heuristic"))]),
                });
            }
        }
    }
    edges.sort_by(|a, b| (a.rel, &a.from, &a.to).cmp(&(b.rel, &b.from, &b.to)));
    edges.dedup_by(|a, b| a.rel == b.rel && a.from == b.from && a.to == b.to && a.props == b.props);
    edges
}

/// Routes matching a request by method and path pattern (structural segment matching).
fn match_routes<'a>(index: &Index<'a>, method: &str, url: &str) -> Vec<SymRef<'a>> {
    let path = request_path(url);
    let req: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut best: Vec<(usize, SymRef<'a>)> = Vec::new();
    for route in &index.routes {
        let symbol = index.sym(*route);
        let route_method = symbol.http_method.as_deref().unwrap_or("ANY");
        if route_method != "ANY" && !route_method.eq_ignore_ascii_case(method) {
            continue;
        }
        let Some(pattern) = &symbol.route_path else { continue };
        let segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let catch_all = segs.last().is_some_and(|s| s.starts_with('*') || s.starts_with("{*") || s.starts_with("<path:"));
        if segs.len() != req.len() && !(catch_all && req.len() >= segs.len() - 1) {
            continue;
        }
        let mut literal_matches = 0;
        let mut ok = true;
        for (i, seg) in segs.iter().enumerate() {
            let is_param = seg.starts_with(':') || (seg.starts_with('{') && seg.ends_with('}')) || (seg.starts_with('<') && seg.ends_with('>')) || seg.starts_with('*');
            if is_param && catch_all && i == segs.len() - 1 {
                break;
            }
            let Some(r) = req.get(i) else {
                ok = false;
                break;
            };
            if is_param {
                continue;
            }
            if r.contains("{}") {
                // A dynamic request segment can only match a literal if its fixed parts agree.
                let fixed: Vec<&str> = r.split("{}").filter(|s| !s.is_empty()).collect();
                if fixed.iter().all(|f| seg.contains(f)) {
                    continue;
                }
                ok = false;
                break;
            }
            if r == seg {
                literal_matches += 1;
            } else {
                ok = false;
                break;
            }
        }
        if ok {
            best.push((literal_matches, *route));
        }
    }
    let top = best.iter().map(|(n, _)| *n).max().unwrap_or(0);
    best.into_iter().filter(|(n, _)| *n == top).map(|(_, r)| r).collect()
}

/// The path part of a request URL: origin, query and fragment removed; a leading base-URL
/// substitution (`${API}/x`) dropped.
pub fn request_path(url: &str) -> String {
    let mut u = url.trim();
    if let Some(pos) = u.find("://") {
        u = &u[pos + 3..];
        u = u.find('/').map(|p| &u[p..]).unwrap_or("/");
    }
    let u = u.split(['?', '#']).next().unwrap_or(u);
    let u = u.strip_prefix("{}").unwrap_or(u);
    if u.starts_with('/') { u.to_owned() } else { format!("/{u}") }
}
