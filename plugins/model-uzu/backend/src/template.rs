//! Chat templating: the Qwen3.8 Jinja template (shipped in `templates/`) rendered with minijinja the
//! way Hugging Face's `apply_chat_template` renders it (trim/lstrip blocks, Python-style `tojson`,
//! Python string methods), plus the conversion of contract messages and tools into template values.

use cc_protocol::RpcError;
use cc_protocol::contracts::model::{ChatMessage, ContentPart, Thinking, ToolSpec};
use minijinja::{Environment, Error, ErrorKind, Value as JinjaValue};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// Reasoning efforts the Qwen3.8 template accepts (`high` is its alias of `xhigh`).
pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// Templates shipped with the plugin (see `templates/README.md`).
pub fn template_source(name: &str) -> Option<&'static str> {
    match name {
        "qwen3.8.jinja" => Some(include_str!("../templates/qwen3.8.jinja")),
        _ => None,
    }
}

/// Python's `json.dumps(value, ensure_ascii=False)`: `", "` and `": "` separators, no HTML escaping
/// (Hugging Face's `tojson`; minijinja's own escapes `<`, `>`, `&` and `'`).
pub fn python_json(value: &Value) -> String {
    struct PythonFormatter;
    impl serde_json::ser::Formatter for PythonFormatter {
        fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { writer.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { writer.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
            writer.write_all(b": ")
        }
    }
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, PythonFormatter);
    if value.serialize(&mut serializer).is_err() {
        return value.to_string();
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

/// A compiled chat template.
pub struct ChatTemplate {
    env: Environment<'static>,
}

impl ChatTemplate {
    pub fn new(source: &'static str) -> Result<Self, String> {
        let mut env = Environment::new();
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
        env.add_filter("tojson", |value: JinjaValue| -> Result<JinjaValue, Error> {
            let json = serde_json::to_value(&value)
                .map_err(|error| Error::new(ErrorKind::InvalidOperation, format!("tojson: {error}")))?;
            Ok(JinjaValue::from_safe_string(python_json(&json)))
        });
        env.add_function("raise_exception", |message: String| -> Result<JinjaValue, Error> {
            Err(Error::new(ErrorKind::InvalidOperation, message))
        });
        env.add_template("chat", source).map_err(|error| format!("chat template: {error}"))?;
        Ok(Self { env })
    }

    /// Render `messages` (template values) with the given extra variables (`add_generation_prompt`
    /// is on unless they set it).
    pub fn render(&self, messages: &[Value], tools: &[Value], vars: &Map<String, Value>) -> Result<String, RpcError> {
        let template = self.env.get_template("chat").map_err(|error| RpcError::internal(error.to_string()))?;
        let mut context = vars.clone();
        context.insert("messages".into(), Value::Array(messages.to_vec()));
        if !tools.is_empty() {
            context.insert("tools".into(), Value::Array(tools.to_vec()));
        }
        context.entry("add_generation_prompt").or_insert(Value::Bool(true));
        template
            .render(JinjaValue::from_serialize(Value::Object(context)))
            .map_err(|error| RpcError::invalid_params(format!("the chat template rejected the conversation: {error:#}")))
    }

    /// A request as the plugin renders it. The system turn is always the one the template renders
    /// without a reasoning-effort announcement (as thinking off renders it), so it is the same
    /// for every effort and stays reusable across calls of different efforts. When `thinking`
    /// makes the template announce its effort there (Qwen3.8: low, high, xhigh, and the default),
    /// that announcement is appended to the last message instead, after everything a later call
    /// reuses. A template that announces nothing, or announces it in a way that is not one
    /// inserted run of text, renders exactly as it is.
    pub fn rendering(&self, mut messages: Vec<Value>, tools: &[Value], thinking: &Thinking) -> Result<Rendering, RpcError> {
        let vars = thinking_vars(thinking)?;
        let Some((quiet, announcement)) = self.announcement(tools, &vars) else {
            return Ok(Rendering { messages, vars });
        };
        if let Some(last) = messages.last_mut() {
            append_text(last, &announcement);
        }
        Ok(Rendering { messages, vars: quiet })
    }

    /// The effort announcement `vars` make the template write, and variables that render the same
    /// prompt without it; `None` when `vars` announce nothing. Found by rendering a probe
    /// conversation (a system and a user message, with `tools`): the conversation part (no
    /// generation prompt) of a quiet rendering equals the one with thinking off; among the efforts
    /// the template accepts, the first that renders it that way is the quiet one; the
    /// announcement is the one run of text the full rendering with `vars` adds to the quiet one
    /// (so both open the generation the same way). It is returned as the sentence preceded by the
    /// whitespace the template put around it, ready to append.
    fn announcement(&self, tools: &[Value], vars: &Map<String, Value>) -> Option<(Map<String, Value>, String)> {
        let probe = [json!({"role": "system", "content": "A"}), json!({"role": "user", "content": "B"})];
        let render = |vars: &Map<String, Value>, generation: bool| {
            let mut vars = vars.clone();
            vars.insert("add_generation_prompt".into(), Value::Bool(generation));
            self.render(&probe, tools, &vars).ok()
        };
        let mut off = vars.clone();
        off.insert("enable_thinking".into(), Value::Bool(false));
        off.remove("reasoning_effort");
        let plain = render(&off, false)?;
        if render(vars, false)? == plain {
            return None;
        }
        let quiet = EFFORTS
            .iter()
            .map(|effort| {
                let mut quiet = vars.clone();
                quiet.insert("reasoning_effort".into(), Value::String((*effort).into()));
                quiet
            })
            .find(|quiet| render(quiet, false).as_deref() == Some(plain.as_str()))?;
        let (without, with) = (render(&quiet, true)?, render(vars, true)?);
        let inserted = insertion(&without, &with)?;
        let sentence = inserted.trim();
        if sentence.is_empty() {
            return None;
        }
        let start = inserted.len() - inserted.trim_start().len();
        let separator = format!("{}{}", &inserted[start + sentence.len()..], &inserted[..start]);
        Some((quiet, format!("{separator}{sentence}")))
    }
}

/// A request's template values and variables, ready for [`ChatTemplate::render`].
pub struct Rendering {
    pub messages: Vec<Value>,
    pub vars: Map<String, Value>,
}

/// Append `text` to a message value's content (a text item when the content is a list).
fn append_text(message: &mut Value, text: &str) {
    match &mut message["content"] {
        Value::String(content) => content.push_str(text),
        Value::Array(items) => items.push(json!({"type": "text", "text": text})),
        other => *other = Value::String(text.to_owned()),
    }
}

/// The run `longer` adds to `shorter` when it is `shorter` with one run of text inserted.
fn insertion<'a>(shorter: &str, longer: &'a str) -> Option<&'a str> {
    if longer.len() <= shorter.len() {
        return None;
    }
    let prefix: usize = shorter.chars().zip(longer.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum();
    let suffix: usize = shorter[prefix..]
        .chars()
        .rev()
        .zip(longer[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    (prefix + suffix == shorter.len()).then(|| &longer[prefix..longer.len() - suffix])
}

/// The template variables for a thinking setting. Qwen3.8's template reads `enable_thinking`
/// (default on), `reasoning_effort` (low | medium | xhigh, `high` = `xhigh`, default xhigh) and
/// `preserve_thinking` (default true: earlier turns keep their reasoning, which keeps the rendered
/// history identical to what was generated, so the per-conversation prefix cache holds).
pub fn thinking_vars(thinking: &Thinking) -> Result<Map<String, Value>, RpcError> {
    let mut vars = Map::new();
    vars.insert("enable_thinking".into(), Value::Bool(thinking.enabled));
    vars.insert("preserve_thinking".into(), Value::Bool(true));
    if thinking.enabled
        && let Some(effort) = &thinking.effort
    {
        if !EFFORTS.contains(&effort.as_str()) {
            return Err(RpcError::invalid_params(format!(
                "unsupported reasoning effort `{effort}` (expected one of {})",
                EFFORTS.join(", ")
            )));
        }
        vars.insert("reasoning_effort".into(), Value::String(effort.clone()));
    }
    Ok(vars)
}

/// A tool as the template expects it (`{"type": "function", "function": {...}}`).
pub fn tool_value(tool: &ToolSpec) -> Value {
    json!({"type": "function", "function": {"name": tool.name, "description": tool.description, "parameters": tool.parameters}})
}

/// A contract message as a template value. Tool-call arguments become mappings (the template renders
/// them as `<parameter=...>` blocks); see [`renderable_arguments`] for cut-off argument text. A
/// message with content parts becomes Qwen's content list (`{"type": "image"}` items render as
/// `<|vision_start|><|image_pad|><|vision_end|>`, text items as their text); without parts the
/// content is the plain string, exactly as before.
pub fn message_value(message: &ChatMessage) -> Value {
    let mut out = Map::new();
    out.insert("role".into(), Value::String(message.role.clone()));
    if message.parts.is_empty() {
        out.insert("content".into(), Value::String(message.content.clone()));
    } else {
        let items = message
            .parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => json!({"type": "text", "text": text}),
                // The digest is not rendered; it keeps derived cache keys apart per image.
                ContentPart::Image { sha256, .. } => json!({"type": "image", "sha256": sha256}),
            })
            .collect();
        out.insert("content".into(), Value::Array(items));
    }
    if let Some(reasoning) = &message.reasoning_content {
        out.insert("reasoning_content".into(), Value::String(reasoning.clone()));
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": renderable_arguments(&call.arguments)}})
            })
            .collect();
        out.insert("tool_calls".into(), Value::Array(calls));
    }
    if let Some(id) = &message.tool_call_id {
        out.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    if let Some(name) = &message.name {
        out.insert("name".into(), Value::String(name.clone()));
    }
    Value::Object(out)
}

