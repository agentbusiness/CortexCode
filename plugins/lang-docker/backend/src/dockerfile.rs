//! Dockerfiles and Containerfiles, from the tree-sitter-containerfile grammar (node kinds and fields
//! of its `source_file`):
//!
//! - every `FROM` starts a build stage: a `class` symbol named by its `AS` alias (`stage-<n>` when
//!   unnamed), its base in `extends` and `type_text`, the last stage `exported` (the image a plain
//!   build produces);
//! - `ARG` (before the first `FROM`: global; inside a stage: the stage's) and `ENV` pairs are `const`
//!   symbols with their values; `EXPOSE` ports are `field` symbols of the stage;
//! - base images and `COPY --from=<image>` are `docker` dependencies (`dependencies` for the final
//!   stage's base, `build` for the rest); `FROM <stage>` and `COPY --from=<stage>` are `Instantiate`
//!   references to that stage; `$VAR` / `${VAR}` in instruction arguments are `Read` references;
//! - `COPY` / `ADD` sources are imports (module = the source as written, relative to the build
//!   context; local = the destination);
//! - `CMD`, `ENTRYPOINT` and `HEALTHCHECK` are commands (`<stage>:cmd`, ...); script files that
//!   `RUN`, `CMD`, `ENTRYPOINT` and `HEALTHCHECK` run are runs, when the container path maps back
//!   through the stage's COPY / ADD instructions or a `RUN --mount=type=bind` to a repository file.
//!   The build context is taken to be the Dockerfile's directory, then the repository root; a file
//!   is reported only when it exists under the repository root (none without a root).

use crate::image::{Mapping, container_join, repo_file, script_words, split_image, substitute};
use cc_lang_kit::cc_protocol::contracts::lang::{CommandFact, DependencyFact, FileFacts, ImportFact, RefFact, RefKind, RefTarget, RunFact, SymbolFact};
use cc_lang_kit::manifest::dir_of;
use cc_lang_kit::syntax::{end_line, field, has_error, hash_hex, line, named_children, parse, squash, text};
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashMap;
use std::path::Path;

pub fn language() -> Language {
    tree_sitter_containerfile::LANGUAGE.into()
}

/// What later instructions of a stage (and stages built on it) need to know.
#[derive(Debug, Clone, Default)]
struct StageState {
    /// Index of the stage's symbol.
    symbol: usize,
    name: String,
    workdir: String,
    /// Where files from the build context were placed.
    copies: Vec<Mapping>,
}

struct Extractor<'a> {
    src: &'a str,
    root: Option<&'a Path>,
    /// Build-context candidates, repository-relative: the Dockerfile's directory, then the root.
    contexts: Vec<&'a str>,
    facts: &'a mut FileFacts,
    /// Global ARG defaults (the values FROM lines can use).
    global_args: HashMap<String, String>,
    stages: Vec<StageState>,
}

pub fn extract(path: &str, text_: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), text_) else {
        facts.parse_errors = 1;
        return;
    };
    let top = tree.root_node();
    facts.parse_errors = has_error(top);
    let dir = dir_of(path);
    let mut contexts = vec![dir];
    if !dir.is_empty() {
        contexts.push("");
    }
    let mut extractor = Extractor { src: text_, root, contexts, facts, global_args: HashMap::new(), stages: Vec::new() };
    let instructions: Vec<Node> = named_children(top).into_iter().filter(|n| n.kind().ends_with("_instruction")).collect();
    // Stage spans: a FROM up to the instruction before the next FROM.
    let froms: Vec<usize> = instructions.iter().enumerate().filter(|(_, n)| n.kind() == "from_instruction").map(|(i, _)| i).collect();
    let last_stage = froms.len().saturating_sub(1);
    for (i, node) in instructions.iter().enumerate() {
        if node.kind() == "from_instruction" {
            let ordinal = extractor.stages.len();
            let next = froms.iter().find(|&&f| f > i).copied().unwrap_or(instructions.len());
            let last = instructions[next - 1];
            extractor.stage(*node, last, ordinal, ordinal == last_stage);
        } else {
            extractor.instruction(*node);
        }
    }
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && ((raw.starts_with('"') && raw.ends_with('"')) || (raw.starts_with('\'') && raw.ends_with('\''))) {
        raw[1..raw.len() - 1].replace("\\\"", "\"")
    } else {
        raw.to_owned()
    }
}

