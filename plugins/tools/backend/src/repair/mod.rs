//! Tool-call repair: turn whatever the model produced (malformed JSON, markup, Python calls, other
//! harnesses' tool names and argument shapes) into one canonical call of `read`, `edit` or `run`,
//! or a structured, retryable error that names the exact problem.
//!
//! Principle: charitable about syntax and shape, strict about meaning. A mapping is applied only
//! when it is unambiguous (a known dialect's documented semantics, or a purely syntactic fix); every
//! repair is recorded in `notes` (journaled); anything that would need a guess is an error. The
//! model is told only what changed the call's meaning (`shown`): an option left out, a unit read one
//! way, a range given twice. A pure rename (`file_path` -> `path`) says nothing.

pub mod json;
pub mod xml;

use crate::read::{parse_range, split_path_range};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepairError {
    /// unknown_tool | bad_json | truncated | missing_argument | bad_argument | unknown_argument |
    /// ambiguous | unsupported
    pub kind: &'static str,
    pub message: String,
    /// Always true: the model can send a corrected call.
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl RepairError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), retryable: true, tool: None, field: None }
    }
    fn tool(mut self, tool: &str) -> Self {
        self.tool = Some(tool.to_owned());
        self
    }
    fn field(mut self, field: &str) -> Self {
        self.field = Some(field.to_owned());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Canonical {
    /// read | edit | run
    pub tool: &'static str,
    pub args: Map<String, Value>,
    /// What was repaired or mapped (journaled).
    pub notes: Vec<String>,
    /// What the model is told, one line each: options left out and meanings chosen.
    pub shown: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Read,
    Edit,
    Run,
    TextEditor,
    ApplyPatch,
    /// CortexCode's interactive terminal (keys in, screen out).
    Terminal,
    Generic,
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Read => "read",
        Family::Edit | Family::TextEditor | Family::ApplyPatch => "edit",
        Family::Run => "run",
        Family::Terminal => "terminal",
        Family::Generic => "?",
    }
}

/// Normalize a tool name: drop namespaces (`functions.`, `default_api:`), case and separators.
fn normalize_name(raw: &str) -> String {
    let mut name = raw.trim().trim_matches(|c: char| c == '"' || c == '\'' || c == '`').to_owned();
    for prefix in ["functions.", "functions:", "default_api.", "default_api:", "tools.", "tool.", "function."] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_owned();
        }
    }
    // camelCase -> snake_case, then lowercase with `_` separators.
    let mut out = String::with_capacity(name.len() + 4);
    let mut previous_lower = false;
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if previous_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            previous_lower = false;
        } else if c == '-' || c == ' ' || c == '.' {
            out.push('_');
            previous_lower = false;
        } else {
            out.push(c);
            previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// Tool names of other harnesses and their family (a dialect table, not a meaning heuristic).
fn family_of(name: &str) -> Option<Family> {
    Some(match name {
        "read" | "read_file" | "readfile" | "file_read" | "view" | "view_file" | "cat" | "open" | "open_file" | "get_file"
        | "get_file_contents" | "read_files" | "read_many_files" | "list_dir" | "list_directory" | "ls" | "list_files"
        | "view_directory" | "read_text_file" => Family::Read,
        "edit" | "edit_file" | "str_replace" | "write" | "write_file" | "writefile" | "create" | "create_file" | "file_write"
        | "multi_edit" | "multiedit" | "replace" | "replace_in_file" | "search_replace" | "write_to_file" | "edit_text_file"
        | "file_edit" => Family::Edit,
        "str_replace_editor" | "str_replace_based_edit_tool" | "text_editor" => Family::TextEditor,
        "apply_patch" | "applypatch" | "patch" | "apply_diff" => Family::ApplyPatch,
        "run"
        | "bash"
        | "shell"
        | "sh"
        | "zsh"
        | "exec"
        | "execute"
        | "execute_command"
        | "execute_bash"
        | "run_command"
        | "run_shell_command"
        | "terminal"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "run_in_terminal"
        | "command"
        | "cmd"
        | "container_exec"
        | "local_shell"
        | "shell_command" => Family::Run,
        "" | "tool_call" | "function" | "call" | "invoke" | "tool" | "use_tool" | "functions" | "tool_use" => Family::Generic,
        _ => return None,
    })
}

const PATH_ALIASES: &[&str] = &[
    "file_path",
    "filepath",
    "file",
    "filename",
    "file_name",
    "target_file",
    "absolute_path",
    "relative_path",
    "uri",
    "target",
    "fname",
    "dir_path",
    "directory",
    "dir",
    "relative_workspace_path",
    "notebook_path",
];
const INTENT_ALIASES: &[&str] = &["i", "description", "explanation", "reason", "purpose", "thought", "rationale"];

struct Args {
    map: Map<String, Value>,
    notes: Vec<String>,
    shown: Vec<String>,
    tool: &'static str,
}

impl Args {
    fn new(map: Map<String, Value>, notes: Vec<String>, tool: &'static str) -> Args {
        Args { map, notes, shown: Vec::new(), tool }
    }

    fn note(&mut self, note: impl Into<String>) {
        let note = note.into();
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// A repair that changes what the call does: journaled and told to the model.
    fn tell(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.note(line.clone());
        if !self.shown.contains(&line) {
            self.shown.push(line);
        }
    }

    /// Take a value given under the canonical key or one of its aliases. Several different values
    /// for the same parameter are an error, never a choice.
    fn take(&mut self, canonical: &str, aliases: &[&str]) -> Result<Option<Value>, RepairError> {
        let mut found: Vec<(String, Value)> = Vec::new();
        for key in std::iter::once(canonical).chain(aliases.iter().copied()) {
            if let Some(value) = self.map.remove(key) {
                if value.is_null() {
                    continue;
                }
                found.push((key.to_owned(), value));
            }
        }
        match found.len() {
            0 => Ok(None),
            _ => {
                let (first_key, first_value) = found[0].clone();
                for (key, value) in &found[1..] {
                    if value != &first_value {
                        return Err(RepairError::new(
                            "ambiguous",
                            format!("`{first_key}` and `{key}` both set `{canonical}` with different values; send one"),
                        )
                        .tool(self.tool)
                        .field(canonical));
                    }
                }
                for (key, _) in &found {
                    if key != canonical {
                        self.note(format!("`{key}` -> `{canonical}`"));
                    }
                }
                Ok(Some(first_value))
            }
        }
    }

    fn string(&mut self, field: &str, value: Value) -> Result<String, RepairError> {
        match value {
            Value::String(text) => Ok(text),
            Value::Number(number) => {
                self.note(format!("`{field}` number -> string"));
                Ok(number.to_string())
            }
            other => Err(RepairError::new("bad_argument", format!("`{field}` must be a string, got {}", kind_of(&other)))
                .tool(self.tool)
                .field(field)),
        }
    }

    fn boolean(&mut self, field: &str, value: Value) -> Result<bool, RepairError> {
        match &value {
            Value::Bool(flag) => Ok(*flag),
            Value::String(text) => {
                let flag = match text.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => {
                        return Err(RepairError::new("bad_argument", format!("`{field}` must be true or false, got \"{text}\""))
                            .tool(self.tool)
                            .field(field));
                    }
                };
                self.note(format!("`{field}` string -> boolean"));
                Ok(flag)
            }
            Value::Number(number) if number.as_u64() == Some(0) || number.as_u64() == Some(1) => {
                self.note(format!("`{field}` number -> boolean"));
                Ok(number.as_u64() == Some(1))
            }
            other => Err(RepairError::new("bad_argument", format!("`{field}` must be true or false, got {}", kind_of(other)))
                .tool(self.tool)
                .field(field)),
        }
    }

    fn integer(&mut self, field: &str, value: Value) -> Result<u64, RepairError> {
        match &value {
            Value::Number(number) => {
                if let Some(integer) = number.as_u64() {
                    return Ok(integer);
                }
                if let Some(float) = number.as_f64()
                    && float >= 0.0
                    && float.fract() == 0.0
                {
                    return Ok(float as u64);
                }
                Err(RepairError::new("bad_argument", format!("`{field}` must be a non-negative integer, got {number}"))
                    .tool(self.tool)
                    .field(field))
            }
            Value::String(text) if text.trim().parse::<u64>().is_ok() => {
                self.note(format!("`{field}` string -> integer"));
                Ok(text.trim().parse::<u64>().unwrap_or(0))
            }
            other => Err(RepairError::new("bad_argument", format!("`{field}` must be a non-negative integer, got {}", describe(other)))
                .tool(self.tool)
                .field(field)),
        }
    }

    /// An array, possibly sent as a JSON string (stringified nested value).
    fn array(&mut self, field: &str, value: Value) -> Result<Vec<Value>, RepairError> {
        match value {
            Value::Array(items) => Ok(items),
            // Escaped one level too deep in part (seen: `[{"op": \"replace\", ...}]`): read again
            // with that level taken off, only when it does not parse as sent.
            Value::String(text) => match json::parse_lenient(&text).or_else(|problem| {
                let unescaped = unescape_once(&text);
                if unescaped == text {
                    return Err(problem);
                }
                json::parse_lenient(&unescaped).map(|(value, mut notes)| {
                    notes.push("took off one level of escaping".to_owned());
                    (value, notes)
                })
            }) {
                Ok((Value::Array(items), notes)) => {
                    self.note(format!("parsed stringified `{field}`"));
                    for note in notes {
                        self.note(note);
                    }
                    Ok(items)
                }
                Ok((Value::Object(object), _)) => {
                    self.note(format!("parsed stringified `{field}` (one item)"));
                    Ok(vec![Value::Object(object)])
                }
                _ => Err(RepairError::new("bad_argument", format!("`{field}` must be an array")).tool(self.tool).field(field)),
            },
            Value::Object(object) => {
                self.note(format!("`{field}` single object -> one-item array"));
                Ok(vec![Value::Object(object)])
            }
            other => Err(RepairError::new("bad_argument", format!("`{field}` must be an array, got {}", kind_of(&other)))
                .tool(self.tool)
                .field(field)),
        }
    }

    fn intent(&mut self) -> Result<Option<String>, RepairError> {
        // Prefer an explicit intent; descriptive fields of other dialects fill it when absent.
        let explicit = self.map.remove("intent");
        let mut fallback = None;
        for alias in INTENT_ALIASES {
            if let Some(value) = self.map.remove(*alias)
                && fallback.is_none()
                && !value.is_null()
            {
                fallback = Some((alias.to_string(), value));
            }
        }
        match (explicit, fallback) {
            (Some(value), _) if !value.is_null() => Ok(Some(self.string("intent", value)?)),
            (_, Some((alias, value))) => {
                self.note(format!("`{alias}` -> `intent`"));
                let text = match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                Ok(Some(text))
            }
            _ => Ok(None),
        }
    }

    /// The call as it will run. Arguments the tool does not take are left out and said so (the
    /// known ones were validated by now, so the call is whole without them): a stray `edit: ""`
    /// beside a complete `content` never costs the file it carries.
    fn finish(mut self, mut out: Map<String, Value>, intent: Option<String>) -> Result<Canonical, RepairError> {
        let keys: Vec<String> = self.map.keys().cloned().collect();
        for key in keys {
            self.tell(not_an_option(&key, self.tool));
        }
        if let Some(intent) = intent {
            out.insert("intent".to_owned(), Value::String(intent));
        }
        Ok(Canonical { tool: self.tool, args: out, notes: self.notes, shown: self.shown })
    }
}

/// The line for an argument a tool does not take (left out, the call runs without it).
fn not_an_option(key: &str, tool: &str) -> String {
    format!("`{key}` is not an option of {tool}; ignored")
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(text) => format!("\"{}\"", text.chars().take(40).collect::<String>()),
        other => kind_of(other).to_owned(),
    }
}

