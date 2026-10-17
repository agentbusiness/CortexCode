//! build.gradle / build.gradle.kts: the project's `group` and the dependencies its `dependencies {}`
//! blocks declare.
//!
//! Structural at the token level, for both the Groovy and the Kotlin DSL: comments and string
//! literals are lexed (`//`, `/* */`, `'..'`, `".."`, `'''..'''`, `""".."""`), braces and parentheses
//! nested. Inside a `dependencies { }` block each statement is `<configuration> <notation>` or
//! `<configuration>(<notation>)`, where the notation is a `"group:artifact:version"` string, a map
//! / named arguments (`group: 'g', name: 'a', version: 'v'` or `group = "g", ...`), `project(":x")`,
//! `platform(...)` / `enforcedPlatform(...)` around a string, or a version-catalog reference
//! (`libs.x.y`). Anything else is left out, never guessed. A `dependencies` block inside
//! `buildscript { }` declares build classpath dependencies (`section` `build`). The artifact name of
//! a Gradle project lives in settings.gradle, so `package` is the group only.

use cc_lang_kit::cc_protocol::contracts::lang::{DependencyFact, FileFacts};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Punct(char),
    Newline,
}

/// Tokens of a Gradle script; Err when a string or block comment does not end.
fn lex(text: &str) -> Result<Vec<Token>, ()> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' || c == ';' {
            tokens.push(Token::Newline);
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let end = (i + 2..chars.len().saturating_sub(1)).find(|&j| chars[j] == '*' && chars[j + 1] == '/').ok_or(())?;
            // Keep the line structure for statement boundaries.
            if chars[i..end].contains(&'\n') {
                tokens.push(Token::Newline);
            }
            i = end + 2;
        } else if c == '"' || c == '\'' {
            let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
            let (open, close_len) = if triple { (i + 3, 3) } else { (i + 1, 1) };
            let mut j = open;
            let mut value = String::new();
            loop {
                let ch = *chars.get(j).ok_or(())?;
                if ch == '\\' && !triple {
                    if let Some(escaped) = chars.get(j + 1) {
                        value.push(*escaped);
                    }
                    j += 2;
                    continue;
                }
                if ch == c && (close_len == 1 || (chars.get(j + 1) == Some(&c) && chars.get(j + 2) == Some(&c))) {
                    break;
                }
                if ch == '\n' && !triple {
                    return Err(());
                }
                value.push(ch);
                j += 1;
            }
            tokens.push(Token::Str(value));
            i = j + close_len;
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let start = i;
            // Dotted names (`libs.spring.boot`) are one identifier.
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '.')) {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            tokens.push(Token::Punct(c));
            i += 1;
        }
    }
    Ok(tokens)
}

