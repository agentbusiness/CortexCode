//! Command-line arguments read against a command's JSON Schema: `--key value`, `--key=value`,
//! boolean flags, positionals in schema order, repeatable array flags, `@file` and `-`/`@-` (stdin).
//! Help text is generated from the same schema.

use serde_json::{Map, Value, json};
use std::io::Read;

#[derive(Debug, Clone, PartialEq)]
pub struct ArgError(pub String);

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reads stdin at most once.
pub struct Inputs {
    stdin: Option<String>,
    stdin_source: Box<dyn FnMut() -> std::io::Result<String>>,
}

impl Default for Inputs {
    fn default() -> Self {
        Self::new(Box::new(|| {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            Ok(text)
        }))
    }
}

impl Inputs {
    pub fn new(stdin_source: Box<dyn FnMut() -> std::io::Result<String>>) -> Self {
        Self { stdin: None, stdin_source }
    }

    fn stdin(&mut self) -> Result<String, ArgError> {
        if self.stdin.is_none() {
            let text = (self.stdin_source)().map_err(|error| ArgError(format!("cannot read stdin: {error}")))?;
            self.stdin = Some(text);
        }
        Ok(self.stdin.clone().unwrap_or_default())
    }

    /// Expand `@file`, `@-` and `-` into their contents; `@@text` escapes a literal `@`.
    pub fn expand(&mut self, raw: &str) -> Result<String, ArgError> {
        if raw == "-" || raw == "@-" {
            return self.stdin();
        }
        if let Some(rest) = raw.strip_prefix("@@") {
            return Ok(format!("@{rest}"));
        }
        if let Some(path) = raw.strip_prefix('@')
            && !path.is_empty()
        {
            return std::fs::read_to_string(path).map_err(|error| ArgError(format!("cannot read {path}: {error}")));
        }
        Ok(raw.to_owned())
    }
}

fn prop_type(schema: &Value, name: &str) -> String {
    schema["properties"][name]["type"].as_str().unwrap_or("string").to_owned()
}

fn convert(schema: &Value, name: &str, text: String) -> Result<Value, ArgError> {
    match prop_type(schema, name).as_str() {
        "integer" => {
            text.trim().parse::<i64>().map(Value::from).map_err(|_| ArgError(format!("--{} expects an integer, got `{text}`", flag(name))))
        }
        "number" => {
            text.trim().parse::<f64>().map(|n| json!(n)).map_err(|_| ArgError(format!("--{} expects a number, got `{text}`", flag(name))))
        }
        "boolean" => match text.trim() {
            "true" | "1" | "yes" => Ok(Value::Bool(true)),
            "false" | "0" | "no" => Ok(Value::Bool(false)),
            other => Err(ArgError(format!("--{} expects true or false, got `{other}`", flag(name)))),
        },
        "object" => serde_json::from_str(&text).map_err(|error| ArgError(format!("--{} expects JSON: {error}", flag(name)))),
        "array" => {
            let item_type = schema["properties"][name]["items"]["type"].as_str().unwrap_or("string");
            if item_type == "string" {
                Ok(Value::String(text))
            } else {
                serde_json::from_str(&text).map_err(|error| ArgError(format!("--{} items must be JSON: {error}", flag(name))))
            }
        }
        _ => Ok(Value::String(text)),
    }
}

/// Properties marked `"x-cx-literal": true` (agent names like `@ada`) are never read from files.
fn literal(schema: &Value, name: &str) -> bool {
    schema["properties"][name]["x-cx-literal"].as_bool().unwrap_or(false)
}

/// `--some-name` <-> `some_name`
pub fn flag(name: &str) -> String {
    name.replace('_', "-")
}

fn unflag(text: &str) -> String {
    text.replace('-', "_")
}

