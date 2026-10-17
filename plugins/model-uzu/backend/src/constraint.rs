//! Output constraints: `json_schema`, a caller's GBNF `grammar`, and `tool_choice` forcing a call.
//! All become llguidance grammars applied inside uzu's sampling (vendor patch P2: GBNF is translated
//! to llguidance's Lark dialect here), where the token mask also prunes speculative draft trees and
//! masks every verified tree node, so drafted tokens that violate the grammar are never accepted. With thinking on, the constraint engages after `</think>` (reasoning is free).

use cc_protocol::RpcError;
use cc_protocol::contracts::model::ToolSpec;
use serde_json::Value;
use uzu_engine::engine::language_model::grammar::GrammarConfig;

/// What a request asks the output to follow.
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    JsonSchema(String),
    Gbnf(String),
    /// A grammar this plugin generates in llguidance's Lark dialect.
    Lark(String),
}

impl Constraint {
    pub fn config(&self) -> Result<GrammarConfig, RpcError> {
        Ok(match self {
            Self::JsonSchema(schema) => GrammarConfig::JsonSchema {
                schema: schema.clone(),
                any_whitespace: true,
                indent: None,
                separators: None,
                strict_mode: true,
            },
            Self::Gbnf(grammar) => GrammarConfig::Lark {
                grammar: gbnf_to_lark(grammar).map_err(|error| RpcError::invalid_params(format!("grammar: {error}")))?,
            },
            Self::Lark(grammar) => GrammarConfig::Lark { grammar: grammar.clone() },
        })
    }
}

/// One lexical element of a GBNF grammar.
#[derive(Debug, Clone, PartialEq)]
enum Lexeme {
    Name(String),
    Define,
    /// A string literal, unescaped.
    Literal(String),
    /// A character class: negated, then items (single characters or inclusive ranges).
    Class(bool, Vec<(char, char)>),
    Any,
    /// `*`, `+`, `?` or a `{m,n}` repetition, verbatim.
    Postfix(String),
    Open,
    Close,
    Bar,
}

fn gbnf_escape(chars: &[char], index: &mut usize) -> Result<char, String> {
    // At the character after the backslash.
    let c = *chars.get(*index).ok_or("dangling escape")?;
    *index += 1;
    let hex = |chars: &[char], index: &mut usize, digits: usize| -> Result<char, String> {
        let text: String = chars.get(*index..*index + digits).ok_or("short escape")?.iter().collect();
        *index += digits;
        u32::from_str_radix(&text, 16).ok().and_then(char::from_u32).ok_or_else(|| format!("bad escape \\{text}"))
    };
    Ok(match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        'x' => hex(chars, index, 2)?,
        'u' => hex(chars, index, 4)?,
        'U' => hex(chars, index, 8)?,
        other => other,
    })
}

fn lex_gbnf(text: &str) -> Result<Vec<Lexeme>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        match c {
            '#' => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            c if c.is_whitespace() => index += 1,
            ':' if chars.get(index + 1) == Some(&':') && chars.get(index + 2) == Some(&'=') => {
                out.push(Lexeme::Define);
                index += 3;
            }
            '"' => {
                index += 1;
                let mut literal = String::new();
                loop {
                    let c = *chars.get(index).ok_or("unterminated string literal")?;
                    index += 1;
                    match c {
                        '"' => break,
                        '\\' => literal.push(gbnf_escape(&chars, &mut index)?),
                        other => literal.push(other),
                    }
                }
                out.push(Lexeme::Literal(literal));
            }
            '[' => {
                index += 1;
                let negated = chars.get(index) == Some(&'^');
                if negated {
                    index += 1;
                }
                let mut items = Vec::new();
                loop {
                    let c = *chars.get(index).ok_or("unterminated character class")?;
                    index += 1;
                    let start = match c {
                        ']' => break,
                        '\\' => gbnf_escape(&chars, &mut index)?,
                        other => other,
                    };
                    let end = if chars.get(index) == Some(&'-') && chars.get(index + 1).is_some_and(|next| *next != ']') {
                        index += 1;
                        let c = chars[index];
                        index += 1;
                        if c == '\\' { gbnf_escape(&chars, &mut index)? } else { c }
                    } else {
                        start
                    };
                    items.push((start, end));
                }
                out.push(Lexeme::Class(negated, items));
            }
            '.' => {
                out.push(Lexeme::Any);
                index += 1;
            }
            '*' | '+' | '?' => {
                out.push(Lexeme::Postfix(c.to_string()));
                index += 1;
            }
            '{' => {
                let close = chars[index..].iter().position(|c| *c == '}').ok_or("unterminated repetition")?;
                let body: String = chars[index..index + close + 1].iter().filter(|c| !c.is_whitespace()).collect();
                out.push(Lexeme::Postfix(body));
                index += close + 1;
            }
            '(' => {
                out.push(Lexeme::Open);
                index += 1;
            }
            ')' => {
                out.push(Lexeme::Close);
                index += 1;
            }
            '|' => {
                out.push(Lexeme::Bar);
                index += 1;
            }
            c if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' => {
                let start = index;
                while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_' || chars[index] == '-') {
                    index += 1;
                }
                out.push(Lexeme::Name(chars[start..index].iter().collect()));
            }
            other => return Err(format!("unexpected character {other:?} in the grammar")),
        }
    }
    Ok(out)
}

