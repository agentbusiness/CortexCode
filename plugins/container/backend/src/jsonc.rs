//! JSON with comments (`devcontainer.json` is JSONC): `//` and `/* */` comments and trailing commas
//! before `}` or `]` are allowed. [`strip`] turns such text into plain JSON (comments become spaces,
//! so error positions keep their line numbers) and [`parse`] parses it.

use serde_json::Value;

/// Remove comments and trailing commas outside of strings.
pub fn strip(text: &str) -> Result<String, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            // A string: copied verbatim, honouring escapes.
            out.push(c);
            i += 1;
            loop {
                let Some(&s) = chars.get(i) else {
                    return Err("unterminated string".into());
                };
                out.push(s);
                i += 1;
                if s == '\\' {
                    let Some(&escaped) = chars.get(i) else {
                        return Err("unterminated string".into());
                    };
                    out.push(escaped);
                    i += 1;
                } else if s == '"' {
                    break;
                }
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            out.push_str("  ");
            i += 2;
            loop {
                if i >= chars.len() {
                    return Err("unterminated /* comment".into());
                }
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    out.push_str("  ");
                    i += 2;
                    break;
                }
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(remove_trailing_commas(&out))
}

/// Drop a `,` whose next non-whitespace character is `}` or `]` (outside strings). Runs on text
/// whose comments are already blanked.
fn remove_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                out.push(' ');
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

pub fn parse(text: &str) -> Result<Value, String> {
    let plain = strip(text)?;
    serde_json::from_str(&plain).map_err(|e| e.to_string())
}
