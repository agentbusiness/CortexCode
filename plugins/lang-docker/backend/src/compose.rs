//! Compose files (`compose.yaml`, `docker-compose.yml`, ...), from the tree-sitter-yaml grammar and
//! the Compose specification's keys:
//!
//! - the top-level `name` is the package; every service is a `class` symbol (its image in
//!   `type_text`, `extends.service` in `extends`); its `ports` and `expose` entries are `field`
//!   symbols and its `environment` entries `const` symbols with their values; top-level `volumes`,
//!   `networks`, `secrets` and `configs` are `variable` symbols;
//! - `image` (of a service that does not build) is a `docker` dependency; `build` is a
//!   `docker-build` dependency (name: the context directory, spec: the Dockerfile) and a run of the
//!   Dockerfile when it exists in the repository;
//! - `depends_on`, `links`, `volumes_from`, `network_mode: service:x` and `extends` (same file) are
//!   `Instantiate` references between services (`depends_on`'s condition as the argument); named
//!   volumes, networks, secrets and configs a service uses are `Read` references; `${VAR}`
//!   interpolations are `ReadConfig` references (environment variables);
//! - bind-mounted repository paths, `env_file`, `include` and `extends.file` are imports (module: the
//!   path as written, relative to the Compose file; local: the container path or the key);
//! - `command`, `entrypoint` and `healthcheck.test` are commands (`<service>:command`, ...); script
//!   files they name are runs when their container path maps back through a bind mount to a
//!   repository file (only with the repository root).
//!
//! [`occurrences`] gives the same names with their columns, for the language-server router's
//! definition and references of Compose files.

use crate::image::{Mapping, container_join, expansions, repo_file, script_words, split_image};
use crate::yaml::{self, Value, Yaml};
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts, ImportFact, RefFact, RefKind, RefTarget, RunFact, SymbolFact};
use cc_lang_kit::manifest::{dir_of, is_repo_file, join_rel};
use cc_lang_kit::syntax::{end_line, has_error, hash_hex, line, parse, squash, text};
use cc_lang_kit::tree_sitter::Node;
use std::path::Path;

/// Top-level sections whose entries services refer to by name.
pub const RESOURCES: &[&str] = &["volumes", "networks", "secrets", "configs"];

/// A name a service refers to: what it names (`services`, `volumes`, ...), the node the name is
/// written in, and how.
#[derive(Debug, Clone)]
pub struct Relation<'t> {
    pub namespace: &'static str,
    pub name: String,
    pub node: Option<Node<'t>>,
    pub kind: RefKind,
    pub args: Vec<String>,
}

/// The names `service` refers to.
pub fn relations<'t>(service: &Yaml<'t>) -> Vec<Relation<'t>> {
    let mut out = Vec::new();
    let mut push = |namespace: &'static str, name: &str, node: Option<Node<'t>>, kind: RefKind, args: Vec<String>| {
        if !name.is_empty() && !name.contains('$') {
            out.push(Relation { namespace, name: name.to_owned(), node, kind, args });
        }
    };
    if let Some(depends) = service.get("depends_on") {
        for item in depends.items() {
            push("services", item.str().unwrap_or(""), item.node, RefKind::Instantiate, Vec::new());
        }
        for pair in depends.pairs() {
            let condition = pair.value.get("condition").and_then(Yaml::str).map(|c| vec![c.to_owned()]).unwrap_or_default();
            push("services", &pair.key, Some(pair.key_node), RefKind::Instantiate, condition);
        }
    }
    for item in service.get("links").map(Yaml::items).unwrap_or_default() {
        let link = item.str().unwrap_or("");
        push("services", link.split(':').next().unwrap_or(link), item.node, RefKind::Instantiate, Vec::new());
    }
    for item in service.get("volumes_from").map(Yaml::items).unwrap_or_default() {
        let from = item.str().unwrap_or("");
        if !from.starts_with("container:") {
            push("services", from.split(':').next().unwrap_or(from), item.node, RefKind::Instantiate, Vec::new());
        }
    }
    if let Some(mode) = service.get("network_mode")
        && let Some(other) = mode.str().and_then(|m| m.strip_prefix("service:"))
    {
        push("services", other, mode.node, RefKind::Instantiate, Vec::new());
    }
    if let Some(extends) = service.get("extends") {
        match extends.str() {
            Some(name) => push("services", name, extends.node, RefKind::Instantiate, Vec::new()),
            None if extends.get("file").is_none() => {
                if let Some(name) = extends.get("service") {
                    push("services", name.str().unwrap_or(""), name.node, RefKind::Instantiate, Vec::new());
                }
            }
            None => {}
        }
    }
    for item in service.get("volumes").map(Yaml::items).unwrap_or_default() {
        if let Some((source, _)) = volume_entry(item)
            && !is_bind_source(&source)
        {
            let node = item.get("source").and_then(|s| s.node).or(item.node);
            push("volumes", &source, node, RefKind::Read, Vec::new());
        }
    }
    for section in ["networks", "secrets", "configs"] {
        let Some(value) = service.get(section) else { continue };
        for item in value.items() {
            match item.str() {
                Some(name) => push(section_name(section), name, item.node, RefKind::Read, Vec::new()),
                None => {
                    if let Some(source) = item.get("source") {
                        push(section_name(section), source.str().unwrap_or(""), source.node, RefKind::Read, Vec::new());
                    }
                }
            }
        }
        for pair in value.pairs() {
            push(section_name(section), &pair.key, Some(pair.key_node), RefKind::Read, Vec::new());
        }
    }
    out
}