/// A character for a Rust-regex class.
fn class_char_regex(c: char) -> String {
    match c {
        '\\' | ']' | '[' | '^' | '-' | '/' | '&' | '~' => format!("\\{c}"),
        c if (c as u32) < 0x20 || c == '\u{7f}' => format!("\\x{{{:x}}}", c as u32),
        other => other.to_string(),
    }
}

/// Rules of a lexed grammar: (name, body lexemes) in definition order.
fn split_rules(lexemes: &[Lexeme]) -> Result<Vec<(String, Vec<Lexeme>)>, String> {
    let mut rules: Vec<(String, Vec<Lexeme>)> = Vec::new();
    let mut index = 0;
    while index < lexemes.len() {
        let (Some(Lexeme::Name(name)), Some(Lexeme::Define)) = (lexemes.get(index), lexemes.get(index + 1)) else {
            return Err(format!("expected a rule definition at element {index}"));
        };
        index += 2;
        let start = index;
        while index < lexemes.len() && !matches!((&lexemes[index], lexemes.get(index + 1)), (Lexeme::Name(_), Some(Lexeme::Define))) {
            index += 1;
        }
        let body = lexemes[start..index].to_vec();
        match rules.iter_mut().find(|(existing, _)| existing == name) {
            // A repeated definition adds alternatives (GBNF semantics).
            Some((_, existing)) => {
                existing.push(Lexeme::Bar);
                existing.extend(body);
            }
            None => rules.push((name.clone(), body)),
        }
    }
    Ok(rules)
}

/// llama.cpp GBNF as a grammar in llguidance's Lark dialect (structural translation): `root` becomes
/// `start`; literals become JSON-escaped strings, character classes and `.` regular expressions, an
/// empty alternative `""`; repetition operators (`*`, `+`, `?`, `{m,n}`) carry over. Rules that are
/// regular (no recursion through them) become Lark terminals, so the lexer reads each as one
/// lexeme: without that, single-character lexemes and longer literals that share a prefix make the
/// maximal-munch lexer reject valid text.
pub fn gbnf_to_lark(text: &str) -> Result<String, String> {
    let lexemes = lex_gbnf(text)?;
    let rules = split_rules(&lexemes)?;
    if !rules.iter().any(|(name, _)| name == "root") {
        return Err("the grammar has no `root` rule".into());
    }
    let names: Vec<&String> = rules.iter().map(|(name, _)| name).collect();
    let references = |body: &[Lexeme]| -> Result<Vec<usize>, String> {
        body.iter()
            .filter_map(|lexeme| match lexeme {
                Lexeme::Name(reference) => Some(
                    names.iter().position(|name| *name == reference).ok_or_else(|| format!("rule `{reference}` is not defined")),
                ),
                _ => None,
            })
            .collect()
    };
    let graph: Vec<Vec<usize>> = rules.iter().map(|(_, body)| references(body)).collect::<Result<_, _>>()?;
    // A rule is regular when no path from it returns to any rule on the path (no recursion).
    fn reaches_cycle(node: usize, graph: &[Vec<usize>], path: &mut Vec<usize>, memo: &mut [Option<bool>]) -> bool {
        if path.contains(&node) {
            return true;
        }
        if let Some(known) = memo[node] {
            return known;
        }
        path.push(node);
        let cyclic = graph[node].iter().any(|&next| reaches_cycle(next, graph, path, memo));
        path.pop();
        memo[node] = Some(cyclic);
        cyclic
    }
    let mut memo = vec![None; rules.len()];
    let regular: Vec<bool> = (0..rules.len()).map(|node| !reaches_cycle(node, &graph, &mut Vec::new(), &mut memo)).collect();
    let mut mapped: Vec<String> = Vec::new();
    for (index, (name, _)) in rules.iter().enumerate() {
        let base: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
        let mut candidate = if regular[index] { format!("T_{}", base.to_ascii_uppercase()) } else { format!("r_{base}") };
        let mut n = 1;
        while mapped.contains(&candidate) {
            n += 1;
            candidate = if regular[index] { format!("T_{}_{n}", base.to_ascii_uppercase()) } else { format!("r_{base}_{n}") };
        }
        mapped.push(candidate);
    }
    let mut out = String::new();
    for (index, (name, body)) in rules.iter().enumerate() {
        let target = &mapped[index];
        if name == "root" {
            out.push_str(&format!("start: {target}\n"));
        }
        out.push_str(target);
        out.push(':');
        // `open` marks a position where an alternative starts.
        let mut open = true;
        for lexeme in body {
            match lexeme {
                Lexeme::Bar => {
                    if open {
                        out.push_str(" \"\"");
                    }
                    out.push_str(" |");
                    open = true;
                }
                Lexeme::Open => {
                    out.push_str(" (");
                    open = true;
                }
                Lexeme::Close => {
                    if open {
                        out.push_str(" \"\"");
                    }
                    out.push_str(" )");
                    open = false;
                }
                Lexeme::Postfix(op) => {
                    out.push_str(op);
                    open = false;
                }
                Lexeme::Name(reference) => {
                    let position = names.iter().position(|name| *name == reference).ok_or_else(|| format!("rule `{reference}` is not defined"))?;
                    out.push(' ');
                    out.push_str(&mapped[position]);
                    open = false;
                }
                Lexeme::Literal(literal) => {
                    out.push(' ');
                    out.push_str(&serde_json::to_string(literal).map_err(|error| error.to_string())?);
                    open = false;
                }
                Lexeme::Class(negated, items) => {
                    out.push_str(" /[");
                    if *negated {
                        out.push('^');
                    }
                    for (start, end) in items {
                        out.push_str(&class_char_regex(*start));
                        if end != start {
                            out.push('-');
                            out.push_str(&class_char_regex(*end));
                        }
                    }
                    out.push_str("]/");
                    open = false;
                }
                Lexeme::Any => {
                    out.push_str(" /(?s:.)/");
                    open = false;
                }
                Lexeme::Define => return Err("unexpected `::=`".into()),
            }
        }
        if open {
            out.push_str(" \"\"");
        }
        out.push('\n');
    }
    Ok(out)
}