pub fn arguments_of(tool: &str) -> &'static str {
    match tool {
        "read" => "intent, path (file, directory or URI; may end in :range), range",
        "edit" => {
            "intent, path, then one of: edits [{op, at, to?, text}] | old + new (+ all) | replacements [{old, new}] | content | patch"
        }
        "run" => "intent, command, timeout_ms, background, wake {pattern, silence_s}, cwd, permission",
        "terminal" => "intent, action (open | send | read | close), id, keys, until, wait_ms, program",
        _ => "",
    }
}

/// [`arguments_of`] as the model's tools describe them: `edit` takes the arguments of the session's
/// edit format only (the other formats still run, but naming them all invites mixing them).
pub fn arguments_in(tool: &str, format: crate::schemas::EditFormat) -> &'static str {
    use crate::schemas::EditFormat;
    match (tool, format) {
        ("edit", EditFormat::Hashline) => "intent, path, edits [{op, at, to?, text}] | content",
        ("edit", EditFormat::Replace) => "intent, path, old + new (+ all) | content",
        ("edit", EditFormat::Whole) => "intent, path, content",
        _ => arguments_of(tool),
    }
}

/// Parsed argument payload before dialect mapping.
enum Payload {
    Object(Map<String, Value>),
    Raw(String),
}

fn json_problem(problem: json::JsonProblem) -> RepairError {
    match problem {
        json::JsonProblem::Truncated { field, detail } => {
            let mut error = RepairError::new(
                "truncated",
                match &field {
                    Some(field) => format!("the arguments were cut off ({detail}) in `{field}`"),
                    None => format!("the arguments were cut off ({detail})"),
                },
            );
            error.field = field;
            error
        }
        json::JsonProblem::Invalid { position, detail } => {
            RepairError::new("bad_json", format!("the arguments are not valid JSON at character {position}: {detail}"))
        }
        json::JsonProblem::Multiple => RepairError::new(
            "ambiguous",
            "the arguments contain more than one JSON object (several calls glued together?); send one call per tool call",
        ),
    }
}

fn markup_problem(problem: xml::MarkupProblem) -> RepairError {
    match problem {
        xml::MarkupProblem::Truncated { field } => {
            let mut error = RepairError::new(
                "truncated",
                match &field {
                    Some(field) => format!("the call was cut off inside parameter `{field}`"),
                    None => "the call markup was cut off".to_owned(),
                },
            );
            error.field = field;
            error
        }
        xml::MarkupProblem::Invalid(detail) => RepairError::new("bad_json", format!("the call markup could not be read: {detail}")),
        xml::MarkupProblem::Json(problem) => json_problem(problem),
    }
}

fn strip_fences(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("```")?;
    let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or("");
    Some(rest.trim_end().strip_suffix("```").unwrap_or(rest))
}

/// Parse the argument text into an object (with an optional embedded tool name) or raw text.
fn parse_payload(text: &str, notes: &mut Vec<String>, depth: usize) -> Result<(Payload, Option<String>), RepairError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok((Payload::Object(Map::new()), None));
    }
    // Everything JSON-shaped goes through the tolerant reader, which (unlike serde_json) refuses
    // duplicate keys instead of silently keeping the last one.
    if trimmed.starts_with('"') {
        let (value, json_notes) = json::parse_lenient(trimmed).map_err(json_problem)?;
        return match value {
            Value::String(inner) if depth < 2 => {
                notes.extend(json_notes);
                notes.push("decoded double-encoded arguments".to_owned());
                parse_payload(&inner, notes, depth + 1)
            }
            other => Err(RepairError::new("bad_json", format!("the arguments must be a JSON object, got {}", kind_of(&other)))),
        };
    }
    if let Some(inner) = strip_fences(trimmed) {
        notes.push("removed markdown code fences".to_owned());
        return parse_payload(inner, notes, depth + 1);
    }
    if xml::looks_like_markup(trimmed) {
        let envelope = xml::parse_markup(trimmed).map_err(markup_problem)?;
        notes.extend(envelope.notes);
        return Ok((Payload::Object(envelope.params), envelope.name));
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let (value, json_notes) = json::parse_lenient(trimmed).map_err(json_problem)?;
        notes.extend(json_notes);
        return match value {
            Value::Object(map) => Ok((Payload::Object(map), None)),
            Value::Array(mut items) if items.len() == 1 && items[0].is_object() => {
                notes.push("unwrapped a one-element array".to_owned());
                match items.pop() {
                    Some(Value::Object(map)) => Ok((Payload::Object(map), None)),
                    _ => Err(RepairError::new("bad_json", "the arguments must be a JSON object")),
                }
            }
            other => Err(RepairError::new("bad_json", format!("the arguments must be a JSON object, got {}", kind_of(&other)))),
        };
    }
    if (trimmed.ends_with(')') || trimmed.ends_with(")]"))
        && let Some(result) = xml::parse_python_call(trimmed)
    {
        return match result {
            Ok(envelope) => {
                notes.extend(envelope.notes);
                Ok((Payload::Object(envelope.params), envelope.name))
            }
            Err(problem) => Err(markup_problem(problem)),
        };
    }
    Ok((Payload::Raw(text.to_owned()), None))
}

