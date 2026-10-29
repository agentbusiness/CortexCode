//! The constrained decisions (router, decide, planner): their shapes, the grammar sent to the model,
//! and their parsed forms.
//!
//! Property order is the order the model writes (`title` and `brief` before the checks). The order
//! must survive the trip to the engine, and JSON objects do not: the frame relays parameters as
//! parsed JSON, whose maps come out with sorted keys (measured with the chat flow's router).
//! So each decision is sent as a GBNF grammar (a string, relayed verbatim) generated mechanically
//! from its shape; [`json_schema`] renders the same shape as a JSON Schema for documentation and
//! tests.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

/// The shape of a constrained output.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Str,
    /// A string of at most this many characters (`maxLength`): free text the model must close.
    StrMax(usize),
    OneOf(Vec<String>),
    List(Box<Shape>),
    /// A list of at most this many items (`maxItems`): a list the model must close (seen in the
    /// chat flow: a decision ran on for 207k tokens, to the end of the context).
    ListMax(Box<Shape>, usize),
    /// Properties in the order they are written; all required.
    Obj(Vec<(String, Shape)>),
    /// Up to `max` picks from `values` (a JSON array of enum strings). The schema holds one
    /// placeholder value (`@verifiers`); the host fills it at run time ([`Shape::filled`]).
    Pick { values: Vec<String>, max: usize },
}

/// Placeholder of a pick of the project's own check commands (the language plugins' verifiers that
/// apply to the project, filled at run time): a task's `checks` can only be those, never a command
/// the model writes, so no check carries an expected value the model worked out.
pub const VERIFIERS_PLACEHOLDER: &str = "@verifiers";

impl Shape {
    /// A top-level property of an object shape.
    pub fn field(&self, name: &str) -> Option<&Shape> {
        match self {
            Shape::Obj(properties) => properties.iter().find(|(key, _)| key == name).map(|(_, shape)| shape),
            _ => None,
        }
    }

    /// The shape with every pick holding only `placeholder` filled with `values`. Nothing to pick
    /// from removes the pick with its property, so the model never writes it.
    pub fn filled(&self, placeholder: &str, values: &[String]) -> Shape {
        fn fill(shape: &Shape, placeholder: &str, values: &[String]) -> Option<Shape> {
            match shape {
                Shape::Pick { values: current, max } if current.len() == 1 && current[0] == placeholder => {
                    (!values.is_empty() && *max > 0).then(|| Shape::Pick { values: values.to_vec(), max: *max })
                }
                Shape::Obj(properties) => Some(Shape::Obj(properties.iter().filter_map(|(key, value)| fill(value, placeholder, values).map(|value| (key.clone(), value))).collect())),
                Shape::List(item) => fill(item, placeholder, values).map(|item| Shape::List(Box::new(item))),
                other => Some(other.clone()),
            }
        }
        fill(self, placeholder, values).unwrap_or(Shape::Obj(Vec::new()))
    }

