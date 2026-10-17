//! Calls to the frame and to peer plugins, with a retry policy that matches what each call does.
//!
//! The frame routes `<category>.<verb>` to the active plugin of that category. During a peer's
//! hot-swap or restart a call can be refused (`unavailable`, `draining`) before anything ran; those
//! are always retried. A call that was cut off while running (`connection_closed`, `timeout`) is
//! retried only when it is idempotent: an effect whose outcome is unknown is reported, never
//! replayed blindly.

use cc_protocol::contracts::frame::{
    self as fc, JournalEntry, JournalReadResult, Project, Session, SessionCreateParams, SessionUpdateParams,
};
use cc_protocol::{Ctx, RpcError, codes, parse_params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long refused calls are retried before the refusal is returned (a peer restart or swap).
const RETRY_WINDOW: Duration = Duration::from_secs(180);

/// Whether a call may be repeated after it was cut off mid-flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Reads and idempotent writes: retried after any transient failure.
    Idempotent,
    /// Effects: retried only when the peer refused the call before running it.
    Effect,
}

#[derive(Clone)]
pub struct Host {
    ctx: Ctx,
}

/// Mechanical classification of error codes (not meaning): refused before running.
pub fn refused(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::DRAINING | codes::BUSY)
}

/// Mechanical classification: interrupted while running.
pub fn interrupted(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::CONNECTION_CLOSED | codes::TIMEOUT)
}

impl Host {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// Call with retries according to `effect`, stopping early when `cancel` fires.
    pub async fn call_with(
        &self,
        method: &str,
        params: Value,
        effect: Effect,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, RpcError> {
        let started = std::time::Instant::now();
        let mut delay = Duration::from_millis(100);
        loop {
            let outcome = match cancel {
                Some(token) => {
                    tokio::select! {
                        outcome = self.ctx.call(method, params.clone()) => outcome,
                        _ = token.cancelled() => return Err(RpcError::cancelled()),
                    }
                }
                None => self.ctx.call(method, params.clone()).await,
            };
            match outcome {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retry = refused(&error) || (effect == Effect::Idempotent && interrupted(&error));
                    if !retry || started.elapsed() > RETRY_WINDOW {
                        return Err(error);
                    }
                    tracing::debug!(method, code = %error.code, "retrying call");
                    match cancel {
                        Some(token) => {
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = token.cancelled() => return Err(RpcError::cancelled()),
                            }
                        }
                        None => tokio::time::sleep(delay).await,
                    }
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        }
    }

    /// Idempotent call.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with(method, params, Effect::Idempotent, None).await
    }

    /// Idempotent call decoded into `T`.
    pub async fn get<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, RpcError> {
        parse_params(self.call(method, params).await?)
    }

    /// Effect call (not replayed after an interruption).
    pub async fn effect(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with(method, params, Effect::Effect, None).await
    }

    // ---- frame: journal -------------------------------------------------------------------

    pub async fn journal_append(&self, session_id: &str, kind: &str, data: Value) -> Result<u64, RpcError> {
        // Appends are not idempotent (a replay would duplicate the entry), so only refusals retry.
        let result = self
            .effect(fc::methods::JOURNAL_APPEND, json!({"session_id": session_id, "kind": kind, "data": data}))
            .await?;
        result
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::new(codes::CONTRACT_VIOLATION, "frame.journal.append returned no seq"))
    }

    pub async fn journal_set(&self, session_id: &str, seq: u64, field: &str, value: Value) -> Result<(), RpcError> {
        self.call(fc::methods::JOURNAL_PATCH, json!({"session_id": session_id, "seq": seq, "field": field, "value": value}))
            .await
            .map(|_| ())
    }

    pub async fn journal_append_text(&self, session_id: &str, seq: u64, field: &str, text: &str) -> Result<(), RpcError> {
        // A text append is not idempotent either.
        self.effect(fc::methods::JOURNAL_PATCH, json!({"session_id": session_id, "seq": seq, "field": field, "text": text}))
            .await
            .map(|_| ())
    }

    pub async fn journal_read(&self, session_id: &str, from: u64) -> Result<Vec<JournalEntry>, RpcError> {
        let result: JournalReadResult =
            self.get(fc::methods::JOURNAL_READ, json!({"session_id": session_id, "from": from})).await?;
        Ok(result.entries)
    }

    // ---- frame: sessions and projects -----------------------------------------------------

    pub async fn session_get(&self, id: &str) -> Result<Session, RpcError> {
        let value = self.call(fc::methods::SESSION_GET, json!({"id": id})).await?;
        unwrap_object::<Session>(value, "session")
    }

    pub async fn session_create(&self, params: &SessionCreateParams) -> Result<Session, RpcError> {
        let value = self.effect(fc::methods::SESSION_CREATE, serde_json::to_value(params)?).await?;
        unwrap_object::<Session>(value, "session")
    }

    pub async fn session_update(&self, params: &SessionUpdateParams) -> Result<(), RpcError> {
        self.call(fc::methods::SESSION_UPDATE, serde_json::to_value(params)?).await.map(|_| ())
    }

    pub async fn session_status(&self, id: &str, status: fc::SessionStatus) -> Result<(), RpcError> {
        self.session_update(&SessionUpdateParams { id: id.to_owned(), title: None, status: Some(status), task_id: None })
            .await
    }

    pub async fn session_list(&self, project_id: Option<&str>) -> Result<Vec<Session>, RpcError> {
        let params = match project_id {
            Some(id) => json!({"project_id": id}),
            None => json!({}),
        };
        let value = self.call(fc::methods::SESSION_LIST, params).await?;
        unwrap_list::<Session>(value, "sessions")
    }

    pub async fn project_get(&self, id: &str) -> Result<Project, RpcError> {
        let value = self.call(fc::methods::PROJECT_GET, json!({"id": id})).await?;
        unwrap_object::<Project>(value, "project")
    }

    pub async fn project_list(&self) -> Result<Vec<Project>, RpcError> {
        let value = self.call(fc::methods::PROJECT_LIST, json!({})).await?;
        unwrap_list::<Project>(value, "projects")
    }
}

/// Decode an object that the frame may return bare or wrapped as `{<key>: object}`.
fn unwrap_object<T: DeserializeOwned>(value: Value, key: &str) -> Result<T, RpcError> {
    match value.get(key) {
        Some(inner) if inner.is_object() => parse_params(inner.clone()),
        _ => parse_params(value),
    }
}

/// Decode a list that the frame may return bare or wrapped as `{<key>: [...]}`.
fn unwrap_list<T: DeserializeOwned>(value: Value, key: &str) -> Result<Vec<T>, RpcError> {
    match value {
        Value::Array(_) => parse_params(value),
        Value::Object(mut map) => match map.remove(key) {
            Some(inner) => parse_params(inner),
            None => Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("expected `{key}` in the frame's answer"))),
        },
        _ => Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("expected a list of {key}"))),
    }
}
