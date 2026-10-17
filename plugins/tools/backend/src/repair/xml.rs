//! Readers for tool calls written as markup or Python calls instead of JSON:
//! - Qwen/Hermes XML: `<tool_call><function=NAME><parameter=KEY>VALUE</parameter></function></tool_call>`
//! - attribute style: `<invoke name="NAME"><parameter name="KEY">VALUE</parameter></invoke>`
//! - tag style (Cline and similar): `<read_file><path>VALUE</path></read_file>` or bare `<path>..</path>`
//! - Hermes JSON inside `<tool_call>` tags
//! - Python calls: `read(path="a.rs", range="1-5")`
//!
//! Values are kept as strings; the caller converts them to the types the target schema demands.

use super::json::{JsonProblem, Parser};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub name: Option<String>,
    pub params: Map<String, Value>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MarkupProblem {
    Truncated { field: Option<String> },
    Invalid(String),
    Json(JsonProblem),
}

/// Whether the text looks like markup we understand (structural: starts with a tag).
pub fn looks_like_markup(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('<') && trimmed[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/')
}

/// Strip one leading and one trailing newline (the template puts values on their own lines).
fn trim_value(value: &str) -> String {
    let value = value.strip_prefix("\r\n").or_else(|| value.strip_prefix('\n')).unwrap_or(value);
    let value = value.strip_suffix("\r\n").or_else(|| value.strip_suffix('\n')).unwrap_or(value);
    value.to_owned()
}

fn strip_wrapper<'a>(text: &'a str, open: &str, close: &str) -> (&'a str, bool) {
    let trimmed = text.trim();
    match trimmed.strip_prefix(open) {
        Some(rest) => {
            let rest = rest.trim_end();
            (rest.strip_suffix(close).unwrap_or(rest), true)
        }
        None => (text, false),
    }
}

pub fn parse_markup(text: &str) -> Result<Envelope, MarkupProblem> {
    let mut notes = Vec::new();
    let (inner, wrapped) = strip_wrapper(text, "<tool_call>", "</tool_call>");
    if wrapped {
        notes.push("unwrapped <tool_call> tags".to_owned());
    }
    let inner_trimmed = inner.trim();
    if inner_trimmed.starts_with('{') {
        // Hermes: JSON object inside tool_call tags.
        let (value, mut json_notes) = super::json::parse_lenient(inner_trimmed).map_err(MarkupProblem::Json)?;
        notes.append(&mut json_notes);
        let Value::Object(map) = value else { return Err(MarkupProblem::Invalid("<tool_call> does not contain an object".into())) };
        return Ok(Envelope { name: None, params: map, notes });
    }
    if let Some(position) = inner.find("<function=") {
        return parse_function_style(&inner[position..], notes);
    }
    if let Some(position) = inner.find("<invoke") {
        return parse_invoke_style(&inner[position..], notes);
    }
    if inner.contains("<parameter name=") || inner.contains("<parameter=") {
        let params = parse_parameters(inner, &mut notes)?;
        return Ok(Envelope { name: None, params, notes });
    }
    parse_tag_style(inner, notes)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[start..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_owned())
    } else {
        Some(rest.split(|c: char| c.is_whitespace() || c == '>').next()?.to_owned())
    }
}

fn parse_function_style(text: &str, mut notes: Vec<String>) -> Result<Envelope, MarkupProblem> {
    let header_end = text.find('>').ok_or(MarkupProblem::Truncated { field: None })?;
    let name = text["<function=".len()..header_end].trim().trim_matches('"').to_owned();
    let body = &text[header_end + 1..];
    let body = match body.rfind("</function>") {
        Some(end) => &body[..end],
        None => {
            notes.push("the closing </function> tag was missing".to_owned());
            body
        }
    };
    let params = parse_parameters(body, &mut notes)?;
    Ok(Envelope { name: Some(name), params, notes })
}

fn parse_invoke_style(text: &str, mut notes: Vec<String>) -> Result<Envelope, MarkupProblem> {
    let header_end = text.find('>').ok_or(MarkupProblem::Truncated { field: None })?;
    let name = attribute(&text[..header_end], "name");
    let body = &text[header_end + 1..];
    let body = body.rfind("</invoke>").map(|end| &body[..end]).unwrap_or(body);
    let params = parse_parameters(body, &mut notes)?;
    Ok(Envelope { name, params, notes })
}

/// `<parameter=KEY>VALUE</parameter>` and `<parameter name="KEY">VALUE</parameter>` sequences.
fn parse_parameters(body: &str, notes: &mut Vec<String>) -> Result<Map<String, Value>, MarkupProblem> {
    let mut params = Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<parameter") {
        let after = &rest[start..];
        let header_end = after.find('>').ok_or(MarkupProblem::Truncated { field: None })?;
        let header = &after[..header_end];
        let key = if let Some(key) = header.strip_prefix("<parameter=") {
            key.trim().trim_matches('"').to_owned()
        } else {
            attribute(header, "name").ok_or_else(|| MarkupProblem::Invalid(format!("parameter without a name: `{header}>`")))?
        };
        let value_start = &after[header_end + 1..];
        let (value, consumed) = match value_start.find("</parameter>") {
            Some(end) => {
                // A missing close tag shows up as another <parameter before the close tag.
                match value_start[..end].find("<parameter") {
                    Some(next) => {
                        notes.push(format!("parameter `{key}` was missing its closing tag"));
                        (&value_start[..next], next)
                    }
                    None => (&value_start[..end], end + "</parameter>".len()),
                }
            }
            None => match value_start.find("<parameter") {
                Some(next) => {
                    notes.push(format!("parameter `{key}` was missing its closing tag"));
                    (&value_start[..next], next)
                }
                None => return Err(MarkupProblem::Truncated { field: Some(key) }),
            },
        };
        if params.contains_key(&key) {
            return Err(MarkupProblem::Invalid(format!("parameter `{key}` is given twice")));
        }
        params.insert(key, Value::String(trim_value(value)));
        rest = &value_start[consumed..];
    }
    Ok(params)
}

