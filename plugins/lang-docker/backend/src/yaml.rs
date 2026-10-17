//! A YAML document as values with their syntax nodes, from the tree-sitter-yaml grammar: mappings
//! (pairs in order), sequences, scalars (quotes and escapes removed). Anchors (`&a`) are recorded
//! and aliases (`*a`) replaced by the anchored value; merge keys (`<<: *a`) add the anchored
//! mapping's pairs a mapping does not set itself.

use cc_lang_kit::syntax::{named_children, text};
use cc_lang_kit::tree_sitter::{Language, Node};
use std::collections::HashMap;

pub fn language() -> Language {
    tree_sitter_yaml::LANGUAGE.into()
}

#[derive(Debug, Clone)]
pub enum Value<'t> {
    Map(Vec<Pair<'t>>),
    Seq(Vec<Yaml<'t>>),
    Scalar(String),
    Null,
}

#[derive(Debug, Clone)]
pub struct Yaml<'t> {
    /// The node the value was written at (an alias's value keeps its anchor's node).
    pub node: Option<Node<'t>>,
    pub value: Value<'t>,
}

#[derive(Debug, Clone)]
pub struct Pair<'t> {
    pub key: String,
    pub key_node: Node<'t>,
    /// The whole `key: value` node.
    pub node: Node<'t>,
    pub value: Yaml<'t>,
}

impl<'t> Yaml<'t> {
    const NULL: Yaml<'static> = Yaml { node: None, value: Value::Null };

    pub fn get(&self, key: &str) -> Option<&Yaml<'t>> {
        self.pairs().iter().find(|p| p.key == key).map(|p| &p.value)
    }

    pub fn pairs(&self) -> &[Pair<'t>] {
        match &self.value {
            Value::Map(pairs) => pairs,
            _ => &[],
        }
    }

    pub fn items(&self) -> &[Yaml<'t>] {
        match &self.value {
            Value::Seq(items) => items,
            _ => &[],
        }
    }

    pub fn str(&self) -> Option<&str> {
        match &self.value {
            Value::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// 1-based line of the value.
    pub fn line(&self) -> u32 {
        self.node.map(|n| n.start_position().row as u32 + 1).unwrap_or(0)
    }

    /// Every scalar under this value with its node, in document order.
    pub fn scalars(&self) -> Vec<(&str, Option<Node<'t>>)> {
        let mut out = Vec::new();
        self.collect_scalars(&mut out);
        out
    }

    fn collect_scalars<'s>(&'s self, out: &mut Vec<(&'s str, Option<Node<'t>>)>) {
        match &self.value {
            Value::Scalar(s) => out.push((s, self.node)),
            Value::Seq(items) => items.iter().for_each(|i| i.collect_scalars(out)),
            Value::Map(pairs) => pairs.iter().for_each(|p| p.value.collect_scalars(out)),
            Value::Null => {}
        }
    }
}

/// The first document of a parsed stream (Null when empty).
pub fn document<'t>(root: Node<'t>, src: &str) -> Yaml<'t> {
    let mut anchors = HashMap::new();
    let Some(document) = named_children(root).into_iter().find(|n| n.kind() == "document") else { return Yaml::NULL };
    match named_children(document).into_iter().find(|n| matches!(n.kind(), "block_node" | "flow_node")) {
        Some(node) => convert(node, src, &mut anchors),
        None => Yaml::NULL,
    }
}

fn double_quoted(raw: &str) -> String {
    let inner = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(raw);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\n') => {}
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A block scalar's content (`|` keeps lines, `>` folds them), indentation removed.
fn block_scalar(raw: &str) -> String {
    let mut lines = raw.lines();
    let header = lines.next().unwrap_or("");
    let body: Vec<&str> = lines.collect();
    let indent = body.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let body: Vec<&str> = body.iter().map(|l| l.get(indent..).unwrap_or("").trim_end()).collect();
    if header.trim_start().starts_with('>') { body.join(" ").trim().to_owned() } else { body.join("\n").trim_end().to_owned() }
}

fn scalar<'t>(node: Node<'t>, src: &str) -> Yaml<'t> {
    let raw = text(node, src);
    let value = match node.kind() {
        "double_quote_scalar" => Value::Scalar(double_quoted(raw)),
        "single_quote_scalar" => Value::Scalar(raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')).unwrap_or(raw).replace("''", "'")),
        "block_scalar" => Value::Scalar(block_scalar(raw)),
        "plain_scalar" if named_children(node).first().is_some_and(|c| c.kind() == "null_scalar") => Value::Null,
        _ => Value::Scalar(raw.trim().to_owned()),
    };
    Yaml { node: Some(node), value }
}

