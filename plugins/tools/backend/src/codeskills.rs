//! Skills attached by diagnostic code. A plugin that owns skills declares, per skill, the exact
//! diagnostic codes it covers (`SkillInfo::codes`, from the plugin's `data/skills/codes.toml`). When
//! a tool result shows one of those codes, the result carries the skill: its text the first time in
//! the session, between "[skill for E0382]" and "[end of skill]"; afterwards nothing (the text is in
//! the conversation already). At most one skill's text per result; another skill the same result
//! leads to is named by its URI ("[skill for E0502: skill://lang-rust/rust-borrow-checker]").
//!
//! Where codes come from, all structural:
//! - a language server's diagnostics (after an edit, and `diagnostics://` reads): the diagnostic's
//!   own `code` field, reported by the language plugin that covers the file;
//! - a `run` whose command is one of the language plugins' verifier commands (`lang@1` describe's
//!   `verifiers`; a simple command of the script that starts with the verifier's words, `{file}` and
//!   `{files}` standing for one and for one or more words): the verifier's plugin's declared codes
//!   that appear as whole tokens of the output.
//!
//! A code is looked up in the skills of the plugin that reported it, then in the skills plugins'
//! (`skills@1`). Which skills a session already got lives in [`ReadState`].

use crate::host::Host;
use crate::readstate::ReadState;
use cc_protocol::contracts::lang::{Description, SkillInfo, SkillResult, Verifier, methods};
use serde_json::json;
use std::collections::HashMap;
use std::io::BufRead;

/// A diagnostic code a tool result shows, with the language plugin (category) that reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeHit {
    pub owner: String,
    pub code: String,
}

/// The declared codes of every plugin that carries skills: code -> skill name, per category.
#[derive(Debug, Clone, Default)]
pub struct CodeIndex {
    /// Language plugins' (and any other owner's) codes by category.
    owners: HashMap<String, HashMap<String, String>>,
    /// Skills plugins' codes, in category order: (category, code -> skill name).
    shared: Vec<(String, HashMap<String, String>)>,
    /// Always-loaded skills (`skill://<category>/<name>`): their text is in every session's system
    /// message already, so a code leads only to a pointer.
    always: std::collections::HashSet<String>,
}

impl CodeIndex {
    /// From (category, skills) of every skill source; `skills@1` categories are the shared ones.
    pub fn new(sources: Vec<(String, Vec<SkillInfo>)>) -> CodeIndex {
        let mut index = CodeIndex::default();
        for (category, skills) in sources {
            for skill in skills.iter().filter(|skill| skill.applies.always) {
                index.always.insert(format!("skill://{category}/{}", skill.name));
            }
            let codes: HashMap<String, String> =
                skills.into_iter().flat_map(|skill| skill.codes.into_iter().map(move |code| (code, skill.name.clone()))).collect();
            if codes.is_empty() {
                continue;
            }
            if cc_protocol::contracts::contract_of(&category) == Some(cc_protocol::contracts::skills::CONTRACT) {
                index.shared.push((category, codes));
            } else {
                index.owners.insert(category, codes);
            }
        }
        index.shared.sort_by(|a, b| a.0.cmp(&b.0));
        index
    }

    /// The skill (category, name) for `code` reported by `owner`: the owner's own skills first,
    /// then the skills plugins'.
    pub fn lookup(&self, owner: &str, code: &str) -> Option<(&str, &str)> {
        if let Some((category, codes)) = self.owners.get_key_value(owner)
            && let Some(skill) = codes.get(code)
        {
            return Some((category.as_str(), skill.as_str()));
        }
        self.shared.iter().find_map(|(category, codes)| codes.get(code).map(|skill| (category.as_str(), skill.as_str())))
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty() && self.shared.is_empty()
    }

    /// Whether `uri` is an always-loaded skill (in every session's system message).
    pub fn is_always(&self, uri: &str) -> bool {
        self.always.contains(uri)
    }
}

/// The index of the active plugins' declared codes.
pub async fn index(host: &dyn Host) -> CodeIndex {
    CodeIndex::new(crate::langs::skill_sources(host).await)
}

/// One skill a result's codes lead to.
#[derive(Debug, Clone, PartialEq)]
pub struct Matched {
    pub category: String,
    pub name: String,
    /// The result's codes that lead to it, in order.
    pub codes: Vec<String>,
}

impl Matched {
    pub fn uri(&self) -> String {
        format!("skill://{}/{}", self.category, self.name)
    }
}

/// The skills `hits` lead to, in the order of their first code.
pub fn matches(index: &CodeIndex, hits: &[CodeHit]) -> Vec<Matched> {
    let mut found: Vec<Matched> = Vec::new();
    for hit in hits {
        let Some((category, name)) = index.lookup(&hit.owner, &hit.code) else { continue };
        match found.iter_mut().find(|m| m.category == category && m.name == name) {
            Some(existing) => {
                if !existing.codes.contains(&hit.code) {
                    existing.codes.push(hit.code.clone());
                }
            }
            None => found.push(Matched { category: category.to_owned(), name: name.to_owned(), codes: vec![hit.code.clone()] }),
        }
    }
    found
}

