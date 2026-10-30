//! Shared services (`cc_protocol::contracts::service`): what this computer's plugins share, which
//! calls of a member are taken, a record of what was served, and where a call of this computer
//! goes. Sharing is always on; there is nothing to set.
//!
//! - Each active plugin is asked `<category>.shares` once per build; what it declares is shown on
//!   the card and is all that members may call (besides the model, which has its own sharing).
//! - A member's call is taken when its method is declared and its verb's access is allowed for
//!   where the member is (`read` from anywhere; `edit` and `run` only from an app on this
//!   machine). The mesh stamps who asked into the params as `caller`.
//! - Every call served for another app is a record of stream `mesh.served` (kept 30 days).

use crate::service::{Mesh, lock};
use cc_protocol::contracts::mesh::{OfferedService, ServiceCallParams, ServiceCard, ServicesResult};
use cc_protocol::contracts::service::{self as svc, Access, Caller, Share, ShareList};
use cc_protocol::{Ctx, RpcError, codes};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Services {
    /// `plugin_id@instance` -> what that plugin build shares.
    declared: Mutex<HashMap<String, Vec<Share>>>,
    /// A member's full declarations, by node, with the card time they were read at.
    theirs: Mutex<HashMap<String, (u64, Vec<Share>)>>,
}

fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

