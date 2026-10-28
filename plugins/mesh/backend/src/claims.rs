//! Resources of the network, claimed for one use (`docs/MESH-IMPLEMENTATION-PLAN.md` §13). Today
//! one kind, `model`: every model call of a session claims the best free model in the network
//! (`mesh.claim`) and gives it back when the call ends (`mesh.release`), so a run holds no model
//! between its calls (a build, a test) and moves to a better model the moment one frees.
//!
//! Each computer decides who uses its own model next ([`Lending`]): one run's call at a time
//! (a person's call on this computer's model only waits for another person's: the engine puts a
//! person's request before agent work), and a freed model goes to the oldest run waiting for it.
//! A run's age is its first claim, kept until `mesh.place.end`, so a run that was building is still
//! old when it calls again. A claim that finds nothing free waits at every model it could use
//! (here, and `mesh.model.claim` on each member); the first grant wins, the others are withdrawn.

use crate::service::{Mesh, lock};
use cc_protocol::contracts::mesh::{ClaimParams, Placement};
use cc_protocol::contracts::model::{LiveMetrics, ModelInfo, Priority, SlotsResult};
use cc_protocol::contracts::now_ms;
use cc_protocol::{RpcError, codes};
use futures_util::future::select_ok;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// How often waiting claims look again whether this computer's model freed from work that does
/// not claim (a project description, a prewarm).
const LOOK_AGAIN: Duration = Duration::from_millis(500);

/// Who uses this computer's model now, and who waits for it.
#[derive(Default)]
pub struct Lending {
    holders: Vec<Holder>,
    waiting: Vec<Waiter>,
}

struct Holder {
    /// The requesting computer (hex); empty: this one.
    requester: String,
    session: String,
    foreground: bool,
}

struct Waiter {
    requester: String,
    session: String,
    since: u64,
    foreground: bool,
    grant: oneshot::Sender<()>,
}

#[derive(Default)]
pub struct Claims {
    pub lending: Mutex<Lending>,
    freed: Notify,
    /// session -> when its run first claimed (its age), until `mesh.place.end`.
    ages: Mutex<HashMap<String, u64>>,
    /// session -> the computer whose model its current call holds (empty: this one).
    held: Mutex<HashMap<String, String>>,
    /// session -> the computer its last call ran on: its prompt is cached there.
    last: Mutex<HashMap<String, String>>,
    /// model id -> its weights' size (a model's files do not change while it is loaded).
    weights: Mutex<HashMap<String, u64>>,
    granting: std::sync::OnceLock<()>,
}

struct ModelOption {
    /// Empty: this computer.
    node: String,
    name: String,
    model_id: String,
    /// 0 this computer, 1 local network, 2 remote.
    tier: u8,
    weights: u64,
    tok_s: f64,
}

impl Mesh {
    // --- the lending side: this computer's model ----------------------------------------------

    /// Whether this computer's model can take one more use now: nobody holds it and it runs
    /// nothing else; or, for a person's call made here, no other person's call holds or runs it.
    async fn model_free_for(&self, requester: &str, foreground: bool) -> bool {
        let Some(ctx) = self.ctx.get() else { return false };
        let Some(slots) = ctx.call("model.slots", json!({})).await.ok().and_then(|value| serde_json::from_value::<SlotsResult>(value).ok()) else { return false };
        let lending = lock(&self.claims.lending);
        if foreground && requester.is_empty() {
            let person_holds = lending.holders.iter().any(|holder| holder.foreground && holder.requester.is_empty());
            let person_runs = slots.slots.iter().any(|slot| slot.busy && slot.priority == Some(Priority::Foreground)) || slots.queued_by_priority.foreground > 0;
            return !person_holds && !person_runs;
        }
        lending.holders.is_empty() && slots.busy == 0 && slots.queued == 0
    }

