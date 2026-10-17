//! Page facts: HTML (tree-sitter-html 0.23.2 on the kit's tree-sitter 0.25), with the CSS of its
//! `<style>` blocks (tree-sitter-css over the page's own text, see [`crate::css::extract_embedded`]).
//!
//! Structural rules (HTML element and attribute semantics, by name):
//! - An element with an `id` is a `variable` symbol named by the id (qname `#id`, `type_text` the
//!   tag, signature the start tag, lines the element; a void element is its start tag).
//! - A `<form>` is a `class` symbol (name: its id, else its `name`, else `form`; qname `form#id`,
//!   `form:name` or `form`; `http_method` its method, GET by default; `route_path` its action as
//!   written; decorators `enctype=`, `novalidate`, `target=`); a form with an action is also a
//!   request (method, the action's path, the field names as arguments). Its fields are the
//!   `input`, `select`, `textarea` and `button` elements with a `name` inside it or naming it with
//!   `form="id"`: `field` symbols (qname `<form qname>.<name>`, `type_text` the input type or tag,
//!   `value` the value attribute, decorators the validation and input attributes as written). A
//!   name repeated in one form (radio and checkbox groups) is one field.
//! - Headings `<h1>`..`<h6>` are the outline: `module` symbols named by their text (`type_text`
//!   `h2`), each spanning its section (up to the next heading of the same or a higher level), nested
//!   under the enclosing heading.
//! - `<style>` blocks are `module` symbols (qname `<style>`, `<style>~2`, ...) whose CSS facts are
//!   extracted in place (rules, custom properties, `var()` reads, `url()` imports, see
//!   [`crate::css`]); inline `<script>` blocks are `module` symbols (qname `<script>`, `type_text`
//!   the `type` attribute). Their JavaScript is not parsed here.
//! - References, from the innermost symbol around the attribute: each `class` token is an
//!   `Instantiate` reference to `.token` and each `id` an `Instantiate` reference to `#id` (the
//!   names of CSS rule symbols); `for`, `form`, `list`, `headers`, `popovertarget` and the
//!   `aria-*` id references, and `href="#x"`, are `Read` references to the id `x`. Attribute values
//!   with template syntax are skipped.
//! - Imports: `script[src]`, `link[href]`, `img[src|srcset]`, `source[src|srcset]`,
//!   `video[src|poster]`, `audio[src]`, `track[src]`, `iframe[src]`, `embed[src]`, `object[data]`,
//!   `input[src]`, `a[href]`, `area[href]`, `use[href]` and `image[href]` (SVG) that name a
//!   repository file ([`crate::paths`], relative URLs against `<base href>` when the page has a
//!   local one): module = the URL as written, name = `<tag>.<attribute>`, local = the repository
//!   path.

use crate::css;
use crate::paths;
use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, ImportFact, RefFact, RefKind, RefTarget, RequestFact, SymbolFact};
use cc_lang_kit::manifest::dir_of;
use cc_lang_kit::syntax::{children, end_line, has_error, hash_hex, line, named_children, parse, squash, text};
use cc_lang_kit::tree_sitter::{Language, Node, Parser, Range};
use std::collections::HashMap;
use std::path::Path;

pub fn language() -> Language {
    tree_sitter_html::LANGUAGE.into()
}

/// Elements without content or end tag.
const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// Attributes whose value is a list of ids of the same page.
const ID_REFERENCES: &[&str] = &[
    "for",
    "form",
    "list",
    "headers",
    "popovertarget",
    "aria-labelledby",
    "aria-describedby",
    "aria-controls",
    "aria-owns",
    "aria-activedescendant",
    "aria-details",
    "aria-errormessage",
    "aria-flowto",
];

/// (tag, attribute) pairs whose value names a resource; `*set` attributes hold a srcset list.
const URL_ATTRIBUTES: &[(&str, &str)] = &[
    ("script", "src"),
    ("link", "href"),
    ("img", "src"),
    ("img", "srcset"),
    ("source", "src"),
    ("source", "srcset"),
    ("video", "src"),
    ("video", "poster"),
    ("audio", "src"),
    ("track", "src"),
    ("iframe", "src"),
    ("embed", "src"),
    ("object", "data"),
    ("input", "src"),
    ("a", "href"),
    ("area", "href"),
    ("use", "href"),
    ("use", "xlink:href"),
    ("image", "href"),
];

/// Field attributes kept as decorators (validation and input behaviour).
const FIELD_ATTRIBUTES: &[&str] = &["required", "pattern", "minlength", "maxlength", "min", "max", "step", "multiple", "accept", "autocomplete", "inputmode", "disabled", "readonly"];

