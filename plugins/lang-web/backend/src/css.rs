//! Stylesheet facts: CSS (tree-sitter-css 0.25.0), SCSS (tree-sitter-scss-modern 0.1.0) and Less
//! (tree-sitter-less 1.0.0), all on the kit's tree-sitter 0.25. Also the CSS of a page's `<style>`
//! blocks ([`extract_embedded`]), parsed in place so positions are the page's.
//!
//! Structural rules (node kinds of the three grammars, CSS selector syntax):
//! - A rule is one symbol per selector key it names: `.class` and `#id` keys (kind `class`), element
//!   keys (`button`, kind `type`), Sass placeholders (`%x`, kind `class`). Keys of the subject
//!   compound (the element the rule styles: the last compound of each complex selector) carry no
//!   decorator; keys of ancestors and siblings (`.nav` in `.nav a`) carry the decorator `context`.
//!   Keys inside `:not(...)` are left out; pseudo-classes and pseudo-elements are not keys. A rule
//!   with no key (`:root`, `*`, `[data-x]`) is one `type` symbol named by its selector text. The
//!   symbol's lines, code and body are the whole rule; its signature is the selector list.
//! - Nesting (CSS nesting, SCSS, Less): `&` stands for the enclosing rule's keys (`&:hover` in
//!   `.card` names `.card`); `&__title` / `&-title` appends to the enclosing subject's class keys
//!   (`.card__title`); a nested rule without `&` is a descendant: the enclosing keys are context.
//! - `@media`, `@supports`, `@container`, `@layer`, `@scope`, `@starting-style`, `@document` blocks
//!   are `module` symbols (qname `@media <prelude>`); everything inside them gets the qname prefix
//!   `<block qname>/`. Rules nest under the innermost block or rule (`parent`).
//! - Custom properties (`--x: v`), Sass `$x` and Less `@x` variables are `const` symbols with their
//!   value; the selector that declares a custom property is its decorator (`:root`,
//!   `[data-theme="dark"]`). Their parent is the innermost at-rule block, never a rule (the code
//!   graph reads a constant through the block it is declared in). `var(--x)`, `$x` and `@x` uses
//!   are `Read` references.
//! - `@keyframes n` is a `const` symbol `n` (qname `@keyframes n`); `animation-name` values and the
//!   names in `animation` shorthands (the words that are not animation keywords) are `Read`
//!   references to it.
//! - Sass `@mixin` / `@function` and Less mixin definitions (`.m() {}`) are `function` symbols;
//!   `@include m` and Less mixin calls (`.m();`) are `Call` references; `@extend` / `:extend()`
//!   targets go to the enclosing rule's symbols' `extends`.
//! - `@import`, `@use`, `@forward` and `url()` are imports when they name a repository file (Sass
//!   partial and index lookup, Less `.less` lookup, see [`crate::paths`]): module = as written,
//!   name = `@import` | `@use` | `@forward` | `url`, local = the repository path.

use crate::paths;
use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, RefKind, SymbolFact};
use cc_lang_kit::syntax::{children, has_error, named_children, new_symbol, parse, push_ref, squash, strip_quotes, text};
use cc_lang_kit::tree_sitter::{Language, Node};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Css,
    Scss,
    Less,
}

impl Dialect {
    pub fn language(self) -> Language {
        match self {
            Dialect::Css => tree_sitter_css::LANGUAGE.into(),
            Dialect::Scss => tree_sitter_scss_modern::LANGUAGE.into(),
            Dialect::Less => tree_sitter_less::language(),
        }
    }

    /// The dialect of a `lang@1` language name.
    pub fn of(language: &str) -> Option<Self> {
        match language {
            "css" => Some(Dialect::Css),
            "scss" => Some(Dialect::Scss),
            "less" => Some(Dialect::Less),
            _ => None,
        }
    }
}