fn section_name(section: &str) -> &'static str {
    RESOURCES.iter().find(|r| **r == section).copied().unwrap_or("services")
}

/// A volume entry's source and container target (short `src:dst[:mode]` or long syntax).
fn volume_entry(item: &Yaml) -> Option<(String, String)> {
    if let Some(short) = item.str() {
        let mut parts = short.splitn(3, ':');
        let first = parts.next()?.to_owned();
        // `- /data` (no target): an anonymous volume at that path.
        return parts.next().map(|target| (first, target.to_owned()));
    }
    let kind = item.get("type").and_then(Yaml::str).unwrap_or("volume");
    if !matches!(kind, "bind" | "volume") {
        return None;
    }
    let source = item.get("source").and_then(Yaml::str)?.to_owned();
    let target = item.get("target").and_then(Yaml::str).unwrap_or("").to_owned();
    if kind == "bind" && !is_bind_source(&source) {
        // A long-syntax bind source may be written without `./`.
        return Some((format!("./{source}"), target));
    }
    Some((source, target))
}

/// A short-syntax volume source that is a host path, not a named volume.
fn is_bind_source(source: &str) -> bool {
    source.starts_with(['.', '/', '~', '$'])
}

fn strings(value: &Yaml) -> Vec<String> {
    match &value.value {
        Value::Scalar(s) => vec![s.clone()],
        Value::Seq(items) => items.iter().filter_map(|i| i.str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn new_symbol(kind: &str, name: &str, qname: String, node: Node, src: &str, parent: Option<usize>) -> SymbolFact {
    let code = text(node, src).to_owned();
    let signature = squash(code.lines().next().unwrap_or(""));
    SymbolFact {
        kind: kind.into(),
        name: name.into(),
        qname,
        sig_hash: hash_hex(signature.as_bytes()),
        signature,
        code,
        start_line: line(node),
        end_line: end_line(node),
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        parent,
        ..SymbolFact::default()
    }
}

pub fn extract(path: &str, src: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Some(tree) = parse(&yaml::language(), src) else {
        facts.parse_errors = 1;
        return;
    };
    facts.parse_errors = has_error(tree.root_node());
    let doc = yaml::document(tree.root_node(), src);
    let dir = dir_of(path);
    facts.package = doc.get("name").and_then(Yaml::str).filter(|n| !n.contains('$')).map(str::to_owned);
    for pair in doc.pairs() {
        match pair.key.as_str() {
            "services" => {
                for service in pair.value.pairs() {
                    extract_service(service, src, dir, root, facts);
                }
            }
            "include" => {
                for item in pair.value.items() {
                    let paths = item.get("path").map(strings).unwrap_or_else(|| strings(item));
                    for module in paths {
                        facts.imports.push(ImportFact { module, name: "*".into(), local: "include".into(), line: item.line() });
                    }
                }
            }
            key if RESOURCES.contains(&key) => {
                let singular = key.trim_end_matches('s');
                for entry in pair.value.pairs() {
                    let mut symbol = new_symbol("variable", &entry.key, format!("{key}.{}", entry.key), entry.node, src, None);
                    symbol.type_text = Some(singular.into());
                    facts.symbols.push(symbol);
                }
                interpolations(&pair.value, None, facts);
            }
            _ => interpolations(&pair.value, None, facts),
        }
    }
}

/// `${VAR}` interpolations under `value` as environment reads.
fn interpolations(value: &Yaml, from: Option<usize>, facts: &mut FileFacts) {
    for (scalar, node) in value.scalars() {
        for expansion in expansions(scalar) {
            let line = node.map(line).unwrap_or(0);
            facts.refs.push(RefFact { from, kind: RefKind::ReadConfig, target: RefTarget { name: expansion.name, qualifier: None }, args: Vec::new(), line });
        }
    }
}

fn extract_service(pair: &yaml::Pair, src: &str, dir: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let name = pair.key.as_str();
    let service = &pair.value;
    let index = facts.symbols.len();
    let image = service.get("image").and_then(Yaml::str).map(str::to_owned);
    let build = service.get("build");
    let mut symbol = new_symbol("class", name, name.to_owned(), pair.node, src, None);
    let summary = match (&image, build) {
        (_, Some(build)) => format!(" build {}", build.str().or_else(|| build.get("context").and_then(Yaml::str)).unwrap_or(".")),
        (Some(image), None) => format!(" image {image}"),
        (None, None) => String::new(),
    };
    symbol.signature = format!("service {name}:{summary}");
    symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
    symbol.body_hash = service.node.map(|n| hash_hex(text(n, src).as_bytes())).unwrap_or_default();
    symbol.type_text = image.clone();
    if let Some(extends) = service.get("extends") {
        let base = extends.str().or_else(|| extends.get("service").and_then(Yaml::str));
        if let Some(base) = base.filter(|_| extends.get("file").is_none()) {
            symbol.extends = vec![base.to_owned()];
        }
        if let Some(file) = extends.get("file").and_then(Yaml::str) {
            facts.imports.push(ImportFact { module: file.to_owned(), name: "*".into(), local: "extends".into(), line: extends.line() });
        }
    }
    if let Some(profiles) = service.get("profiles") {
        symbol.decorators = strings(profiles).into_iter().map(|p| format!("profile:{p}")).collect();
    }
    facts.symbols.push(symbol);

    // Images and builds.
    match build {
        Some(build) => {
            let context = build.str().or_else(|| build.get("context").and_then(Yaml::str)).unwrap_or(".").to_owned();
            let remote = context.contains("://") || context.starts_with("git@");
            let dockerfile = build.get("dockerfile").and_then(Yaml::str).unwrap_or("Dockerfile").to_owned();
            let inline = build.get("dockerfile_inline").is_some();
            let context_rel = if remote { context.clone() } else { join_rel(dir, &context) };
            let dockerfile_rel = if remote || inline { String::new() } else { join_rel(&context_rel, &dockerfile) };
            let name = if context_rel.is_empty() { ".".to_owned() } else { context_rel };
            facts.dependencies.push(DependencyFact {
                name,
                ecosystem: "docker-build".into(),
                spec: if inline { "dockerfile_inline".into() } else if remote { dockerfile.clone() } else { dockerfile_rel.clone() },
                section: "build".into(),
                installed_version: None,
                source_dir: None,
            });
            if !dockerfile_rel.is_empty() && is_repo_file(root, &dockerfile_rel) {
                facts.runs.push(RunFact { from: Some(index), target: dockerfile_rel, line: build.line() });
            }
        }
        None => {
            if let Some(image) = image.filter(|i| !i.is_empty()) {
                let (name, spec) = split_image(&image);
                if !name.contains('$') {
                    let dependency = DependencyFact { name, ecosystem: "docker".into(), spec, section: "dependencies".into(), installed_version: None, source_dir: None };
                    if !facts.dependencies.contains(&dependency) {
                        facts.dependencies.push(dependency);
                    }
                }
            }
        }
    }

    // Ports, exposed ports and environment.
    let child = |facts: &mut FileFacts, kind: &str, section: &str, label: &str, node: Option<Node>, value: Option<String>| {
        let Some(node) = node else { return };
        let mut symbol = new_symbol(kind, label, format!("{name}.{section}.{label}"), node, src, Some(index));
        symbol.type_text = Some(if kind == "field" { "port".into() } else { "environment".into() });
        symbol.value = value;
        facts.symbols.push(symbol);
    };
    for item in service.get("ports").map(Yaml::items).unwrap_or_default() {
        let label = match item.str() {
            Some(short) => short.to_owned(),
            None => {
                let target = item.get("target").and_then(Yaml::str).unwrap_or("");
                let protocol = item.get("protocol").and_then(Yaml::str).map(|p| format!("/{p}")).unwrap_or_default();
                match item.get("published").and_then(Yaml::str) {
                    Some(published) => format!("{published}:{target}{protocol}"),
                    None => format!("{target}{protocol}"),
                }
            }
        };
        let target = label.rsplit(':').next().map(|t| t.split('/').next().unwrap_or(t).to_owned());
        child(facts, "field", "ports", &label, item.node, target);
    }
    for item in service.get("expose").map(Yaml::items).unwrap_or_default() {
        if let Some(port) = item.str() {
            child(facts, "field", "expose", port, item.node, None);
        }
    }
    if let Some(environment) = service.get("environment") {
        for entry in environment.pairs() {
            child(facts, "const", "environment", &entry.key, Some(entry.node), entry.value.str().map(str::to_owned));
        }
        for item in environment.items() {
            let Some(entry) = item.str() else { continue };
            let (key, value) = match entry.split_once('=') {
                Some((k, v)) => (k, Some(v.to_owned())),
                None => (entry, None),
            };
            child(facts, "const", "environment", key, item.node, value);
        }
    }
    for item in service.get("env_file").map(|e| if e.items().is_empty() { vec![e.clone()] } else { e.items().to_vec() }).unwrap_or_default() {
        let path = item.str().or_else(|| item.get("path").and_then(Yaml::str));
        if let Some(path) = path {
            facts.imports.push(ImportFact { module: path.to_owned(), name: "*".into(), local: "env_file".into(), line: item.line() });
        }
    }

    // Bind mounts: repository paths placed in the container.
    let working_dir = service.get("working_dir").and_then(Yaml::str).unwrap_or("/").to_owned();
    let mut mappings = Vec::new();
    for item in service.get("volumes").map(Yaml::items).unwrap_or_default() {
        let Some((source, target)) = volume_entry(item) else { continue };
        if !is_bind_source(&source) || target.is_empty() {
            continue;
        }
        facts.imports.push(ImportFact { module: source.clone(), name: "*".into(), local: target.clone(), line: item.line() });
        if source.starts_with(['/', '~', '$']) {
            continue;
        }
        let container = container_join(&working_dir, &target);
        mappings.push(Mapping { container: container.clone(), source: source.clone(), into_dir: true });
        mappings.push(Mapping { container, source, into_dir: false });
    }

    // Commands.
    let mut commands: Vec<(&str, Yaml)> = Vec::new();
    for key in ["entrypoint", "command"] {
        if let Some(value) = service.get(key) {
            commands.push((key, value.clone()));
        }
    }
    if let Some(test) = service.get("healthcheck").and_then(|h| h.get("test")) {
        commands.push(("healthcheck", test.clone()));
    }
    for (label, value) in commands {
        let mut words = strings(&value);
        if label == "healthcheck" && matches!(words.first().map(String::as_str), Some("CMD" | "CMD-SHELL" | "NONE")) {
            words.remove(0);
        }
        let command = words.join(" ");
        if command.is_empty() {
            continue;
        }
        let mut runs_files = Vec::new();
        for word in script_words(&command) {
            let container = container_join(&working_dir, &word);
            if let Some(file) = repo_file(&container, &mappings, &[dir], root)
                && !runs_files.contains(&file)
            {
                runs_files.push(file);
            }
        }
        for file in &runs_files {
            facts.runs.push(RunFact { from: Some(index), target: file.clone(), line: value.line() });
        }
        facts.commands.push(CommandFact { name: format!("{name}:{label}"), command, line: value.line(), runs_files, runs_commands: Vec::new(), entry_point: None });
    }

    // References to other services and resources.
    for relation in relations(service) {
        let line = relation.node.map(line).unwrap_or_else(|| line(pair.node));
        facts.refs.push(RefFact { from: Some(index), kind: relation.kind, target: RefTarget { name: relation.name, qualifier: None }, args: relation.args, line });
    }
    interpolations(service, Some(index), facts);
}

/// A name written in a Compose file: what it names, where (0-based line, byte columns in that
/// line), and whether this is its definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub namespace: &'static str,
    pub name: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub definition: bool,
}

fn occurrence(namespace: &'static str, name: &str, node: Node, src: &str, definition: bool) -> Option<Occurrence> {
    let raw = text(node, src);
    let offset = raw.find(name)?;
    let start = node.start_byte() + offset;
    let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = src[..start].matches('\n').count();
    Some(Occurrence { namespace, name: name.to_owned(), line, start: start - line_start, end: start - line_start + name.len(), definition })
}

/// Every definition and use of a service, volume, network, secret or config name in `src`.
pub fn occurrences(src: &str) -> Vec<Occurrence> {
    let Some(tree) = parse(&yaml::language(), src) else { return Vec::new() };
    let doc = yaml::document(tree.root_node(), src);
    let mut out = Vec::new();
    for pair in doc.pairs() {
        let namespace = match pair.key.as_str() {
            "services" => "services",
            key => match RESOURCES.iter().find(|r| **r == key) {
                Some(resource) => *resource,
                None => continue,
            },
        };
        for entry in pair.value.pairs() {
            out.extend(occurrence(namespace, &entry.key, entry.key_node, src, true));
            if namespace == "services" {
                for relation in relations(&entry.value) {
                    if let Some(node) = relation.node {
                        out.extend(occurrence(relation.namespace, &relation.name, node, src, false));
                    }
                }
            }
        }
    }
    out.sort_by_key(|o| (o.line, o.start));
    out.dedup();
    out
}
