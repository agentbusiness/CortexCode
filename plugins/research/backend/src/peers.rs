//! Calls to other plugins through the frame (the model, the browser, the frame's project registry)
//! and event publishing, behind a trait so tests can stand in for them.

use cc_protocol::{BoxFuture, Ctx, RpcError, codes};
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

pub trait Peers: Send + Sync {
    /// Call a method; `cancel` cancels the request at its plugin (a model call stops generating).
    fn call(&self, method: &str, params: Value, cancel: CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>>;
    fn publish(&self, topic: &str, data: Value);
}

/// Peers through the frame connection (set once the frame has answered hello).
#[derive(Clone, Default)]
pub struct FramePeers {
    pub ctx: Arc<OnceLock<Ctx>>,
}

impl Peers for FramePeers {
    fn call(&self, method: &str, params: Value, cancel: CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ctx = self.ctx.get().cloned();
        let method = method.to_owned();
        Box::pin(async move {
            let Some(ctx) = ctx else {
                return Err(RpcError::new(codes::UNAVAILABLE, format!("no frame connection for `{method}`")));
            };
            let conn = ctx.frame()?.clone();
            let stream = ctx.call_stream(&method, params)?;
            let id = stream.id();
            // Streamed items (model tokens) are not needed: wait for the final response, and on
            // cancellation ask the remote plugin to stop.
            tokio::select! {
                outcome = stream.finish() => outcome,
                _ = cancel.cancelled() => {
                    conn.cancel(id);
                    Err(RpcError::cancelled())
                }
            }
        })
    }

    fn publish(&self, topic: &str, data: Value) {
        if let Some(ctx) = self.ctx.get() {
            ctx.publish(topic, data);
        }
    }
}

/// Peers for a probe or a test without a frame: every call is unavailable.
pub struct NoPeers;

impl Peers for NoPeers {
    fn call(&self, method: &str, _params: Value, _cancel: CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>> {
        let method = method.to_owned();
        Box::pin(async move { Err(RpcError::new(codes::UNAVAILABLE, format!("no frame connection for `{method}`"))) })
    }

    fn publish(&self, _topic: &str, _data: Value) {}
}
