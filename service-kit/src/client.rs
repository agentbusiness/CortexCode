//! Calling a shared service on this machine (`service.rs`): find the app that offers it, send the
//! call through this app's mesh (`mesh.service.call`, which stamps who asked), and say what the
//! caller should do with the answer. Nothing here waits without a limit: a provider that does not
//! answer is a provider that is not there.

use cc_protocol::contracts::mesh::ServicesResult;
use cc_protocol::contracts::service::{Job, Share};
use cc_protocol::{BoxFuture, Ctx, RpcError, codes};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long the list of services may take.
const ASK: Duration = Duration::from_secs(3);

/// A plugin's connection to its frame.
pub trait Link: Send + Sync {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
    /// A call whose stream items go to `items`; cancelling `cancel` cancels the call.
    fn call_stream(&self, method: &str, params: Value, items: mpsc::UnboundedSender<Value>, cancel: CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>>;
}

impl Link for Ctx {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ctx = self.clone();
        let method = method.to_owned();
        Box::pin(async move { ctx.call(&method, params).await })
    }

    fn call_stream(&self, method: &str, params: Value, items: mpsc::UnboundedSender<Value>, cancel: CancellationToken) -> BoxFuture<'static, Result<Value, RpcError>> {
        let started = self.call_stream(method, params);
        Box::pin(async move {
            let mut call = started?;
            loop {
                tokio::select! {
                    item = call.next() => match item {
                        Some(item) => { let _ = items.send(item); }
                        None => return call.finish().await,
                    },
                    _ = cancel.cancelled() => {
                        call.cancel();
                        return call.finish().await;
                    }
                }
            }
        })
    }
}

/// The app on this machine that offers a service.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Its mesh node; empty when it is this app itself.
    pub node: String,
    /// The computer's name, for people.
    pub name: String,
    pub share: Share,
}

/// The app on this machine that offers `service` at `version` now: this app first, then another
/// app here. None when there is none, or the mesh does not answer in time.
pub async fn provider(link: &dyn Link, service: &str, version: u32) -> Option<Provider> {
    let answer = tokio::time::timeout(ASK, link.call("mesh.services", json!({}))).await.ok()?.ok()?;
    let services = serde_json::from_value::<ServicesResult>(answer).ok()?.services;
    let fits = |here: bool| services.iter().find(|o| o.share.service == service && o.share.version == version && o.local && o.reachable && o.here == here);
    let offer = fits(true).or_else(|| fits(false))?;
    Some(Provider { node: if offer.here { String::new() } else { offer.node.clone() }, name: offer.name.clone(), share: offer.share.clone() })
}

/// Who asks, as the caller says: the mesh adds which app and computer.
#[derive(Debug, Clone, Default)]
pub struct Asker {
    pub session_id: String,
    /// person | agent
    pub by: String,
}

/// What came of a `run` call.
#[derive(Debug)]
pub enum Ran {
    /// The provider answered with its job: done, failed, stopped, refused (ask the person, then
    /// call again with `approved`), or running (a background run).
    Job(Job),
    /// The provider is not there or is restarting: do the work yourself.
    Elsewhere(RpcError),
    /// The call failed in a way doing it elsewhere would not fix: report it.
    Failed(RpcError),
}

/// Whether an error means "do it yourself": no provider is there, or it is restarting, so it did
/// not take the call. A connection that closed while a run was going is not one: the command may
/// have run, and it never runs twice (the provider reports it `lost` when it is back).
pub fn elsewhere(error: &RpcError) -> bool {
    matches!(error.code.as_str(), codes::UNAVAILABLE | codes::BUSY)
}

/// The params of a call with who asked: `caller` is replaced by the serving mesh except for the
/// session and who, which it keeps from here.
fn with_asker(mut params: Value, asker: &Asker) -> Value {
    if let Some(map) = params.as_object_mut() {
        map.insert("caller".into(), json!({"session_id": asker.session_id, "by": asker.by}));
    }
    params
}

fn envelope(provider: &Provider, method: &str, params: Value) -> Value {
    json!({"method": method, "params": params, "node": provider.node})
}

/// A `run` method of the provider. Output items (`{"kind": "output", "text"}`) go to `items` while
/// it runs; cancelling `cancel` stops it.
pub async fn run(link: &dyn Link, provider: &Provider, method: &str, params: Value, asker: &Asker, items: mpsc::UnboundedSender<Value>, cancel: CancellationToken) -> Ran {
    let call = link.call_stream("mesh.service.call", envelope(provider, method, with_asker(params, asker)), items, cancel);
    match call.await {
        Ok(answer) => match serde_json::from_value::<Job>(answer) {
            Ok(job) => Ran::Job(job),
            Err(error) => Ran::Failed(RpcError::new(codes::INTERNAL, format!("the answer of `{method}` is not a job: {error}"))),
        },
        Err(error) if elsewhere(&error) => Ran::Elsewhere(error),
        Err(error) => Ran::Failed(error),
    }
}

/// A `read`, `list` or `get` method of the provider, within `limit`.
pub async fn read(link: &dyn Link, provider: &Provider, method: &str, params: Value, asker: &Asker, limit: Duration) -> Result<Value, RpcError> {
    let call = link.call("mesh.service.call", envelope(provider, method, with_asker(params, asker)));
    match tokio::time::timeout(limit, call).await {
        Ok(answer) => answer,
        Err(_) => Err(RpcError::new(codes::TIMEOUT, format!("{} did not answer `{method}` in {} s", provider_name(provider), limit.as_secs()))),
    }
}

/// An `edit`-verb method (create, edit, delete) of the provider, within `limit`.
pub async fn change(link: &dyn Link, provider: &Provider, method: &str, params: Value, asker: &Asker, limit: Duration) -> Result<Value, RpcError> {
    read(link, provider, method, params, asker, limit).await
}

fn provider_name(provider: &Provider) -> String {
    if provider.node.is_empty() { "this app".into() } else { format!("the app on {}", provider.name) }
}