    /// The same shape with one value removed from a top-level enum (e.g. DECIDE without MORE once
    /// the read rounds are spent).
    pub fn without(&self, field: &str, value: &str) -> Shape {
        match self {
            Shape::Obj(properties) => Shape::Obj(
                properties
                    .iter()
                    .map(|(key, shape)| match shape {
                        Shape::OneOf(values) if key == field => {
                            (key.clone(), Shape::OneOf(values.iter().filter(|v| v.as_str() != value).cloned().collect()))
                        }
                        other => (key.clone(), other.clone()),
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Read a JSON Schema (the subset the decisions use: object, string, enum of strings, array).
/// Object property order is the `required` order, and every property must be required: JSON
/// object keys carry no order through the frame.
pub fn shape_from_schema(schema: &Value) -> Result<Shape, String> {
    // A placeholder pick (`@verifiers`): a capped array of one run-time enum.
    if schema.get("type").and_then(Value::as_str) == Some("array")
        && let Some(placeholder) = schema
            .get("items")
            .and_then(|items| items.get("enum"))
            .and_then(Value::as_array)
            .filter(|values| values.len() == 1)
            .and_then(|values| values[0].as_str())
            .filter(|value| value.starts_with('@'))
    {
        let max = schema.get("maxItems").and_then(Value::as_u64).ok_or("a placeholder pick needs `maxItems`")?;
        return Ok(Shape::Pick { values: vec![placeholder.to_owned()], max: max as usize });
    }
    if let Some(values) = schema.get("enum") {
        let values: Vec<String> = values
            .as_array()
            .ok_or("`enum` must be an array")?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or("enum values must be strings"))
            .collect::<Result<_, _>>()?;
        if values.is_empty() {
            return Err("empty `enum`".into());
        }
        return Ok(Shape::OneOf(values));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => Ok(match schema.get("maxLength").and_then(Value::as_u64) {
            Some(max) => Shape::StrMax(max as usize),
            None => Shape::Str,
        }),
        Some("array") => {
            let item = Box::new(shape_from_schema(schema.get("items").ok_or("array without `items`")?)?);
            Ok(match schema.get("maxItems").and_then(Value::as_u64) {
                Some(max) => Shape::ListMax(item, max as usize),
                None => Shape::List(item),
            })
        }
        Some("object") => {
            let properties = schema.get("properties").and_then(Value::as_object).ok_or("object without `properties`")?;
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .ok_or("object without `required` (it gives the property order)")?
                .iter()
                .map(|value| value.as_str().ok_or("`required` entries must be strings"))
                .collect::<Result<_, _>>()?;
            if required.len() != properties.len() || required.iter().any(|name| !properties.contains_key(*name)) {
                return Err("`required` must list every property exactly once".into());
            }
            let mut fields = Vec::new();
            for name in required {
                let shape = shape_from_schema(&properties[name]).map_err(|error| format!("{name}: {error}"))?;
                fields.push((name.to_owned(), shape));
            }
            Ok(Shape::Obj(fields))
        }
        other => Err(format!("unsupported schema type {other:?}")),
    }
}

/// The JSON Schema of a shape (documentation and tests).
pub fn json_schema(shape: &Shape) -> Value {
    match shape {
        Shape::Str => json!({"type": "string"}),
        Shape::StrMax(max) => json!({"type": "string", "maxLength": max}),
        Shape::OneOf(values) => json!({"enum": values}),
        Shape::List(item) => json!({"type": "array", "items": json_schema(item)}),
        Shape::ListMax(item, max) => json!({"type": "array", "items": json_schema(item), "maxItems": max}),
        Shape::Obj(properties) => {
            let mut map = serde_json::Map::new();
            for (name, shape) in properties {
                map.insert(name.clone(), json_schema(shape));
            }
            let required: Vec<&str> = properties.iter().map(|(name, _)| name.as_str()).collect();
            json!({"type": "object", "properties": map, "required": required, "additionalProperties": false})
        }
        Shape::Pick { values, max } => json!({"type": "array", "items": {"enum": values}, "maxItems": max}),
    }
}

/// A GBNF string literal for `text` (mechanical escaping).
fn literal(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// GBNF for a shape: JSON with the properties in the shape's order. The `space` rule is
/// llama.cpp's own (bounded indentation, so whitespace cannot run away); strings are unbounded.
pub fn grammar(shape: &Shape) -> String {
    let mut rules: Vec<String> = Vec::new();
    let root = rule_for(shape, &mut rules);
    let mut out = format!("root ::= {root}\n");
    for rule in rules {
        out.push_str(&rule);
        out.push('\n');
    }
    out.push_str("space ::= | \" \" | \"\\n\"{1,2} [ \\t]{0,20}\n");
    out.push_str("string ::= \"\\\"\" char* \"\\\"\" space\n");
    out.push_str("char ::= [^\"\\\\\\x7F\\x00-\\x1F] | [\\\\] ([\"\\\\bfnrt] | \"u\" [0-9a-fA-F]{4})\n");
    out
}

fn rule_for(shape: &Shape, rules: &mut Vec<String>) -> String {
    match shape {
        Shape::Str => "string".to_owned(),
        Shape::StrMax(max) => {
            let name = format!("text{max}");
            let rule = format!("{name} ::= \"\\\"\" char{{0,{max}}} \"\\\"\" space");
            if !rules.contains(&rule) {
                rules.push(rule);
            }
            name
        }
        Shape::OneOf(values) => {
            let name = format!("enum{}", rules.len());
            // Each value as its JSON string (a command may hold quotes or backslashes).
            let alternatives: Vec<String> = values.iter().map(|value| literal(&serde_json::to_string(value).unwrap_or_default())).collect();
            rules.push(format!("{name} ::= ({}) space", alternatives.join(" | ")));
            name
        }
        Shape::List(item) => {
            let item_rule = rule_for(item, rules);
            let name = format!("list{}", rules.len());
            rules.push(format!("{name} ::= \"[\" space ({item_rule} (\",\" space {item_rule})*)? \"]\" space"));
            name
        }
        Shape::ListMax(item, max) => {
            let item_rule = rule_for(item, rules);
            let name = format!("list{}", rules.len());
            let more = max.saturating_sub(1);
            rules.push(format!("{name} ::= \"[\" space ({item_rule} (\",\" space {item_rule}){{0,{more}}})? \"]\" space"));
            name
        }
        Shape::Pick { values, max } => {
            let item = rule_for(&Shape::OneOf(values.clone()), rules);
            // item ("," item (...)?)? nested `max` deep: at most `max` picks, no repetition operator.
            let mut tail = String::new();
            for _ in 1..*max {
                tail = format!("(\",\" space {item} {tail})?");
            }
            let name = format!("pick{}", rules.len());
            rules.push(format!("{name} ::= \"[\" space ({item} {tail})? \"]\" space"));
            name
        }
        Shape::Obj(properties) => {
            let mut parts = Vec::new();
            for (key, value) in properties {
                let value_rule = rule_for(value, rules);
                parts.push(format!("{} space \":\" space {value_rule}", literal(&format!("\"{key}\""))));
            }
            let name = format!("obj{}", rules.len());
            rules.push(format!("{name} ::= \"{{\" space {} \"}}\" space", parts.join(" \",\" space ")));
            name
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedTask {
    pub title: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_effort")]
    pub effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub tasks: Vec<PlannedTask>,
}

/// REVIEW: the planner's judgement of a proposed contract revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Review {
    pub reason: String,
    /// accept | refuse
    pub decision: String,
}

fn default_effort() -> String {
    "medium".to_owned()
}

/// Decode a constrained generation. The engine enforces the schema; this also tolerates text
/// around the object (a model that wrote a preamble despite the grammar) by taking the outermost
/// JSON object, a mechanical recovery.
pub fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<T>(trimmed) {
        return Ok(value);
    }
    let start = trimmed.find('{').ok_or_else(|| "no JSON object in the output".to_owned())?;
    let end = trimmed.rfind('}').ok_or_else(|| "unterminated JSON object".to_owned())?;
    if end < start {
        return Err("malformed JSON object".to_owned());
    }
    serde_json::from_str::<T>(&trimmed[start..=end]).map_err(|error| error.to_string())
}

/// Normalize a model-chosen effort to a known level (unknown values mean the default).
pub fn effort_or_default(effort: &str, default: &str) -> String {
    let lower = effort.trim().to_ascii_lowercase();
    if EFFORTS.contains(&lower.as_str()) { lower } else { default.to_owned() }
}
