//! The constrained decisions (router, decide, checks): their shapes, the grammar sent to the model,
//! and their parsed forms.
//!
//! Property order is the order the model writes: `work` before `route` (the router states what is
//! asked before choosing), `missing` before `route` (missing-first DECIDE). The order
//! must survive the trip to the engine, and JSON objects do not: the frame relays parameters as
//! parsed JSON, whose maps come out with sorted keys (measured: the router's `work` arrived last).
//! So each decision is sent as a GBNF grammar (a string, relayed verbatim) generated mechanically
//! from its shape. Shapes come from the data set's JSON Schemas (`data/schemas/*.json`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];
pub const ROUTES: &[&str] = &["ANSWER", "LOOKUP", "DO", "DELEGATE", "STATUS", "MANAGE", "PLAN", "IMPLEMENT"];
pub const DECISIONS: &[&str] = &["MORE", "ANSWER", "DO", "DELEGATE", "PLAN"];

/// The shape of a constrained output.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Str,
    /// A string of at most this many characters (`maxLength`): free text the model must close
    /// (seen: a plan's `analysis`, written with no thinking before it, run on for 124k characters).
    StrMax(usize),
    OneOf(Vec<String>),
    List(Box<Shape>),
    /// A list of at most this many items (`maxItems`): a list the model must close (seen: a
    /// DECIDE run on for 207k tokens, to the end of the context).
    ListMax(Box<Shape>, usize),
    /// Properties in the order they are written; all required.
    Obj(Vec<(String, Shape)>),
    /// Up to `max` distinct-intent picks from `values` (a JSON array of enum strings). `values` is
    /// filled at run time (the session's skill catalog); the schema holds a placeholder.
    /// (An `enum` holding one placeholder, e.g. `["@requirements"]`, is a [`Shape::OneOf`] filled
    /// the same way: see [`Shape::filled`].)
    Pick { values: Vec<String>, max: usize },
    /// A property written only when an earlier property `on` took one of `when` (e.g. `skills`
    /// only after `route` DO): the grammar branches on that property's value.
    Gated { on: String, when: Vec<String>, inner: Box<Shape> },
}

/// Placeholder enum value of a pick filled at run time (`"items": {"enum": ["@catalog"]}`).
pub const CATALOG_PLACEHOLDER: &str = "@catalog";
/// Placeholder of a pick of the code names the turn read or edited (KNOWLEDGE's `binds`: the host
/// narrows what an item can bind to; an item never names code the turn did not see).
pub const READS_PLACEHOLDER: &str = "@reads";
/// Placeholder of an enum of a plan's task numbers ("1" .. `limits.plan_max_tasks`).
pub const TASKS_PLACEHOLDER: &str = "@tasks";
/// Placeholder of an enum of the IDEAS stage's idea numbers ("1" .. the ideas written).
pub const IDEAS_PLACEHOLDER: &str = "@ideas";
/// Placeholder of a pick of what the host found for the ideas (code, notes, sources): an option's
/// grounding can only name a finding, never one the model made up.
pub const GROUNDING_PLACEHOLDER: &str = "@grounding";
/// Placeholder of a pick of the project's own check commands (the language plugins' verifiers that
/// apply to the project, filled at run time): a decision's `checks` can only be those, never a
/// command the model writes, so no check carries an expected value the model worked out.
pub const VERIFIERS_PLACEHOLDER: &str = "@verifiers";
/// Placeholder of an enum of the agents a session started whose work is still open (MANAGE).
pub const AGENTS_PLACEHOLDER: &str = "@agents";
/// Placeholder of a pick of a session's open background work (a plan's `replaces`): a plan can
/// only replace work that exists; none open removes the property.
pub const OPEN_WORK_PLACEHOLDER: &str = "@open_work";

