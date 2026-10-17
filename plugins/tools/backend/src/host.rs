//! The plugin's view of the rest of the system: calls routed by the frame and event publishing.

use cc_protocol::{BoxFuture, Ctx, RpcError};
use serde_json::Value;

pub trait Host: Send + Sync {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
    fn publish(&self, topic: &str, data: Value);
}

/// Host backed by the frame connection.
#[derive(Clone)]
pub struct CtxHost(pub Ctx);

impl Host for CtxHost {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ctx = self.0.clone();
        let method = method.to_owned();
        Box::pin(async move { ctx.call(&method, params).await })
    }

    fn publish(&self, topic: &str, data: Value) {
        self.0.publish(topic, data);
    }
}

impl crate::jobs::Publisher for CtxHost {
    fn publish(&self, topic: &str, data: Value) {
        self.0.publish(topic, data);
    }
}

/// A host with no peers (used where no frame is attached, e.g. the probe).
pub struct NoHost;

impl Host for NoHost {
    fn call(&self, method: &str, _params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let method = method.to_owned();
        Box::pin(async move { Err(RpcError::new(cc_protocol::codes::UNAVAILABLE, format!("no frame connection for `{method}`"))) })
    }

    fn publish(&self, _topic: &str, _data: Value) {}
}
