//! Streaming output handling for Qwen3.8 generations: incremental detokenization, stop strings, and
//! the structural split of the text into reasoning, content and native tool calls.
//!
//! The format is the one the chat template teaches (structural parsing, no judgement of meaning):
//! `<think>` reasoning `</think>`, then content, then zero or more
//! `<tool_call>\n<function=NAME>\n<parameter=P>\nVALUE\n</parameter>\n</function>\n</tool_call>`.
//! Tool arguments are streamed as JSON text built from the parameters: values of parameters the
//! tool's schema declares as strings are JSON strings streamed as they generate; other values are
//! emitted as JSON when they parse as JSON, else as strings. A generation cut off inside a call
//! leaves the argument text unterminated (the raw state), never repaired here.

use std::collections::HashMap;
use std::sync::Arc;

use cc_protocol::contracts::model::{ToolCall, ToolSpec};
use serde_json::Value;
use tokenizers::Tokenizer;

/// Incremental token-to-text decoding that never splits a multi-byte character.
pub struct Detokenizer {
    tokenizer: Arc<Tokenizer>,
    ids: Vec<u32>,
    prefix_offset: usize,
    read_offset: usize,
}

impl Detokenizer {
    pub fn new(tokenizer: Arc<Tokenizer>) -> Self {
        Self { tokenizer, ids: Vec::new(), prefix_offset: 0, read_offset: 0 }
    }

    fn decode(&self, ids: &[u32]) -> String {
        self.tokenizer.decode(ids, false).unwrap_or_default()
    }

    /// Text completed by this token (empty while a character is still incomplete).
    pub fn push(&mut self, id: u32) -> String {
        self.ids.push(id);
        let prefix = self.decode(&self.ids[self.prefix_offset..self.read_offset]);
        let text = self.decode(&self.ids[self.prefix_offset..]);
        if text.len() > prefix.len() && !text.ends_with('\u{FFFD}') && text.is_char_boundary(prefix.len()) {
            let delta = text[prefix.len()..].to_owned();
            self.prefix_offset = self.read_offset;
            self.read_offset = self.ids.len();
            delta
        } else {
            String::new()
        }
    }

    /// Whatever is still held (an incomplete character at the end of a stream).
    pub fn flush(&mut self) -> String {
        let prefix = self.decode(&self.ids[self.prefix_offset..self.read_offset]);
        let text = self.decode(&self.ids[self.prefix_offset..]);
        self.prefix_offset = self.ids.len();
        self.read_offset = self.ids.len();
        if text.len() > prefix.len() && text.is_char_boundary(prefix.len()) {
            text[prefix.len()..].to_owned()
        } else {
            String::new()
        }
    }
}

/// Length of the longest suffix of `text` that is a proper prefix of one of `markers`.
fn held_suffix(text: &str, markers: &[&str]) -> usize {
    let mut best = 0;
    for marker in markers {
        for (index, _) in marker.char_indices().skip(1) {
            let prefix = &marker[..index];
            if index > best && text.ends_with(prefix) {
                best = index;
            }
        }
    }
    best
}

/// Ends a stream at the first stop string; holds back text that might begin one.
#[derive(Default)]
pub struct StopScanner {
    stops: Vec<String>,
    pending: String,
    pub stopped: bool,
}

impl StopScanner {
    pub fn new(stops: &[String]) -> Self {
        Self { stops: stops.iter().filter(|stop| !stop.is_empty()).cloned().collect(), pending: String::new(), stopped: false }
    }

    /// Text that is safe to pass on.
    pub fn push(&mut self, text: &str) -> String {
        if self.stopped {
            return String::new();
        }
        if self.stops.is_empty() {
            return text.to_owned();
        }
        self.pending.push_str(text);
        if let Some(at) = self.stops.iter().filter_map(|stop| self.pending.find(stop.as_str())).min() {
            self.stopped = true;
            let out = self.pending[..at].to_owned();
            self.pending.clear();
            return out;
        }
        let markers: Vec<&str> = self.stops.iter().map(String::as_str).collect();
        let hold = held_suffix(&self.pending, &markers);
        let cut = self.pending.len() - hold;
        let out = self.pending[..cut].to_owned();
        self.pending.drain(..cut);
        out
    }

    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

/// One piece of parsed output.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Reasoning(String),
    Content(String),
    ToolStart { index: u32, id: String, name: String },
    ToolArgs { index: u32, text: String },
}

