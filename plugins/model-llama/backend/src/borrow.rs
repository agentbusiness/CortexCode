//! A member's Qwen, through the mesh: another app's on this machine (the same model is not
//! loaded twice on one machine; other models stay, each app lends its own), or, on a machine
//! without a GPU that runs Qwen, any member's. Cards show the model each member holds and lends. `look` finds
//! one; `call` forwards a `model.*` call to it (`mesh.call`), stream items as the model produces
//! them.

use cc_protocol::contracts::mesh::PeersResult;
use cc_protocol::contracts::model::tags;
use cc_protocol::{Ctx, RpcError};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The member whose Qwen serves here.
#[derive(Debug, Clone, PartialEq)]
pub struct Lender {
    pub node: String,
    pub name: String,
    /// The app (`cortexcode`), as the window shows it.
    pub software: String,
    pub model_id: String,
    /// The model on the member's computer (its card's `plugin`), named on every call to it.
    pub plugin: String,
}

/// A reachable member's model tagged `coding` (CortexCode needs Qwen): another app's on this
/// machine while none is loaded here; `anywhere` (no GPU that runs Qwen here): any member's, this
/// machine's first.
pub async fn look(ctx: &Ctx, own_loaded: bool, anywhere: bool) -> Option<Lender> {
    let peers = tokio::time::timeout(Duration::from_secs(3), ctx.call("mesh.peers", json!({}))).await.ok()?.ok()?;
    let peers: PeersResult = serde_json::from_value(peers).ok()?;
    let mut found: Vec<(usize, Lender)> = peers
        .peers
        .into_iter()
        .filter(|peer| (anywhere || peer.member.local) && peer.reachable)
        .filter_map(|peer| {
            let card = peer.card?;
            let software = card.software;
            if own_loaded {
                return None;
            }
            let model = card.models.into_iter().find(|model| model.tags.iter().any(|tag| tag == tags::CODING))?;
            let rank = if peer.member.local { 0 } else { 1 };
            Some((rank, Lender { node: peer.member.node, name: peer.member.name, software, model_id: model.model_id, plugin: model.plugin }))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().next().map(|(_, lender)| lender)
}

/// Give a forwarded call its own conversation space: `cache_key` gets the `cortexterm:` prefix.
pub fn prefix_cache_key(params: &mut Value) {
    if let Some(key) = params.get_mut("cache_key")
        && let Some(text) = key.as_str()
        && !text.starts_with("cortexcode:")
    {
        *key = Value::String(format!("cortexcode:{text}"));
    }
}

/// One call on the lender's model. Stream items go to `on_item` as they arrive; `cancel` cancels
/// the call there.
pub async fn call(ctx: &Ctx, lender: &Lender, method: &str, mut params: Value, on_item: &mut (dyn FnMut(Value) + Send), cancel: Option<CancellationToken>) -> Result<Value, RpcError> {
    if let Some(params) = params.as_object_mut() {
        params.insert(cc_protocol::contracts::model::PLUGIN_PARAM.to_owned(), Value::String(lender.plugin.clone()));
    }
    let mut stream = ctx.call_stream("mesh.call", json!({"node": lender.node, "method": method, "params": params}))?;
    let cancel = cancel.unwrap_or_default();
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(item) => on_item(item),
                None => break,
            },
            _ = cancel.cancelled() => {
                stream.cancel();
                break;
            }
        }
    }
    stream.finish().await
}