/// `{name, arguments}` style envelopes inside the arguments (OpenAI, Hermes, ...).
fn unwrap_envelope(map: &Map<String, Value>) -> Option<(String, Value)> {
    if let Some(Value::Object(function)) = map.get("function")
        && map.keys().all(|key| matches!(key.as_str(), "function" | "type" | "id"))
        && let Some(Value::String(name)) = function.get("name")
    {
        let arguments = function.get("arguments").or_else(|| function.get("parameters")).cloned().unwrap_or(json!({}));
        return Some((name.clone(), arguments));
    }
    let name_key = ["name", "tool", "tool_name", "function_name"].into_iter().find(|key| map.get(*key).is_some_and(Value::is_string))?;
    let args_key = ["arguments", "parameters", "input", "args", "tool_input", "params"].into_iter().find(|key| map.contains_key(*key))?;
    if !map.keys().all(|key| key == name_key || key == args_key || matches!(key.as_str(), "id" | "type" | "call_id")) {
        return None;
    }
    let name = map.get(name_key)?.as_str()?.to_owned();
    Some((name, map.get(args_key).cloned().unwrap_or(json!({}))))
}

/// Normalize one raw tool call.
pub fn normalize(name: &str, arguments: &str) -> Result<Canonical, RepairError> {
    let raw_name = name.trim().to_owned();
    let mut notes = Vec::new();
    let (mut payload, mut embedded_name) = parse_payload(arguments, &mut notes, 0)?;

    // Unwrap envelopes, at most twice.
    for _ in 0..2 {
        let Payload::Object(map) = &payload else { break };
        let Some((inner_name, inner_args)) = unwrap_envelope(map) else { break };
        notes.push(format!("unwrapped a nested `{inner_name}` call"));
        embedded_name = Some(inner_name);
        payload = match inner_args {
            Value::Object(map) => Payload::Object(map),
            Value::String(text) => {
                let (inner, name) = parse_payload(&text, &mut notes, 1)?;
                if name.is_some() {
                    embedded_name = name;
                }
                inner
            }
            other => {
                return Err(RepairError::new("bad_json", format!("nested call arguments must be an object, got {}", kind_of(&other))));
            }
        };
    }

    let outer = normalize_name(&raw_name);
    let outer_family = family_of(&outer);
    let (effective_name, family) = match embedded_name {
        Some(inner) => {
            let inner_family = match family_of(&normalize_name(&inner)) {
                Some(Family::Generic) | None => return Err(unknown_tool(&inner)),
                Some(family) => family,
            };
            match outer_family {
                None => return Err(unknown_tool(&raw_name)),
                Some(Family::Generic) => (inner, inner_family),
                Some(family) => {
                    if family_name(inner_family) != family_name(family) {
                        return Err(RepairError::new(
                            "ambiguous",
                            format!("the call is named `{raw_name}` but its arguments describe a `{inner}` call; send one tool name"),
                        ));
                    }
                    // The more specific dialect decides the argument shape.
                    if matches!(inner_family, Family::TextEditor | Family::ApplyPatch) {
                        (inner, inner_family)
                    } else {
                        (raw_name.clone(), family)
                    }
                }
            }
        }
        None => match outer_family {
            Some(Family::Generic) => {
                return Err(RepairError::new("unknown_tool", "the tool call has no tool name; call read, edit, run or terminal"));
            }
            Some(family) => (raw_name.clone(), family),
            // An unknown name whose arguments are read's (a `path` and nothing that writes or
            // runs) is a read (seen: `research` with `{intent, path: "research://search/..."}`).
            None if matches!(&payload, Payload::Object(map) if map.get("path").is_some_and(Value::is_string) && !["edits", "content", "command"].iter().any(|key| map.contains_key(*key))) => {
                (raw_name.clone(), Family::Read)
            }
            None => return Err(unknown_tool(&raw_name)),
        },
    };
    // `terminal` names a one-shot command in other harnesses (`{command}`) and CortexCode's
    // interactive terminal (`{action}` / `{keys}`): the arguments' shape tells which.
    let family = match (&payload, family) {
        (Payload::Object(map), Family::Run) if normalize_name(&effective_name) == "terminal" && (map.contains_key("action") || map.contains_key("keys")) => Family::Terminal,
        (_, family) => family,
    };
    let normalized_name = normalize_name(&effective_name);
    if family_name(family) != normalized_name && family != Family::Generic {
        notes.push(format!("`{effective_name}` -> `{}`", family_name(family)));
    }

    match family {
        Family::Read => map_read(payload, &effective_name, notes),
        Family::Edit => map_edit(payload, notes),
        Family::Run => map_run(payload, &effective_name, notes),
        Family::TextEditor => map_text_editor(payload, notes),
        Family::ApplyPatch => map_apply_patch(payload, notes),
        Family::Terminal => map_terminal(payload, notes),
        Family::Generic => Err(RepairError::new("unknown_tool", "the tool call has no tool name; call read, edit, run or terminal")),
    }
}

fn unknown_tool(name: &str) -> RepairError {
    RepairError::new(
        "unknown_tool",
        format!(
            "there is no tool `{name}`. The tools are read (files, directories, URIs), edit (change files), run (shell commands: search, tests, git, `cx`) and terminal (interactive programs)"
        ),
    )
}

fn object_payload(payload: Payload, tool: &'static str) -> Result<Map<String, Value>, RepairError> {
    match payload {
        Payload::Object(map) => Ok(map),
        Payload::Raw(text) => Err(RepairError::new(
            "bad_json",
            format!(
                "`{tool}` arguments must be a JSON object like {{{}}}, got plain text `{}`",
                arguments_of(tool),
                text.chars().take(60).collect::<String>()
            ),
        )
        .tool(tool)),
    }
}

fn map_read(payload: Payload, raw_name: &str, mut notes: Vec<String>) -> Result<Canonical, RepairError> {
    let map = match payload {
        Payload::Object(map) => map,
        Payload::Raw(text) => {
            let line = text.trim();
            if line.is_empty() || line.contains('\n') {
                return Err(RepairError::new("bad_json", "`read` arguments must be a JSON object with `path`").tool("read"));
            }
            notes.push("plain-text argument -> `path`".to_owned());
            let mut map = Map::new();
            map.insert("path".to_owned(), Value::String(line.to_owned()));
            map
        }
    };
    let mut args = Args::new(map, notes, "read");
    let intent = args.intent()?;
    let mut out = Map::new();
    let path = args.take("path", PATH_ALIASES)?;
    let paths = args.take("paths", &["file_paths", "files", "filenames"])?;
    let mut path_list: Vec<String> = Vec::new();
    match (path, paths) {
        (Some(one), Some(many)) => {
            // Both given: read them all (the call goes through, the note says so).
            args.note("`path` and `paths` -> all of them");
            for item in std::iter::once(one).chain(match many { Value::Array(items) => items, other => vec![other] }) {
                let text = args.string("paths", item)?;
                if !path_list.contains(&text) {
                    path_list.push(text);
                }
            }
        }
        (Some(Value::Array(items)), None) | (None, Some(Value::Array(items))) => {
            for item in items {
                let text = args.string("paths", item)?;
                path_list.push(text);
            }
        }
        (Some(value), None) | (None, Some(value)) => {
            let text = args.string("path", value)?;
            path_list.push(text);
        }
        (None, None) => {
            if matches!(normalize_name(raw_name).as_str(), "list_dir" | "list_directory" | "ls" | "list_files" | "view_directory") {
                args.note("no path -> `.`");
                path_list.push(".".to_owned());
            } else {
                return Err(RepairError::new("missing_argument", "`read` needs `path` (a file, a directory or a URI)")
                    .tool("read")
                    .field("path"));
            }
        }
    }
    // Options read cannot honour: the read goes through without them, and the note says so and
    // what to use instead (a refusal cost the model a whole step).
    for key in ["recursive", "depth", "max_depth", "pattern", "glob", "include", "ignore", "exclude", "regex", "query"] {
        if args.map.remove(key).is_some() {
            args.tell(not_an_option(key, "read"));
        }
    }
    let range = read_range(&mut args, raw_name)?;
    if path_list.len() == 1 {
        let path = path_list.remove(0);
        // A language-server position URI ends in `:line:column`, which is not a range.
        let (base, suffix) = if crate::langs::is_position_uri(&path) { (path.clone(), None) } else { split_path_range(&path) };
        match (suffix, range) {
            (Some(suffix), Some(range)) if suffix != range => {
                args.tell(format!("`range` {range} and the path's :{suffix} differ; read :{suffix}"));
                out.insert("path".to_owned(), Value::String(base));
                out.insert("range".to_owned(), Value::String(suffix));
            }
            (Some(suffix), _) => {
                out.insert("path".to_owned(), Value::String(base));
                out.insert("range".to_owned(), Value::String(suffix));
            }
            (None, range) => {
                out.insert("path".to_owned(), Value::String(path));
                if let Some(range) = range {
                    out.insert("range".to_owned(), Value::String(range));
                }
            }
        }
    } else {
        if range.is_some() {
            args.tell("`range` applies to one path; ignored (put :range on each path)");
        }
        out.insert("paths".to_owned(), Value::Array(path_list.into_iter().map(Value::String).collect()));
    }
    args.finish(out, intent)
}