fn key_text<'t>(node: Node<'t>, src: &str, anchors: &mut HashMap<String, Yaml<'t>>) -> String {
    match convert(node, src, anchors).value {
        Value::Scalar(s) => s,
        _ => text(node, src).trim().to_owned(),
    }
}

fn convert<'t>(node: Node<'t>, src: &str, anchors: &mut HashMap<String, Yaml<'t>>) -> Yaml<'t> {
    match node.kind() {
        "block_node" | "flow_node" => {
            let children = named_children(node);
            let anchor = children.iter().find(|c| c.kind() == "anchor").and_then(|a| named_children(*a).first().map(|n| text(*n, src).to_owned()));
            let content = children.iter().find(|c| !matches!(c.kind(), "anchor" | "tag" | "comment"));
            let mut value = match content {
                Some(content) => convert(*content, src, anchors),
                None => Yaml { node: Some(node), value: Value::Null },
            };
            if value.node.is_none() {
                value.node = Some(node);
            }
            if let Some(anchor) = anchor {
                anchors.insert(anchor, value.clone());
            }
            value
        }
        "alias" => {
            let name = named_children(node).first().map(|n| text(*n, src).to_owned()).unwrap_or_default();
            anchors.get(&name).cloned().unwrap_or(Yaml { node: Some(node), value: Value::Null })
        }
        "block_mapping" | "flow_mapping" => {
            let mut pairs: Vec<Pair<'t>> = Vec::new();
            let mut merged: Vec<Pair<'t>> = Vec::new();
            for child in named_children(node) {
                let (key, value) = match child.kind() {
                    "block_mapping_pair" | "flow_pair" => (child.child_by_field_name("key"), child.child_by_field_name("value")),
                    // A flow mapping entry without a value (`{a, b}`).
                    "flow_node" => (Some(child), None),
                    _ => continue,
                };
                let Some(key_node) = key else { continue };
                let key = key_text(key_node, src, anchors);
                let value = match value {
                    Some(v) => convert(v, src, anchors),
                    None => Yaml { node: None, value: Value::Null },
                };
                if key == "<<" {
                    let sources: Vec<Yaml> = match &value.value {
                        Value::Seq(items) => items.clone(),
                        _ => vec![value.clone()],
                    };
                    for source in sources {
                        merged.extend(source.pairs().iter().cloned());
                    }
                    continue;
                }
                pairs.push(Pair { key, key_node, node: child, value });
            }
            for pair in merged {
                if !pairs.iter().any(|p| p.key == pair.key) {
                    pairs.push(pair);
                }
            }
            Yaml { node: Some(node), value: Value::Map(pairs) }
        }
        "block_sequence" | "flow_sequence" => {
            let mut items = Vec::new();
            for child in named_children(node) {
                match child.kind() {
                    "block_sequence_item" => {
                        if let Some(inner) = named_children(child).into_iter().find(|c| matches!(c.kind(), "block_node" | "flow_node")) {
                            items.push(convert(inner, src, anchors));
                        }
                    }
                    "flow_node" => items.push(convert(child, src, anchors)),
                    // `[a: b]`: a single-pair mapping.
                    "flow_pair" => {
                        let key = child.child_by_field_name("key").map(|k| key_text(k, src, anchors)).unwrap_or_default();
                        let value = child.child_by_field_name("value").map(|v| convert(v, src, anchors)).unwrap_or(Yaml { node: None, value: Value::Null });
                        let key_node = child.child_by_field_name("key").unwrap_or(child);
                        items.push(Yaml { node: Some(child), value: Value::Map(vec![Pair { key, key_node, node: child, value }]) });
                    }
                    _ => {}
                }
            }
            Yaml { node: Some(node), value: Value::Seq(items) }
        }
        "plain_scalar" | "double_quote_scalar" | "single_quote_scalar" | "block_scalar" => scalar(node, src),
        _ => Yaml { node: Some(node), value: Value::Null },
    }
}