    /// Lend this computer's model for one use: at once when it is free and no older run waits for
    /// it; else, with `wait`, when it is this run's turn (false: not now, or cancelled).
    pub async fn lend(&self, requester: &str, session: &str, since: u64, foreground: bool, wait: bool, cancel: CancellationToken) -> bool {
        // A person here waits only for an older person here; anyone else for any older run.
        let person = foreground && requester.is_empty();
        let older_waits = lock(&self.claims.lending).waiting.iter().any(|waiter| waiter.since < since && (!person || (waiter.foreground && waiter.requester.is_empty())));
        if !older_waits && self.model_free_for(requester, foreground).await {
            let mut lending = lock(&self.claims.lending);
            lending.holders.retain(|holder| !(holder.requester == requester && holder.session == session));
            lending.holders.push(Holder { requester: requester.to_owned(), session: session.to_owned(), foreground });
            return true;
        }
        if !wait {
            return false;
        }
        let (grant, granted) = oneshot::channel();
        lock(&self.claims.lending).waiting.push(Waiter { requester: requester.to_owned(), session: session.to_owned(), since, foreground, grant });
        self.claims.freed.notify_one();
        tokio::select! {
            outcome = granted => outcome.is_ok(),
            _ = cancel.cancelled() => {
                let mut lending = lock(&self.claims.lending);
                lending.waiting.retain(|waiter| !(waiter.requester == requester && waiter.session == session));
                // Granted in the same moment: the use is given back at once.
                lending.holders.retain(|holder| !(holder.requester == requester && holder.session == session));
                drop(lending);
                self.claims.freed.notify_one();
                false
            }
        }
    }

    /// A use of this computer's model is over (or a waiting one withdrawn).
    pub fn give_back(&self, requester: &str, session: &str) {
        let mut lending = lock(&self.claims.lending);
        lending.holders.retain(|holder| !(holder.requester == requester && holder.session == session));
        lending.waiting.retain(|waiter| !(waiter.requester == requester && waiter.session == session));
        drop(lending);
        self.claims.freed.notify_one();
    }

    /// A member went away: what it held or waited for here is over.
    pub fn requester_gone(&self, node: &str) {
        let mut lending = lock(&self.claims.lending);
        lending.holders.retain(|holder| holder.requester != node);
        lending.waiting.retain(|waiter| waiter.requester != node);
        drop(lending);
        self.claims.freed.notify_one();
    }

    /// Start handing this computer's model to waiting runs (once).
    pub fn start_granting(self: &Arc<Self>) {
        let mesh = self.clone();
        if self.claims.granting.set(()).is_ok() {
            tokio::spawn(mesh.grant_loop());
        }
    }

    /// Hand this computer's model to the oldest waiting run whenever it frees: on a release, on a
    /// new waiter, and every half second while someone waits (work that does not claim ends too).
    async fn grant_loop(self: Arc<Self>) {
        loop {
            let waiting = !lock(&self.claims.lending).waiting.is_empty();
            if waiting {
                tokio::select! {
                    _ = self.claims.freed.notified() => {}
                    _ = tokio::time::sleep(LOOK_AGAIN) => {}
                }
            } else {
                self.claims.freed.notified().await;
            }
            // The oldest waiter the model can take now: every run but a person here waits on the
            // same condition, so this keeps their order; a person here may pass agent work.
            loop {
                let waiting: Vec<(String, String, bool)> = {
                    let mut lending = lock(&self.claims.lending);
                    lending.waiting.retain(|waiter| !waiter.grant.is_closed());
                    lending.waiting.sort_by_key(|waiter| waiter.since);
                    lending.waiting.iter().map(|waiter| (waiter.requester.clone(), waiter.session.clone(), waiter.foreground)).collect()
                };
                let mut next = None;
                for (requester, session, foreground) in waiting {
                    if self.model_free_for(&requester, foreground).await {
                        next = Some((requester, session));
                        break;
                    }
                }
                let Some((requester, session)) = next else { break };
                let mut lending = lock(&self.claims.lending);
                let Some(position) = lending.waiting.iter().position(|waiter| waiter.requester == requester && waiter.session == session) else { continue };
                let waiter = lending.waiting.remove(position);
                lending.holders.push(Holder { requester: waiter.requester.clone(), session: waiter.session.clone(), foreground: waiter.foreground });
                if waiter.grant.send(()).is_err() {
                    lending.holders.retain(|holder| !(holder.requester == waiter.requester && holder.session == waiter.session));
                }
            }
        }
    }