/// Whether `text` is a whole number (digits only, around spaces).
fn is_number(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn range_text(args: &mut Args, field: &str, value: Value) -> Result<String, RepairError> {
    let text = match value {
        // A stringified array (markup values are text): `"[4, 9]"`.
        Value::String(text) if text.trim().starts_with('[') => {
            return match json::parse_lenient(text.trim()) {
                Ok((array @ Value::Array(_), _)) => {
                    args.note(format!("parsed stringified `{field}`"));
                    range_text(args, field, array)
                }
                _ => Err(RepairError::new("bad_argument", format!("`{field}` must be [start, end]")).tool("read").field(field)),
            };
        }
        Value::String(text) => {
            let text = text.trim().trim_start_matches(':');
            // `95:290` or `95..290` mean lines 95 to 290: read so, and said.
            let pair = text.split_once(':').or_else(|| text.split_once(".."));
            match pair {
                Some((start, end)) if is_number(start) && is_number(end) => {
                    let range = format!("{}-{}", start.trim(), end.trim());
                    args.tell(format!("read `{text}` as lines {range}"));
                    range
                }
                _ => text.to_owned(),
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Array(items) => {
            let numbers: Vec<i64> = items.iter().filter_map(Value::as_i64).collect();
            if numbers.len() != items.len() || numbers.is_empty() || numbers.len() > 2 {
                return Err(RepairError::new("bad_argument", format!("`{field}` must be [start, end]")).tool("read").field(field));
            }
            args.note(format!("`{field}` [start, end] -> range"));
            match numbers.as_slice() {
                [start] => format!("{start}-"),
                [start, -1] => format!("{start}-"),
                [start, end] => format!("{start}-{end}"),
                _ => String::new(),
            }
        }
        other => {
            return Err(RepairError::new("bad_argument", format!("`{field}` must be a range like \"50-80\", got {}", kind_of(&other)))
                .tool("read")
                .field(field));
        }
    };
    parse_range(&text).map_err(|message| RepairError::new("bad_argument", format!("`{field}`: {message}")).tool("read").field(field))?;
    Ok(text)
}

fn read_range(args: &mut Args, raw_name: &str) -> Result<Option<String>, RepairError> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Some(value) = args.take("range", &["lines", "line_range", "line_numbers"])? {
        let text = range_text(args, "range", value)?;
        candidates.push(("range".into(), text));
    }
    if let Some(value) = args.take("view_range", &[])? {
        let text = range_text(args, "view_range", value)?;
        candidates.push(("view_range".into(), text));
    }
    let start = args.take("start_line", &["line_start", "start", "from_line", "start_line_one_indexed", "first_line", "line"])?;
    let end = args.take("end_line", &["line_end", "end", "to_line", "end_line_one_indexed_inclusive", "last_line"])?;
    if start.is_some() || end.is_some() {
        let start = match start {
            Some(value) => Some(args.integer("start_line", value)?),
            None => None,
        };
        let end = match end {
            Some(value) => Some(args.integer("end_line", value)?),
            None => None,
        };
        let text = match (start, end) {
            (Some(start), Some(end)) => format!("{start}-{end}"),
            (Some(start), None) => format!("{start}-"),
            (None, Some(end)) => format!("1-{end}"),
            (None, None) => String::new(),
        };
        parse_range(&text).map_err(|message| RepairError::new("bad_argument", format!("start/end lines: {message}")).tool("read"))?;
        candidates.push(("start_line/end_line".into(), text));
    }
    let offset = args.take("offset", &[])?;
    let limit = args.take("limit", &["max_lines", "num_lines", "n_lines"])?;
    if offset.is_some() || limit.is_some() {
        // `offset` is 1-based in Claude Code's Read and 0-based in Gemini CLI's read_file; other
        // dialects do not say, so they cannot be mapped without guessing.
        let base = match raw_name {
            "Read" => Some(1u64),
            "read_file" => Some(0u64),
            _ => None,
        };
        let offset = match offset {
            Some(value) => Some(args.integer("offset", value)?),
            None => None,
        };
        let limit = match limit {
            Some(value) => Some(args.integer("limit", value)?),
            None => None,
        };
        let start = match (offset, base) {
            (None, _) => 1,
            (Some(offset), Some(1)) => offset.max(1),
            (Some(offset), Some(_)) => offset + 1,
            (Some(_), None) => {
                return Err(RepairError::new(
                    "ambiguous",
                    "`offset` could be 0- or 1-based; use `range` (e.g. \"120-180\" or \"120+60\") or put it on the path (file.rs:120-180)",
                )
                .tool("read")
                .field("offset"));
            }
        };
        let text = match limit {
            Some(0) => return Err(RepairError::new("bad_argument", "`limit` of 0 lines selects nothing").tool("read").field("limit")),
            Some(limit) => format!("{start}+{limit}"),
            None => format!("{start}-"),
        };
        if let Some(Ok(segments)) = Some(parse_range(&text)) {
            let normalized = match segments.first() {
                Some(segment) => match segment.end {
                    Some(end) => format!("{}-{end}", segment.start),
                    None => format!("{}-", segment.start),
                },
                None => text.clone(),
            };
            candidates.push(("offset/limit".into(), normalized));
        }
    }
    let entire = match args.take("should_read_entire_file", &[])? {
        Some(value) => args.boolean("should_read_entire_file", value)?,
        None => false,
    };
    if entire {
        if !candidates.is_empty() {
            args.tell("the line range is ignored: should_read_entire_file is true");
        }
        return Ok(None);
    }
    let normalized: Vec<(String, Vec<crate::read::Segment>)> =
        candidates.iter().filter_map(|(source, text)| parse_range(text).ok().map(|segments| (source.clone(), segments))).collect();
    if let Some((first_source, first)) = normalized.first() {
        for (source, segments) in &normalized[1..] {
            if segments != first {
                return Err(
                    RepairError::new("ambiguous", format!("`{first_source}` and `{source}` give different line ranges")).tool("read")
                );
            }
        }
    }
    Ok(candidates.into_iter().next().map(|(_, text)| text))
}

const OLD_ALIASES: &[&str] = &["old_string", "old_str", "old_text", "search", "find", "original", "old_code", "search_text", "oldText"];
const NEW_ALIASES: &[&str] = &["new_string", "new_str", "new_text", "replace", "replacement", "new_code", "replace_text", "newText"];
const ALL_ALIASES: &[&str] = &["replace_all", "global", "all_occurrences", "replaceAll"];
const CONTENT_ALIASES: &[&str] = &["contents", "file_text", "code", "data", "body", "file_content", "new_content", "fileContent"];
const PATCH_ALIASES: &[&str] = &["diff", "input", "patch_text", "patchText"];

fn normalize_op(args: &mut Args, value: Value) -> Result<String, RepairError> {
    let text = args.string("op", value)?;
    let normalized = normalize_name(&text);
    match normalized.as_str() {
        "replace" | "insert_after" | "insert_before" | "delete" => {
            if normalized != text {
                args.note(format!("op `{text}` -> `{normalized}`"));
            }
            Ok(normalized)
        }
        _ => Err(RepairError::new("bad_argument", format!("unknown edit op `{text}`; use replace, insert_after, insert_before or delete"))
            .tool("edit")
            .field("op")),
    }
}

/// Anchor value: `N:hh` (also from a copied `N:hh|text` line), `0`, or a bare line number. A bare
/// number goes through as a number: the edit uses it only while the file is exactly as the session
/// last read it, and refuses it otherwise (seen: `at: 307` refused while the file had not changed).
fn anchor_value(args: &mut Args, field: &str, value: Value) -> Result<Value, RepairError> {
    let mut bare = |line: u64| {
        args.note(format!("`{field}` = {line}: a bare line number, used while the file is as last read"));
        Ok(Value::Number(line.into()))
    };
    match value {
        Value::Number(number) => match number.as_u64() {
            Some(0) => Ok(Value::String("0".to_owned())),
            Some(line) => bare(line),
            None => Err(RepairError::new("bad_argument", format!("`{field}` = {number} is not a line; use the line's anchor N:hh from read output")).tool("edit").field(field)),
        },
        Value::String(text) => {
            let trimmed = text.trim();
            if let Some(anchor) = cc_protocol::hashline::parse_anchor(trimmed) {
                let canonical = anchor.to_string();
                if canonical != trimmed {
                    args.note(format!("anchor `{}` -> `{canonical}`", trimmed.chars().take(30).collect::<String>()));
                }
                return Ok(Value::String(canonical));
            }
            if trimmed == "0" || trimmed.starts_with("0:") {
                return Ok(Value::String("0".to_owned()));
            }
            if let Ok(line) = trimmed.parse::<u64>() {
                return bare(line);
            }
            Err(RepairError::new(
                "bad_argument",
                format!(
                    "`{field}` = \"{}\" is not an anchor; copy the N:hh prefix of the line from read output (e.g. \"12:ab\")",
                    trimmed.chars().take(40).collect::<String>()
                ),
            )
            .tool("edit")
            .field(field))
        }
        other => Err(RepairError::new("bad_argument", format!("`{field}` must be an anchor like \"12:ab\", got {}", kind_of(&other)))
            .tool("edit")
            .field(field)),
    }
}

/// Replacement text given as a string or as an array of lines.
fn text_value(args: &mut Args, field: &str, value: Value) -> Result<String, RepairError> {
    match value {
        Value::String(text) => Ok(text),
        Value::Array(items) => {
            let mut lines = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(line) => lines.push(line),
                    other => {
                        return Err(RepairError::new("bad_argument", format!("`{field}` lines must be strings, got {}", kind_of(&other)))
                            .tool("edit")
                            .field(field));
                    }
                }
            }
            // Lines that each carry their own terminator are joined without adding another.
            let all_terminated = !lines.is_empty() && lines.iter().all(|line| line.ends_with('\n'));
            args.note(format!("`{field}` array of lines -> text"));
            Ok(if all_terminated { lines.concat() } else { lines.join("\n") })
        }
        other => {
            Err(RepairError::new("bad_argument", format!("`{field}` must be a string, got {}", kind_of(&other))).tool("edit").field(field))
        }
    }
}

