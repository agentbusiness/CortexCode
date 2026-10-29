//! Where a session's model calls run. Decided here, by the host, never by the model: a person's
//! turn or an agent asks once (`mesh.place`), and from then on the frame routes that session's
//! model calls (its `cache_key` is the session id) to the chosen computer until `mesh.place.end`.
//!
//! The best free model wins, wherever it runs: the strongest first (of one family, the larger
//! weights: more bits per weight or more parameters), then this computer, the local network,
//! remote members, then the faster one. Only a model of the same family as this computer's, and
//! tagged for the session's work (`coding`), counts. A model is taken only when it is free: running nothing
//! (an agent must not slow another's tokens down) or, for a person's turn, nothing a person
//! waits for (the engine gives a person's request every step over agent work). A person's turn
//! waits for the first model to free up; an agent takes what is free now or stays queued here.

use crate::borrow::Client;
use crate::service::{Mesh, lock};
use cc_protocol::contracts::mesh::{Capacity, CapacityParams, PlaceParams, Placement};
use cc_protocol::contracts::model::{LiveMetrics, ModelInfo, Priority, SlotsResult};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often a waiting turn looks again.
const WAIT_STEP: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Placements {
    /// session id -> the member serving its model calls.
    pub placed: Mutex<HashMap<String, String>>,
    /// node -> the client connection the frame routes those sessions to.
    clients: Mutex<HashMap<String, Arc<Client>>>,
}

pub(crate) struct Candidate {
    /// None: this computer.
    pub(crate) node: Option<String>,
    name: String,
    model_id: String,
    /// 0 this computer, 1 local network, 2 remote.
    tier: u8,
    /// The model's files on disk: within one family, the larger the stronger.
    pub(crate) weights: u64,
    tok_s: f64,
}

/// Best first: the strongest model, then the nearest computer, then the faster one.
pub(crate) fn best_first(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.weights.cmp(&a.weights).then(a.tier.cmp(&b.tier)).then(b.tok_s.partial_cmp(&a.tok_s).unwrap_or(std::cmp::Ordering::Equal))
}

struct Local {
    family: String,
    tags: Vec<String>,
    weights: u64,
    tok_s: f64,
    busy: u32,
    busy_foreground: u32,
    queued: u32,
    queued_foreground: u32,
}

impl Mesh {
    /// This computer's own model as it stands now (None: no model serving here).
    async fn local_model(&self) -> Option<Local> {
        let ctx = self.ctx.get()?;
        // A borrowed category is a member's model already: it counts as not local.
        if lock(&self.borrows).contains_key("model") {
            return None;
        }
        let info: ModelInfo = serde_json::from_value(ctx.call("model.info", json!({})).await.ok()?).ok()?;
        // Another app's model on this machine, borrowed by this one's model plugin: not local.
        if info.borrowed() {
            return None;
        }
        let slots: SlotsResult = serde_json::from_value(ctx.call("model.slots", json!({})).await.ok()?).ok()?;
        let tok_s = ctx.call("model.metrics", json!({})).await.ok().and_then(|value| serde_json::from_value::<LiveMetrics>(value).ok()).map_or(0.0, |metrics| metrics.decode_tps_per_stream);
        Some(Local {
            family: info.family,
            tags: info.tags,
            weights: info.file_bytes,
            tok_s,
            busy: slots.busy,
            busy_foreground: slots.slots.iter().filter(|slot| slot.busy && slot.priority == Some(Priority::Foreground)).count() as u32,
            queued: slots.queued,
            queued_foreground: slots.queued_by_priority.foreground,
        })
    }

    /// Whether this computer's own model (not a borrowed one) is tagged for `tag`.
    pub async fn local_serves(&self, tag: &str) -> bool {
        self.local_model().await.is_some_and(|local| local.tags.iter().any(|own| own == tag))
    }