const FIELD_TAGS: &[&str] = &["input", "select", "textarea", "button"];

/// Decode the character references an attribute value or text may hold.
pub fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether an attribute value holds template syntax (server-side or client-side templates).
fn templated(value: &str) -> bool {
    ["{{", "{%", "${", "<%", "<?"].iter().any(|marker| value.contains(marker))
}

/// A start tag: the element's name, attributes (lowercase names, decoded values) and node.
struct Tag<'t> {
    name: String,
    attributes: Vec<(String, Option<String>, Node<'t>)>,
    node: Node<'t>,
}

impl Tag<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(n, _, _)| n == name).map(|(_, v, _)| v.as_deref().unwrap_or(""))
    }

    fn has(&self, name: &str) -> bool {
        self.attributes.iter().any(|(n, _, _)| n == name)
    }
}

fn tag_of<'t>(element: Node<'t>, src: &str) -> Option<Tag<'t>> {
    let node = children(element).into_iter().find(|c| matches!(c.kind(), "start_tag" | "self_closing_tag"))?;
    let mut name = String::new();
    let mut attributes = Vec::new();
    for part in named_children(node) {
        match part.kind() {
            "tag_name" => name = text(part, src).to_ascii_lowercase(),
            "attribute" => {
                let parts = named_children(part);
                let Some(attribute) = parts.iter().find(|c| c.kind() == "attribute_name") else { continue };
                let value = parts.iter().find_map(|c| match c.kind() {
                    "attribute_value" => Some(text(*c, src).to_owned()),
                    "quoted_attribute_value" => Some(named_children(*c).first().map(|v| text(*v, src).to_owned()).unwrap_or_default()),
                    _ => None,
                });
                attributes.push((text(*attribute, src).to_ascii_lowercase(), value.map(|v| decode_entities(&v)), part));
            }
            _ => {}
        }
    }
    Some(Tag { name, attributes, node })
}

/// The element's range as a symbol: a void element is its start tag.
fn extent<'t>(element: Node<'t>, tag: &Tag<'t>) -> Node<'t> {
    if VOID.contains(&tag.name.as_str()) || tag.node.kind() == "self_closing_tag" { tag.node } else { element }
}

/// A symbol of an element: lines and code of the element, signature the start tag, body hash of
/// what follows the start tag.
fn element_symbol(kind: &str, name: &str, qname: String, element: Node, tag: &Tag, src: &str) -> SymbolFact {
    let decl = extent(element, tag);
    let signature = squash(text(tag.node, src));
    let body = src.get(tag.node.end_byte()..decl.end_byte()).unwrap_or("");
    SymbolFact {
        kind: kind.to_owned(),
        name: name.to_owned(),
        qname,
        sig_hash: hash_hex(signature.as_bytes()),
        signature,
        body_hash: if body.is_empty() { String::new() } else { hash_hex(body.as_bytes()) },
        code: text(decl, src).to_owned(),
        start_line: line(decl),
        end_line: end_line(decl),
        start_byte: decl.start_byte() as u32,
        end_byte: decl.end_byte() as u32,
        type_text: Some(tag.name.clone()),
        ..SymbolFact::default()
    }
}

/// 1-based line of the last character before byte `end` (a range ending at a line start ends on
/// the line before).
fn line_before(src: &str, end: usize) -> u32 {
    let end = end.min(src.len());
    let before = src.get(..end).unwrap_or(src);
    let trimmed = before.strip_suffix('\n').unwrap_or(before);
    trimmed.matches('\n').count() as u32 + 1
}

struct Element<'t> {
    node: Node<'t>,
    tag: Tag<'t>,
}

struct Field {
    form: Option<usize>,
    form_id: Option<String>,
    element: usize,
}

struct Walk<'t> {
    elements: Vec<Element<'t>>,
    /// Indices into `elements` by role, in document order.
    ids: Vec<usize>,
    forms: Vec<usize>,
    fields: Vec<Field>,
    headings: Vec<usize>,
    styles: Vec<Node<'t>>,
    scripts: Vec<usize>,
    base: Option<String>,
}