impl Mesh {
    /// What this computer's active plugins share (asked once per plugin build).
    pub async fn local_shares(&self) -> Vec<Share> {
        let Some(ctx) = self.ctx.get().cloned() else { return Vec::new() };
        let listed: Value = ctx.call("frame.plugins.list", json!({})).await.unwrap_or(Value::Null);
        let plugins: Vec<(String, String)> = listed
            .get("plugins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p["state"] == "active" && p["transport"] != "websocket")
            .filter_map(|p| Some((format!("{}@{}", p["plugin_id"].as_str()?, p["instance"].as_str().unwrap_or("")), p["category"].as_str()?.to_owned())))
            .collect();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (key, category) in plugins {
            seen.insert(key.clone());
            let known = lock(&self.services.declared).get(&key).cloned();
            let shares = match known {
                Some(shares) => shares,
                None => {
                    let shares = ask_shares(&ctx, &category).await;
                    lock(&self.services.declared).insert(key, shares.clone());
                    shares
                }
            };
            out.extend(shares);
        }
        lock(&self.services.declared).retain(|key, _| seen.contains(key));
        out.sort_by(|a, b| a.service.cmp(&b.service));
        out
    }

    pub fn service_cards(shares: &[Share]) -> Vec<ServiceCard> {
        shares
            .iter()
            .map(|share| ServiceCard { service: share.service.clone(), version: share.version, title: share.title.clone(), methods: share.methods.iter().map(|m| m.method.clone()).collect() })
            .collect()
    }

    /// Whether a member's call of `method` is taken; its params with `caller` stamped in.
    pub async fn accept_call(&self, node: &str, method: &str, mut params: Value) -> Result<Value, RpcError> {
        let refuse = |why: String| RpcError::new(codes::CONTRACT_VIOLATION, why);
        let access = svc::access_of(method).ok_or_else(|| refuse(format!("`{method}` is not shared with members")))?;
        let service = svc::service_of(method).unwrap_or("");
        let shares = self.local_shares().await;
        let declared = shares.iter().any(|share| share.service == service && share.method(method).is_some());
        if !declared {
            return Err(refuse(format!("`{method}` is not shared with members")));
        }
        let local = self.is_local(node);
        if access != Access::Read && !local {
            return Err(refuse(format!(
                "`{method}` {}s on this machine only for apps on this machine: work runs where its project is",
                access.as_str()
            )));
        }
        let card = lock(&self.cards).get(node).cloned();
        let theirs: Caller = params.get("caller").cloned().and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default();
        let caller = Caller {
            app: card.as_ref().map(|card| card.software.clone()).unwrap_or_default(),
            node: node.to_owned(),
            name: self.member(node).map(|member| member.name).unwrap_or_default(),
            local,
            session_id: theirs.session_id,
            by: theirs.by,
        };
        if !params.is_object() {
            params = json!({});
        }
        params["caller"] = serde_json::to_value(caller).unwrap_or(Value::Null);
        Ok(params)
    }

    /// One served call, for the record.
    pub fn served(&self, node: &str, method: &str, ok: bool, ms: u64) {
        self.write(crate::store::Write::Served(json!({"at": now_ms(), "node": node, "method": method, "ok": ok, "ms": ms})));
    }

    /// A member's full declarations (asked of it when its card changed).
    async fn member_shares(&self, node: &str) -> Vec<Share> {
        let updated = lock(&self.cards).get(node).map(|card| card.updated_ms).unwrap_or(0);
        if let Some((at, shares)) = lock(&self.services.theirs).get(node).cloned()
            && at == updated
        {
            return shares;
        }
        let asked = self.call_member(node, "mesh.shares", json!({}), None, CancellationToken::new()).await;
        let shares = asked.ok().and_then(|value| serde_json::from_value::<ShareList>(value).ok()).map(|list| list.shares).unwrap_or_default();
        lock(&self.services.theirs).insert(node.to_owned(), (updated, shares.clone()));
        shares
    }

    /// Every service this computer and its members offer, and what a call from here may do there.
    pub async fn services(&self) -> ServicesResult {
        let me = self.me.hex();
        let mut out: Vec<OfferedService> = self
            .local_shares()
            .await
            .into_iter()
            .map(|share| OfferedService { node: me.clone(), name: self.name(), here: true, local: true, reachable: true, share, access: vec![Access::Read, Access::Edit, Access::Run] })
            .collect();
        let peers: Vec<(String, String, bool, bool)> = self
            .members()
            .into_values()
            .filter(|member| member.node != me)
            .map(|member| {
                let offers = lock(&self.cards).get(&member.node).is_some_and(|card| !card.services.is_empty());
                (member.node.clone(), member.name.clone(), member.local, offers)
            })
            .collect();
        for (node, name, local, offers) in peers {
            if !offers {
                continue;
            }
            let reachable = self.connection(&node).is_some();
            let shares = if reachable { self.member_shares(&node).await } else { Vec::new() };
            for share in shares {
                let access = if local { vec![Access::Read, Access::Edit, Access::Run] } else { vec![Access::Read] };
                out.push(OfferedService { node: node.clone(), name: name.clone(), here: false, local, reachable, share, access });
            }
        }
        ServicesResult { services: out }
    }

    /// One call of a shared service from this computer: to this app's own plugin, or to the
    /// provider the rules allow (`edit` and `run` on this machine only).
    pub async fn service_call(&self, mut params: ServiceCallParams, items: mpsc::UnboundedSender<Value>, cancel: CancellationToken) -> Result<Value, RpcError> {
        let access = svc::access_of(&params.method).ok_or_else(|| RpcError::invalid_params(format!("`{}` does not end with a shareable verb", params.method)))?;
        let service = svc::service_of(&params.method).unwrap_or("").to_owned();
        let me = self.me.hex();
        if !params.node.is_empty() {
            params.node = self.resolve_node(&params.node).ok_or_else(|| RpcError::not_found("no such member"))?;
        }
        let offered = self.services().await.services;
        let fits = |offer: &&OfferedService| offer.share.service == service && offer.share.method(&params.method).is_some() && offer.reachable;
        let chosen = if params.node.is_empty() {
            // This app first, then another app on this machine; a read may go anywhere.
            offered.iter().filter(fits).find(|o| o.here).or_else(|| offered.iter().filter(fits).find(|o| o.local)).or_else(|| if access == Access::Read { offered.iter().find(fits) } else { None })
        } else {
            offered.iter().filter(fits).find(|o| o.node == params.node)
        };
        let Some(offer) = chosen else {
            let where_ = if access == Access::Read { "on the network" } else { "on this machine" };
            return Err(RpcError::new(codes::UNAVAILABLE, format!("no app {where_} offers `{}` now", params.method)));
        };
        if access != Access::Read && !offer.local {
            return Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("`{}` runs only on the machine where your work runs; {} is another computer", params.method, offer.name)));
        }
        if offer.node == me {
            let ctx = self.ctx()?;
            let mut call = ctx.call_stream(&params.method, params.params)?;
            loop {
                tokio::select! {
                    item = call.next() => match item {
                        Some(item) => { let _ = items.send(item); }
                        None => return call.finish().await,
                    },
                    _ = cancel.cancelled() => {
                        call.cancel();
                        return Err(RpcError::new(codes::CANCELLED, "cancelled"));
                    }
                }
            }
        }
        let node = offer.node.clone();
        self.call_member(&node, &params.method, params.params, Some(items), cancel).await
    }
}

/// What one plugin declares; nothing when it shares nothing (`unknown_method`) or does not answer.
async fn ask_shares(ctx: &Ctx, category: &str) -> Vec<Share> {
    let method = format!("{category}.{}", svc::SHARES);
    let answer = tokio::time::timeout(Duration::from_secs(3), ctx.call(&method, json!({}))).await;
    let Ok(Ok(value)) = answer else { return Vec::new() };
    let Ok(list) = serde_json::from_value::<ShareList>(value) else {
        tracing::warn!(%method, "the shares a plugin declared could not be read");
        return Vec::new();
    };
    list.shares
        .into_iter()
        .filter(|share| share.service.split('.').next() == Some(category))
        .map(|mut share| {
            for problem in share.problems() {
                tracing::warn!(service = %share.service, %problem, "a shared method is not lent");
            }
            let service = share.service.clone();
            share.methods.retain(|m| svc::service_of(&m.method) == Some(service.as_str()) && svc::access_of(&m.method).is_some());
            share
        })
        .filter(|share| !share.methods.is_empty())
        .collect()
}
