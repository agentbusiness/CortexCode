//! Prefilling a chat session's prompt prefix before its next model call (the `prewarm_block` / `prime_history` pattern).
//!
//! Every call of a session starts with the same messages: the system message (with the skills
//! catalog, see [`crate::context::Prefix::system_message`]) and the conversation projected from
//! the journal ([`conversation`], the one builder every call uses, so the bytes match). A turn's
//! first call (ROUTE) adds only the new user message and its stage message. When that prefix is
//! not in the model's cache (a session's first turn, a conversation opened again, another
//! conversation used the slot), ROUTE re-reads all of it while the user waits; prefilled in the
//! background beforehand, ROUTE reads only what is new.
//!
//! [`closed`] ends the prefix with a placeholder message: the model plugin keeps a reuse point
//! before a prompt's last message, so the point lands right after the conversation, which the next
//! call continues (a prompt ending in the template's open answer would not be a prefix of it).
//! The prefill runs at background priority (`model.prewarm`); the model plugin stops it when a real
//! request for the conversation arrives (that request continues from what it prefilled), and the
//! session's next turn cancels it here too ([`crate::fit::Prewarm`] on drop).

use crate::context::Prefix;
use crate::data::Dataset;
use crate::fit::Prewarm;
use crate::host::Host;
use crate::projection::project_with;
use cc_protocol::contracts::frame::JournalEntry;
use cc_protocol::contracts::model::{ChatMessage, ToolSpec, methods as model_methods};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// The placeholder last message of a prefilled prefix (never seen by a real call).
pub const PLACEHOLDER: &str = ".";

/// What every call of a session starts with: its system message and the projected conversation.
pub fn conversation(prefix: &Prefix, data: &Dataset, entries: &[JournalEntry], image_input: bool) -> Vec<ChatMessage> {
    let mut messages = vec![prefix.system_message(data)];
    messages.extend(project_with(entries, data, image_input));
    messages
}

/// What a project's next chat conversation starts with, before it exists: its system message
/// (the prefix a new session builds, [`crate::context::build`]), closed. Prefilled `shared_only`:
/// the model keeps the block for whichever conversation starts with it, so the new conversation's
/// first call (ROUTE) reads only its message.
pub fn next_conversation(prefix: &Prefix, data: &Dataset) -> Vec<ChatMessage> {
    closed(vec![prefix.system_message(data)])
}

/// `messages` closed with the placeholder, as prefilled.
pub fn closed(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    messages.push(ChatMessage::new("user", PLACEHOLDER));
    messages
}

/// Identity of one prefill: the cache key and the exact request (same state, same key).
pub fn state_key(cache_key: &str, messages: &[ChatMessage], tools: &[ToolSpec]) -> String {
    let mut hash = Sha256::new();
    hash.update(cache_key.as_bytes());
    hash.update([0]);
    hash.update(json!({"messages": messages, "tools": tools}).to_string().as_bytes());
    hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// When a prefill runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// As soon as the model has room: the conversation is in front of the user, or at work.
    Soon,
    /// Only while the model serves no other conversation: a guess that the conversation goes on
    /// (its turn just ended) must never slow work under way; another conversation's request
    /// stops it, and what it prefilled until then is kept.
    Idle,
}

/// Send `model.prewarm` for `messages` in the background. `ended(true)` when the model plugin
/// prefilled them (or stopped for a real request), `ended(false)` when it refused or failed.
/// Dropping the returned handle cancels it. `shared_only`: only the leading system block is
/// prefilled, for a conversation not created yet ([`next_conversation`]).
#[allow(clippy::too_many_arguments)]
pub fn send(
    host: &Host,
    cache_key: &str,
    messages: Vec<ChatMessage>,
    tools: &[ToolSpec],
    what: &'static str,
    shared_only: bool,
    when: When,
    ended: impl FnOnce(bool) + Send + 'static,
) -> Prewarm {
    let token = CancellationToken::new();
    let cancel = token.clone();
    let host = host.clone();
    let mut params = json!({"cache_key": cache_key, "messages": messages, "tools": tools});
    if shared_only {
        params["shared_only"] = json!(true);
    }
    if when == When::Idle {
        params["idle_only"] = json!(true);
    }
    tokio::spawn(async move {
        let Ok(mut stream) = host.ctx().call_stream(model_methods::PREWARM, params) else {
            ended(false);
            return;
        };
        let cancelled = tokio::select! {
            _ = async { while stream.next().await.is_some() {} } => false,
            _ = cancel.cancelled() => true,
        };
        if cancelled {
            stream.cancel();
        }
        match stream.finish().await {
            Ok(done) => {
                tracing::info!(what, prompt_tokens = ?done.get("prompt_tokens"), cached_tokens = ?done.get("cached_tokens"), prefill_ms = ?done.get("prefill_ms"), "prewarmed");
                ended(true);
            }
            Err(error) => {
                tracing::debug!(what, %error, "model.prewarm ended");
                ended(cancelled);
            }
        }
    });
    Prewarm(token)
}