/// Parse arguments for a command with the given schema and positional order.
pub fn parse(schema: &Value, positional: &[&str], args: &[String], inputs: &mut Inputs) -> Result<Map<String, Value>, ArgError> {
    let properties = schema["properties"].as_object().cloned().unwrap_or_default();
    let mut out = Map::new();
    let mut next_positional = 0;
    let mut index = 0;
    let push = |out: &mut Map<String, Value>, name: &str, value: Value| -> Result<(), ArgError> {
        if prop_type(schema, name) == "array" {
            let entry = out.entry(name.to_owned()).or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(items) = entry {
                match value {
                    Value::Array(more) => items.extend(more),
                    other => items.push(other),
                }
            }
            return Ok(());
        }
        if out.contains_key(name) {
            return Err(ArgError(format!("--{} given twice", flag(name))));
        }
        out.insert(name.to_owned(), value);
        Ok(())
    };
    let mut only_positional = false;
    while index < args.len() {
        let arg = &args[index];
        if !only_positional && arg == "--" {
            only_positional = true;
            index += 1;
            continue;
        }
        if !only_positional && arg.starts_with("--") && arg.len() > 2 {
            let body = &arg[2..];
            let (key, inline) = match body.split_once('=') {
                Some((key, value)) => (key, Some(value.to_owned())),
                None => (body, None),
            };
            let mut name = unflag(key);
            let mut negated = false;
            if !properties.contains_key(&name)
                && let Some(stripped) = name.strip_prefix("no_")
                && prop_type(schema, stripped) == "boolean"
                && properties.contains_key(stripped)
            {
                name = stripped.to_owned();
                negated = true;
            }
            if !properties.contains_key(&name) {
                let known: Vec<String> = properties.keys().map(|key| format!("--{}", flag(key))).collect();
                return Err(ArgError(format!("unknown option --{key} (options: {})", known.join(", "))));
            }
            if prop_type(schema, &name) == "boolean" && inline.is_none() {
                push(&mut out, &name, Value::Bool(!negated))?;
                index += 1;
                continue;
            }
            let raw = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).cloned().ok_or_else(|| ArgError(format!("--{} needs a value", flag(&name))))?
                }
            };
            let text = if literal(schema, &name) { raw } else { inputs.expand(&raw)? };
            let value = convert(schema, &name, text)?;
            push(&mut out, &name, value)?;
            index += 1;
            continue;
        }
        // Positional.
        let Some(name) = positional.get(next_positional).copied() else {
            return Err(ArgError(format!("unexpected argument `{arg}`")));
        };
        let text = if literal(schema, name) { arg.clone() } else { inputs.expand(arg)? };
        let value = convert(schema, name, text)?;
        push(&mut out, name, value)?;
        // An array positional takes every remaining positional argument.
        if prop_type(schema, name) != "array" {
            next_positional += 1;
        }
        index += 1;
    }
    if let Some(required) = schema["required"].as_array() {
        for name in required.iter().filter_map(Value::as_str) {
            if !out.contains_key(name) {
                let how = if positional.contains(&name) {
                    format!("<{}>", flag(name))
                } else {
                    format!("--{} <{}>", flag(name), prop_type(schema, name))
                };
                return Err(ArgError(format!("missing {how}")));
            }
        }
    }
    Ok(out)
}

/// One-line usage generated from the schema.
pub fn usage(command: &str, schema: &Value, positional: &[&str]) -> String {
    let required: Vec<&str> =
        schema["required"].as_array().map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut parts = vec![format!("cx {command}")];
    for name in positional {
        let array = prop_type(schema, name) == "array";
        let text = format!("<{}>{}", flag(name), if array { "..." } else { "" });
        parts.push(if required.contains(name) { text } else { format!("[{text}]") });
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (name, property) in properties {
            if positional.contains(&name.as_str()) {
                continue;
            }
            let kind = match property["type"].as_str().unwrap_or("string") {
                "array" => format!("{}>...", property["items"]["type"].as_str().unwrap_or("string")),
                other => format!("{other}>"),
            };
            let text = if kind == "boolean>" { format!("--{}", flag(name)) } else { format!("--{} <{kind}", flag(name)) };
            parts.push(if required.contains(&name.as_str()) { text } else { format!("[{text}]") });
        }
    }
    parts.join(" ")
}

/// Detailed help generated from the schema.
pub fn help(command: &str, summary: &str, schema: &Value, positional: &[&str]) -> String {
    let mut out = format!("{}\n  {summary}\n", usage(command, schema, positional));
    if let Some(properties) = schema["properties"].as_object() {
        for (name, property) in properties {
            let kind = property["type"].as_str().unwrap_or("string");
            let description = property["description"].as_str().unwrap_or("");
            let name_text = if positional.contains(&name.as_str()) { format!("<{}>", flag(name)) } else { format!("--{}", flag(name)) };
            let mut line = format!("    {name_text} ({kind})");
            if let Some(values) = property["enum"].as_array() {
                let values: Vec<String> = values.iter().filter_map(Value::as_str).map(str::to_owned).collect();
                line.push_str(&format!(" one of {}", values.join("|")));
            }
            if !description.is_empty() {
                line.push_str(": ");
                line.push_str(description);
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}