fn walk<'t>(node: Node<'t>, src: &str, form: Option<usize>, state: &mut Walk<'t>) {
    let mut form = form;
    if matches!(node.kind(), "element" | "script_element" | "style_element")
        && let Some(tag) = tag_of(node, src)
    {
        let index = state.elements.len();
        let name = tag.name.clone();
        if tag.get("id").is_some_and(|id| !id.trim().is_empty()) {
            state.ids.push(index);
        }
        match name.as_str() {
            "form" => {
                state.forms.push(index);
                form = Some(index);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => state.headings.push(index),
            "base" if state.base.is_none() => state.base = tag.get("href").map(str::to_owned),
            "style" if node.kind() == "style_element" => state.styles.push(node),
            "script" if node.kind() == "script_element" && !tag.has("src") => state.scripts.push(index),
            _ => {}
        }
        if FIELD_TAGS.contains(&name.as_str()) && tag.get("name").is_some_and(|n| !n.trim().is_empty()) {
            state.fields.push(Field { form, form_id: tag.get("form").map(str::to_owned), element: index });
        }
        state.elements.push(Element { node, tag });
    }
    for child in named_children(node) {
        if matches!(child.kind(), "element" | "script_element" | "style_element" | "ERROR" | "document") {
            walk(child, src, form, state);
        }
    }
}

/// The visible text of an element (text and character references), whitespace collapsed.
fn inner_text(node: Node, src: &str) -> String {
    fn collect(node: Node, src: &str, out: &mut String) {
        for child in named_children(node) {
            match child.kind() {
                "text" | "entity" => {
                    out.push_str(&decode_entities(text(child, src)));
                    out.push(' ');
                }
                "element" => collect(child, src, out),
                _ => {}
            }
        }
    }
    let mut out = String::new();
    collect(node, src, &mut out);
    squash(&out)
}

struct Page<'a> {
    src: &'a str,
    path: &'a str,
    root: Option<&'a Path>,
    base_dir: Option<String>,
    /// The page's base names another site: its relative URLs name no repository file.
    base_foreign: bool,
    facts: &'a mut FileFacts,
    /// Symbols of the page itself (not of its CSS): (index, start byte, end byte).
    scopes: Vec<(usize, usize, usize)>,
}

impl Page<'_> {
    fn push(&mut self, symbol: SymbolFact) -> usize {
        let range = (symbol.start_byte as usize, symbol.end_byte as usize);
        self.facts.symbols.push(symbol);
        let index = self.facts.symbols.len() - 1;
        self.scopes.push((index, range.0, range.1));
        index
    }

    /// The innermost page symbol around byte `at`.
    fn scope_at(&self, at: usize) -> Option<usize> {
        // Equal ranges (a form or field that also has an id): the later, more specific symbol.
        self.scopes.iter().filter(|(_, start, end)| *start <= at && at < *end).min_by_key(|(index, start, end)| (end - start, std::cmp::Reverse(*index))).map(|(index, _, _)| *index)
    }

    fn reference(&mut self, kind: RefKind, name: String, node: Node) {
        if name.is_empty() {
            return;
        }
        let from = self.scope_at(node.start_byte());
        self.facts.refs.push(RefFact { from, kind, target: RefTarget { name, qualifier: None }, args: Vec::new(), line: line(node) });
    }

    fn resolve(&self, url: &str) -> Option<String> {
        if self.base_foreign && !url.trim_start().starts_with('/') {
            return None;
        }
        paths::resolve(self.root, self.path, self.base_dir.as_deref(), url)
    }
}

fn headings(page: &mut Page, walk: &Walk) {
    let src = page.src;
    // The last section ends before `</body>` (or with the page).
    let body_end = walk
        .elements
        .iter()
        .find(|e| e.tag.name == "body")
        .map(|e| children(e.node).into_iter().find(|c| c.kind() == "end_tag").map(|t| t.start_byte()).unwrap_or(e.node.end_byte()))
        .unwrap_or(src.len());
    let list: Vec<(u32, usize, String)> = walk
        .headings
        .iter()
        .map(|i| {
            let element = &walk.elements[*i];
            let level = element.tag.name[1..].parse::<u32>().unwrap_or(1);
            (level, *i, inner_text(element.node, src))
        })
        .collect();
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for (position, (level, element, title)) in list.iter().enumerate() {
        if title.is_empty() {
            continue;
        }
        let node = walk.elements[*element].node;
        let next = list[position + 1..].iter().find(|(l, _, _)| l <= level).map(|(_, e, _)| walk.elements[*e].node.start_byte()).unwrap_or(body_end);
        let start = node.start_byte();
        let end = src.get(start..next.max(start)).map(|section| start + section.trim_end().len()).unwrap_or(node.end_byte()).max(node.end_byte());
        while stack.last().is_some_and(|(l, _)| l >= level) {
            stack.pop();
        }
        let name: String = title.chars().take(120).collect();
        let tag = &walk.elements[*element].tag.name;
        let signature = format!("{tag} {name}");
        let code = src.get(start..end).unwrap_or("").to_owned();
        let body = src.get(walk.elements[*element].tag.node.end_byte().min(end)..end).unwrap_or("");
        let symbol = SymbolFact {
            kind: "module".into(),
            name: name.clone(),
            qname: name,
            sig_hash: hash_hex(signature.as_bytes()),
            signature,
            body_hash: hash_hex(body.as_bytes()),
            code,
            start_line: line(node),
            end_line: line_before(src, end),
            start_byte: start as u32,
            end_byte: end as u32,
            parent: stack.last().map(|(_, index)| *index),
            type_text: Some(tag.clone()),
            ..SymbolFact::default()
        };
        let index = page.push(symbol);
        stack.push((*level, index));
    }
}

