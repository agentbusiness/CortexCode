//! The plain-English card text of a proposed code change (what, why, risk), written by the local
//! model from the change itself. The instructions are this plugin's data (`backend/data/summary.md`),
//! read live from the code folder like any plugin's data.

use cc_protocol::Ctx;
use cc_protocol::contracts::model::{ChatMessage, ChatParams, ChatResult, Priority, Thinking, methods as model_methods};
use cc_protocol::contracts::source::FileDiff;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

const EMBEDDED: &str = include_str!("../data/summary.md");
/// Characters of the diff the model reads (the start of every file first, then the rest).
const DIFF_CHARS: usize = 16_000;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Card {
    pub summary: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub risk: String,
}

fn instructions(ctx: &Ctx) -> String {
    let live = ctx.env.live_or("backend/data", PathBuf::new()).join("summary.md");
    std::fs::read_to_string(live).ok().filter(|text| !text.trim().is_empty()).unwrap_or_else(|| EMBEDDED.to_owned())
}

fn change_text(plugins: &[String], diffs: &[FileDiff]) -> String {
    let mut text = format!("Parts changed: {}\n\n", plugins.join(", "));
    for diff in diffs {
        if text.len() >= DIFF_CHARS {
            text.push_str(&format!("(and {} more)\n", diff.path));
            continue;
        }
        let room = DIFF_CHARS.saturating_sub(text.len());
        let patch: String = diff.patch.chars().take(room).collect();
        text.push_str(&format!("--- {} ({})\n{patch}\n", diff.path, diff.status));
    }
    text
}

fn message(role: &str, content: String) -> ChatMessage {
    ChatMessage { role: role.into(), content, reasoning_content: None, tool_calls: Vec::new(), tool_call_id: None, name: None, parts: Vec::new() }
}

/// Ask the model for the card; `None` when the model is not there or answers nothing usable.
pub async fn write(ctx: &Ctx, plugins: &[String], diffs: &[FileDiff]) -> Option<Card> {
    let params = ChatParams {
        messages: vec![
            message("system", instructions(ctx)),
            message("user", change_text(plugins, diffs)),
        ],
        json_schema: Some(json!({
            "type": "object",
            "properties": {"summary": {"type": "string"}, "why": {"type": "string"}, "risk": {"type": "string"}},
            "required": ["summary", "why", "risk"]
        })),
        thinking: Thinking { enabled: false, effort: None, budget_tokens: None },
        priority: Priority::Background,
        cache_key: Some("source-manager.summary".into()),
        max_tokens: Some(400),
        tools: Vec::new(),
        tool_choice: None,
        grammar: None,
        sampling: Default::default(),
        stop: Vec::new(),
    };
    let value = ctx.call(model_methods::CHAT, serde_json::to_value(params).ok()?).await.ok()?;
    let result: ChatResult = serde_json::from_value(value).ok()?;
    let card: Card = serde_json::from_str(result.content.trim()).ok()?;
    (!card.summary.trim().is_empty()).then_some(card)
}

/// The card when the model cannot write one: what changed, mechanically.
pub fn fallback(plugin_names: &[String], files: &[String]) -> Card {
    Card {
        summary: format!("Changes {} file{} in {}.", files.len(), if files.len() == 1 { "" } else { "s" }, plugin_names.join(", ")),
        why: String::new(),
        risk: String::new(),
    }
}
