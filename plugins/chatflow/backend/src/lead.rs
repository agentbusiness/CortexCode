//! Lead mode (docs/FRONTIER-AGENTS.md): while a frontier CLI (Claude Code, Codex) does agent
//! work, a user's chat session leads. It plans, watches and directs agents and changes no file
//! itself: its tools are read and run only (the tools plugin's lead specs and guide, and `edit` is
//! refused), and every change of the work goes to agents (`implement`, the auto plan and the
//! skills' units hand their tasks over whatever the local capacity is).
//!
//! Who does agent work is the agents plugin's knowledge (`agents.workers`): the chat flow asks it
//! and never looks at the CLIs or the setting itself. Without an agents plugin, nothing leads.

use crate::host::Host;
use crate::journal::Journal;
use cc_protocol::contracts::agents::{WorkersResult, methods as agents_methods, workers};
use cc_protocol::contracts::frame::{Session, SessionKind, kinds};
use serde_json::{Value, json};

/// Whether a frontier worker does agent work now.
pub async fn frontier_active(host: &Host) -> bool {
    match host.get::<WorkersResult>(agents_methods::WORKERS, json!({})).await {
        Ok(listed) => workers::frontier(&listed.active),
        Err(_) => false,
    }
}

/// Whether `session` leads: a user's conversation (not an agent's) while a frontier worker does
/// agent work.
pub async fn leads(host: &Host, session: &Session) -> bool {
    session.kind == SessionKind::Chat && frontier_active(host).await
}

/// What a lead's conversation holds in the model's pool now: its newest call's prompt and
/// answer (that prompt held the whole conversation). 0 before its first call. Never the context
/// window: a reservation of the window leaves no room for the agents the lead waits for.
pub fn held_tokens(journal: &Journal) -> u32 {
    journal
        .entries()
        .filter(|entry| entry.kind == kinds::USAGE)
        .last()
        .map(|usage| {
            let tokens = |field: &str| usage.data.get(field).and_then(Value::as_u64).unwrap_or(0);
            u32::try_from(tokens("prompt_tokens") + tokens("completion_tokens")).unwrap_or(u32::MAX)
        })
        .unwrap_or(0)
}

/// Lead sessions whose model reservation this instance holds (to give them all back when lead
/// mode ends). Runtime state, like the claims themselves: the mesh keeps reservations in memory.
static RESERVED: std::sync::Mutex<std::collections::BTreeSet<String>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Keep (or give back) this computer's model for a lead session between its calls
/// (`mesh.reserve`): the lead's large conversation stays in the model's pool while it waits for
/// its agents, and a member's work is placed here only when it fits beside it. Without the mesh
/// plugin nothing is kept, and nothing needs to be.
pub async fn reserve(host: &Host, session_id: &str, tokens: u32, keep: bool) {
    let params = json!({"session_id": session_id, "tag": crate::chatflow::FLOW, "tokens": tokens, "reserve": keep});
    if let Err(error) = host.call(cc_protocol::contracts::mesh::methods::RESERVE, params).await {
        tracing::debug!(%error, "the model was not reserved for the lead");
        return;
    }
    let mut reserved = RESERVED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if keep {
        reserved.insert(session_id.to_owned());
    } else {
        reserved.remove(session_id);
    }
}

/// Lead mode ended (the local model does agent work again): every lead's reservation goes back.
pub async fn release_all(host: &Host) {
    let sessions: Vec<String> = RESERVED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().cloned().collect();
    for session in sessions {
        reserve(host, &session, 0, false).await;
    }
}