fn forms_and_fields(page: &mut Page, walk: &Walk) {
    let src = page.src;
    let mut form_symbols: HashMap<usize, usize> = HashMap::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for element in &walk.forms {
        let Element { node, tag } = &walk.elements[*element];
        let id = tag.get("id").map(str::trim).filter(|i| !i.is_empty());
        let form_name = tag.get("name").map(str::trim).filter(|n| !n.is_empty());
        let (name, qname) = match (id, form_name) {
            (Some(id), _) => (id.to_owned(), format!("form#{id}")),
            (None, Some(n)) => (n.to_owned(), format!("form:{n}")),
            (None, None) => ("form".to_owned(), "form".to_owned()),
        };
        let mut symbol = element_symbol("class", &name, qname, *node, tag, src);
        symbol.http_method = Some(tag.get("method").map(str::trim).filter(|m| !m.is_empty()).unwrap_or("get").to_ascii_uppercase());
        symbol.route_path = tag.get("action").map(str::to_owned);
        for attribute in ["enctype", "novalidate", "target", "autocomplete"] {
            match tag.get(attribute) {
                Some("") => symbol.decorators.push(attribute.to_owned()),
                Some(value) => symbol.decorators.push(format!("{attribute}={value}")),
                None => {}
            }
        }
        let index = page.push(symbol);
        form_symbols.insert(*element, index);
        if let Some(id) = id {
            by_id.entry(id.to_owned()).or_insert(index);
        }
    }
    let mut seen: Vec<(usize, String)> = Vec::new();
    let mut names: HashMap<usize, Vec<String>> = HashMap::new();
    for field in &walk.fields {
        let form = match field.form_id.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(id) => by_id.get(id).copied(),
            None => field.form.and_then(|f| form_symbols.get(&f).copied()),
        };
        let Some(form) = form else { continue };
        let Element { node, tag } = &walk.elements[field.element];
        let name = tag.get("name").unwrap_or("").trim().to_owned();
        if seen.contains(&(form, name.clone())) {
            continue;
        }
        seen.push((form, name.clone()));
        let form_qname = page.facts.symbols[form].qname.clone();
        let mut symbol = element_symbol("field", &name, format!("{form_qname}.{name}"), *node, tag, src);
        symbol.parent = Some(form);
        symbol.type_text = Some(match tag.name.as_str() {
            "input" => tag.get("type").map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).unwrap_or_else(|| "text".into()),
            "button" => format!("button:{}", tag.get("type").map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).unwrap_or_else(|| "submit".into())),
            other => other.to_owned(),
        });
        symbol.value = tag.get("value").map(str::to_owned);
        for attribute in FIELD_ATTRIBUTES {
            match tag.get(attribute) {
                Some("") => symbol.decorators.push((*attribute).to_owned()),
                Some(value) => symbol.decorators.push(format!("{attribute}={value}")),
                None => {}
            }
        }
        page.push(symbol);
        names.entry(form).or_default().push(name);
    }
    // A form with an action submits to it: a request, with the field names as arguments.
    for element in &walk.forms {
        let index = form_symbols[element];
        let symbol = &page.facts.symbols[index];
        let Some(action) = symbol.route_path.clone().filter(|a| paths::is_local(a)) else { continue };
        let url = paths::url_path(&action);
        if url.is_empty() {
            continue;
        }
        let request = RequestFact {
            from: Some(index),
            method: symbol.http_method.clone().unwrap_or_else(|| "GET".into()),
            url,
            line: symbol.start_line,
            args: names.get(&index).cloned().unwrap_or_default(),
        };
        page.facts.requests.push(request);
    }
}

fn ids(page: &mut Page, walk: &Walk) {
    for element in &walk.ids {
        let Element { node, tag } = &walk.elements[*element];
        let id = tag.get("id").unwrap_or("").trim().to_owned();
        if templated(&id) {
            continue;
        }
        let symbol = element_symbol("variable", &id, format!("#{id}"), *node, tag, page.src);
        page.push(symbol);
    }
}