    // --- the claiming side: a session's model call -----------------------------------------------

    /// The weights' size of a model (asked once per model id).
    async fn weights_of(&self, node: &str, model_id: &str) -> u64 {
        if let Some(known) = lock(&self.claims.weights).get(model_id).copied() {
            return known;
        }
        let info = if node.is_empty() {
            match self.ctx.get() {
                Some(ctx) => ctx.call("model.info", json!({})).await.ok(),
                None => None,
            }
        } else {
            self.call_member(node, "model.info", json!({}), None, CancellationToken::new()).await.ok()
        };
        let Some(info) = info.and_then(|value| serde_json::from_value::<ModelInfo>(value).ok()) else { return 0 };
        lock(&self.claims.weights).insert(model_id.to_owned(), info.file_bytes);
        info.file_bytes
    }

    /// Every model the session could use, best first: the strongest, then the one its prompt is
    /// cached on, then this computer, the local network, remote, then the faster.
    async fn model_options(&self, session: &str) -> Vec<ModelOption> {
        let mut options = Vec::new();
        let mut family = None;
        if let Some(ctx) = self.ctx.get()
            && !lock(&self.borrows).contains_key("model")
            && let Some(info) = ctx.call("model.info", json!({})).await.ok().and_then(|value| serde_json::from_value::<ModelInfo>(value).ok())
        {
            let tok_s = ctx.call("model.metrics", json!({})).await.ok().and_then(|value| serde_json::from_value::<LiveMetrics>(value).ok()).map_or(0.0, |metrics| metrics.decode_tps_per_stream);
            lock(&self.claims.weights).insert(info.model_id.clone(), info.file_bytes);
            options.push(ModelOption { node: String::new(), name: self.name(), model_id: info.model_id.clone(), tier: 0, weights: info.file_bytes, tok_s });
            family = Some(info.family);
        }
        let me = self.me.hex();
        let cards = lock(&self.cards).clone();
        let conns = self.connections();
        for member in self.members().into_values().filter(|member| member.node != me) {
            let Some(connection) = conns.get(&member.node) else { continue };
            let Some(model) = cards.get(&member.node).and_then(|card| card.model.clone()) else { continue };
            if family.as_deref().is_some_and(|family| !family.is_empty() && model.family != family) {
                continue;
            }
            let tier = if Self::path_of(connection).0 == "lan" { 1 } else { 2 };
            let weights = self.weights_of(&member.node, &model.model_id).await;
            options.push(ModelOption { node: member.node.clone(), name: member.name.clone(), model_id: model.model_id.clone(), tier, weights, tok_s: model.tok_s });
        }
        let last = lock(&self.claims.last).get(session).cloned();
        options.sort_by(|a, b| {
            b.weights
                .cmp(&a.weights)
                .then((Some(&b.node) == last.as_ref()).cmp(&(Some(&a.node) == last.as_ref())))
                .then(a.tier.cmp(&b.tier))
                .then(b.tok_s.partial_cmp(&a.tok_s).unwrap_or(std::cmp::Ordering::Equal))
        });
        options
    }

    /// Ask one computer's model for one use (`wait`: until it is this run's turn).
    async fn ask(&self, node: &str, session: &str, since: u64, foreground: bool, wait: bool, cancel: CancellationToken) -> bool {
        if node.is_empty() {
            return self.lend("", session, since, foreground, wait, cancel).await;
        }
        let params = json!({"session_id": session, "since_ms": since, "wait": wait});
        let answer = self.call_member(node, "mesh.model.claim", params, None, cancel).await;
        answer.ok().and_then(|value| value.get("granted").and_then(Value::as_bool)).unwrap_or(false)
    }

    /// Give one computer's model back for a session (a use, or a wait withdrawn).
    async fn give_back_at(&self, node: &str, session: &str) {
        if node.is_empty() {
            self.give_back("", session);
        } else {
            let _ = self.call_member(node, "mesh.model.release", json!({"session_id": session}), None, CancellationToken::new()).await;
        }
    }

