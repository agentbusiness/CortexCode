//! A tool call the model wrote into its reply as text instead of sending it as a call.
//!
//! A reply without a tool call ends the loop as the model's last word, so a call written as text
//! ended tasks as "done" with nothing run (seen: two agent tasks ended this way, one with six
//! compile errors on screen). The model's own notations for a call are recognised here:
//! `<tool_call>{"name": ..., "arguments": {...}}</tool_call>`, `<function=NAME><parameter=KEY>`
//! blocks (with or without the `<tool_call>` tags), and a reply that is nothing but the JSON object
//! of a call. Mechanical: the shape decides, never the words.
//!
//! One clean call of a tool the session has, with little text around it, is the call itself and is
//! run. Another reply that ends with the shape (several calls, an unknown tool, arguments that do
//! not parse, a long text before it) is not run: the model is told to send it as a call. A block
//! the reply goes on after is a call quoted in an answer, and the reply stands as words.

use cc_protocol::contracts::model::ToolSpec;
use serde_json::{Map, Value};

/// Characters of text around a call that still make the reply "the call" (a line saying what it is
/// for); more is prose that quotes a call, which is never run.
const REST_MAX: usize = 200;

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";
const FUNCTION_OPEN: &str = "<function=";
const FUNCTION_CLOSE: &str = "</function>";
const PARAMETER_OPEN: &str = "<parameter=";
const PARAMETER_CLOSE: &str = "</parameter>";

#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// One call of a tool the session has: its name, its arguments as JSON text, and the reply's
    /// text around it.
    Call { name: String, arguments: String, rest: String },
    /// The reply carries a call's shape that cannot be run as it is.
    Shape,
    /// A reply in words.
    None,
}

/// What `content` (a reply that came without a tool call) holds.
pub fn find(content: &str, tools: &[ToolSpec]) -> Found {
    let text = content.trim();
    if text.is_empty() {
        return Found::None;
    }
    if text.contains(CALL_OPEN) || text.contains(FUNCTION_OPEN) {
        return tagged(text, tools);
    }
    // A reply that is nothing but a call's JSON object (also inside a code fence).
    let bare = unfenced(text);
    if bare.starts_with('{')
        && let Some((name, arguments)) = json_call(bare)
        && tools.iter().any(|tool| tool.name == name)
    {
        return Found::Call { name, arguments, rest: String::new() };
    }
    Found::None
}

/// The reply without a code fence around all of it.
fn unfenced(text: &str) -> &str {
    let Some(body) = text.strip_prefix("```") else { return text };
    let Some(body) = body.strip_suffix("```") else { return text };
    // The fence's first line names a language (```json).
    body.split_once('\n').map(|(_, rest)| rest).unwrap_or(body).trim()
}

/// A reply with a call's tags: the one call it holds, when it can be run.
fn tagged(text: &str, tools: &[ToolSpec]) -> Found {
    // Several calls are never run from text; a reply that ends with one was meant as calls.
    if text.matches(CALL_OPEN).count() > 1 || text.matches(FUNCTION_OPEN).count() > 1 {
        return if text.ends_with(CALL_CLOSE) || text.ends_with(FUNCTION_CLOSE) { Found::Shape } else { Found::None };
    }
    // The block: from its opening tag to its closing tag (a reply cut off before it: to the end).
    let (start, end, inner) = match text.find(CALL_OPEN) {
        Some(start) => {
            let inner_start = start + CALL_OPEN.len();
            match text[inner_start..].find(CALL_CLOSE) {
                Some(close) => (start, inner_start + close + CALL_CLOSE.len(), &text[inner_start..inner_start + close]),
                None => (start, text.len(), &text[inner_start..]),
            }
        }
        None => {
            let Some(start) = text.find(FUNCTION_OPEN) else { return Found::None };
            match text[start..].find(FUNCTION_CLOSE) {
                Some(close) => (start, start + close + FUNCTION_CLOSE.len(), &text[start..start + close + FUNCTION_CLOSE.len()]),
                None => (start, text.len(), &text[start..]),
            }
        }
    };
    // A block the reply goes on after is a call quoted in an answer (how a tool is called, an
    // example): the reply is words. A block the reply ends with was meant as the call.
    let quoted = if text[end..].trim().is_empty() { Found::Shape } else { Found::None };
    let inner = inner.trim();
    let call = if inner.starts_with('{') { json_call(inner) } else { function_call(inner, tools) };
    let Some((name, arguments)) = call else { return quoted };
    if !tools.iter().any(|tool| tool.name == name) {
        return quoted;
    }
    let rest = format!("{}\n{}", text[..start].trim(), text[end..].trim()).trim().to_owned();
    if rest.chars().count() > REST_MAX || rest.contains("```") {
        return quoted;
    }
    Found::Call { name, arguments, rest }
}

/// `{"name": "run", "arguments": {...}}`: the name and the arguments as JSON text.
fn json_call(text: &str) -> Option<(String, String)> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?.trim().to_owned();
    let arguments = match object.get("arguments").or_else(|| object.get("parameters"))? {
        Value::Object(map) => Value::Object(map.clone()),
        // Arguments sent as the JSON text of an object.
        Value::String(text) => serde_json::from_str::<Value>(text).ok().filter(Value::is_object)?,
        _ => return None,
    };
    (!name.is_empty()).then(|| (name, arguments.to_string()))
}

/// `<function=run><parameter=command>\nls\n</parameter></function>`: the name and the arguments
/// as JSON text. A value is text unless the tool's schema gives its parameter another type and the
/// value reads as JSON (a number, a flag, a list of edits).
fn function_call(text: &str, tools: &[ToolSpec]) -> Option<(String, String)> {
    let after = &text[text.find(FUNCTION_OPEN)? + FUNCTION_OPEN.len()..];
    let (name, mut body) = after.split_once('>')?;
    let name = name.trim().to_owned();
    if name.is_empty() {
        return None;
    }
    let properties = tools.iter().find(|tool| tool.name == name).and_then(|tool| tool.parameters.get("properties")).and_then(Value::as_object);
    let mut arguments = Map::new();
    while let Some(at) = body.find(PARAMETER_OPEN) {
        let (key, value_on) = body[at + PARAMETER_OPEN.len()..].split_once('>')?;
        let (value, rest) = value_on.split_once(PARAMETER_CLOSE)?;
        // Only the line breaks that set the value off from its tags are not part of it.
        let value = value.strip_prefix('\n').unwrap_or(value);
        let value = value.strip_suffix('\n').unwrap_or(value);
        let key = key.trim();
        let is_text = properties.and_then(|properties| properties.get(key)).and_then(|property| property.get("type")).and_then(Value::as_str).is_none_or(|kind| kind == "string");
        let parsed = if is_text { None } else { serde_json::from_str::<Value>(value.trim()).ok() };
        arguments.insert(key.to_owned(), parsed.unwrap_or_else(|| Value::String(value.to_owned())));
        body = rest;
    }
    Some((name, Value::Object(arguments).to_string()))
}