/// At-rules whose block groups rules (conditional and cascade blocks).
const GROUPING: &[&str] = &["@media", "@supports", "@container", "@layer", "@scope", "@starting-style", "@document", "@-moz-document"];

/// Words of the `animation` shorthand that are not animation names (CSS Animations: timing
/// functions, iteration count, direction, fill mode, play state, global keywords).
const ANIMATION_KEYWORDS: &[&str] = &[
    "none", "initial", "inherit", "unset", "revert", "revert-layer", "ease", "ease-in", "ease-out", "ease-in-out", "linear", "step-start", "step-end", "infinite", "normal", "reverse",
    "alternate", "alternate-reverse", "forwards", "backwards", "both", "running", "paused", "auto",
];

/// Selector node kinds whose children are other selectors (combinators).
const COMBINATORS: &[&str] = &["descendant_selector", "child_selector", "sibling_selector", "adjacent_sibling_selector"];

/// The keys a rule's selectors name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleKeys {
    pub all: Vec<String>,
    pub subjects: Vec<String>,
}

impl RuleKeys {
    fn add(&mut self, key: String, subject: bool) {
        if key.is_empty() || key.contains("#{") || key.contains("@{") {
            return;
        }
        if subject && !self.subjects.contains(&key) {
            self.subjects.push(key.clone());
        }
        if !self.all.contains(&key) {
            self.all.push(key);
        }
    }

    fn context(&self) -> impl Iterator<Item = &String> {
        self.all.iter().filter(|k| !self.subjects.contains(k))
    }
}

/// The kind of a key's symbol.
pub fn key_kind(key: &str) -> &'static str {
    if key.starts_with(['.', '#', '%']) { "class" } else { "type" }
}

/// Where the subject compound of a complex selector starts (what the selector styles): after its
/// last combinator. The grammars nest combinators inside the compound's other parts
/// (`nav > a#home` is an id selector around a child selector), so this is a byte position, not a
/// node. Combinators inside pseudo-class arguments (`:is(.a .b)`) do not count.
fn subject_start(node: Node) -> usize {
    fn last_combinator(node: Node) -> Option<usize> {
        let own = if COMBINATORS.contains(&node.kind()) { named_children(node).last().map(|last| last.start_byte()) } else { None };
        named_children(node).into_iter().filter(|child| child.kind() != "arguments").filter_map(last_combinator).chain(own).max()
    }
    last_combinator(node).unwrap_or(node.start_byte())
}

struct KeyWalk<'p> {
    parent: Option<&'p RuleKeys>,
    nested: bool,
}