    /// One use of the best free model for a session's call; the frame routes the call there.
    pub async fn claim(self: &Arc<Self>, params: ClaimParams, cancel: CancellationToken) -> Result<Placement, RpcError> {
        if params.kind != "model" {
            return Err(RpcError::invalid_params(format!("`{}` is not a resource of the network (model)", params.kind)));
        }
        let session = params.session_id.clone();
        if session.is_empty() {
            return Err(RpcError::invalid_params("session_id must not be empty"));
        }
        // A use still held (a call that never gave it back) is over now.
        self.release(&session).await;
        // A run's age starts with its first waiting claim; a claim that does not wait (a prompt
        // warm-up) is no run and leaves none behind.
        let since = if params.wait { *lock(&self.claims.ages).entry(session.clone()).or_insert_with(now_ms) } else { lock(&self.claims.ages).get(&session).copied().unwrap_or_else(now_ms) };
        let options = self.model_options(&session).await;
        if options.is_empty() {
            // No model anywhere: the call goes to this computer's model plugin, which says so.
            return Ok(Placement { node: None, name: self.name(), model_id: String::new(), tier: String::new() });
        }
        let mut chosen = None;
        for (index, option) in options.iter().enumerate() {
            if self.ask(&option.node, &session, since, params.foreground, false, CancellationToken::new()).await {
                chosen = Some(index);
                break;
            }
        }
        if chosen.is_none() && !params.wait {
            return Ok(Placement { node: None, name: String::new(), model_id: String::new(), tier: "busy".to_owned() });
        }
        if chosen.is_none() {
            // Nothing free: wait at every model; the first grant wins.
            let stop = cancel.child_token();
            let waits = options.iter().enumerate().map(|(index, option)| {
                let mesh = self.clone();
                let node = option.node.clone();
                let session = session.clone();
                let stop = stop.clone();
                Box::pin(async move { if mesh.ask(&node, &session, since, params.foreground, true, stop).await { Ok(index) } else { Err(()) } })
            });
            let outcome = select_ok(waits).await;
            stop.cancel();
            let Ok((index, _)) = outcome else {
                for option in &options {
                    self.give_back_at(&option.node, &session).await;
                }
                return Err(RpcError::new(codes::CANCELLED, "cancelled while waiting for a free model"));
            };
            // A wait elsewhere may have been granted in the same moment: given back.
            for (other, option) in options.iter().enumerate() {
                if other != index {
                    self.give_back_at(&option.node, &session).await;
                }
            }
            chosen = Some(index);
        }
        let option = &options[chosen.unwrap_or(0)];
        if option.node.is_empty() {
            if let Some(ctx) = self.ctx.get() {
                let _ = ctx.call("frame.route.clear", json!({"category": "model", "key": session})).await;
            }
            lock(&self.placements.placed).remove(&session);
        } else if let Err(error) = self.bind(&session, &option.node).await {
            self.give_back_at(&option.node, &session).await;
            return Err(error);
        }
        lock(&self.claims.held).insert(session.clone(), option.node.clone());
        let moved = lock(&self.claims.last).insert(session.clone(), option.node.clone()).is_some_and(|before| before != option.node);
        if moved {
            tracing::info!(session = %session, to = %option.name, "a session's next model call moves to a better free model");
            self.notify("placed");
        }
        let tier = match option.tier {
            0 => "local",
            1 => "lan",
            _ => "remote",
        };
        Ok(Placement { node: (!option.node.is_empty()).then(|| option.node.clone()), name: option.name.clone(), model_id: option.model_id.clone(), tier: tier.to_owned() })
    }

    /// The session's call is over: its model is free for the next run.
    pub async fn release(&self, session: &str) {
        let held = lock(&self.claims.held).remove(session);
        if let Some(node) = held {
            self.give_back_at(&node, session).await;
        }
    }

    /// The run is over: its age and its cached-prompt preference go.
    pub async fn claims_end(&self, session: &str) {
        self.release(session).await;
        lock(&self.claims.ages).remove(session);
        lock(&self.claims.last).remove(session);
    }
}