/// How a parameter's value is encoded in the argument JSON.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParamKind {
    /// Declared `"type": "string"`: a JSON string, streamed.
    String,
    /// Anything else: JSON when the text parses as JSON, else a string (emitted when complete).
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Reasoning,
    Content,
    /// After `<tool_call>`, reading `<function=NAME>`.
    ToolHeader,
    /// Between parameters, reading `<parameter=P>` or `</function>`.
    ToolBody,
    /// Inside a parameter value.
    Value,
    /// After `</function>`, reading `</tool_call>`.
    ToolTail,
}

/// Leading whitespace of a section is dropped and trailing whitespace is held until more text
/// follows, so streamed text concatenates exactly to the trimmed final text (the template renders
/// reasoning and content trimmed, which keeps a replayed history identical to the generation).
#[derive(Default)]
struct Trimmed {
    started: bool,
    held: String,
}

impl Trimmed {
    fn push(&mut self, text: &str) -> String {
        let text = if self.started { text } else { text.trim_start() };
        if text.is_empty() {
            return String::new();
        }
        self.started = true;
        let body = text.trim_end();
        if body.is_empty() {
            self.held.push_str(text);
            return String::new();
        }
        let mut out = std::mem::take(&mut self.held);
        out.push_str(body);
        self.held.push_str(&text[body.len()..]);
        out
    }
}

struct OpenCall {
    index: u32,
    name: String,
    params: usize,
    param: Option<(String, ParamKind)>,
    /// The value read so far (the leading newline and a possible closing tag are held).
    value: String,
    value_started: bool,
    arguments: String,
    id: String,
}

pub struct OutputParser {
    mode: Mode,
    /// Raw continuation (`model.complete`): content only, untrimmed.
    raw: bool,
    buffer: String,
    parse_tools: bool,
    kinds: HashMap<String, HashMap<String, ParamKind>>,
    reasoning_trim: Trimmed,
    content_trim: Trimmed,
    pub reasoning: String,
    pub content: String,
    pub calls: Vec<ToolCall>,
    open: Option<OpenCall>,
}

const THINK_END: &str = "</think>";
const TOOL_START: &str = "<tool_call>";
const FUNCTION: &str = "<function=";
const PARAMETER: &str = "<parameter=";
const PARAMETER_END: &str = "</parameter>";
const FUNCTION_END: &str = "</function>";
const TOOL_END: &str = "</tool_call>";