impl KeyWalk<'_> {
    fn parent_keys(&mut self, out: &mut RuleKeys, subject: bool, suffix: Option<&str>) {
        self.nested = true;
        let Some(parent) = self.parent else { return };
        for key in parent.context() {
            out.add(key.clone(), false);
        }
        for key in &parent.subjects {
            match suffix {
                Some(suffix) if key.starts_with(['.', '#', '%']) => out.add(format!("{key}{suffix}"), subject),
                Some(_) => {}
                None => out.add(key.clone(), subject),
            }
        }
    }

    fn walk(&mut self, node: Node, src: &str, subject_start: usize, out: &mut RuleKeys) {
        let subject = |at: usize| at >= subject_start;
        match node.kind() {
            "class_selector" | "id_selector" => {
                let parts = children(node);
                let marker = parts.iter().any(|c| matches!(c.kind(), "." | "#"));
                let name_node = parts.iter().find(|c| matches!(c.kind(), "class_name" | "id_name"));
                let name = name_node.map(|c| text(*c, src)).unwrap_or("");
                let at = name_node.map(|c| c.start_byte()).unwrap_or(node.end_byte());
                for part in &parts {
                    if part.is_named() && !matches!(part.kind(), "class_name" | "id_name" | "nesting_selector") {
                        self.walk(*part, src, subject_start, out);
                    }
                }
                let nesting = parts.iter().find(|c| c.kind() == "nesting_selector");
                if marker {
                    if let Some(nesting) = nesting {
                        self.parent_keys(out, subject(nesting.start_byte()), None);
                    }
                    let sigil = if node.kind() == "class_selector" { '.' } else { '#' };
                    out.add(format!("{sigil}{name}"), subject(at));
                } else if let Some(nesting) = nesting {
                    // `&__title`, `&-title`: the enclosing subject with a suffix.
                    self.parent_keys(out, subject(nesting.start_byte()), Some(name));
                }
            }
            "tag_name" => {
                let name = text(node, src);
                if !name.is_empty() {
                    out.add(name.to_ascii_lowercase(), subject(node.start_byte()));
                }
            }
            "placeholder" => out.add(squash(text(node, src)), subject(node.start_byte())),
            "nesting_selector" => self.parent_keys(out, subject(node.start_byte()), None),
            "pseudo_class_selector" => {
                let parts = named_children(node);
                let name = parts.iter().find(|c| c.kind() == "class_name").map(|c| text(*c, src).to_ascii_lowercase()).unwrap_or_default();
                for part in parts {
                    match part.kind() {
                        "class_name" => {}
                        "arguments" if name == "not" => {}
                        _ => self.walk(part, src, subject_start, out),
                    }
                }
            }
            "pseudo_element_selector" => {
                // `a::after`: the selector before `::`; the pseudo-element's name is not a key.
                for part in children(node) {
                    if part.kind() == "::" {
                        break;
                    }
                    if part.is_named() {
                        self.walk(part, src, subject_start, out);
                    }
                }
            }
            "attribute_selector" => {
                for part in named_children(node) {
                    if !matches!(part.kind(), "attribute_name" | "string_value" | "plain_value" | "identifier") {
                        self.walk(part, src, subject_start, out);
                    }
                }
            }
            "class_name" | "id_name" | "attribute_name" | "string_value" | "plain_value" | "integer_value" | "universal_selector" | "comment" => {}
            _ => {
                for part in named_children(node) {
                    self.walk(part, src, subject_start, out);
                }
            }
        }
    }
}

/// The keys of a `selectors` node (or one selector), with the enclosing rule's keys for nesting.
pub fn rule_keys(selectors: Node, src: &str, parent: Option<&RuleKeys>) -> RuleKeys {
    let mut out = RuleKeys::default();
    let complex: Vec<Node> = if selectors.kind() == "selectors" { named_children(selectors) } else { vec![selectors] };
    for selector in complex {
        if selector.kind() == "comment" {
            continue;
        }
        let mut walk = KeyWalk { parent, nested: false };
        let mut keys = RuleKeys::default();
        walk.walk(selector, src, subject_start(selector), &mut keys);
        // A nested selector without `&` is a descendant of the enclosing rule.
        if !walk.nested
            && let Some(parent) = parent
        {
            for key in &parent.all {
                keys.add(key.clone(), false);
            }
        }
        for key in &keys.all {
            out.add(key.clone(), keys.subjects.contains(key));
        }
    }
    out
}

#[derive(Clone, Default)]
struct Scope {
    /// Innermost at-rule block or `<style>` block: (symbol, qname). Constants' parent, qname prefix.
    module: Option<(usize, String)>,
    /// Innermost container symbol (block or rule): rules' parent.
    parent: Option<usize>,
    /// The enclosing rule's keys and symbols.
    rule: Option<(RuleKeys, Vec<usize>)>,
    /// Where references come from.
    owner: Option<usize>,
    /// The enclosing rule's selector text.
    selector: Option<String>,
}

impl Scope {
    fn qname(&self, name: &str) -> String {
        match &self.module {
            Some((_, prefix)) => format!("{prefix}/{name}"),
            None => name.to_owned(),
        }
    }
}

struct Extractor<'a> {
    src: &'a str,
    path: &'a str,
    root: Option<&'a Path>,
    dialect: Dialect,
    /// Directory relative `url()` / `@import` resolve against (a page's base for `<style>`).
    url_base: Option<String>,
    facts: &'a mut FileFacts,
}