fn hashline_item(args: &mut Args, item: Map<String, Value>) -> Result<Value, RepairError> {
    let mut sub = Args::new(item, Vec::new(), "edit");
    let op = sub.take("op", &["operation", "type", "action", "kind"])?.ok_or_else(|| {
        RepairError::new("missing_argument", "each edit needs `op` (replace, insert_after, insert_before or delete)")
            .tool("edit")
            .field("op")
    })?;
    let op = normalize_op(&mut sub, op)?;
    let at = sub.take("at", &["anchor", "line", "start", "from", "pos", "position", "start_anchor"])?.ok_or_else(|| {
        RepairError::new("missing_argument", format!("edit op `{op}` needs `at` (an anchor like \"12:ab\")")).tool("edit").field("at")
    })?;
    let at = anchor_value(&mut sub, "at", at)?;
    let to = match sub.take("to", &["end", "until", "through", "end_anchor", "end_line", "at2", "end_at", "at_end", "to_anchor", "last"])? {
        Some(value) => Some(anchor_value(&mut sub, "to", value)?),
        None => None,
    };
    let text = match sub.take("text", &["content", "lines", "new_text", "code", "new", "replacement"])? {
        Some(value) => Some(text_value(&mut sub, "text", value)?),
        None => None,
    };
    let mut out = Map::new();
    out.insert("op".to_owned(), Value::String(op.clone()));
    out.insert("at".to_owned(), at);
    if let Some(to) = to {
        out.insert("to".to_owned(), to);
    }
    match (op.as_str(), text) {
        ("delete", Some(text)) if !text.is_empty() => {
            return Err(RepairError::new(
                "ambiguous",
                "a delete op also carries `text`; use replace to change lines, delete to remove them",
            )
            .tool("edit")
            .field("text"));
        }
        ("delete", _) => {}
        (_, Some(text)) => {
            out.insert("text".to_owned(), Value::String(text));
        }
        (op, None) => {
            return Err(RepairError::new("missing_argument", format!("edit op `{op}` needs `text` (the new lines)"))
                .tool("edit")
                .field("text"));
        }
    }
    // A key an edit op does not take is left out and said so: the op itself is clear, the call runs.
    if !sub.map.is_empty() {
        let keys: Vec<String> = sub.map.keys().map(|key| format!("`{key}`")).collect();
        args.tell(format!("{} is not part of an edit op; ignored", keys.join(", ")));
    }
    for note in sub.notes {
        args.note(note);
    }
    Ok(Value::Object(out))
}

fn replacement_item(args: &mut Args, item: Map<String, Value>) -> Result<Value, RepairError> {
    let mut sub = Args::new(item, Vec::new(), "edit");
    let old = sub
        .take("old", OLD_ALIASES)?
        .ok_or_else(|| RepairError::new("missing_argument", "a replacement needs `old`").tool("edit").field("old"))?;
    let new = sub
        .take("new", NEW_ALIASES)?
        .ok_or_else(|| RepairError::new("missing_argument", "a replacement needs `new`").tool("edit").field("new"))?;
    let old = sub.string("old", old)?;
    let new = sub.string("new", new)?;
    let all = match sub.take("all", ALL_ALIASES)? {
        Some(value) => sub.boolean("all", value)?,
        None => false,
    };
    if !sub.map.is_empty() {
        let keys: Vec<String> = sub.map.keys().map(|key| format!("`{key}`")).collect();
        args.tell(format!("{} is not part of a replacement; ignored", keys.join(", ")));
    }
    if old.is_empty() {
        return Err(RepairError::new("bad_argument", "`old` is empty; to create or overwrite a whole file send `content`")
            .tool("edit")
            .field("old"));
    }
    for note in sub.notes {
        args.note(note);
    }
    let mut out = Map::new();
    out.insert("old".to_owned(), Value::String(old));
    out.insert("new".to_owned(), Value::String(new));
    if all {
        out.insert("all".to_owned(), Value::Bool(true));
    }
    Ok(Value::Object(out))
}

fn is_hashline_item(item: &Map<String, Value>) -> bool {
    ["op", "operation", "at", "anchor"].iter().any(|key| item.contains_key(*key))
}

fn is_replacement_item(item: &Map<String, Value>) -> bool {
    std::iter::once("old").chain(OLD_ALIASES.iter().copied()).any(|key| item.contains_key(key))
}

/// Parse aider-style SEARCH/REPLACE blocks: (optional file path, search text, replace text).
pub fn parse_search_replace(text: &str) -> Option<Vec<(Option<String>, String, String)>> {
    if !text.contains("<<<<<<< SEARCH") {
        return None;
    }
    let mut blocks = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0;
    let mut last_path: Option<String> = None;
    while index < lines.len() {
        let line = lines[index].trim_end_matches('\r');
        if line.trim() == "<<<<<<< SEARCH" {
            // The path is the nearest preceding non-fence, non-empty line.
            let mut back = index;
            let mut path = None;
            while back > 0 {
                back -= 1;
                let candidate = lines[back].trim();
                if candidate.is_empty() || candidate.starts_with("```") {
                    continue;
                }
                if candidate.starts_with(">>>>>>>") || candidate.starts_with("=======") {
                    break;
                }
                path = Some(candidate.trim_matches('`').to_owned());
                break;
            }
            if path.is_some() {
                last_path = path.clone();
            }
            let mut search = Vec::new();
            index += 1;
            while index < lines.len() && lines[index].trim_end_matches('\r') != "=======" {
                search.push(lines[index].trim_end_matches('\r'));
                index += 1;
            }
            if index >= lines.len() {
                return Some(Vec::new());
            }
            index += 1;
            let mut replace = Vec::new();
            while index < lines.len() && !lines[index].trim_end_matches('\r').starts_with(">>>>>>> REPLACE") {
                replace.push(lines[index].trim_end_matches('\r'));
                index += 1;
            }
            if index >= lines.len() {
                return Some(Vec::new());
            }
            blocks.push((path.or_else(|| last_path.clone()), search.join("\n"), replace.join("\n")));
        }
        index += 1;
    }
    Some(blocks)
}