/// `<tool><key>value</key>...</tool>` or bare `<key>value</key>` sequences.
fn parse_tag_style(text: &str, mut notes: Vec<String>) -> Result<Envelope, MarkupProblem> {
    let elements = elements(text)?;
    if elements.len() == 1 {
        let (tag, content) = &elements[0];
        if let Ok(children) = elements_strict(content)
            && !children.is_empty()
        {
            let mut params = Map::new();
            for (key, value) in children {
                if params.contains_key(&key) {
                    return Err(MarkupProblem::Invalid(format!("parameter `{key}` is given twice")));
                }
                params.insert(key, Value::String(trim_value(&value)));
            }
            notes.push(format!("read <{tag}> markup parameters"));
            return Ok(Envelope { name: Some(tag.clone()), params, notes });
        }
    }
    let mut params = Map::new();
    for (key, value) in elements {
        if params.contains_key(&key) {
            return Err(MarkupProblem::Invalid(format!("parameter `{key}` is given twice")));
        }
        params.insert(key, Value::String(trim_value(&value)));
    }
    notes.push("read markup parameters".to_owned());
    Ok(Envelope { name: None, params, notes })
}

fn elements(text: &str) -> Result<Vec<(String, String)>, MarkupProblem> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let Some(open) = rest.strip_prefix('<') else {
            return Err(MarkupProblem::Invalid(format!("text outside of tags: `{}`", rest.chars().take(30).collect::<String>())));
        };
        let tag_end = open.find('>').ok_or(MarkupProblem::Truncated { field: None })?;
        let tag = open[..tag_end].trim().to_owned();
        if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(MarkupProblem::Invalid(format!("unexpected tag `<{tag}>`")));
        }
        let body = &open[tag_end + 1..];
        let close = format!("</{tag}>");
        let end = body.rfind(&close).ok_or(MarkupProblem::Truncated { field: Some(tag.clone()) })?;
        // Use the last close tag only if no other sibling starts after the first one.
        let first = body.find(&close).unwrap_or(end);
        let end = if body[first + close.len()..].trim().starts_with('<') && first != end { first } else { end };
        out.push((tag, body[..end].to_owned()));
        rest = body[end + close.len()..].trim();
    }
    Ok(out)
}

fn elements_strict(text: &str) -> Result<Vec<(String, String)>, MarkupProblem> {
    if !text.trim().starts_with('<') {
        return Err(MarkupProblem::Invalid("not markup".into()));
    }
    elements(text)
}

/// Python-style call: `name(key=value, ...)`, optionally inside `[...]`.
pub fn parse_python_call(text: &str) -> Option<Result<Envelope, MarkupProblem>> {
    let trimmed = text.trim();
    let trimmed = trimmed.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).map(str::trim).unwrap_or(trimmed);
    let open = trimmed.find('(')?;
    let name = trimmed[..open].trim();
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        || !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
    {
        return None;
    }
    let body = &trimmed[open + 1..];
    Some(parse_kwargs(name, body))
}

fn parse_kwargs(name: &str, body: &str) -> Result<Envelope, MarkupProblem> {
    let mut params = Map::new();
    let mut pos = 0usize;
    let skip_ws = |pos: &mut usize| {
        while body[*pos..].starts_with(|c: char| c.is_whitespace()) {
            *pos += body[*pos..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
    };
    loop {
        skip_ws(&mut pos);
        let rest = &body[pos..];
        if rest.is_empty() {
            return Err(MarkupProblem::Truncated { field: None });
        }
        if rest.starts_with(')') {
            break;
        }
        let key_len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
        if key_len == 0 || !rest[key_len..].trim_start().starts_with('=') {
            return Err(MarkupProblem::Invalid(format!("`{name}(...)` has a positional argument; name every argument (key=value)")));
        }
        let key = rest[..key_len].to_owned();
        let equals = rest[key_len..].find('=').map(|index| index + key_len + 1).unwrap_or(key_len);
        let value_start = pos + equals;
        let mut parser = Parser::new(&body[value_start..]);
        let value = parser.parse_value().map_err(|problem| match problem {
            JsonProblem::Truncated { .. } => MarkupProblem::Truncated { field: Some(key.clone()) },
            other => MarkupProblem::Json(other),
        })?;
        if params.contains_key(&key) {
            return Err(MarkupProblem::Invalid(format!("argument `{key}` is given twice")));
        }
        params.insert(key, value);
        pos = value_start + parser.position();
        skip_ws(&mut pos);
        let rest = &body[pos..];
        if rest.starts_with(',') {
            pos += 1;
            continue;
        }
        if rest.starts_with(')') {
            break;
        }
        if rest.is_empty() {
            return Err(MarkupProblem::Truncated { field: None });
        }
        return Err(MarkupProblem::Invalid(format!("unexpected `{}` in `{name}(...)`", rest.chars().take(20).collect::<String>())));
    }
    Ok(Envelope { name: Some(name.to_owned()), params, notes: vec!["read a Python-style call".to_owned()] })
}