/// Tool-call arguments as a mapping the template can render. A generation that ended inside a call
/// (a token limit) leaves unterminated JSON; it is completed structurally (close the open string,
/// drop a dangling separator, close open objects and arrays), which renders exactly the prefix the
/// model generated. Text that still is not an object becomes one `arguments` string parameter.
pub fn renderable_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(raw) {
        return value;
    }
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&complete_json(raw)) {
        return value;
    }
    json!({"arguments": raw})
}

/// Close a truncated JSON text (structural: string/escape state and a bracket stack).
pub fn complete_json(raw: &str) -> String {
    let mut out = raw.trim_end().to_owned();
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in out.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if in_string {
        if escaped {
            out.pop();
        }
        out.push('"');
    } else {
        let trimmed = out.trim_end();
        if trimmed.ends_with(',') {
            out.truncate(trimmed.len() - 1);
        } else if trimmed.ends_with(':') {
            out.push_str(" null");
        }
    }
    while let Some(close) = stack.pop() {
        out.push(close);
    }
    out
}

/// A cache key for requests that bring none (the OpenAI endpoint): the first two messages (normally
/// the system block and the first user turn), hashed. Requests continuing the same conversation share
/// it. Structural (FNV-1a over bytes).
pub fn derived_key(messages: &[Value]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for message in messages.iter().take(2) {
        for byte in message.to_string().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("auto:{hash:016x}")
}