fn map_edit(payload: Payload, notes: Vec<String>) -> Result<Canonical, RepairError> {
    let map = match payload {
        Payload::Raw(text) => {
            let trimmed = text.trim_start();
            if trimmed.starts_with("*** Begin Patch")
                || trimmed.starts_with("--- ")
                || trimmed.starts_with("diff --git")
                || text.contains("<<<<<<< SEARCH")
            {
                let mut map = Map::new();
                map.insert("patch".to_owned(), Value::String(text));
                let mut notes = notes;
                notes.push("plain-text patch -> `patch`".to_owned());
                return map_edit(Payload::Object(map), notes);
            }
            return Err(object_payload(Payload::Raw(text), "edit").err().unwrap_or_else(|| RepairError::new("bad_json", "edit")));
        }
        Payload::Object(map) => map,
    };
    // str_replace_editor-shaped arguments under another edit name.
    if let Some(Value::String(command)) = map.get("command")
        && matches!(command.as_str(), "view" | "create" | "str_replace" | "insert" | "undo_edit")
    {
        return map_text_editor(Payload::Object(map), notes);
    }
    let mut args = Args::new(map, notes, "edit");
    let intent = args.intent()?;
    if args.map.contains_key("code_edit") {
        return Err(RepairError::new(
            "unsupported",
            "`code_edit` sketches with `// ... existing code ...` markers need a merge step that edit does not guess; send hashline `edits`, `old`/`new`, or the whole `content`",
        )
        .tool("edit")
        .field("code_edit"));
    }
    let _ = args.map.remove("instructions").map(|_| args.note("`instructions` -> ignored (descriptive only)"));
    let mut path = match args.take("path", PATH_ALIASES)? {
        Some(value) => Some(args.string("path", value)?),
        None => None,
    };
    // Each op naming the file (`{"op": .., "path": "a.js"}`): one file for every op is the call's
    // path and the call runs; different files are refused (one edit call changes one file).
    if let Some(Value::Array(items)) = args.map.get_mut("edits") {
        let mut named: Vec<String> = Vec::new();
        for item in items.iter_mut() {
            if let Value::Object(item) = item {
                for key in std::iter::once("path").chain(PATH_ALIASES.iter().copied()) {
                    if let Some(Value::String(file)) = item.remove(key)
                        && !named.contains(&file)
                    {
                        named.push(file);
                    }
                }
            }
        }
        match (named.as_slice(), &path) {
            ([], _) => {}
            ([one], None) => path = Some(one.clone()),
            ([one], Some(given)) if one == given => {}
            _ => {
                return Err(RepairError::new("ambiguous", "the edits name different files; one edit call changes one file: send one call per file")
                    .tool("edit")
                    .field("path"));
            }
        }
    }

    // A single flattened hashline op ({op, at, text}).
    let flattened = ["op", "operation"].iter().any(|key| args.map.contains_key(*key));
    let mut modes: Vec<&'static str> = Vec::new();
    let mut out = Map::new();
    if flattened {
        let mut item = Map::new();
        for key in ["op", "operation", "at", "anchor", "to", "end", "text", "lines", "new_text", "start", "line"] {
            if let Some(value) = args.map.remove(key) {
                item.insert(key.to_owned(), value);
            }
        }
        if let Some(value) = args.map.remove("content") {
            item.insert("content".to_owned(), value);
        }
        let edit = hashline_item(&mut args, item)?;
        args.note("single op -> `edits`");
        out.insert("edits".to_owned(), Value::Array(vec![edit]));
        modes.push("edits");
    }

    if let Some(value) = args.take("edits", &["operations", "ops", "changes"])? {
        let items = args.array("edits", value)?;
        if items.is_empty() {
            return Err(RepairError::new("missing_argument", "`edits` is empty").tool("edit").field("edits"));
        }
        let mut hashline = Vec::new();
        let mut replacements = Vec::new();
        for item in items {
            let Value::Object(item) = item else {
                return Err(RepairError::new("bad_argument", "each item of `edits` must be an object").tool("edit").field("edits"));
            };
            if is_hashline_item(&item) {
                hashline.push(hashline_item(&mut args, item)?);
            } else if is_replacement_item(&item) {
                replacements.push(replacement_item(&mut args, item)?);
            } else {
                return Err(RepairError::new(
                    "bad_argument",
                    "an item of `edits` has neither `op`/`at` (hashline edit) nor `old`/`new` (replacement)",
                )
                .tool("edit")
                .field("edits"));
            }
        }
        if !hashline.is_empty() && !replacements.is_empty() {
            return Err(
                RepairError::new("ambiguous", "`edits` mixes hashline ops and old/new replacements; send one kind per call").tool("edit")
            );
        }
        if !hashline.is_empty() {
            if out.contains_key("edits") {
                return Err(RepairError::new("ambiguous", "a top-level op and an `edits` list were both given").tool("edit"));
            }
            out.insert("edits".to_owned(), Value::Array(hashline));
            modes.push("edits");
        } else {
            args.note("`edits` old/new items -> `replacements`");
            out.insert("replacements".to_owned(), Value::Array(replacements));
            modes.push("replacements");
        }
    }
    if let Some(value) = args.take("replacements", &[])? {
        let items = args.array("replacements", value)?;
        let mut replacements = Vec::new();
        for item in items {
            let Value::Object(item) = item else {
                return Err(RepairError::new("bad_argument", "each replacement must be an object {old, new}").tool("edit"));
            };
            replacements.push(replacement_item(&mut args, item)?);
        }
        out.insert("replacements".to_owned(), Value::Array(replacements));
        modes.push("replacements");
    }
    let old = args.take("old", OLD_ALIASES)?;
    let new = args.take("new", NEW_ALIASES)?;
    let all = args.take("all", ALL_ALIASES)?;
    if old.is_some() || new.is_some() {
        let mut item = Map::new();
        if let Some(old) = old {
            item.insert("old".to_owned(), old);
        }
        if let Some(new) = new {
            item.insert("new".to_owned(), new);
        }
        if let Some(all) = all {
            item.insert("all".to_owned(), all);
        }
        let replacement = match replacement_item(&mut args, item)? {
            Value::Object(map) => map,
            _ => unreachable_object()?,
        };
        for (key, value) in replacement {
            out.insert(key, value);
        }
        modes.push("old/new");
    } else if all.is_some() {
        return Err(RepairError::new("missing_argument", "`all` was given without `old` and `new`").tool("edit"));
    }
    // `text` is the content alias only outside op items (handled above).
    let content = match args.take("content", CONTENT_ALIASES)? {
        Some(value) => Some(value),
        None => args.take("text", &[])?,
    };
    if let Some(value) = content {
        match value {
            Value::String(text) => {
                out.insert("content".to_owned(), Value::String(text));
            }
            other => {
                return Err(RepairError::new(
                    "bad_argument",
                    format!(
                        "`content` must be the file text as a string, got {}; serialize it yourself so the formatting is yours",
                        kind_of(&other)
                    ),
                )
                .tool("edit")
                .field("content"));
            }
        }
        modes.push("content");
    }
    if let Some(value) = args.take("patch", PATCH_ALIASES)? {
        let text = args.string("patch", value)?;
        match parse_search_replace(&text) {
            Some(blocks) => {
                if blocks.is_empty() {
                    return Err(RepairError::new("truncated", "a SEARCH/REPLACE block is not closed (=======, >>>>>>> REPLACE)")
                        .tool("edit")
                        .field("patch"));
                }
                let files: std::collections::BTreeSet<String> = blocks.iter().filter_map(|(path, _, _)| path.clone()).collect();
                let block_path = match (files.len(), &path) {
                    (0, Some(path)) => path.clone(),
                    (0, None) => {
                        return Err(RepairError::new("missing_argument", "SEARCH/REPLACE blocks need a file path")
                            .tool("edit")
                            .field("path"));
                    }
                    (1, explicit) => {
                        let file = files.iter().next().cloned().unwrap_or_default();
                        if let Some(explicit) = explicit
                            && explicit != &file
                        {
                            return Err(RepairError::new(
                                "ambiguous",
                                format!("`path` is {explicit} but the SEARCH/REPLACE block names {file}"),
                            )
                            .tool("edit"));
                        }
                        file
                    }
                    _ => {
                        return Err(RepairError::new(
                            "unsupported",
                            "SEARCH/REPLACE blocks for several files: send one edit call per file",
                        )
                        .tool("edit"));
                    }
                };
                let mut replacements = Vec::new();
                for (_, search, replace) in blocks {
                    if search.is_empty() {
                        return Err(
                            RepairError::new("unsupported", "an empty SEARCH block (file creation): send `content` instead").tool("edit")
                        );
                    }
                    let mut item = Map::new();
                    item.insert("old".to_owned(), Value::String(search));
                    item.insert("new".to_owned(), Value::String(replace));
                    replacements.push(Value::Object(item));
                }
                args.note("SEARCH/REPLACE blocks -> `replacements`");
                out.insert("replacements".to_owned(), Value::Array(replacements));
                out.insert("path".to_owned(), Value::String(block_path));
                modes.push("replacements");
            }
            None => {
                out.insert("patch".to_owned(), Value::String(text));
                modes.push("patch");
            }
        }
    }
    if let Some(value) = args.take("insert_line", &[])? {
        // str_replace_editor `insert` semantics under another name.
        let line = args.integer("insert_line", value)?;
        let text = out.remove("content").and_then(|value| value.as_str().map(str::to_owned));
        let Some(text) = text else {
            return Err(RepairError::new("missing_argument", "`insert_line` needs the text to insert").tool("edit"));
        };
        modes.retain(|mode| *mode != "content");
        // A line number in str_replace_editor's own semantics (insert after line N of the file
        // as last viewed); executed only while the session's view of the file is current.
        let edit = if line == 0 {
            json!({ "op": "insert_after", "at": "0", "text": text })
        } else {
            json!({ "op": "insert_after", "line": line, "text": text })
        };
        out.insert("edits".to_owned(), json!([edit]));
        modes.push("edits");
    }
    modes.dedup();
    // `content` and `edits` together are one change in both forms: the edit runs with the one the
    // file allows (whole when new or empty, else the edits) instead of being refused.
    if modes.len() == 2 && modes.contains(&"content") && modes.contains(&"edits") {
        args.tell("`content` and `edits` were both sent: the file is written whole when it is new, otherwise the edits are applied");
        modes.retain(|mode| *mode != "content");
    }
    match modes.len() {
        0 => {
            return Err(RepairError::new(
                "missing_argument",
                "`edit` needs one of: `edits` (hashline ops), `old` + `new`, `replacements`, `content` (whole file) or `patch`",
            )
            .tool("edit"));
        }
        1 => {}
        _ => {
            return Err(RepairError::new(
                "ambiguous",
                format!("`edit` got several kinds of change at once ({}); send one", modes.join(", ")),
            )
            .tool("edit"));
        }
    }
    if !out.contains_key("path") {
        match path {
            Some(path) => {
                out.insert("path".to_owned(), Value::String(path));
            }
            None if modes[0] == "patch" => {}
            None => return Err(RepairError::new("missing_argument", "`edit` needs `path`").tool("edit").field("path")),
        }
    }
    args.finish(out, intent)
}