    /// Reachable members with a model of `family` (any family when None), tagged for `tag`, that
    /// no session of ours is placed on. Whether the model is free now, and how strong it is, are asked live
    /// ([`Self::free_weights`]): a card is up to 15 s old.
    pub(crate) fn candidates(&self, family: Option<&str>, tag: &str) -> Vec<Candidate> {
        let me = self.me.hex();
        let cards = lock(&self.cards).clone();
        let conns = self.connections();
        let placed = lock(&self.placements.placed).clone();
        let found: Vec<Candidate> = self
            .members()
            .into_values()
            .filter(|member| member.node != me)
            .filter_map(|member| {
                let connection = conns.get(&member.node)?;
                let card = cards.get(&member.node)?;
                let model = card.model.as_ref()?;
                if family.is_some_and(|family| !family.is_empty() && model.family != family) {
                    return None;
                }
                if !model.tags.iter().any(|own| own == tag) {
                    return None;
                }
                if placed.values().any(|node| *node == member.node) {
                    return None;
                }
                let (path, _) = Self::path_of(connection);
                Some(Candidate { node: Some(member.node.clone()), name: member.name.clone(), model_id: model.model_id.clone(), tier: if path == "lan" { 1 } else { 2 }, weights: 0, tok_s: model.tok_s })
            })
            .collect();
        found
    }

    /// The size of a member's model when it runs nothing now (None: busy or not answering).
    pub(crate) async fn free_weights(&self, node: &str) -> Option<u64> {
        let fresh = self.call_member(node, "mesh.card", json!({}), None, CancellationToken::new()).await.ok()?;
        let model = fresh.get("model")?;
        if model.get("slots_busy").and_then(Value::as_u64) != Some(0) || model.get("queued").and_then(Value::as_u64) != Some(0) {
            return None;
        }
        let info: ModelInfo = serde_json::from_value(self.call_member(node, "model.info", json!({}), None, CancellationToken::new()).await.ok()?).ok()?;
        Some(info.file_bytes)
    }

    /// Free members' models of `family` tagged for `tag`, best first.
    async fn free_candidates(&self, family: Option<&str>, tag: &str) -> Vec<Candidate> {
        let mut free = Vec::new();
        for mut candidate in self.candidates(family, tag) {
            let Some(node) = candidate.node.clone() else { continue };
            if let Some(weights) = self.free_weights(&node).await {
                candidate.weights = weights;
                free.push(candidate);
            }
        }
        free.sort_by(best_first);
        free
    }

    /// How many members could take a session's model calls for `tag` now.
    pub async fn capacity(&self, params: CapacityParams) -> Capacity {
        let family = self.local_model().await.map(|local| local.family);
        let names: Vec<String> = self.free_candidates(family.as_deref(), &params.tag).await.into_iter().map(|candidate| candidate.name).collect();
        Capacity { free: names.len() as u32, names }
    }

    pub async fn place(self: &Arc<Self>, params: PlaceParams, cancel: CancellationToken) -> Result<Placement, RpcError> {
        if params.session_id.is_empty() {
            return Err(RpcError::invalid_params("session_id must not be empty"));
        }
        loop {
            // Placed already, on a member still here: keep it (a resumed run).
            let current = lock(&self.placements.placed).get(&params.session_id).cloned();
            if let Some(node) = current {
                if self.connection(&node).is_some() {
                    let member = self.member(&node);
                    let model_id = lock(&self.cards).get(&node).and_then(|card| card.model.as_ref()).map(|model| model.model_id.clone()).unwrap_or_default();
                    let tier = self.connection(&node).map(|connection| if Self::path_of(&connection).0 == "lan" { "lan" } else { "remote" }).unwrap_or("remote");
                    return Ok(Placement { node: Some(node.clone()), name: member.map(|member| member.name).unwrap_or_default(), model_id, tier: tier.to_owned() });
                }
                self.place_end(&params.session_id).await;
            }
            let local = self.local_model().await;
            let family = local.as_ref().map(|local| local.family.clone());
            let mut options = self.free_candidates(family.as_deref(), &params.tag).await;
            if !params.remote_only
                && let Some(local) = local.as_ref().filter(|local| local.tags.contains(&params.tag))
            {
                // A person's turn: free unless another person's request runs or waits here. An
                // agent: free only when nothing runs here at all.
                let free = if params.wait { local.busy_foreground == 0 && local.queued_foreground == 0 } else { local.busy == 0 && local.queued == 0 };
                if free {
                    options.push(Candidate { node: None, name: self.name(), model_id: String::new(), tier: 0, weights: local.weights, tok_s: local.tok_s });
                    options.sort_by(best_first);
                }
            }
            for candidate in options {
                let Some(node) = candidate.node else {
                    return Ok(Placement { node: None, name: candidate.name, model_id: String::new(), tier: "local".to_owned() });
                };
                match self.bind(&params.session_id, &node).await {
                    Ok(()) => {
                        tracing::info!(session = %params.session_id, member = %candidate.name, "a session's model calls go to a member");
                        self.notify("placed");
                        return Ok(Placement { node: Some(node), name: candidate.name, model_id: candidate.model_id, tier: if candidate.tier == 1 { "lan".to_owned() } else { "remote".to_owned() } });
                    }
                    Err(error) => tracing::debug!(member = %candidate.name, %error, "could not route a session to a member"),
                }
            }
            // Nothing to wait for: no model here and no member with one (the call fails or
            // queues as it would without the mesh).
            let any_model = local.is_some() || lock(&self.cards).values().any(|card| card.model.is_some());
            if !params.wait || !any_model {
                // Nothing free: the call runs here, queued behind what runs (or, for an agent whose
                // queue here is full, it stays queued).
                return Ok(Placement { node: None, name: self.name(), model_id: String::new(), tier: if local.is_some() { "local".to_owned() } else { String::new() } });
            }
            tokio::select! {
                _ = tokio::time::sleep(WAIT_STEP) => {}
                _ = cancel.cancelled() => return Err(RpcError::new(codes::CANCELLED, "cancelled while waiting for a free model")),
            }
        }
    }