/// The index just past the bracket closing the one at `open` (`(`/`)` or `{`/`}`).
fn matching(tokens: &[Token], open: usize) -> Option<usize> {
    let (o, c) = match tokens.get(open)? {
        Token::Punct('(') => ('(', ')'),
        Token::Punct('{') => ('{', '}'),
        _ => return None,
    };
    let mut depth = 0i32;
    for (j, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Punct(p) if *p == o => depth += 1,
            Token::Punct(p) if *p == c => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Configurations whose dependencies only tests see (Gradle's `test*` source-set naming,
/// Android's `androidTest*`).
fn section_of(configuration: &str, buildscript: bool) -> &'static str {
    if buildscript {
        "build"
    } else if configuration.starts_with("test") || configuration.contains("Test") {
        "dev"
    } else if matches!(configuration, "annotationProcessor" | "kapt" | "ksp") {
        "build"
    } else if configuration == "compileOnly" {
        "provided"
    } else {
        "dependencies"
    }
}

/// A dependency notation's (name, spec) from the tokens of one statement after the configuration.
fn notation(tokens: &[Token]) -> Option<(String, String)> {
    let inner: Vec<&Token> = tokens.iter().filter(|t| !matches!(t, Token::Punct('(' | ')') | Token::Newline)).collect();
    match inner.as_slice() {
        [Token::Str(coordinate), ..] => {
            let mut parts = coordinate.splitn(4, ':');
            let (group, artifact) = (parts.next()?, parts.next()?);
            Some((format!("{group}:{artifact}"), parts.next().unwrap_or("").to_owned()))
        }
        [Token::Ident(f), Token::Str(coordinate), ..] if f == "platform" || f == "enforcedPlatform" => {
            let mut parts = coordinate.splitn(4, ':');
            let (group, artifact) = (parts.next()?, parts.next()?);
            Some((format!("{group}:{artifact}"), parts.next().unwrap_or("").to_owned()))
        }
        [Token::Ident(f), Token::Str(path), ..] if f == "project" => Some((path.clone(), "project".into())),
        [Token::Ident(reference), ..] if reference.starts_with("libs.") => Some((reference.clone(), "catalog".into())),
        _ => {
            // Map / named-argument notation: group, name, version keys followed by `:` or `=`.
            let mut values = std::collections::HashMap::new();
            for window in inner.windows(3) {
                if let [Token::Ident(key), Token::Punct(':' | '='), Token::Str(value)] = window {
                    values.insert(key.as_str(), value.clone());
                }
            }
            let (group, name) = (values.get("group")?, values.get("name")?);
            Some((format!("{group}:{name}"), values.get("version").cloned().unwrap_or_default()))
        }
    }
}

/// The dependencies of the `dependencies` block whose body is `tokens[start..end]`.
fn block(tokens: &[Token], start: usize, end: usize, buildscript: bool, out: &mut Vec<DependencyFact>) {
    let mut i = start;
    while i < end {
        let Token::Ident(configuration) = &tokens[i] else {
            i += 1;
            continue;
        };
        // The statement: up to the end of the line, past any brackets it opens.
        let mut j = i + 1;
        while j < end {
            match tokens[j] {
                Token::Newline => break,
                Token::Punct('(' | '{') => j = matching(tokens, j).unwrap_or(end).min(end),
                _ => j += 1,
            }
        }
        let statement = &tokens[i + 1..j];
        // `exclude`, `constraints { }` and closures configure, they declare nothing themselves.
        if !statement.is_empty() && !matches!(statement.first(), Some(Token::Punct('{'))) && let Some((name, spec)) = notation(statement) {
            out.push(DependencyFact { name, ecosystem: "maven".into(), spec, section: section_of(configuration, buildscript).into(), installed_version: None, source_dir: None });
        }
        i = j + 1;
    }
}

pub fn extract(text: &str, facts: &mut FileFacts) {
    let Ok(tokens) = lex(text) else {
        facts.parse_errors = 1;
        return;
    };
    // Walk the tree of `{ }` blocks, remembering which are `buildscript`.
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Punct('{') => depth += 1,
            Token::Punct('}') => {
                depth -= 1;
                while stack.last().is_some_and(|(d, _)| *d as i32 > depth) {
                    stack.pop();
                }
            }
            Token::Ident(name) if name == "buildscript" && tokens.get(i + 1) == Some(&Token::Punct('{')) => stack.push((depth as usize + 1, true)),
            Token::Ident(name) if name == "dependencies" && tokens.get(i + 1) == Some(&Token::Punct('{')) => {
                let Some(end) = matching(&tokens, i + 1) else {
                    facts.parse_errors = 1;
                    return;
                };
                let buildscript = stack.iter().any(|(_, b)| *b);
                block(&tokens, i + 2, end - 1, buildscript, &mut facts.dependencies);
                i = end;
                continue;
            }
            Token::Ident(name) if name == "group" && depth == 0 => {
                if let (Some(Token::Punct('=')), Some(Token::Str(group))) = (tokens.get(i + 1), tokens.get(i + 2)) {
                    facts.package = Some(group.clone());
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        facts.parse_errors = 1;
    }
}