/// What a result carries for `hits` in `session`: the first skill not yet attached in the session
/// inline (its text fetched with `<category>.skill`), every other one not yet shown as a one-line
/// pointer. A skill the session already has (attached earlier, or always loaded) adds nothing. None
/// when nothing is to be added.
pub async fn attach(host: &dyn Host, index: &CodeIndex, state: &ReadState, session: &str, hits: &[CodeHit]) -> Option<String> {
    attach_with_uris(host, index, state, session, hits).await.map(|(text, _)| text)
}

/// [`attach`], with the URIs of the skills whose text it carries (for the result's display: the
/// text names only the code).
pub async fn attach_with_uris(host: &dyn Host, index: &CodeIndex, state: &ReadState, session: &str, hits: &[CodeHit]) -> Option<(String, Vec<String>)> {
    let mut blocks = Vec::new();
    let mut uris = Vec::new();
    let mut inlined = false;
    for skill in matches(index, hits) {
        let uri = skill.uri();
        if index.is_always(&uri) || state.skill_attached(session, &uri) {
            continue;
        }
        let codes = skill.codes.join(", ");
        if !inlined && let Some(text) = skill_text(host, &skill.category, &skill.name).await {
            state.record_skill(session, &uri);
            inlined = true;
            blocks.push(format!("[skill for {codes}]\n{}\n[end of skill]", text.trim_end()));
            uris.push(uri);
            continue;
        }
        blocks.push(format!("[skill for {codes}: {uri}]"));
    }
    (!blocks.is_empty()).then(|| (blocks.join("\n"), uris))
}

async fn skill_text(host: &dyn Host, category: &str, name: &str) -> Option<String> {
    let value = host.call(&format!("{category}.{}", methods::SKILL), json!({"name": name})).await.ok()?;
    let skill: SkillResult = serde_json::from_value(value).ok()?;
    (!skill.text.trim().is_empty()).then_some(skill.text)
}

/// The categories of the language plugins one of whose verifiers `command` runs.
pub fn verifier_owners(plugins: &[(String, Description)], command: &str) -> Vec<String> {
    let commands = crate::approval::simple_commands(command);
    let mut owners: Vec<String> = Vec::new();
    for (category, description) in plugins {
        let runs = description.verifiers.iter().any(|verifier| commands.iter().any(|words| runs_verifier(words, verifier)));
        if runs && !owners.contains(category) {
            owners.push(category.clone());
        }
    }
    owners
}

/// Whether a simple command (its words; None for a word only known at run time) starts with the
/// verifier's command: equal literal words, `{file}` one word, `{files}` one or more words. Words
/// after the verifier's are allowed (`cargo test some_test`, `pytest -q tests/test_a.py`).
pub fn runs_verifier(words: &[Option<String>], verifier: &Verifier) -> bool {
    fn prefix(words: &[Option<String>], pattern: &[String]) -> bool {
        let Some((first, rest)) = pattern.split_first() else { return true };
        match first.as_str() {
            "{file}" => !words.is_empty() && prefix(&words[1..], rest),
            "{files}" => (1..=words.len()).any(|taken| prefix(&words[taken..], rest)),
            literal => words.first().is_some_and(|word| word.as_deref() == Some(literal)) && prefix(&words[1..], rest),
        }
    }
    !verifier.command.is_empty() && prefix(words, &verifier.command)
}

/// The tokens of a line: maximal runs of ASCII letters, digits, `_`, `-`, `.` and `+`, without a
/// trailing `.` (a sentence end). `error[E0382]:` -> `error`, `E0382`; `[-Wsign-compare]` ->
/// `-Wsign-compare`; `(reportMissingImports)` -> `reportMissingImports`.
pub fn tokens(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')))
        .map(|token| token.trim_end_matches('.'))
        .filter(|token| !token.is_empty())
}

/// The owners' declared codes that appear as whole tokens of a verifier's output, in order of first
/// appearance (each once). A code made only of digits (a language server's bare `2322`) is matched
/// only from a diagnostic's structured `code` field, never in output text, where any number could
/// be it; the printed form (`TS2322`) is matched here.
pub fn output_hits(index: &CodeIndex, owners: &[String], output: impl BufRead) -> Vec<CodeHit> {
    let mut hits: Vec<CodeHit> = Vec::new();
    if owners.is_empty() || index.is_empty() {
        return hits;
    }
    for line in output.split(b'\n').map_while(Result::ok) {
        let line = String::from_utf8_lossy(&line);
        for token in tokens(&line).filter(|token| !token.bytes().all(|byte| byte.is_ascii_digit())) {
            for owner in owners {
                if index.lookup(owner, token).is_some() {
                    let hit = CodeHit { owner: owner.clone(), code: token.to_owned() };
                    if !hits.contains(&hit) {
                        hits.push(hit);
                    }
                }
            }
        }
    }
    hits
}