/// The text of a declaration's value: after the `:`, without the closing `;`.
fn value_text(node: Node, src: &str) -> String {
    let parts = children(node);
    let Some(colon) = parts.iter().position(|c| c.kind() == ":") else { return String::new() };
    let start = parts.get(colon + 1).map(|c| c.start_byte()).unwrap_or(node.end_byte());
    let end = parts.iter().rev().find(|c| c.kind() != ";").map(|c| c.end_byte()).unwrap_or(node.end_byte());
    squash(src.get(start..end.max(start)).unwrap_or(""))
}

impl Extractor<'_> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        self.facts.symbols.push(symbol);
        self.facts.symbols.len() - 1
    }

    fn read(&mut self, from: Option<usize>, name: &str, node: Node) {
        push_ref(&mut self.facts.refs, from, RefKind::Read, name, None, Vec::new(), node);
    }

    fn import(&mut self, written: &str, name: &str, resolved: Option<String>, node: Node) {
        let Some(local) = resolved else { return };
        let fact = ImportFact { module: written.to_owned(), name: name.to_owned(), local, line: cc_lang_kit::syntax::line(node) };
        if !self.facts.imports.contains(&fact) {
            self.facts.imports.push(fact);
        }
    }

    fn statements(&mut self, node: Node, scope: &Scope) {
        for child in named_children(node) {
            self.statement(child, scope);
        }
    }

    fn statement(&mut self, node: Node, scope: &Scope) {
        match node.kind() {
            "rule_set" => self.rule(node, scope),
            "declaration" => self.declaration(node, scope),
            "media_statement" | "supports_statement" | "container_statement" | "scope_statement" => self.at_block(node, scope),
            "at_rule" => {
                let keyword = named_children(node).into_iter().find(|c| c.kind() == "at_keyword").map(|c| text(c, self.src).to_ascii_lowercase()).unwrap_or_default();
                let block = named_children(node).into_iter().find(|c| c.kind() == "block");
                match block {
                    Some(_) if GROUPING.contains(&keyword.as_str()) => self.at_block(node, scope),
                    // Descriptor blocks (`@font-face`, `@page`, `@property`): their values only.
                    Some(block) => self.statements(block, scope),
                    None => self.values(node, scope.owner),
                }
            }
            "keyframes_statement" => self.keyframes(node, scope),
            "import_statement" | "use_statement" | "forward_statement" => self.import_statement(node),
            "mixin_statement" if self.dialect == Dialect::Scss => self.definition(node, scope),
            "function_statement" | "mixin_definition" => self.definition(node, scope),
            "include_statement" => {
                if let Some(name) = named_children(node).into_iter().find(|c| c.kind() == "identifier") {
                    push_ref(&mut self.facts.refs, scope.owner, RefKind::Call, text(name, self.src), None, Vec::new(), node);
                }
                for part in named_children(node) {
                    match part.kind() {
                        "block" => self.statements(part, scope),
                        "identifier" => {}
                        _ => self.values(part, scope.owner),
                    }
                }
            }
            "mixin_statement" => {
                // Less `.m(args);` / `#ns.m();`.
                if let Some(name) = named_children(node).into_iter().find(|c| c.kind() == "function_name") {
                    push_ref(&mut self.facts.refs, scope.owner, RefKind::Call, &squash(text(name, self.src)), None, Vec::new(), node);
                }
                for part in named_children(node).into_iter().filter(|c| c.kind() == "arguments") {
                    self.values(part, scope.owner);
                }
            }
            "extend_statement" => self.extend(node, scope),
            "comment" | "js_comment" => {}
            "block" | "ERROR" | "else_clause" | "else_if_clause" | "each_statement" | "for_statement" | "while_statement" | "if_statement" | "at_root_statement" => {
                for part in named_children(node) {
                    match part.kind() {
                        "block" | "else_clause" | "else_if_clause" | "rule_set" | "declaration" => self.statement(part, scope),
                        _ => self.values(part, scope.owner),
                    }
                }
            }
            _ => self.values(node, scope.owner),
        }
    }

    fn rule(&mut self, node: Node, scope: &Scope) {
        let Some(selectors) = named_children(node).into_iter().find(|c| c.kind() == "selectors") else { return };
        let block = named_children(node).into_iter().find(|c| c.kind() == "block");
        let keys = rule_keys(selectors, self.src, scope.rule.as_ref().map(|(k, _)| k));
        let selector_text = squash(text(selectors, self.src));
        let mut symbols = Vec::new();
        if keys.all.is_empty() {
            let mut symbol = new_symbol("type", &selector_text, scope.qname(&selector_text), node, block, self.src);
            symbol.parent = scope.parent;
            symbols.push(self.push(symbol));
        }
        for key in &keys.all {
            let mut symbol = new_symbol(key_kind(key), key, scope.qname(key), node, block, self.src);
            symbol.parent = scope.parent;
            if !keys.subjects.contains(key) {
                symbol.decorators.push("context".into());
            }
            symbols.push(self.push(symbol));
        }
        let inner = Scope {
            module: scope.module.clone(),
            parent: symbols.first().copied(),
            owner: symbols.first().copied(),
            rule: Some((keys, symbols)),
            selector: Some(selector_text),
        };
        if let Some(block) = block {
            self.statements(block, &inner);
        }
    }

    fn declaration(&mut self, node: Node, scope: &Scope) {
        let Some(property) = named_children(node).into_iter().find(|c| c.kind() == "property_name") else {
            self.values(node, scope.owner);
            return;
        };
        let name = text(property, self.src).to_owned();
        let variable = name.starts_with("--") || (self.dialect == Dialect::Scss && name.starts_with('$')) || (self.dialect == Dialect::Less && name.starts_with('@'));
        let mut from = scope.owner;
        if variable {
            let mut symbol = new_symbol("const", &name, scope.qname(&name), node, None, self.src);
            symbol.value = Some(value_text(node, self.src)).filter(|v| !v.is_empty());
            symbol.parent = scope.module.as_ref().map(|(index, _)| *index);
            if let Some(selector) = &scope.selector {
                symbol.decorators.push(selector.clone());
            }
            from = Some(self.push(symbol));
        }
        let lowered = name.to_ascii_lowercase();
        // `-webkit-animation`: the vendor prefix is not part of the property.
        let unprefixed = lowered.strip_prefix('-').and_then(|rest| rest.split_once('-')).map(|(_, rest)| rest).unwrap_or(&lowered);
        let animation = !variable && matches!(unprefixed, "animation" | "animation-name");
        for part in named_children(node) {
            if part.kind() == "property_name" {
                continue;
            }
            if animation && part.kind() == "plain_value" {
                let word = text(part, self.src);
                if !ANIMATION_KEYWORDS.contains(&word.to_ascii_lowercase().as_str()) {
                    self.read(from, word, part);
                }
                continue;
            }
            if part.kind() == "block" {
                // SCSS nested properties (`font: { family: x }`).
                self.statements(part, scope);
                continue;
            }
            self.values(part, from);
        }
    }

    /// References and imports in a value: `var(--x)`, `$x` / `@x`, `url(...)`.
    fn values(&mut self, node: Node, from: Option<usize>) {
        match node.kind() {
            "call_expression" => {
                let function = named_children(node).into_iter().find(|c| c.kind() == "function_name").map(|c| text(c, self.src).to_ascii_lowercase()).unwrap_or_default();
                let arguments: Vec<Node> = named_children(node).into_iter().filter(|c| c.kind() == "arguments").flat_map(named_children).collect();
                match function.as_str() {
                    "var" => {
                        for (i, argument) in arguments.iter().enumerate() {
                            let word = text(*argument, self.src);
                            if i == 0 && word.starts_with("--") {
                                self.read(from, word, *argument);
                            } else {
                                self.values(*argument, from);
                            }
                        }
                    }
                    "url" => {
                        if let Some(argument) = arguments.first() {
                            let written = strip_quotes(text(*argument, self.src).trim());
                            let resolved = paths::resolve(self.root, self.path, self.url_base.as_deref(), &written);
                            self.import(&written, "url", resolved, node);
                        }
                    }
                    _ => {
                        for argument in arguments {
                            self.values(argument, from);
                        }
                    }
                }
            }
            "variable" => {
                let name = text(node, self.src);
                if name.len() > 1 {
                    self.read(from, name, node);
                }
            }
            "block" => {}
            _ => {
                for child in named_children(node) {
                    self.values(child, from);
                }
            }
        }
    }

    fn at_block(&mut self, node: Node, scope: &Scope) {
        let block = named_children(node).into_iter().find(|c| c.kind() == "block");
        let keyword = children(node).first().map(|c| text(*c, self.src).to_ascii_lowercase()).unwrap_or_default();
        let mut symbol = new_symbol("module", &keyword, String::new(), node, block, self.src);
        symbol.qname = scope.qname(&symbol.signature);
        symbol.parent = scope.parent;
        let qname = symbol.qname.clone();
        let index = self.push(symbol);
        // Values of the prelude (`@media (min-width: $bp)`).
        for part in named_children(node) {
            if part.kind() != "block" {
                self.values(part, scope.owner.or(Some(index)));
            }
        }
        let inner = Scope {
            module: Some((index, qname)),
            parent: Some(index),
            rule: scope.rule.clone(),
            owner: scope.owner.or(Some(index)),
            selector: scope.selector.clone(),
        };
        if let Some(block) = block {
            self.statements(block, &inner);
        }
    }

    fn keyframes(&mut self, node: Node, scope: &Scope) {
        let Some(name) = named_children(node).into_iter().find(|c| c.kind() == "keyframes_name") else { return };
        let name = text(name, self.src).to_owned();
        let body = named_children(node).into_iter().find(|c| c.kind() == "keyframe_block_list");
        let mut symbol = new_symbol("const", &name, scope.qname(&format!("@keyframes {name}")), node, body, self.src);
        symbol.parent = scope.module.as_ref().map(|(index, _)| *index);
        let index = self.push(symbol);
        if let Some(body) = body {
            let inner = Scope { owner: Some(index), rule: None, selector: None, ..scope.clone() };
            for frame in named_children(body) {
                for block in named_children(frame).into_iter().filter(|c| c.kind() == "block") {
                    self.statements(block, &inner);
                }
            }
        }
    }

    fn definition(&mut self, node: Node, scope: &Scope) {
        let parts = named_children(node);
        let name = match node.kind() {
            // Less `.m(@a) {}` / `#ns() {}`: the selector text before the parameters.
            "mixin_definition" => {
                let end = parts.iter().find(|c| matches!(c.kind(), "parameters" | "block")).map(|c| c.start_byte()).unwrap_or(node.end_byte());
                squash(self.src.get(node.start_byte()..end).unwrap_or(""))
            }
            _ => node.child_by_field_name("name").or_else(|| parts.iter().copied().find(|c| c.kind() == "identifier")).map(|c| text(c, self.src).to_owned()).unwrap_or_default(),
        };
        if name.is_empty() {
            return;
        }
        let block = parts.iter().copied().find(|c| c.kind() == "block");
        let mut symbol = new_symbol("function", &name, scope.qname(&name), node, block, self.src);
        symbol.parent = scope.parent;
        if let Some(parameters) = parts.iter().find(|c| c.kind() == "parameters") {
            for (position, parameter) in named_children(*parameters).into_iter().enumerate() {
                let words: Vec<Node> = named_children(parameter);
                let param_name = words.first().map(|w| text(*w, self.src)).unwrap_or_else(|| text(parameter, self.src));
                let mut param = cc_lang_kit::syntax::param(param_name, position as u32);
                param.default_value = words.get(1).map(|d| squash(text(*d, self.src)));
                param.optional = param.default_value.is_some();
                param.variadic = text(parameter, self.src).ends_with("...");
                symbol.params.push(param);
            }
        }
        let index = self.push(symbol);
        let inner = Scope { owner: Some(index), parent: Some(index), rule: None, selector: None, module: scope.module.clone() };
        if let Some(block) = block {
            self.statements(block, &inner);
        }
    }

    fn extend(&mut self, node: Node, scope: &Scope) {
        let Some((_, symbols)) = &scope.rule else { return };
        let mut targets = RuleKeys::default();
        for part in named_children(node) {
            let selectors: Vec<Node> = if part.kind() == "arguments" { named_children(part) } else { vec![part] };
            for selector in selectors {
                if selector.kind() == "nesting_selector" {
                    continue;
                }
                for key in rule_keys(selector, self.src, None).all {
                    targets.add(key, true);
                }
            }
        }
        for index in symbols.clone() {
            for key in targets.all.iter().filter(|k| k.starts_with(['.', '#', '%'])) {
                if let Some(symbol) = self.facts.symbols.get_mut(index)
                    && !symbol.extends.contains(key)
                {
                    symbol.extends.push(key.clone());
                }
            }
        }
    }

    fn import_statement(&mut self, node: Node) {
        let keyword = children(node).first().map(|c| text(*c, self.src).to_ascii_lowercase()).unwrap_or_default();
        for part in named_children(node) {
            let (written, via_url) = match part.kind() {
                "string_value" => (strip_quotes(text(part, self.src)), false),
                "call_expression" => {
                    let function = named_children(part).into_iter().find(|c| c.kind() == "function_name").map(|c| text(c, self.src).to_ascii_lowercase());
                    if function.as_deref() != Some("url") {
                        continue;
                    }
                    let argument = named_children(part).into_iter().filter(|c| c.kind() == "arguments").flat_map(named_children).next();
                    (argument.map(|a| strip_quotes(text(a, self.src).trim())).unwrap_or_default(), true)
                }
                _ => continue,
            };
            let resolved = match self.dialect {
                Dialect::Scss if !via_url => paths::resolve_sass(self.root, self.path, &written),
                Dialect::Less if !via_url => paths::resolve_less(self.root, self.path, &written),
                _ => paths::resolve(self.root, self.path, self.url_base.as_deref(), &written),
            };
            self.import(&written, &keyword, resolved, node);
        }
    }
}

/// Facts of one stylesheet.
pub fn extract(dialect: Dialect, path: &str, text: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Some(tree) = parse(&dialect.language(), text) else { return };
    let node = tree.root_node();
    facts.parse_errors += has_error(node);
    let mut extractor = Extractor { src: text, path, root, dialect, url_base: None, facts };
    extractor.statements(node, &Scope::default());
}

/// Facts of CSS embedded in a page (`<style>`): `node` is the root of a CSS tree parsed over the
/// page's text with included ranges, so its positions are the page's. Everything is placed under
/// the block's symbol `parent` (qname prefix `parent_qname/`); relative URLs resolve against
/// `url_base` (the page's directory or `<base href>`).
pub fn extract_embedded(node: Node, page: &str, path: &str, root: Option<&Path>, url_base: Option<String>, parent: (usize, String), facts: &mut FileFacts) {
    facts.parse_errors += has_error(node);
    let index = parent.0;
    let mut extractor = Extractor { src: page, path, root, dialect: Dialect::Css, url_base, facts };
    let scope = Scope { module: Some(parent), parent: Some(index), owner: Some(index), ..Scope::default() };
    extractor.statements(node, &scope);
}
