//! Assembly of what the model reads from the host. The words themselves are data
//! (`data/prompts/*.md`, `data/texts.toml`, see [`crate::data`]); this module only arranges them.
//!
//! Layout (stable prefix first; a per-turn value in the prefix measured 44x slower):
//! 1. a byte-stable system prompt per session and role: policy, the stage protocol of the role's
//!    stages, the tools guide, project rules, the user's style profile and the repository map;
//! 2. the conversation projected from the journal;
//! 3. one trailing host message naming the stage and carrying that call's live data, and the rules
//!    of a stage the role's system prompt does not carry (the one-shot grammar stages).

use crate::data::Dataset;
use crate::projection::host_message;
use cc_protocol::contracts::model::ChatMessage;
use std::collections::HashSet;

/// The prompt ids `role`'s system prompt carries in its stage protocol.
fn carried(data: &Dataset, role: &str) -> HashSet<String> {
    data.protocol(role).iter().flat_map(|stage| data.rule_parts(stage).iter().cloned()).collect()
}

/// The stage protocol carried in `role`'s cached prefix: each rule part once, in stage order.
pub fn stage_protocol(data: &Dataset, role: &str) -> String {
    let mut text = data.text("protocol_heading", &[]);
    let mut seen: HashSet<&str> = HashSet::new();
    for stage in data.protocol(role) {
        let rules: Vec<&str> = data.rule_parts(stage).iter().filter(|id| seen.insert(id.as_str())).map(|id| data.prompt(id)).collect();
        text.push_str("\n\n");
        text.push_str(&data.text("protocol_stage", &[("stage", stage), ("rules", &rules.join("\n"))]));
    }
    text
}

/// The rules a stage's own message carries for `role`: the parts of its rules that `role`'s system
/// prompt does not carry (none for a stage in the role's protocol).
pub fn stage_rules(data: &Dataset, role: &str, stage: &str) -> String {
    if data.in_prefix(role, stage) {
        return String::new();
    }
    let carried = carried(data, role);
    data.rule_parts(stage).iter().filter(|id| !carried.contains(*id)).map(|id| data.prompt(id)).collect::<Vec<_>>().join("\n")
}

/// Parts of the system prompt besides the policy and the protocol. Empty parts are left out.
pub struct PrefixParts<'a> {
    pub tools_guide: &'a str,
    pub rules: &'a [String],
    pub style: &'a [String],
    /// The project's knowledge brief (`memory.knowledge.brief`), already within its token budget.
    pub brief: &'a str,
    /// The project's own contract section (its `AGENTS.md` or `CLAUDE.md`), already rendered.
    pub contract: &'a str,
    pub repo_map: &'a str,
}

fn list(data: &Dataset, items: &[String]) -> String {
    items.iter().map(|item| data.text("list_item", &[("text", item.trim())])).collect::<Vec<_>>().join("\n")
}

/// The session's system prompt for `role`. Contains nothing that changes during a session.
pub fn system_prompt(data: &Dataset, role: &str, parts: &PrefixParts<'_>) -> String {
    let mut sections = vec![data.prompt("policy").to_owned(), stage_protocol(data, role)];
    if !parts.tools_guide.trim().is_empty() {
        sections.push(data.text("section_tools", &[("text", parts.tools_guide.trim())]));
    }
    if !parts.rules.is_empty() {
        sections.push(data.text("section_rules", &[("text", &list(data, parts.rules))]));
    }
    if !parts.style.is_empty() {
        sections.push(data.text("section_style", &[("text", &list(data, parts.style))]));
    }
    if !parts.contract.trim().is_empty() {
        sections.push(parts.contract.trim().to_owned());
    }
    if !parts.brief.trim().is_empty() {
        sections.push(data.text("section_knowledge", &[("text", parts.brief.trim())]));
    }
    if !parts.repo_map.trim().is_empty() {
        sections.push(data.text("section_map", &[("text", parts.repo_map.trim())]));
    }
    sections.join("\n\n")
}

/// The skills catalog as a stage message shows it: `skill://<plugin>/<name>  <summary>` lines in
/// the order given. Empty for no lines.
pub fn skills_list(data: &Dataset, lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    data.text("skills_list", &[("text", &list(data, lines))])
}

/// The trailing host message of one call for `role`: the stage, the rules its system prompt does not
/// carry, and the call's live data.
pub fn stage_message(data: &Dataset, role: &str, stage: &str, live: &str) -> ChatMessage {
    let rules = stage_rules(data, role, stage);
    let live = [rules.as_str(), live.trim()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
    if live.is_empty() {
        host_message(data, &data.text("stage", &[("stage", stage)]))
    } else {
        host_message(data, &data.text("stage_live", &[("stage", stage), ("live", &live)]))
    }
}

/// Corrective message after a structural repetition (identical call, identical result).
/// `research`: the research plugin is active, so finding out is a step the model can take.
pub fn repetition_correction(data: &Dataset, what: &str, research: bool) -> String {
    let lookup = if research { data.text("repetition_research", &[]) } else { String::new() };
    data.text("repetition", &[("what", what), ("research", &lookup)])
}

/// The host's message after a failing contract.
pub fn contract_failure(data: &Dataset, report: &str) -> String {
    data.text("contract_failure", &[("report", report)])
}