impl OutputParser {
    /// `reasoning`: the generation starts inside `<think>` (thinking on). `parse_tools`: tool calls
    /// are recognized (false for `tool_choice: "none"`).
    pub fn new(reasoning: bool, parse_tools: bool, tools: &[ToolSpec]) -> Self {
        let kinds = tools
            .iter()
            .map(|tool| {
                let properties = tool.parameters.get("properties").and_then(Value::as_object);
                let kinds = properties
                    .map(|properties| {
                        properties
                            .iter()
                            .map(|(name, schema)| {
                                let kind = if schema.get("type").and_then(Value::as_str) == Some("string") {
                                    ParamKind::String
                                } else {
                                    ParamKind::Json
                                };
                                (name.clone(), kind)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (tool.name.clone(), kinds)
            })
            .collect();
        Self {
            mode: if reasoning { Mode::Reasoning } else { Mode::Content },
            raw: false,
            buffer: String::new(),
            parse_tools,
            kinds,
            reasoning_trim: Trimmed::default(),
            content_trim: Trimmed::default(),
            reasoning: String::new(),
            content: String::new(),
            calls: Vec::new(),
            open: None,
        }
    }

    /// A parser for raw text continuation: everything is content, nothing is trimmed.
    pub fn raw() -> Self {
        let mut parser = Self::new(false, false, &[]);
        parser.raw = true;
        parser
    }

    /// Whether the generation is inside a tool call.
    pub fn in_tool_call(&self) -> bool {
        self.open.is_some()
    }

    fn emit_reasoning(&mut self, text: &str, out: &mut Vec<Piece>) {
        let text = self.reasoning_trim.push(text);
        if !text.is_empty() {
            self.reasoning.push_str(&text);
            out.push(Piece::Reasoning(text));
        }
    }

    fn emit_content(&mut self, text: &str, out: &mut Vec<Piece>) {
        let text = if self.raw { text.to_owned() } else { self.content_trim.push(text) };
        if !text.is_empty() {
            self.content.push_str(&text);
            out.push(Piece::Content(text));
        }
    }

    fn emit_args(&mut self, text: &str, out: &mut Vec<Piece>) {
        if text.is_empty() {
            return;
        }
        if let Some(call) = self.open.as_mut() {
            call.arguments.push_str(text);
            out.push(Piece::ToolArgs { index: call.index, text: text.to_owned() });
        }
    }

    pub fn push(&mut self, text: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        self.buffer.push_str(text);
        while self.step(&mut out) {}
        out
    }

    /// Process the buffer as far as it is unambiguous; true when the mode changed and it should run again.
    fn step(&mut self, out: &mut Vec<Piece>) -> bool {
        match self.mode {
            Mode::Reasoning => {
                if let Some(at) = self.buffer.find(THINK_END) {
                    let before = self.buffer[..at].to_owned();
                    self.buffer.drain(..at + THINK_END.len());
                    self.emit_reasoning(&before, out);
                    self.mode = Mode::Content;
                    return true;
                }
                let hold = held_suffix(&self.buffer, &[THINK_END]);
                let cut = self.buffer.len() - hold;
                let text: String = self.buffer.drain(..cut).collect();
                self.emit_reasoning(&text, out);
                false
            }
            Mode::Content => {
                if !self.parse_tools {
                    let text = std::mem::take(&mut self.buffer);
                    self.emit_content(&text, out);
                    return false;
                }
                if let Some(at) = self.buffer.find(TOOL_START) {
                    let before = self.buffer[..at].to_owned();
                    self.buffer.drain(..at + TOOL_START.len());
                    self.emit_content(&before, out);
                    self.mode = Mode::ToolHeader;
                    return true;
                }
                let hold = held_suffix(&self.buffer, &[TOOL_START]);
                let cut = self.buffer.len() - hold;
                let text: String = self.buffer.drain(..cut).collect();
                self.emit_content(&text, out);
                false
            }
            Mode::ToolHeader => {
                let trimmed = self.buffer.trim_start();
                let skipped = self.buffer.len() - trimmed.len();
                if trimmed.len() < FUNCTION.len() {
                    if FUNCTION.starts_with(trimmed) {
                        return false;
                    }
                    return self.malformed_call(out);
                }
                if !trimmed.starts_with(FUNCTION) {
                    return self.malformed_call(out);
                }
                let Some(close) = trimmed.find('>') else { return false };
                let name = trimmed[FUNCTION.len()..close].trim().to_owned();
                self.buffer.drain(..skipped + close + 1);
                let index = self.calls.len() as u32;
                let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                out.push(Piece::ToolStart { index, id: id.clone(), name: name.clone() });
                self.calls.push(ToolCall { id: id.clone(), name: name.clone(), arguments: String::new() });
                self.open = Some(OpenCall {
                    index,
                    name,
                    params: 0,
                    param: None,
                    value: String::new(),
                    value_started: false,
                    arguments: String::new(),
                    id,
                });
                self.mode = Mode::ToolBody;
                true
            }
            Mode::ToolBody => {
                let trimmed = self.buffer.trim_start();
                let skipped = self.buffer.len() - trimmed.len();
                if trimmed.starts_with(PARAMETER) {
                    let Some(close) = trimmed.find('>') else { return false };
                    let name = trimmed[PARAMETER.len()..close].trim().to_owned();
                    self.buffer.drain(..skipped + close + 1);
                    let kind = self
                        .open
                        .as_ref()
                        .and_then(|call| self.kinds.get(&call.name))
                        .and_then(|kinds| kinds.get(&name))
                        .copied()
                        .unwrap_or(ParamKind::String);
                    let first = self.open.as_ref().is_some_and(|call| call.params == 0);
                    let key = serde_json::to_string(&name).unwrap_or_else(|_| format!("\"{name}\""));
                    let mut prefix = String::from(if first { "{" } else { "," });
                    prefix.push_str(&key);
                    prefix.push(':');
                    if kind == ParamKind::String {
                        prefix.push('"');
                    }
                    self.emit_args(&prefix, out);
                    if let Some(call) = self.open.as_mut() {
                        call.params += 1;
                        call.param = Some((name, kind));
                        call.value.clear();
                        call.value_started = false;
                    }
                    self.mode = Mode::Value;
                    return true;
                }
                if trimmed.starts_with(FUNCTION_END) {
                    self.buffer.drain(..skipped + FUNCTION_END.len());
                    let empty = self.open.as_ref().is_some_and(|call| call.params == 0);
                    self.emit_args(if empty { "{}" } else { "}" }, out);
                    self.mode = Mode::ToolTail;
                    return true;
                }
                if PARAMETER.starts_with(trimmed) || FUNCTION_END.starts_with(trimmed) {
                    return false;
                }
                // Text outside the grammar inside a call: kept verbatim as part of the arguments
                // text would corrupt them, so the call ends here and the text is content.
                self.close_call();
                self.mode = Mode::Content;
                true
            }
            Mode::Value => {
                let Some(call) = self.open.as_mut() else {
                    self.mode = Mode::Content;
                    return true;
                };
                let kind = call.param.as_ref().map(|(_, kind)| *kind).unwrap_or(ParamKind::String);
                // The template writes `<parameter=P>\nVALUE\n</parameter>`: one leading and one
                // trailing newline belong to the markup.
                if !call.value_started {
                    if self.buffer.is_empty() {
                        return false;
                    }
                    if self.buffer.starts_with('\n') {
                        self.buffer.drain(..1);
                    }
                    call.value_started = true;
                }
                let end = format!("\n{PARAMETER_END}");
                let found = self.buffer.find(&end).map(|at| (at, end.len())).or_else(|| {
                    // A value without the trailing newline.
                    self.buffer.find(PARAMETER_END).map(|at| (at, PARAMETER_END.len()))
                });
                if let Some((at, length)) = found {
                    let value: String = self.buffer[..at].to_owned();
                    self.buffer.drain(..at + length);
                    match kind {
                        ParamKind::String => {
                            let escaped = escape(&value);
                            self.emit_args(&escaped, out);
                            self.emit_args("\"", out);
                        }
                        ParamKind::Json => {
                            let mut whole = std::mem::take(&mut call.value);
                            whole.push_str(&value);
                            let encoded = match serde_json::from_str::<Value>(whole.trim()) {
                                Ok(parsed) => parsed.to_string(),
                                Err(_) => serde_json::to_string(&whole).unwrap_or_default(),
                            };
                            self.emit_args(&encoded, out);
                        }
                    }
                    if let Some(call) = self.open.as_mut() {
                        call.param = None;
                    }
                    self.mode = Mode::ToolBody;
                    return true;
                }
                let hold = held_suffix(&self.buffer, &[end.as_str(), PARAMETER_END]).max(usize::from(self.buffer.ends_with('\n')));
                let cut = self.buffer.len() - hold;
                let text: String = self.buffer.drain(..cut).collect();
                match kind {
                    ParamKind::String => {
                        let escaped = escape(&text);
                        self.emit_args(&escaped, out);
                    }
                    ParamKind::Json => call.value.push_str(&text),
                }
                false
            }
            Mode::ToolTail => {
                let trimmed = self.buffer.trim_start();
                let skipped = self.buffer.len() - trimmed.len();
                if trimmed.starts_with(TOOL_END) {
                    self.buffer.drain(..skipped + TOOL_END.len());
                    self.close_call();
                    self.mode = Mode::Content;
                    return true;
                }
                if TOOL_END.starts_with(trimmed) {
                    return false;
                }
                // A call without its closing tag: complete as it is.
                self.close_call();
                self.mode = Mode::Content;
                true
            }
        }
    }

    /// `<tool_call>` not followed by a function header: the text was content after all.
    fn malformed_call(&mut self, out: &mut Vec<Piece>) -> bool {
        let text = format!("{TOOL_START}{}", std::mem::take(&mut self.buffer));
        self.emit_content(&text, out);
        self.mode = Mode::Content;
        false
    }

    fn close_call(&mut self) {
        if let Some(call) = self.open.take()
            && let Some(slot) = self.calls.get_mut(call.index as usize)
        {
            slot.arguments = call.arguments;
            slot.id = call.id;
        }
    }

    /// The generation ended: flush what was held. A call still open keeps its raw, unterminated
    /// argument text (the caller sees exactly what was generated).
    pub fn finish(&mut self) -> Vec<Piece> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buffer);
        match self.mode {
            Mode::Reasoning => self.emit_reasoning(&rest, &mut out),
            Mode::Content => self.emit_content(&rest, &mut out),
            Mode::ToolHeader => {
                if rest.trim().is_empty() {
                    // A bare `<tool_call>` at the very end.
                } else {
                    let text = format!("{TOOL_START}{rest}");
                    self.emit_content(&text, &mut out);
                }
            }
            Mode::Value => {
                if let Some(call) = self.open.as_mut() {
                    let pending = format!("{}{rest}", std::mem::take(&mut call.value));
                    let text = escape(&pending);
                    self.emit_args(&text, &mut out);
                }
            }
            Mode::ToolBody | Mode::ToolTail => {}
        }
        self.close_call();
        out
    }
}

/// JSON string escaping of a fragment (without the quotes).
fn escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted.get(1..quoted.len().saturating_sub(1)).unwrap_or("").to_owned()
}
