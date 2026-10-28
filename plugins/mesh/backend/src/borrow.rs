//! Borrowing: a member's plugin serves a category here. The mesh plugin connects to its own frame
//! as a client that serves the category (the frame already lets a client do that: the shell serves
//! `browser` this way) and forwards every request the frame routes to it to the member, piping
//! items, the result and cancellation; the member's events of that category come back over an
//! events stream and are published here. Two shapes:
//!
//! - a whole category (`start_borrow`): this node has no model of its own, so a member's serves
//!   every session here; the local plugin, if any, is set aside for the borrow;
//! - one session (`place.rs`): the client says hello with scope `sessions`, and only the sessions
//!   the frame routes to it (`frame.route.set`) reach the member; this node's own model keeps
//!   serving everything else.
//!
//! Closing the client ends it: the frame routes the category to its own plugin again.

use crate::service::Mesh;
use cc_protocol::contracts::lifecycle::{Hello, HelloAck};
use cc_protocol::{Message, RpcError, codes};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

pub struct Borrow {
    pub category: String,
    pub node: String,
    pub provides: Vec<String>,
    /// This node's own plugin of the category, disabled for the borrow and enabled again after.
    pub local_plugin: Option<String>,
    stop: CancellationToken,
}

impl Borrow {
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// A client connection to this node's own frame that forwards to one member.
pub struct Client {
    /// `ws-<peer>`: how the frame names the client (`frame.route.set` binds sessions to it).
    pub instance: String,
    pub stop: CancellationToken,
}

#[derive(serde::Deserialize)]
pub(crate) struct FrameInfo {
    pub port: u16,
    pub token: String,
}

pub(crate) fn frame_info(home: &std::path::Path) -> Result<FrameInfo, String> {
    let path = home.join("run").join("frame.json");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("the frame is not running ({}: {error})", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} is not valid: {error}", path.display()))
}

impl Mesh {
    /// Start serving `category` here from the member `node`, for every session.
    pub async fn start_borrow(self: &Arc<Self>, category: &str, node: &str) -> Result<Vec<String>, RpcError> {
        let member = self.member(node).ok_or_else(|| RpcError::not_found("no such member"))?;
        if !crate::net::lendable(&format!("{category}.x")) && !crate::net::lendable("tools.run") {
            return Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("`{category}` is not something members lend")));
        }
        let provides = self.member_provides(node, category).await?;
        if provides.is_empty() {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("{} has no plugin serving `{category}` now", member.name)));
        }
        self.stop_borrow(category);
        // The frame lets a client take a category over only while the category's own plugin is
        // disabled; disabling it also unloads a local model, which is the point of borrowing one.
        let ctx = self.ctx()?;
        let listed: Value = ctx.call("frame.plugins.list", json!({})).await.unwrap_or(Value::Null);
        let local_plugin = listed
            .get("plugins")
            .and_then(Value::as_array)
            .and_then(|plugins| plugins.iter().find(|plugin| plugin.get("category").and_then(Value::as_str) == Some(category) && plugin.get("transport").and_then(Value::as_str) != Some("websocket")))
            .and_then(|plugin| plugin.get("plugin").and_then(Value::as_str).filter(|unit| !unit.is_empty()).map(str::to_owned));
        let local_was_active = listed
            .get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|plugins| plugins.iter().any(|plugin| plugin.get("category").and_then(Value::as_str) == Some(category) && plugin.get("state").and_then(Value::as_str) != Some("disabled") && plugin.get("transport").and_then(Value::as_str) != Some("websocket")));
        if let Some(unit) = &local_plugin
            && local_was_active
        {
            ctx.call("frame.plugins.disable", json!({"plugin": unit})).await.map_err(|error| RpcError::new(codes::UNAVAILABLE, format!("cannot set this node's own `{category}` plugin aside: {}", error.message)))?;
        }
        let local_plugin = local_plugin.filter(|_| local_was_active);
        let category_owned = category.to_owned();
        let mesh = self.clone();
        let client = self.open_client(category, node, provides.clone(), "", true, move || mesh.borrow_ended(&category_owned)).await?;
        let borrow = Borrow { category: category.to_owned(), node: node.to_owned(), provides: provides.clone(), local_plugin, stop: client.stop };
        self.borrows.lock().unwrap_or_else(|p| p.into_inner()).insert(category.to_owned(), borrow);
        self.notify("serving");
        Ok(provides)
    }

    /// The methods a member's plugin of `category` serves now.
    pub async fn member_provides(&self, node: &str, category: &str) -> Result<Vec<String>, RpcError> {
        let answer = self.call_member(node, "mesh.provides", json!({"category": category}), None, CancellationToken::new()).await?;
        Ok(serde_json::from_value(answer.get("provides").cloned().unwrap_or(Value::Null)).unwrap_or_default())
    }

    /// Connect to this node's frame as a client serving `category` from the member `node`:
    /// `scope` empty takes the category over, `sessions` serves only routed sessions. `events`:
    /// the member's events of the category are published here too (only when the category is
    /// taken over; a session's lender must not speak for this node's own plugin). `ended` runs
    /// when the connection closes.
    pub async fn open_client(self: &Arc<Self>, category: &str, node: &str, provides: Vec<String>, scope: &str, events: bool, ended: impl FnOnce() + Send + 'static) -> Result<Client, RpcError> {
        let contract = cc_protocol::contracts::contract_of(category).ok_or_else(|| RpcError::invalid_params(format!("`{category}` is not a category this frame knows")))?;
        let info = frame_info(&self.home).map_err(|e| RpcError::new(codes::UNAVAILABLE, e))?;
        let url = format!("ws://127.0.0.1:{}/ws?token={}", info.port, info.token);
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("cannot reach the frame's socket: {e}")))?;
        let (mut sink, mut source) = socket.split();
        let hello = Hello {
            plugin_id: format!("mesh-borrow:{}", &node[..node.len().min(10)]),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            category: category.to_owned(),
            contract: contract.to_owned(),
            provides,
            subscribes: Vec::new(),
            // A borrowed category serves this machine's chats, not the model's shell commands.
            commands: Vec::new(),
            scope: scope.to_owned(),
            instance: String::new(),
            pid: std::process::id(),
            swap: Default::default(),
        };
        let hello_message = serde_json::to_string(&Message::Req { id: 1, method: "frame.hello".to_owned(), params: serde_json::to_value(&hello).unwrap_or(Value::Null) }).unwrap_or_default();
        sink.send(WsMessage::Text(hello_message.into())).await.map_err(|e| RpcError::new(codes::UNAVAILABLE, e.to_string()))?;
        let ack = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let Some(Ok(WsMessage::Text(text))) = source.next().await else {
                    return Err(RpcError::new(codes::UNAVAILABLE, "the frame closed the socket before answering hello"));
                };
                if let Ok(Message::Res { id: 1, ok, result, error }) = serde_json::from_str::<Message>(&text) {
                    if !ok {
                        return Err(error.unwrap_or_else(|| RpcError::internal("the frame refused the hello")));
                    }
                    return Ok(result.and_then(|value| serde_json::from_value::<HelloAck>(value).ok()));
                }
            }
        })
        .await
        .map_err(|_| RpcError::new(codes::UNAVAILABLE, "the frame did not answer the hello in time"))??;
        let ack = ack.ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the frame's hello answer could not be read"))?;
        if !ack.accepted {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("the frame refused to let a member serve `{category}`: {}", ack.reason)));
        }
        let stop = CancellationToken::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        // The writer: everything the forwarders answer goes out in order.
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        next = out_rx.recv() => match next {
                            Some(message) => {
                                let text = serde_json::to_string(&message).unwrap_or_default();
                                if sink.send(WsMessage::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        },
                        _ = stop.cancelled() => {
                            let _ = sink.close().await;
                            break;
                        }
                    }
                }
            });
        }
        // The events of the category at the member come here and are published as this frame's.
        if events {
            let mesh = self.clone();
            let node = node.to_owned();
            let stop = stop.clone();
            let out = out_tx.clone();
            let prefix = format!("{category}.");
            tokio::spawn(async move {
                let (items_tx, mut items_rx) = mpsc::unbounded_channel::<Value>();
                let forward = {
                    let out = out.clone();
                    tokio::spawn(async move {
                        while let Some(item) = items_rx.recv().await {
                            if let (Some(topic), Some(data)) = (item.get("topic").and_then(Value::as_str), item.get("data")) {
                                let _ = out.send(Message::Evt { topic: topic.to_owned(), data: data.clone() });
                            }
                        }
                    })
                };
                let _ = mesh.call_member(&node, "mesh.events", json!({"prefixes": [prefix]}), Some(items_tx), stop.clone()).await;
                forward.abort();
            });
        }
        // The reader: requests the frame routes here are forwarded to the member.
        {
            let mesh = self.clone();
            let node = node.to_owned();
            let stop = stop.clone();
            let out = out_tx.clone();
            tokio::spawn(async move {
                let open: Arc<Mutex<HashMap<u64, CancellationToken>>> = Arc::new(Mutex::new(HashMap::new()));
                loop {
                    let next = tokio::select! {
                        next = source.next() => next,
                        _ = stop.cancelled() => break,
                    };
                    let Some(Ok(WsMessage::Text(text))) = next else {
                        if matches!(next, None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))) {
                            break;
                        }
                        continue;
                    };
                    let Ok(message) = serde_json::from_str::<Message>(&text) else { continue };
                    match message {
                        Message::Req { id, method, params } => {
                            let cancel = CancellationToken::new();
                            open.lock().unwrap_or_else(|p| p.into_inner()).insert(id, cancel.clone());
                            let mesh = mesh.clone();
                            let node = node.clone();
                            let out = out.clone();
                            let open = open.clone();
                            tokio::spawn(async move {
                                let (items_tx, mut items_rx) = mpsc::unbounded_channel::<Value>();
                                let pump = {
                                    let out = out.clone();
                                    tokio::spawn(async move {
                                        while let Some(data) = items_rx.recv().await {
                                            let _ = out.send(Message::Item { id, data });
                                        }
                                    })
                                };
                                let outcome = mesh.call_member(&node, &method, params, Some(items_tx), cancel).await;
                                let _ = pump.await;
                                let message = match outcome {
                                    Ok(result) => Message::Res { id, ok: true, result: Some(result), error: None },
                                    Err(error) => Message::Res { id, ok: false, result: None, error: Some(error) },
                                };
                                let _ = out.send(message);
                                open.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                            });
                        }
                        Message::Cancel { id } => {
                            if let Some(cancel) = open.lock().unwrap_or_else(|p| p.into_inner()).get(&id) {
                                cancel.cancel();
                            }
                        }
                        _ => {}
                    }
                }
                ended();
            });
        }
        Ok(Client { instance: ack.instance, stop })
    }

    /// Stop serving `category` from a member; the frame's own plugin serves it again.
    pub fn stop_borrow(&self, category: &str) -> bool {
        let removed = self.borrows.lock().unwrap_or_else(|p| p.into_inner()).remove(category);
        match removed {
            Some(borrow) => {
                borrow.stop();
                // This node's own plugin serves the category again.
                if let (Some(unit), Some(ctx)) = (borrow.local_plugin.clone(), self.ctx.get().cloned()) {
                    tokio::spawn(async move {
                        if let Err(error) = ctx.call("frame.plugins.enable", json!({"plugin": unit})).await {
                            tracing::warn!(%error, "the node's own plugin could not be enabled again");
                        }
                    });
                }
                self.notify("serving");
                true
            }
            None => false,
        }
    }

    fn borrow_ended(&self, category: &str) {
        let mut borrows = self.borrows.lock().unwrap_or_else(|p| p.into_inner());
        if borrows.contains_key(category) {
            borrows.remove(category);
            drop(borrows);
            self.notify("serving");
        }
    }
}