fn blocks(page: &mut Page, walk: &Walk) {
    let src = page.src;
    let mut style_count = 0;
    for style in &walk.styles {
        let Some(tag) = tag_of(*style, src) else { continue };
        style_count += 1;
        let qname = if style_count == 1 { "<style>".to_owned() } else { format!("<style>~{style_count}") };
        let mut symbol = element_symbol("module", "style", qname.clone(), *style, &tag, src);
        if let Some(media) = tag.get("media") {
            symbol.decorators.push(format!("media={media}"));
        }
        let index = page.push(symbol);
        let Some(raw) = named_children(*style).into_iter().find(|c| c.kind() == "raw_text") else { continue };
        let mut parser = Parser::new();
        if parser.set_language(&css::Dialect::Css.language()).is_err() {
            continue;
        }
        let range = Range { start_byte: raw.start_byte(), end_byte: raw.end_byte(), start_point: raw.start_position(), end_point: raw.end_position() };
        if parser.set_included_ranges(&[range]).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(src, None) else { continue };
        let base = page.base_dir.clone().unwrap_or_else(|| dir_of(page.path).to_owned());
        css::extract_embedded(tree.root_node(), src, page.path, if page.base_foreign { None } else { page.root }, Some(base), (index, qname), page.facts);
    }
    let mut script_count = 0;
    for element in &walk.scripts {
        let Element { node, tag } = &walk.elements[*element];
        script_count += 1;
        let qname = if script_count == 1 { "<script>".to_owned() } else { format!("<script>~{script_count}") };
        let mut symbol = element_symbol("module", "script", qname, *node, tag, src);
        symbol.type_text = tag.get("type").map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
        page.push(symbol);
    }
}

fn references_and_imports(page: &mut Page, walk: &Walk) {
    let mut imports: Vec<ImportFact> = Vec::new();
    for Element { tag, .. } in &walk.elements {
        for (attribute, value, node) in &tag.attributes {
            let Some(value) = value.as_deref() else { continue };
            if templated(value) {
                continue;
            }
            match attribute.as_str() {
                "class" => {
                    for token in value.split_ascii_whitespace() {
                        page.reference(RefKind::Instantiate, format!(".{token}"), *node);
                    }
                }
                "id" if !value.trim().is_empty() => page.reference(RefKind::Instantiate, format!("#{}", value.trim()), *node),
                name if ID_REFERENCES.contains(&name) && !(name == "form" && tag.name == "form") => {
                    for id in value.split_ascii_whitespace() {
                        page.reference(RefKind::Read, id.to_owned(), *node);
                    }
                }
                _ => {}
            }
            if attribute == "href"
                && matches!(tag.name.as_str(), "a" | "area")
                && let Some(id) = value.trim().strip_prefix('#').filter(|id| !id.is_empty())
            {
                page.reference(RefKind::Read, decode_entities(id), *node);
            }
            if !URL_ATTRIBUTES.contains(&(tag.name.as_str(), attribute.as_str())) {
                continue;
            }
            let urls: Vec<&str> = if attribute.ends_with("srcset") {
                value.split(',').filter_map(|candidate| candidate.split_ascii_whitespace().next()).collect()
            } else {
                vec![value.trim()]
            };
            for url in urls {
                if let Some(local) = page.resolve(url) {
                    let fact = ImportFact { module: url.to_owned(), name: format!("{}.{attribute}", tag.name), local, line: line(*node) };
                    if !imports.contains(&fact) {
                        imports.push(fact);
                    }
                }
            }
        }
    }
    page.facts.imports.extend(imports);
}

/// Facts of one page.
pub fn extract(path: &str, text: &str, root: Option<&Path>, facts: &mut FileFacts) {
    let Some(tree) = parse(&language(), text) else { return };
    let node = tree.root_node();
    facts.parse_errors += has_error(node);
    let mut state = Walk { elements: Vec::new(), ids: Vec::new(), forms: Vec::new(), fields: Vec::new(), headings: Vec::new(), styles: Vec::new(), scripts: Vec::new(), base: None };
    walk(node, text, None, &mut state);
    let (base_dir, usable) = match state.base.as_deref() {
        Some(href) => paths::base_dir(root, path, href),
        None => (None, true),
    };
    let mut page = Page { src: text, path, root, base_dir, base_foreign: !usable, facts, scopes: Vec::new() };
    headings(&mut page, &state);
    ids(&mut page, &state);
    forms_and_fields(&mut page, &state);
    blocks(&mut page, &state);
    references_and_imports(&mut page, &state);
}