fn unreachable_object() -> Result<Map<String, Value>, RepairError> {
    Err(RepairError::new("bad_argument", "internal: replacement was not an object"))
}

fn map_text_editor(payload: Payload, notes: Vec<String>) -> Result<Canonical, RepairError> {
    let map = object_payload(payload, "edit")?;
    let mut args = Args::new(map, notes, "edit");
    let command = match args.map.remove("command") {
        Some(Value::String(command)) => command,
        _ => {
            return Err(RepairError::new("missing_argument", "str_replace_editor needs `command` (view, create, str_replace, insert)")
                .tool("edit")
                .field("command"));
        }
    };
    let mut map = std::mem::take(&mut args.map);
    let mut notes = std::mem::take(&mut args.notes);
    notes.push(format!("str_replace_editor `{command}`"));
    match command.as_str() {
        "view" => map_read(Payload::Object(map), "str_replace_editor", notes),
        "create" => {
            if !map.contains_key("file_text") && !map.contains_key("content") {
                return Err(RepairError::new("missing_argument", "`create` needs `file_text`").tool("edit").field("file_text"));
            }
            map_edit(Payload::Object(map), notes)
        }
        "str_replace" => {
            if !map.contains_key("new_str") && !map.contains_key("new_string") && !map.contains_key("new") {
                // str_replace_editor documents a missing new_str as "replace with nothing".
                map.insert("new_str".to_owned(), Value::String(String::new()));
                notes.push("missing `new_str` -> empty (deletes the match), as str_replace_editor defines".to_owned());
            }
            map_edit(Payload::Object(map), notes)
        }
        "insert" => {
            let line = map.remove("insert_line");
            let text = map.remove("new_str").or_else(|| map.remove("text"));
            let (Some(line), Some(text)) = (line, text) else {
                return Err(RepairError::new("missing_argument", "`insert` needs `insert_line` and `new_str`").tool("edit"));
            };
            map.insert("insert_line".to_owned(), line);
            map.insert("content".to_owned(), text);
            map_edit(Payload::Object(map), notes)
        }
        "undo_edit" => Err(RepairError::new(
            "unsupported",
            "undo_edit is not available; read the file and edit it back (every change is in the session journal)",
        )
        .tool("edit")),
        other => {
            Err(RepairError::new("bad_argument", format!("unknown str_replace_editor command `{other}`")).tool("edit").field("command"))
        }
    }
}

fn map_apply_patch(payload: Payload, notes: Vec<String>) -> Result<Canonical, RepairError> {
    match payload {
        Payload::Raw(text) => {
            let mut map = Map::new();
            map.insert("patch".to_owned(), Value::String(text));
            let mut notes = notes;
            notes.push("plain-text patch -> `patch`".to_owned());
            map_edit(Payload::Object(map), notes)
        }
        Payload::Object(map) => map_edit(Payload::Object(map), notes),
    }
}

fn map_terminal(payload: Payload, notes: Vec<String>) -> Result<Canonical, RepairError> {
    let mut map = object_payload(payload, "terminal")?;
    // `keys` without an action is sending them.
    let action = match map.get("action") {
        Some(Value::String(action)) => action.trim().to_ascii_lowercase(),
        None if map.contains_key("keys") => "send".to_owned(),
        Some(other) => {
            return Err(RepairError::new("bad_argument", format!("`action` must be open, send, read or close, got {}", describe(other)))
                .tool("terminal")
                .field("action"));
        }
        None => return Err(RepairError::new("missing_argument", "`terminal` needs `action`: open, send, read or close").tool("terminal").field("action")),
    };
    if !matches!(action.as_str(), "open" | "send" | "read" | "close") {
        return Err(RepairError::new("bad_argument", format!("`action` must be open, send, read or close, got \"{action}\""))
            .tool("terminal")
            .field("action"));
    }
    if action != "open" && !map.get("id").is_some_and(Value::is_string) {
        return Err(RepairError::new("missing_argument", format!("`terminal` {action} needs the `id` that open returned")).tool("terminal").field("id"));
    }
    if action == "send" && !map.get("keys").is_some_and(Value::is_string) {
        return Err(RepairError::new("missing_argument", "`terminal` send needs `keys` (text; <Enter>, <C-c>, <Up> ... for keys)").tool("terminal").field("keys"));
    }
    if let Some(until) = map.get("until").and_then(Value::as_str)
        && let Err(error) = regex::Regex::new(until)
    {
        return Err(RepairError::new("bad_argument", format!("`until` is not a valid regex: {error}")).tool("terminal").field("until"));
    }
    map.insert("action".to_owned(), Value::String(action));
    Ok(Canonical { tool: "terminal", args: map, notes, shown: Vec::new() })
}