    /// Route one session's model calls to a member: the client serving `model` for sessions
    /// (one per member, opened on first use), then the frame's session route.
    pub(crate) async fn bind(self: &Arc<Self>, session_id: &str, node: &str) -> Result<(), RpcError> {
        let client = lock(&self.placements.clients).get(node).cloned();
        let client = match client {
            Some(client) if !client.stop.is_cancelled() => client,
            _ => {
                let provides = self.member_provides(node, "model").await?;
                if provides.is_empty() {
                    return Err(RpcError::new(codes::UNAVAILABLE, "the member's model is not serving"));
                }
                let mesh = self.clone();
                let gone = node.to_owned();
                let client = Arc::new(self.open_client("model", node, provides, "sessions", false, move || mesh.client_closed(&gone)).await?);
                lock(&self.placements.clients).insert(node.to_owned(), client.clone());
                client
            }
        };
        let ctx = self.ctx()?;
        ctx.call("frame.route.set", json!({"category": "model", "key": session_id, "instance": client.instance})).await?;
        lock(&self.placements.placed).insert(session_id.to_owned(), node.to_owned());
        Ok(())
    }

    /// The session's model calls stay here again.
    pub async fn place_end(&self, session_id: &str) {
        let node = lock(&self.placements.placed).remove(session_id);
        let Some(node) = node else { return };
        if let Some(ctx) = self.ctx.get() {
            let _ = ctx.call("frame.route.clear", json!({"category": "model", "key": session_id})).await;
        }
        let others = lock(&self.placements.placed).values().any(|placed| *placed == node);
        if !others
            && let Some(client) = lock(&self.placements.clients).remove(&node)
        {
            client.stop.cancel();
        }
        self.notify("placed");
    }

    /// A member went away: the sessions placed on it run here from their next call (the frame
    /// drops the routes when the client closes).
    pub fn member_gone(&self, node: &str) {
        let sessions: Vec<String> = lock(&self.placements.placed).iter().filter(|(_, placed)| *placed == node).map(|(session, _)| session.clone()).collect();
        if sessions.is_empty() {
            return;
        }
        lock(&self.placements.placed).retain(|_, placed| placed != node);
        if let Some(client) = lock(&self.placements.clients).remove(node) {
            client.stop.cancel();
        }
        tracing::info!(member = %&node[..node.len().min(10)], sessions = sessions.len(), "a lender went away; its sessions' model calls run here");
        self.notify("placed");
    }

    fn client_closed(&self, node: &str) {
        let removed = lock(&self.placements.clients).remove(node).is_some();
        let had = {
            let mut placed = lock(&self.placements.placed);
            let before = placed.len();
            placed.retain(|_, placed| placed != node);
            before != placed.len()
        };
        if removed || had {
            self.notify("placed");
        }
    }

    pub fn placed(&self) -> Vec<cc_protocol::contracts::mesh::Placed> {
        let members = self.members();
        lock(&self.placements.placed)
            .iter()
            .map(|(session_id, node)| cc_protocol::contracts::mesh::Placed { session_id: session_id.clone(), node: node.clone(), name: members.get(node).map(|member| member.name.clone()).unwrap_or_default() })
            .collect()
    }
}