/// `--name=value` of a `param` node.
fn param(node: Node, src: &str) -> Option<(String, String)> {
    let raw = text(node, src).trim_start_matches('-');
    raw.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))
}

/// The strings of an exec-form array (`["a", "b"]`).
fn json_strings(node: Node, src: &str) -> Vec<String> {
    named_children(node)
        .into_iter()
        .filter(|n| n.kind() == "json_string")
        .map(|n| serde_json::from_str::<String>(text(n, src)).unwrap_or_else(|_| unquote(text(n, src))))
        .collect()
}

/// The command an instruction runs: the exec-form array joined, or the shell text with line
/// continuations removed (heredoc bodies included).
fn command_text(node: Node, src: &str) -> String {
    let mut parts = Vec::new();
    for child in named_children(node) {
        match child.kind() {
            "json_string_array" => parts.push(json_strings(child, src).join(" ")),
            "shell_command" => parts.push(squash(&text(child, src).replace("\\\n", " "))),
            "heredoc_block" => {
                for content in named_children(child).into_iter().filter(|c| c.kind() == "heredoc_content") {
                    parts.push(text(content, src).trim().to_owned());
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

impl Extractor<'_> {
    fn current(&self) -> Option<usize> {
        self.stages.last().map(|s| s.symbol)
    }

    fn refs_in(&mut self, node: Node, from: Option<usize>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "variable" {
                let name = text(current, self.src).to_owned();
                self.facts.refs.push(RefFact { from, kind: RefKind::Read, target: RefTarget { name, qualifier: None }, args: Vec::new(), line: line(current) });
            }
            stack.extend(named_children(current).into_iter().rev());
        }
    }

    fn symbol(&self, kind: &str, name: &str, qname: String, node: Node, parent: Option<usize>) -> SymbolFact {
        let code = text(node, self.src).to_owned();
        let signature = squash(&code.replace("\\\n", " "));
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

    /// The stage a `FROM` / `--from` value names: an earlier stage by name (case-insensitive) or
    /// by its index.
    fn stage_named(&self, name: &str) -> Option<usize> {
        if let Ok(index) = name.parse::<usize>() {
            return (index < self.stages.len()).then_some(index);
        }
        self.stages.iter().position(|s| s.name.eq_ignore_ascii_case(name))
    }

    fn stage(&mut self, from: Node, last: Node, ordinal: usize, is_final: bool) {
        let src = self.src;
        let alias = field(from, "as").map(|n| text(n, src).to_owned());
        let name = alias.clone().unwrap_or_else(|| format!("stage-{ordinal}"));
        let spec_node = named_children(from).into_iter().find(|n| n.kind() == "image_spec");
        let written = spec_node.map(|n| text(n, src).to_owned()).unwrap_or_default();
        let resolved = substitute(&written, &self.global_args);
        let base = resolved.clone().unwrap_or_else(|| written.clone());
        let params: Vec<String> = named_children(from).into_iter().filter(|n| n.kind() == "param").map(|n| text(n, src).to_owned()).collect();
        let index = self.facts.symbols.len();
        let code = src.get(from.start_byte()..last.end_byte()).unwrap_or("").to_owned();
        let body = src.get(from.end_byte()..last.end_byte()).unwrap_or("");
        let signature = squash(text(from, src));
        let parent_stage = self.stage_named(&base).filter(|_| resolved.is_some());
        let (image_name, image_spec) = split_image(&base);
        let symbol = SymbolFact {
            kind: "class".into(),
            name: name.clone(),
            qname: name.clone(),
            sig_hash: hash_hex(signature.as_bytes()),
            signature,
            body_hash: if body.trim().is_empty() { String::new() } else { hash_hex(body.as_bytes()) },
            code,
            start_line: line(from),
            end_line: end_line(last),
            start_byte: from.start_byte() as u32,
            end_byte: last.end_byte() as u32,
            type_text: Some(base.clone()),
            extends: if base.is_empty() || resolved.is_none() { Vec::new() } else { vec![parent_stage.map(|p| self.stages[p].name.clone()).unwrap_or_else(|| image_name.clone())] },
            decorators: params,
            exported: is_final,
            ..SymbolFact::default()
        };
        self.facts.symbols.push(symbol);
        self.refs_in(from, Some(index));
        let mut state = StageState { symbol: index, name, workdir: "/".into(), copies: Vec::new() };
        match parent_stage {
            Some(parent) => {
                let parent_state = &self.stages[parent];
                state.workdir = parent_state.workdir.clone();
                state.copies = parent_state.copies.clone();
                let line = line(from);
                let target = parent_state.name.clone();
                self.facts.refs.push(RefFact { from: Some(index), kind: RefKind::Instantiate, target: RefTarget { name: target, qualifier: None }, args: Vec::new(), line });
            }
            None if resolved.is_some() && !image_name.is_empty() && !image_name.eq_ignore_ascii_case("scratch") => {
                let section = if is_final { "dependencies" } else { "build" };
                self.push_dependency(image_name, image_spec, section);
            }
            None => {}
        }
        self.stages.push(state);
    }

    fn push_dependency(&mut self, name: String, spec: String, section: &str) {
        let dependency = DependencyFact { name, ecosystem: "docker".into(), spec, section: section.into(), installed_version: None, source_dir: None };
        if !self.facts.dependencies.contains(&dependency) {
            self.facts.dependencies.push(dependency);
        }
    }

    fn instruction(&mut self, node: Node) {
        let src = self.src;
        let from = self.current();
        let kind = node.kind();
        match kind {
            "arg_instruction" | "env_instruction" => {
                let (pair_kind, value_field, label) = if kind == "arg_instruction" { ("arg_pair", "default", "ARG") } else { ("env_pair", "value", "ENV") };
                for pair in named_children(node).into_iter().filter(|n| n.kind() == pair_kind) {
                    let Some(name) = field(pair, "name").map(|n| text(n, src).to_owned()) else { continue };
                    let value = field(pair, value_field).map(|n| unquote(text(n, src)));
                    let qname = match &self.stages.last() {
                        Some(stage) => format!("{}.{name}", stage.name),
                        None => name.clone(),
                    };
                    let mut symbol = self.symbol("const", &name, qname, pair, from);
                    symbol.signature = format!("{label} {}", symbol.signature);
                    symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
                    symbol.type_text = Some(label.into());
                    symbol.value = value.clone();
                    self.facts.symbols.push(symbol);
                    if from.is_none() && kind == "arg_instruction" {
                        self.global_args.insert(name, value.unwrap_or_default());
                    }
                    if let Some(value) = field(pair, value_field) {
                        self.refs_in(value, from);
                    }
                }
            }
            "expose_instruction" => {
                for port in named_children(node).into_iter().filter(|n| n.kind() == "expose_port") {
                    let name = text(port, src).to_owned();
                    let qname = match self.stages.last() {
                        Some(stage) => format!("{}.{name}", stage.name),
                        None => name.clone(),
                    };
                    let mut symbol = self.symbol("field", &name, qname, port, from);
                    symbol.signature = format!("EXPOSE {name}");
                    symbol.sig_hash = hash_hex(symbol.signature.as_bytes());
                    symbol.type_text = Some("port".into());
                    self.facts.symbols.push(symbol);
                    self.refs_in(port, from);
                }
            }
            "workdir_instruction" => {
                if let Some(path) = named_children(node).into_iter().find(|n| n.kind() == "path")
                    && let Some(stage) = self.stages.last_mut()
                {
                    stage.workdir = container_join(&stage.workdir, &unquote(text(path, src)));
                }
                self.refs_in(node, from);
            }
            "copy_instruction" | "add_instruction" => self.copy(node, from),
            "run_instruction" => {
                let command = command_text(node, src);
                let mut mounts = Vec::new();
                let workdir = self.stages.last().map(|s| s.workdir.clone()).unwrap_or_else(|| "/".into());
                for mount in named_children(node).into_iter().filter(|n| n.kind() == "mount_param") {
                    let values: HashMap<String, String> = named_children(mount)
                        .into_iter()
                        .filter_map(|v| text(v, src).split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
                        .collect();
                    if values.get("type").map(String::as_str) == Some("bind") && !values.contains_key("from") {
                        let Some(target) = values.get("target").or_else(|| values.get("dst")).or_else(|| values.get("destination")) else { continue };
                        let source = values.get("source").or_else(|| values.get("src")).cloned().unwrap_or_else(|| ".".into());
                        let container = container_join(&workdir, target);
                        mounts.push(Mapping { container: container.clone(), source: source.clone(), into_dir: true });
                        mounts.push(Mapping { container, source, into_dir: false });
                    }
                }
                self.runs(&command, &mounts, line(node), from);
                self.refs_in(node, from);
            }
            "cmd_instruction" | "entrypoint_instruction" | "healthcheck_instruction" => {
                let (label, command_node) = match kind {
                    "cmd_instruction" => ("cmd", Some(node)),
                    "entrypoint_instruction" => ("entrypoint", Some(node)),
                    _ => ("healthcheck", named_children(node).into_iter().find(|n| n.kind() == "cmd_instruction")),
                };
                let Some(command_node) = command_node else { return };
                let command = command_text(command_node, src);
                let runs_files = self.runs(&command, &[], line(node), from);
                let stage = self.stages.last().map(|s| s.name.clone()).unwrap_or_default();
                self.facts.commands.push(CommandFact {
                    name: format!("{stage}:{label}"),
                    command,
                    line: line(node),
                    runs_files,
                    runs_commands: Vec::new(),
                    entry_point: None,
                });
                self.refs_in(node, from);
            }
            _ => self.refs_in(node, from),
        }
    }

    fn copy(&mut self, node: Node, from: Option<usize>) {
        let src = self.src;
        let mut source_stage = None;
        for p in named_children(node).into_iter().filter(|n| n.kind() == "param") {
            if let Some((key, value)) = param(p, src)
                && key == "from"
            {
                let value = substitute(&value, &self.global_args).unwrap_or(value);
                match self.stage_named(&value) {
                    Some(stage) => {
                        let target = self.stages[stage].name.clone();
                        self.facts.refs.push(RefFact { from, kind: RefKind::Instantiate, target: RefTarget { name: target, qualifier: None }, args: Vec::new(), line: line(p) });
                    }
                    None if !value.contains('$') => {
                        let (name, spec) = split_image(&value);
                        self.push_dependency(name, spec, "build");
                    }
                    None => {}
                }
                source_stage = Some(value);
            }
        }
        self.refs_in(node, from);
        // Paths: sources then the destination (heredoc markers are inline files, not sources).
        let paths: Vec<Node> = named_children(node).into_iter().filter(|n| n.kind() == "path").collect();
        let arrays: Vec<String> = named_children(node).into_iter().filter(|n| n.kind() == "json_string_array").flat_map(|n| json_strings(n, src)).collect();
        let mut words: Vec<(String, u32, bool)> = paths.iter().map(|p| (unquote(text(*p, src)), line(*p), named_children(*p).iter().any(|c| c.kind() == "heredoc_marker"))).collect();
        words.extend(arrays.into_iter().map(|w| (w, line(node), false)));
        let Some((dest, _, _)) = words.pop() else { return };
        if source_stage.is_some() {
            return;
        }
        let workdir = self.stages.last().map(|s| s.workdir.clone()).unwrap_or_else(|| "/".into());
        let container = container_join(&workdir, &dest);
        let into_dir = dest.ends_with('/') || words.len() > 1;
        for (source, line, heredoc) in words {
            if heredoc || source.contains("://") || source.starts_with("git@") {
                continue;
            }
            self.facts.imports.push(ImportFact { module: source.clone(), name: "*".into(), local: dest.clone(), line });
            if let Some(stage) = self.stages.last_mut() {
                stage.copies.push(Mapping { container: container.clone(), source: source.clone(), into_dir: true });
                if !into_dir {
                    stage.copies.push(Mapping { container: container.clone(), source, into_dir: false });
                }
            }
        }
    }

    /// Runs of the repository files `command` names; returns them (repository-relative).
    fn runs(&mut self, command: &str, extra: &[Mapping], line: u32, from: Option<usize>) -> Vec<String> {
        let Some(stage) = self.stages.last() else { return Vec::new() };
        let mut mappings = stage.copies.clone();
        mappings.extend_from_slice(extra);
        let mut files = Vec::new();
        for word in script_words(command) {
            let container = container_join(&stage.workdir, &word);
            if let Some(file) = repo_file(&container, &mappings, &self.contexts, self.root)
                && !files.contains(&file)
            {
                files.push(file);
            }
        }
        for file in &files {
            self.facts.runs.push(RunFact { from, target: file.clone(), line });
        }
        files
    }
}
