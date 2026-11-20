//! Lead mode (docs/FRONTIER-AGENTS.md): while a frontier CLI (Claude Code, Codex) does agent
//! work, a user's chat session leads. It plans, watches and directs agents and changes no file
//! itself: its tools are read and run only (the tools plugin's lead specs and guide, and `edit` is
//! refused), and every change of the work goes to agents (`implement`, the auto plan and the
//! skills' units hand their tasks over whatever the local capacity is).
//!
//! Who does agent work is the agents plugin's knowledge (`agents.workers`): the chat flow asks it
//! and never looks at the CLIs or the setting itself. Without an agents plugin, nothing leads.

use crate::host::Host;
use cc_protocol::contracts::agents::{WorkersResult, methods as agents_methods, workers};
use cc_protocol::contracts::frame::{Session, SessionKind};
use serde_json::json;

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