/// How the request treats tools.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    /// Tools stay in the prompt (the prefix cache is shared with tool-using turns) but the output is
    /// not parsed for calls.
    None,
    Required,
    Named(String),
}

pub fn tool_choice(value: Option<&Value>, tools: &[ToolSpec]) -> Result<ToolChoice, RpcError> {
    let choice = match value {
        None | Some(Value::Null) => ToolChoice::Auto,
        Some(Value::String(text)) => match text.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" | "any" => ToolChoice::Required,
            other => return Err(RpcError::invalid_params(format!("unknown tool_choice `{other}`"))),
        },
        Some(object) => match object.pointer("/function/name").or_else(|| object.get("name")).and_then(Value::as_str) {
            Some(name) => ToolChoice::Named(name.to_owned()),
            None => return Err(RpcError::invalid_params(format!("unsupported tool_choice {object}"))),
        },
    };
    match &choice {
        ToolChoice::Required if tools.is_empty() => {
            Err(RpcError::invalid_params("tool_choice `required` needs at least one tool"))
        }
        ToolChoice::Named(name) if !tools.iter().any(|tool| &tool.name == name) => {
            Err(RpcError::invalid_params(format!("tool_choice names `{name}`, which is not among the tools")))
        }
        _ => Ok(choice),
    }
}

/// A Lark grammar forcing one native Qwen3.8 tool call (of `name`, or of any of `tools`): the exact
/// markup the template teaches, parameter names from each tool's schema, and each value free text
/// up to the first `\n</parameter>` (a lazy lexeme: it ends at the first closing tag).
pub fn tool_call_grammar(tools: &[ToolSpec], name: Option<&str>) -> String {
    // `<tool_call>` and `</tool_call>` are single added tokens of the Qwen vocabulary, which
    // llguidance addresses as special tokens (`<name>`), not as text.
    let mut out = String::from("start: <tool_call> \"\\n\" call \"</function>\\n\" </tool_call>\n");
    let mut calls = Vec::new();
    for (index, tool) in tools.iter().filter(|tool| name.is_none_or(|name| tool.name == name)).enumerate() {
        let parameters: Vec<String> = tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        let header = serde_json::to_string(&format!("<function={}>\n", tool.name)).unwrap_or_default();
        calls.push(format!("{header} param{index}*"));
        let names = if parameters.is_empty() {
            "/[^>\\n]+/".to_owned()
        } else {
            parameters.iter().map(|parameter| serde_json::to_string(parameter).unwrap_or_default()).collect::<Vec<_>>().join(" | ")
        };
        out.push_str(&format!("param{index}: \"<parameter=\" ({names}) \">\\n\" value\n"));
    }
    out.push_str(&format!("call: {}\n", calls.join(" | ")));
    // Lazy: the value ends at the first closing tag.
    out.push_str("value[lazy]: /(?s:.*)\\n<\\/parameter>\\n/\n");
    out
}
