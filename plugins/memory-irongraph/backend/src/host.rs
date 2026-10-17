//! What the memory plugin calls other plugins through: the frame in production ([`CtxHost`]), an
//! in-test peer in the tests (one that serves `frame.plugins.list` and routes `lang-*` methods to
//! real language plugins).

use cc_protocol::{BoxFuture, Ctx, RpcError};
use serde_json::Value;

/// Calls any method on any category.
pub trait Host: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
}

/// The frame, through the plugin's context.
pub struct CtxHost(pub Ctx);

impl Host for CtxHost {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ctx = self.0.clone();
        let method = method.to_owned();
        Box::pin(async move { ctx.call(&method, params).await })
    }
}