fn map_run(payload: Payload, raw_name: &str, notes: Vec<String>) -> Result<Canonical, RepairError> {
    let map = match payload {
        Payload::Object(map) => map,
        Payload::Raw(text) => {
            let mut notes = notes;
            notes.push("plain-text argument -> `command`".to_owned());
            let mut map = Map::new();
            map.insert("command".to_owned(), Value::String(text.trim().to_owned()));
            return map_run(Payload::Object(map), raw_name, notes);
        }
    };
    let mut args = Args::new(map, notes, "run");
    let intent = args.intent()?;
    let mut out = Map::new();
    if args.map.contains_key("commands") {
        return Err(RepairError::new(
            "ambiguous",
            "`commands` (a list) does not say whether later commands run after a failure; send one `command` line joined with && or ;",
        )
        .tool("run")
        .field("commands"));
    }
    let command = args
        .take("command", &["cmd", "script", "shell_command", "bash_command", "code"])?
        .ok_or_else(|| RepairError::new("missing_argument", "`run` needs `command` (a shell command line)").tool("run").field("command"))?;
    let command = match command {
        Value::String(text) => text,
        Value::Array(items) => {
            let mut words = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(word) => words.push(word),
                    other => {
                        return Err(RepairError::new(
                            "bad_argument",
                            format!("`command` array items must be strings, got {}", kind_of(&other)),
                        )
                        .tool("run")
                        .field("command"));
                    }
                }
            }
            let shells = ["bash", "sh", "zsh", "/bin/bash", "/bin/sh", "/bin/zsh", "/usr/bin/bash", "/usr/bin/env"];
            if words.len() == 3 && shells.contains(&words[0].as_str()) && matches!(words[1].as_str(), "-c" | "-lc" | "-ic" | "-cl") {
                args.note("argv [shell, -c, script] -> script");
                words[2].clone()
            } else {
                args.note("argv array -> quoted command line");
                words.iter().map(|word| crate::util::shell_quote(word)).collect::<Vec<_>>().join(" ")
            }
        }
        other => {
            return Err(RepairError::new("bad_argument", format!("`command` must be a string, got {}", kind_of(&other)))
                .tool("run")
                .field("command"));
        }
    };
    if command.trim().is_empty() {
        return Err(RepairError::new("missing_argument", "`command` is empty").tool("run").field("command"));
    }
    out.insert("command".to_owned(), Value::String(command));
    // Timeouts: unit known from the key, or from the dialect for a bare `timeout`.
    let mut timeout_ms: Option<u64> = None;
    if let Some(value) = args.take("timeout_ms", &["timeoutMs", "timeout_millis"])? {
        timeout_ms = Some(args.integer("timeout_ms", value)?);
    }
    if let Some(value) = args.take("timeout_s", &["timeout_sec", "timeout_secs", "timeout_seconds", "timeoutSeconds"])? {
        let seconds = args.integer("timeout_s", value)?;
        let ms = seconds.saturating_mul(1000);
        if timeout_ms.is_some_and(|existing| existing != ms) {
            return Err(RepairError::new("ambiguous", "two different timeouts were given").tool("run"));
        }
        args.note("timeout seconds -> timeout_ms");
        timeout_ms = Some(ms);
    }
    if let Some(value) = args.take("timeout", &[])? {
        let number = args.integer("timeout", value)?;
        // Claude Code's Bash documents `timeout` in milliseconds; other dialects use seconds or do
        // not say, so a bare `timeout` elsewhere is refused instead of guessed.
        let ms = match raw_name {
            "Bash" => number,
            "execute_bash" | "run_shell_command" => number.saturating_mul(1000),
            // Elsewhere a bare `timeout` is read as seconds (the shell's `timeout` and most
            // dialects), and the note says so; a refusal cost the model a whole step.
            _ => {
                args.tell(format!("`timeout` {number} read as seconds"));
                number.saturating_mul(1000)
            }
        };
        if timeout_ms.is_some_and(|existing| existing != ms) {
            return Err(RepairError::new("ambiguous", "two different timeouts were given").tool("run"));
        }
        args.note(format!("`timeout` ({raw_name} units) -> timeout_ms"));
        timeout_ms = Some(ms);
    }
    if let Some(ms) = timeout_ms {
        if ms == 0 {
            return Err(RepairError::new("bad_argument", "`timeout_ms` must be positive").tool("run").field("timeout_ms"));
        }
        out.insert("timeout_ms".to_owned(), Value::Number(ms.into()));
    }
    if let Some(value) = args.take("background", &["run_in_background", "is_background", "detach", "async"])?
        && args.boolean("background", value)?
    {
        out.insert("background".to_owned(), Value::Bool(true));
    }
    if let Some(value) = args.take("cwd", &["workdir", "working_directory", "working_dir", "directory", "dir", "cd"])? {
        let cwd = args.string("cwd", value)?;
        if !cwd.trim().is_empty() {
            out.insert("cwd".to_owned(), Value::String(cwd));
        }
    }
    // Why the command must go past the sandbox; a list of folders (the older `write_outside`) asks
    // to write there.
    if let Some(value) = args.take("permission", &["write_outside", "write_paths", "allow_write", "writable_paths", "sandbox_permission"])? {
        let why = match value {
            Value::String(why) => why,
            Value::Array(items) => {
                let paths: Vec<String> = items.into_iter().filter_map(|item| item.as_str().map(str::to_owned)).collect();
                args.note("folders -> permission");
                format!("write to {}", paths.join(", "))
            }
            other => {
                return Err(RepairError::new("bad_argument", format!("`permission` must say why, as text; got {}", kind_of(&other)))
                    .tool("run")
                    .field("permission"));
            }
        };
        if !why.trim().is_empty() {
            out.insert("permission".to_owned(), Value::String(why.trim().to_owned()));
        }
    }
    let mut wake = Map::new();
    if let Some(value) = args.take("wake", &[])? {
        match value {
            Value::Object(map) => {
                let mut sub = Args::new(map, Vec::new(), "run");
                if let Some(pattern) = sub.take("pattern", &["regex", "match", "on_match"])? {
                    let pattern = sub.string("wake.pattern", pattern)?;
                    wake.insert("pattern".to_owned(), Value::String(pattern));
                }
                if let Some(silence) = sub.take("silence_s", &["silence", "silence_seconds", "idle_s"])? {
                    let seconds = match silence {
                        Value::Number(number) => number.as_f64().unwrap_or(0.0),
                        other => sub.integer("wake.silence_s", other)? as f64,
                    };
                    wake.insert("silence_s".to_owned(), json!(seconds));
                }
                if !sub.map.is_empty() {
                    return Err(RepairError::new("unknown_argument", "`wake` takes pattern and silence_s").tool("run").field("wake"));
                }
                for note in sub.notes {
                    args.note(note);
                }
            }
            other => {
                return Err(RepairError::new(
                    "bad_argument",
                    format!("`wake` must be an object {{pattern, silence_s}}, got {}", kind_of(&other)),
                )
                .tool("run")
                .field("wake"));
            }
        }
    }
    if let Some(pattern) = args.take("wake_pattern", &[])? {
        let pattern = args.string("wake_pattern", pattern)?;
        wake.insert("pattern".to_owned(), Value::String(pattern));
    }
    if let Some(value) = args.take("wake_silence_s", &[])? {
        let seconds = args.integer("wake_silence_s", value)?;
        wake.insert("silence_s".to_owned(), json!(seconds as f64));
    }
    if let Some(pattern) = wake.get("pattern").and_then(Value::as_str)
        && let Err(error) = regex::Regex::new(pattern)
    {
        return Err(RepairError::new("bad_argument", format!("wake pattern is not a valid regex: {error}")).tool("run").field("wake"));
    }
    if !wake.is_empty() {
        out.insert("wake".to_owned(), Value::Object(wake));
    }
    // Cursor's and Cline's approval flags: approval is decided by the session's ask/auto mode.
    for key in ["require_user_approval", "requires_approval"] {
        if args.map.remove(key).is_some() {
            args.tell(not_an_option(key, "run"));
        }
    }
    args.finish(out, intent)
}

/// `\"` and `\\` read as the character they escape; every other character as it is.
fn unescape_once(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
            && (next == '"' || next == '\\')
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}
