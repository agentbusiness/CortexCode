//! Lent frontier CLIs (`agents.frontier.*`): a computer that has Claude Code or Codex signed in
//! runs it for a member, as it lends its model. Part of the shared standard (`AGENTS.md`): the
//! mesh passes these methods on to the serving computer's frame ([`lent`]), under the same switch
//! as its model (`mesh.lend`).
//!
//! - **The CLI only thinks there.** It runs with no tools of its own, in an empty folder. Its one
//!   MCP server is a pipe back to the caller: every MCP message the CLI writes comes to the caller
//!   as a stream item, and the caller answers with [`methods::MCP`]. So the caller's own tools do
//!   the work, on the caller's computer, where the project is; the lending computer needs no copy
//!   of it and nothing of its disk is read or changed.
//! - **One run is one CLI process.** [`methods::RUN`] starts it with its first message and answers
//!   when it ends; the CLI's own output lines come as stream items. A CLI that takes further
//!   messages on its input (Claude Code) gets them with [`methods::SAY`], and [`methods::CLOSE`]
//!   ends its input.
//! - **Places.** A computer runs as many CLIs at once as it says ([`Offer::limit`]), its own work
//!   and its members' together; a run beyond that waits its turn there and says so ([`Item::queued`]).
//! - **Optional.** An app with no such plugin answers nothing here, and a caller that finds no
//!   offer does without.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The methods the mesh passes on for a member.
pub const PREFIX: &str = "agents.frontier.";

/// Whether a member may ask this of another computer's frame.
pub fn lent(method: &str) -> bool {
    method.starts_with(PREFIX)
}

pub mod methods {
    /// `{}` -> [`super::Offers`]: the frontier CLIs this computer can run now.
    pub const OFFERS: &str = "agents.frontier.offers";
    /// [`super::RunParams`] -> [`super::RunResult`]; stream items are [`super::Item`]s.
    pub const RUN: &str = "agents.frontier.run";
    /// [`super::SayParams`] -> `{}`: the run's next message (Claude Code: a running session takes
    /// it when its turn ends).
    pub const SAY: &str = "agents.frontier.say";
    /// [`super::RunRef`] -> `{}`: no further message comes; the CLI ends after its turn.
    pub const CLOSE: &str = "agents.frontier.close";
    /// [`super::McpParams`] -> `{}`: one MCP message for the run's CLI (the answer to a message it
    /// sent as an [`super::Item::mcp`]).
    pub const MCP: &str = "agents.frontier.mcp";
}

/// One CLI a computer can run for others.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Offer {
    /// `claude` | `openai`.
    pub id: String,
    pub label: String,
    /// What the CLI's `--version` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Why it cannot work now, in plain words (absent when it can).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// CLI runs going on this computer now (every worker's together).
    #[serde(default)]
    pub running: u32,
    /// CLI runs this computer takes at once.
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Offers {
    pub workers: Vec<Offer>,
}

/// One CLI process to start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunParams {
    /// The caller's name for this run, unique (`run-<12 hex>`): [`methods::SAY`],
    /// [`methods::CLOSE`] and [`methods::MCP`] carry it.
    pub run: String,
    /// `claude` | `openai`.
    pub worker: String,
    pub model: String,
    #[serde(default)]
    pub effort: String,
    /// The role prompt (appended to the CLI's own system prompt).
    #[serde(default)]
    pub prompt: String,
    /// The JSON Schema the run's last answer follows.
    #[serde(default)]
    pub schema: Value,
    /// The caller's session the run is for: the CLI's folder on the serving computer is named by
    /// it, so a later run finds the CLI's session again.
    #[serde(default)]
    pub session: String,
    /// The CLI's own session to go on with (from an earlier run on this computer); absent: a new
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The first message.
    pub message: String,
}

/// One stream item of a run: exactly one field is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Item {
    /// One line of the CLI's own output (its JSON events), as it wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
    /// One MCP message (JSON-RPC) the CLI sent to its tools: the caller's tools answer it with
    /// [`methods::MCP`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Value>,
    /// The run waits for a free place on the serving computer: how many runs are ahead of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunResult {
    /// The CLI ended cleanly.
    pub ok: bool,
    /// The end of the CLI's error output, when it did not.
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunRef {
    pub run: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SayParams {
    pub run: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct McpParams {
    pub run: String,
    /// The JSON-RPC message.
    pub message: Value,
}