impl Shape {
    /// The top-level properties written as free text (a plain string, gated or not), in order:
    /// what the model writes for a reader (ROUTE's `work`, DECIDE's `missing`, `brief`, `note`).
    pub fn text_fields(&self) -> Vec<String> {
        let Shape::Obj(properties) = self else { return Vec::new() };
        properties
            .iter()
            .filter(|(_, shape)| matches!(shape, Shape::Str | Shape::StrMax(_)) || matches!(shape, Shape::Gated { inner, .. } if matches!(**inner, Shape::Str | Shape::StrMax(_))))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// A top-level property of an object shape.
    pub fn field(&self, name: &str) -> Option<&Shape> {
        match self {
            Shape::Obj(properties) => properties.iter().find(|(key, _)| key == name).map(|(_, shape)| shape),
            _ => None,
        }
    }

    /// Whether a top-level property is written when the object's controlling property takes
    /// `value`: always for an ungated property, only on its `x-when` values for a gated one, never
    /// for one the shape does not have (e.g. `skills` removed for want of a catalog).
    pub fn live_on(&self, name: &str, value: &str) -> bool {
        match self.field(name) {
            Some(Shape::Gated { when, .. }) => when.iter().any(|known| known == value),
            Some(_) => true,
            None => false,
        }
    }

    /// The shape with every pick filled from `catalog` and capped at `max`; a pick with nothing to
    /// pick from (or `max` 0) is removed with its property, so the model never writes it.
    pub fn with_catalog(&self, catalog: &[String], max: usize) -> Shape {
        self.filled(CATALOG_PLACEHOLDER, catalog, Some(max))
    }

    /// The shape with the `@reads` pick filled with the code names a turn read or edited (none: the
    /// `binds` property goes, and items bind to nothing).
    pub fn with_reads(&self, reads: &[String], max: usize) -> Shape {
        self.filled(READS_PLACEHOLDER, reads, Some(max))
    }

    /// The shape with every enum and pick holding only `placeholder` filled with `values` (a pick
    /// capped at `max` when given). Nothing to choose from removes what cannot be written: a pick
    /// (or a list of such enums) loses its property; an object whose own property is such an enum
    /// cannot be written at all, so it goes too (and the list or property holding it).
    pub fn filled(&self, placeholder: &str, values: &[String], max: Option<usize>) -> Shape {
        fn is_placeholder(values: &[String], placeholder: &str) -> bool {
            values.len() == 1 && values[0] == placeholder
        }
        fn fill(shape: &Shape, placeholder: &str, values: &[String], max: Option<usize>) -> Option<Shape> {
            match shape {
                Shape::Pick { values: current, max: cap } if is_placeholder(current, placeholder) => {
                    let cap = max.unwrap_or(*cap);
                    (!values.is_empty() && cap > 0).then(|| Shape::Pick { values: values.to_vec(), max: cap })
                }
                Shape::OneOf(current) if is_placeholder(current, placeholder) => (!values.is_empty()).then(|| Shape::OneOf(values.to_vec())),
                Shape::Gated { on, when, inner } => {
                    fill(inner, placeholder, values, max).map(|inner| Shape::Gated { on: on.clone(), when: when.clone(), inner: Box::new(inner) })
                }
                Shape::Obj(properties) => {
                    let mut kept = Vec::new();
                    for (key, value) in properties {
                        match fill(value, placeholder, values, max) {
                            Some(value) => kept.push((key.clone(), value)),
                            // An object cannot be written without its enum property.
                            None if matches!(value, Shape::OneOf(_)) => return None,
                            None => {}
                        }
                    }
                    Some(Shape::Obj(kept))
                }
                Shape::List(item) => fill(item, placeholder, values, max).map(|item| Shape::List(Box::new(item))),
                Shape::ListMax(item, most) => fill(item, placeholder, values, max).map(|item| Shape::ListMax(Box::new(item), *most)),
                other => Some(other.clone()),
            }
        }
        fill(self, placeholder, values, max).unwrap_or(Shape::Obj(Vec::new()))
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
    if let Some(gate) = schema.get("x-when") {
        let (on, values) = gate.as_object().and_then(|gate| gate.iter().next()).ok_or("`x-when` must name one property")?;
        let when: Vec<String> = values
            .as_array()
            .ok_or("`x-when` values must be an array")?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or("`x-when` values must be strings"))
            .collect::<Result<_, _>>()?;
        let mut inner = schema.clone();
        if let Some(map) = inner.as_object_mut() {
            map.remove("x-when");
        }
        return Ok(Shape::Gated { on: on.clone(), when, inner: Box::new(shape_from_schema(&inner)?) });
    }
    if schema.get("type").and_then(Value::as_str) == Some("array")
        && schema.get("items").and_then(|items| items.get("enum")) == Some(&json!([CATALOG_PLACEHOLDER]))
    {
        let max = schema.get("maxItems").and_then(Value::as_u64).ok_or("a catalog pick needs `maxItems`")? as usize;
        return Ok(Shape::Pick { values: vec![CATALOG_PLACEHOLDER.to_owned()], max });
    }
    // Any other placeholder pick (`@verifiers`): a capped array of one run-time enum.
    if schema.get("type").and_then(Value::as_str) == Some("array")
        && let Some(placeholder) = schema
            .get("items")
            .and_then(|items| items.get("enum"))
            .and_then(Value::as_array)
            .filter(|values| values.len() == 1)
            .and_then(|values| values[0].as_str())
            .filter(|value| value.starts_with('@'))
        && let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
    {
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
        Shape::Gated { on, when, inner } => {
            let mut schema = json_schema(inner);
            if let Some(map) = schema.as_object_mut() {
                map.insert("x-when".into(), json!({ on: when }));
            }
            schema
        }
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
            let alternatives: Vec<String> = values.iter().map(|value| literal(&format!("\"{value}\""))).collect();
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
        // A gated property is written by its object (see below); alone it is its inner shape.
        Shape::Gated { inner, .. } => rule_for(inner, rules),
        Shape::Obj(properties) => {
            let field = |key: &str, rule: &str| format!("{} space \":\" space {rule}", literal(&format!("\"{key}\"")));
            let value_rules: Vec<String> = properties.iter().map(|(_, value)| rule_for(value, rules)).collect();
            // The gated properties of an object share one controlling enum written before them: the
            // object branches on its value, one branch per set of gated properties the values write
            // (DECIDE: `skills` on the working routes, `brief` on DELEGATE and PLAN only), with the
            // ungated properties written the same way on every branch.
            let gates: Vec<(usize, usize, &Vec<String>)> = properties
                .iter()
                .enumerate()
                .filter_map(|(index, (_, value))| match value {
                    Shape::Gated { on, when, .. } => properties.iter().position(|(key, _)| key == on).filter(|c| *c < index).map(|c| (c, index, when)),
                    _ => None,
                })
                .collect();
            let controller = gates.first().map(|(controller, _, _)| *controller).filter(|c| gates.iter().all(|(other, _, _)| other == c));
            let body = match controller {
                Some(controller) => {
                    let Shape::OneOf(values) = &properties[controller].1 else { unreachable!("a gate is controlled by an enum") };
                    let choice = |values: &[&String]| values.iter().map(|value| literal(&format!("\"{value}\""))).collect::<Vec<_>>().join(" | ");
                    let writes = |value: &String| -> Vec<bool> { gates.iter().map(|(_, _, when)| when.contains(value)).collect() };
                    // The values grouped by the gated properties they write, in enum order.
                    let mut branches: Vec<(Vec<&String>, Vec<bool>)> = Vec::new();
                    for value in values {
                        let signature = writes(value);
                        match branches.iter_mut().find(|(_, known)| *known == signature) {
                            Some((members, _)) => members.push(value),
                            None => branches.push((vec![value], signature)),
                        }
                    }
                    let branch = |members: &[&String], signature: &[bool]| {
                        let mut parts = vec![format!("{} space \":\" space ({}) space", literal(&format!("\"{}\"", properties[controller].0)), choice(members))];
                        for index in controller + 1..properties.len() {
                            let written = gates.iter().position(|(_, gated, _)| *gated == index).is_none_or(|gate| signature[gate]);
                            if written {
                                parts.push(field(&properties[index].0, &value_rules[index]));
                            }
                        }
                        parts.join(" \",\" space ")
                    };
                    let mut parts: Vec<String> = (0..controller).map(|i| field(&properties[i].0, &value_rules[i])).collect();
                    parts.push(format!("({})", branches.iter().map(|(members, signature)| branch(members, signature)).collect::<Vec<_>>().join(" | ")));
                    parts.join(" \",\" space ")
                }
                None => properties.iter().zip(&value_rules).map(|((key, _), rule)| field(key, rule)).collect::<Vec<_>>().join(" \",\" space "),
            };
            let name = format!("obj{}", rules.len());
            rules.push(format!("{name} ::= \"{{\" space {body} \"}}\" space"));
            name
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouterDecision {
    pub work: String,
    /// Each thing the message asks for, in its words (empty for small talk): what the flow
    /// checks the turn's work against before the reply goes back.
    #[serde(default)]
    pub asks: Vec<String>,
    pub route: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default)]
    pub focus: Vec<String>,
    /// Skills chosen for the work (`skill://<plugin>/<name>`), written right after the route on the
    /// routes `router.json` gates it to (the turn's first generation).
    #[serde(default)]
    pub skills: Vec<String>,
    /// no | yes: the latest message states something durable about the project's world (a
    /// KNOWLEDGE stage proposes items after the answer; docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    #[serde(default)]
    pub teaches: String,
    /// no | yes: the latest message sets aside the direction the user gave earlier in the
    /// conversation: the conversation's decisions in the project's knowledge are withdrawn.
    #[serde(default)]
    pub drops: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    #[serde(default)]
    pub missing: String,
    pub route: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub checks: Vec<String>,
    /// Skills that reach this decision's run (`skill://<plugin>/<name>`): what DECIDE adds after
    /// the reads (ROUTE chose the turn's first ones), or an IMPLEMENT decision's plan task skills.
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// REVIEW: after a task, whether the tasks still to do fit what the work showed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Review {
    #[serde(default)]
    pub found: String,
    #[serde(default)]
    pub replan: String,
}

/// MANAGE: what happens to a session's background agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Manage {
    #[serde(default)]
    pub actions: Vec<ManageAction>,
    #[serde(default)]
    pub replan: String,
    #[serde(default)]
    pub brief: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ManageAction {
    #[serde(default)]
    pub agent: String,
    /// stop | restart | message
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checks {
    #[serde(default)]
    pub checks: Vec<String>,
}

/// PLAN / PLAN_EDIT: a plan document and its task graph as the model writes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanOutput {
    #[serde(default)]
    pub analysis: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub approach: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<PlanTaskOutput>,
    /// Open background work this plan takes the place of (labels of the `@open_work` pick).
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanTaskOutput {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub brief: String,
    /// What the conversation already established that the task needs.
    #[serde(default)]
    pub known: Vec<String>,
    /// The request's rules this task makes true, one line each, in the request's words.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Numbers of earlier tasks ("1" = the first written).
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
    /// Skills the task's implementation reads (`skill://<plugin>/<name>`, from the catalog).
    #[serde(default)]
    pub skills: Vec<String>,
}

/// KNOWLEDGE: what the latest message stated about the project's world.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct KnowledgeProposed {
    #[serde(default)]
    pub items: Vec<ProposedKnowledge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProposedKnowledge {
    /// concept | fact | hypothesis | question
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub text: String,
    /// asserted | open | supported | refuted (the host keeps what the kind allows).
    #[serde(default)]
    pub state: String,
    /// Code names from the turn's reads the item binds to (the `@reads` enum; absent when the
    /// turn read nothing).
    #[serde(default)]
    pub binds: Vec<String>,
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
pub fn effort_or_default(effort: &str) -> String {
    let lower = effort.trim().to_ascii_lowercase();
    if EFFORTS.contains(&lower.as_str()) { lower } else { default_effort() }
}

/// The stronger of two efforts.
pub fn max_effort(a: &str, b: &str) -> String {
    let rank = |effort: &str| EFFORTS.iter().position(|known| *known == effort).unwrap_or(2);
    if rank(a) >= rank(b) { a.to_owned() } else { b.to_owned() }
}
