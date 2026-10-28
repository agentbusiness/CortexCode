//! The connections between members and what flows over them.
//!
//! One QUIC connection per member (iroh: dialed by key, direct on the LAN, hole-punched or relayed
//! elsewhere, TLS 1.3 end to end). A connection from a key the membership log does not hold is
//! closed at once. On `cortex/frame/1` every request is its own bidirectional stream carrying the
//! frame's own envelopes ([`cc_protocol::envelope::Message`]): the requester writes one `Req`, the
//! serving side answers with `Item`s and one `Res`, and a `Cancel` or a closed stream cancels the
//! forwarded call. What a member may ask is fixed ([`lendable`]: the model, the hands); the serving
//! side forwards such a call to its own frame (`ctx.call_stream`) and nothing else.
//!
//! Mesh-level requests on the same streams: `mesh.card.push` (a member's card), `mesh.log.push`
//! (the membership log), `mesh.provides {category}` (the methods a category's plugin serves here),
//! `mesh.events {prefixes}` (a stream of the frame's events with those prefixes, as items),
//! `mesh.window.ticket` (this frame's token) and `mesh.window` (the rest of the stream is one
//! connection of the member's window to this frame: [`crate::projects`]).
//!
//! `cortex/pair/1` runs the pairing exchange while a code is shown ([`crate::pairing`]).

use crate::members::Entry;
use crate::pairing;
use crate::service::Mesh;
use cc_protocol::codec::{read_message, write_message};
use cc_protocol::contracts::mesh::{Card, topics};
use cc_protocol::contracts::now_ms;
use cc_protocol::{Message, RpcError, codes};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const FRAME_ALPN: &[u8] = b"cortex/frame/1";

/// How long a dial may take before the member counts as unreachable for now.
const DIAL_TIMEOUT: Duration = Duration::from_secs(12);

/// What every member lends every other: its model and its hands (commands, language servers,
/// containers). Everything else is private to the node, never forwarded.
pub fn lendable(method: &str) -> bool {
    let category = cc_protocol::contracts::category_of(method);
    match category {
        "model" | "container" => true,
        "tools" => matches!(method, "tools.run" | "tools.job.status" | "tools.job.kill" | "tools.job.list" | "tools.artifact" | "tools.files" | "tools.read"),
        _ => category.starts_with("lang-"),
    }
}

// --- accepting ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FrameHandler(pub Arc<Mesh>);

impl ProtocolHandler for FrameHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let node = connection.remote_id().to_string();
        if !self.0.is_member(&node) {
            tracing::info!(node = %&node[..node.len().min(10)], "a connection from a key that is not a member; closed");
            connection.close(1u32.into(), b"not a member");
            return Ok(());
        }
        self.0.connected(&node, connection.clone());
        serve_connection(self.0.clone(), node.clone(), connection.clone()).await;
        self.0.disconnected(&node, &connection);
        Ok(())
    }
}

/// Serve a member's requests on a connection, dialed or accepted, until it closes: each stream it
/// opens is one request.
async fn serve_connection(mesh: Arc<Mesh>, node: String, connection: Connection) {
    while let Ok((send, recv)) = connection.accept_bi().await {
        let mesh = mesh.clone();
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_stream(mesh, node, send, recv).await {
                tracing::debug!(%error, "a member's request ended with an error");
            }
        });
    }
}

#[derive(Debug, Clone)]
pub struct PairHandler(pub Arc<Mesh>);

impl ProtocolHandler for PairHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Some(code) = self.0.pending_code() else {
            connection.close(2u32.into(), b"not pairing");
            return Ok(());
        };
        let Ok((send, recv)) = connection.accept_bi().await else { return Ok(()) };
        let mut stream = tokio::io::join(recv, send);
        let mesh = self.0.clone();
        let outcome = pairing::invite(&mut stream, &code, |record| mesh.admit(record)).await;
        match outcome {
            Ok(record) => {
                tracing::info!(name = %record.name, "a computer joined the cortex");
                self.0.pairing_done();
                let mesh = self.0.clone();
                tokio::spawn(async move { mesh.dial_all().await });
            }
            Err(reason) => {
                tracing::info!(%reason, "a pairing attempt failed");
                self.0.pairing_attempt_failed();
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
        Ok(())
    }
}

/// A member's request on one stream: answer mesh-level ones here, forward the allowed rest to the
/// frame, piping items and the result back; a `Cancel` or a closed stream cancels the call.
async fn serve_stream(mesh: Arc<Mesh>, node: String, mut send: SendStream, mut recv: RecvStream) -> Result<(), String> {
    let first = read_message(&mut recv).await.map_err(|e| e.to_string())?;
    let Some(Message::Req { id, method, params }) = first else {
        return Err("the stream did not start with a request".to_owned());
    };
    let answer = |outcome: Result<Value, RpcError>| {
        match outcome {
            Ok(result) => Message::Res { id, ok: true, result: Some(result), error: None },
            Err(error) => Message::Res { id, ok: false, result: None, error: Some(error) },
        }
    };
    match method.as_str() {
        "mesh.card.push" => {
            if let Ok(card) = serde_json::from_value::<Card>(params) {
                mesh.card_received(&node, card);
            }
            let message = answer(Ok(json!({})));
            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
        }
        "mesh.log.push" => {
            let entries: Vec<Entry> = serde_json::from_value(params.get("entries").cloned().unwrap_or(Value::Null)).unwrap_or_default();
            let outcome = mesh.log_received(entries).map(|changed| json!({"changed": changed})).map_err(RpcError::invalid_params);
            let message = answer(outcome);
            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
        }
        "mesh.provides" => {
            let category = params.get("category").and_then(Value::as_str).unwrap_or("").to_owned();
            let provides = mesh.provides(&category).await;
            let message = answer(Ok(json!({"category": category, "provides": provides})));
            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
        }
        "mesh.window.ticket" => {
            let message = answer(crate::projects::ticket(&mesh.home));
            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
        }
        "mesh.window" => {
            // The rest of the stream is a connection of the member's window to this frame.
            crate::projects::serve_window(&mesh.home, &mut send, &mut recv).await?;
        }
        "mesh.card" => {
            let card = mesh.my_card().await;
            let message = answer(serde_json::to_value(card).map_err(|e| RpcError::internal(e.to_string())));
            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
        }
        "mesh.events" => {
            let prefixes: Vec<String> = serde_json::from_value(params.get("prefixes").cloned().unwrap_or(Value::Null)).unwrap_or_default();
            let prefixes: Vec<String> = prefixes.into_iter().filter(|prefix| lendable(&format!("{}x", prefix.trim_end_matches('.').to_owned() + "."))).collect();
            let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
            mesh.subscribe_events(&node, prefixes, tx);
            loop {
                tokio::select! {
                    next = rx.recv() => match next {
                        Some((topic, data)) => {
                            if write_message(&mut send, &Message::Item { id, data: json!({"topic": topic, "data": data}) }).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    closed = read_message(&mut recv) => {
                        // A `Cancel` or the end of the stream ends the subscription.
                        if !matches!(closed, Ok(Some(_))) || matches!(closed, Ok(Some(Message::Cancel { .. }))) {
                            break;
                        }
                    }
                }
            }
            drop(rx);
        }
        "mesh.tree.ready" | "mesh.tree.apply" | "mesh.tree.files" => {
            if !mesh.tree_request_allowed(&node, &method, &params) {
                let message = answer(Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("`{method}` is not open to this member for that tree"))));
                write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                return Ok(());
            }
            match method.as_str() {
                "mesh.tree.ready" => {
                    let outcome = mesh.tree_ready(&node, params).await;
                    let message = answer(outcome);
                    write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                }
                "mesh.tree.apply" => {
                    let outcome = match serde_json::from_value::<cc_protocol::contracts::mesh::TreeApplyParams>(params) {
                        Ok(params) => mesh.tree_apply(&node, params).await,
                        Err(error) => Err(RpcError::invalid_params(error.to_string())),
                    };
                    let message = answer(outcome);
                    write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                }
                _ => {
                    // The tree's files stream from this node's own method.
                    let ctx = mesh.ctx().map_err(|e| e.message)?;
                    let mut call = match ctx.call_stream(&method, params) {
                        Ok(call) => call,
                        Err(error) => {
                            let message = answer(Err(error));
                            write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                            return Ok(());
                        }
                    };
                    while let Some(data) = call.next().await {
                        if write_message(&mut send, &Message::Item { id, data }).await.is_err() {
                            call.cancel();
                            break;
                        }
                    }
                    let message = answer(call.finish().await);
                    let _ = write_message(&mut send, &message).await;
                }
            }
        }
        _ => {
            if !lendable(&method) {
                let message = answer(Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("`{method}` is not lent to this member"))));
                write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                return Ok(());
            }
            let ctx = mesh.ctx().map_err(|e| e.message)?;
            let mut call = match ctx.call_stream(&method, params) {
                Ok(call) => call,
                Err(error) => {
                    let message = answer(Err(error));
                    write_message(&mut send, &message).await.map_err(|e| e.to_string())?;
                    return Ok(());
                }
            };
            let cancelled = CancellationToken::new();
            let watcher = {
                let cancelled = cancelled.clone();
                tokio::spawn(async move {
                    loop {
                        match read_message(&mut recv).await {
                            Ok(Some(Message::Cancel { .. })) | Ok(None) | Err(_) => {
                                cancelled.cancel();
                                break;
                            }
                            Ok(Some(_)) => {}
                        }
                    }
                })
            };
            loop {
                tokio::select! {
                    item = call.next() => match item {
                        Some(data) => {
                            if write_message(&mut send, &Message::Item { id, data }).await.is_err() {
                                call.cancel();
                                break;
                            }
                        }
                        None => {
                            let outcome = call.finish().await;
                            let message = answer(outcome);
                            let _ = write_message(&mut send, &message).await;
                            break;
                        }
                    },
                    _ = cancelled.cancelled() => {
                        call.cancel();
                        let message = answer(Err(RpcError::new(codes::CANCELLED, "cancelled by the member")));
                        let _ = write_message(&mut send, &message).await;
                        break;
                    }
                }
            }
            watcher.abort();
        }
    }
    let _ = send.finish();
    Ok(())
}

// --- dialing and calling -------------------------------------------------------------------------

impl Mesh {
    /// Bind the endpoint, start the accept loop and the local-network lookup.
    pub async fn start_net(self: &Arc<Self>) -> Result<(), String> {
        let secret = self.me.secret.clone();
        let mut builder = Endpoint::builder(iroh::endpoint::presets::N0).secret_key(secret).alpns(vec![FRAME_ALPN.to_vec(), pairing::ALPN.to_vec()]);
        // Relays: the plugin's configured ones (`[config] relays`, for a company with its own), in
        // front of n0's public relays, which carry members on other networks that cannot be
        // reached directly. A relay sees only encrypted traffic.
        let mut relays: Vec<iroh::RelayUrl> = Vec::new();
        for url in lock_config(self).relays.iter() {
            match url.parse::<iroh::RelayUrl>() {
                Ok(parsed) => relays.push(parsed),
                Err(error) => tracing::warn!(url, %error, "a relay URL could not be read"),
            }
        }
        if !relays.is_empty() {
            let defaults: Vec<iroh::RelayUrl> = iroh::RelayMode::Default.relay_map().urls();
            relays.extend(defaults);
            builder = builder.relay_mode(iroh::RelayMode::custom(relays));
        }
        let endpoint = builder.bind().await.map_err(|e| format!("cannot bind the mesh endpoint: {e}"))?;
        match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id()) {
            Ok(mdns) => {
                if let Ok(services) = endpoint.address_lookup() {
                    services.add(mdns.clone());
                }
                let _ = self.mdns.set(mdns);
            }
            Err(error) => tracing::warn!(%error, "local-network lookup is not available; members are found by their addresses"),
        }
        let router = Router::builder(endpoint.clone()).accept(FRAME_ALPN, FrameHandler(self.clone())).accept(pairing::ALPN, PairHandler(self.clone())).spawn();
        let _ = self.endpoint.set(endpoint);
        let _ = self.router.set(router);
        let mesh = self.clone();
        tokio::spawn(async move {
            mesh.dial_all().await;
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                mesh.dial_all().await;
                mesh.push_card().await;
                mesh.auto_place().await;
            }
        });
        Ok(())
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.get()
    }

    /// This node's address, for the others to dial without a lookup.
    pub fn my_addr(&self) -> Option<EndpointAddr> {
        self.endpoint.get().map(Endpoint::addr)
    }

    /// Dial every member not connected now.
    pub async fn dial_all(self: &Arc<Self>) {
        let Some(endpoint) = self.endpoint.get() else { return };
        let members = self.members();
        for (node, member) in members {
            if node == self.me.hex() || self.connection(&node).is_some() {
                continue;
            }
            let Ok(id) = node.parse::<EndpointId>() else { continue };
            let addr = member.addrs.first().and_then(|text| serde_json::from_str::<EndpointAddr>(text).ok()).unwrap_or_else(|| EndpointAddr::new(id));
            let mesh = self.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, FRAME_ALPN)).await {
                    Ok(Ok(connection)) => {
                        mesh.connected(&node, connection.clone());
                        mesh.push_card_to(&node).await;
                        mesh.push_log_to(&node).await;
                        // The member's requests come over this connection too.
                        serve_connection(mesh.clone(), node.clone(), connection.clone()).await;
                        mesh.disconnected(&node, &connection);
                    }
                    Ok(Err(error)) => tracing::debug!(node = %&node[..node.len().min(10)], %error, "member not reachable"),
                    Err(_) => tracing::debug!(node = %&node[..node.len().min(10)], "member not reachable (timeout)"),
                }
            });
        }
    }

    /// One call on a member's frame: `items` gets every stream item; the result comes back.
    pub async fn call_member(&self, node: &str, method: &str, params: Value, items: Option<mpsc::UnboundedSender<Value>>, cancel: CancellationToken) -> Result<Value, RpcError> {
        let Some(connection) = self.connection(node) else {
            return Err(RpcError::new(codes::UNAVAILABLE, "the member is not reachable now").retryable());
        };
        let (mut send, mut recv) = connection.open_bi().await.map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("cannot open a stream to the member: {e}")).retryable())?;
        let id = 1;
        write_message(&mut send, &Message::Req { id, method: method.to_owned(), params }).await.map_err(|e| RpcError::new(codes::UNAVAILABLE, e.to_string()).retryable())?;
        loop {
            tokio::select! {
                next = read_message(&mut recv) => match next {
                    Ok(Some(Message::Item { data, .. })) => {
                        if let Some(items) = &items {
                            let _ = items.send(data);
                        }
                    }
                    Ok(Some(Message::Res { ok, result, error, .. })) => {
                        let _ = send.finish();
                        return if ok { Ok(result.unwrap_or(Value::Null)) } else { Err(error.unwrap_or_else(|| RpcError::internal("the member answered with an empty error"))) };
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return Err(RpcError::new(codes::CONNECTION_CLOSED, "the member closed the stream").retryable()),
                    Err(error) => return Err(RpcError::new(codes::CONNECTION_CLOSED, error.to_string()).retryable()),
                },
                _ = cancel.cancelled() => {
                    let _ = write_message(&mut send, &Message::Cancel { id }).await;
                    let _ = send.finish();
                    return Err(RpcError::new(codes::CANCELLED, "cancelled"));
                }
            }
        }
    }

    pub async fn push_card(self: &Arc<Self>) {
        let nodes: Vec<String> = self.connections().keys().cloned().collect();
        for node in nodes {
            self.push_card_to(&node).await;
        }
    }

    pub async fn push_card_to(&self, node: &str) {
        let card = self.my_card().await;
        let _ = self.call_member(node, "mesh.card.push", serde_json::to_value(card).unwrap_or(Value::Null), None, CancellationToken::new()).await;
    }

    pub async fn push_log_to(&self, node: &str) {
        let entries = self.log_entries();
        let _ = self.call_member(node, "mesh.log.push", json!({"entries": entries}), None, CancellationToken::new()).await;
    }

    pub async fn push_log(&self) {
        let nodes: Vec<String> = self.connections().keys().cloned().collect();
        for node in nodes {
            self.push_log_to(&node).await;
        }
    }

    /// The path a connection uses now: `direct`, `relay` or empty, and its round trip.
    /// The selected path to a member: `lan` (direct, a private address: the local network),
    /// `direct` (hole-punched across networks) or `relay`, with its round trip.
    pub fn path_of(connection: &Connection) -> (String, u32) {
        for path in connection.paths().iter() {
            if path.is_selected() {
                let kind = if path.is_relay() {
                    "relay"
                } else if let iroh::TransportAddr::Ip(addr) = path.remote_addr()
                    && is_local_network(&addr.ip())
                {
                    "lan"
                } else {
                    "direct"
                };
                return (kind.to_owned(), path.rtt().as_millis() as u32);
            }
        }
        (String::new(), 0)
    }

    /// Join another cortex with a code: the inviter is found on the local network or named by the
    /// invitation's ticket; the exchange gives this node the membership log.
    pub async fn join(self: &Arc<Self>, code: &str, name: &str) -> Result<(), String> {
        let endpoint = self.endpoint.get().ok_or("the mesh is not started")?;
        let (words, ticket) = match code.split_once('@') {
            Some((words, ticket)) => (crate::words::normalize(words), Some(ticket.trim().to_owned())),
            None => (crate::words::normalize(code), None),
        };
        let my_addr = self.my_addr().map(|addr| serde_json::to_string(&addr).unwrap_or_default()).into_iter().collect::<Vec<_>>();
        let try_one = |addr: EndpointAddr| {
            let words = words.clone();
            let name = name.to_owned();
            let my_addr = my_addr.clone();
            let endpoint = endpoint.clone();
            let me = self.clone();
            async move {
                let connection = tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, pairing::ALPN)).await.map_err(|_| "no answer".to_owned())?.map_err(|e| e.to_string())?;
                let (send, recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
                let mut stream = tokio::io::join(recv, send);
                let entries = pairing::join(&mut stream, &words, &me.me, &name, my_addr).await?;
                let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
                Ok::<Vec<Entry>, String>(entries)
            }
        };
        let entries = match ticket {
            Some(ticket) => {
                let addr = decode_ticket(&ticket)?;
                try_one(addr).await?
            }
            None => {
                let mdns = self.mdns.get().ok_or("local-network lookup is not available here; use the full invitation")?;
                let mut found = None;
                let mut tried = std::collections::HashSet::new();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
                let mut stream = mdns.subscribe().await;
                use futures_util::StreamExt;
                while found.is_none() {
                    let next = tokio::time::timeout_at(deadline, stream.next()).await;
                    let Ok(Some(event)) = next else { break };
                    let iroh_mdns_address_lookup::DiscoveryEvent::Discovered { endpoint_info, .. } = event else { continue };
                    let addr: EndpointAddr = endpoint_info.into();
                    if addr.id == endpoint.id() || !tried.insert(addr.id) {
                        continue;
                    }
                    match try_one(addr).await {
                        Ok(entries) => found = Some(entries),
                        Err(reason) => tracing::debug!(%reason, "a nearby node did not take the code"),
                    }
                }
                found.ok_or("no computer nearby accepted the code (is the code shown there, and are both on the same network?)")?
            }
        };
        self.adopt_log(entries)?;
        self.dial_all().await;
        Ok(())
    }
}

/// A private, link-local or loopback address: the other computer is on this network.
fn is_local_network(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private() || v4.is_link_local()),
    }
}

/// An invitation's ticket: this node's address, base32 of its JSON.
pub fn encode_ticket(addr: &EndpointAddr) -> String {
    let json = serde_json::to_vec(addr).unwrap_or_default();
    data_encoding::BASE32_NOPAD.encode(&json).to_lowercase()
}

pub fn decode_ticket(ticket: &str) -> Result<EndpointAddr, String> {
    let bytes = data_encoding::BASE32_NOPAD.decode(ticket.trim().to_uppercase().as_bytes()).map_err(|_| "the invitation is not readable".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|_| "the invitation is not readable".to_owned())
}

/// A received event for the events subscribers (called from the plugin's `event`).
pub fn event_matches(prefixes: &[String], topic: &str) -> bool {
    prefixes.iter().any(|prefix| topic.starts_with(prefix.as_str()))
}

pub fn changed(what: &str) -> Value {
    json!({"what": what, "ts_ms": now_ms()})
}

pub const CHANGED: &str = topics::CHANGED;

fn lock_config(mesh: &Mesh) -> crate::service::Config {
    mesh.config.lock().unwrap_or_else(|p| p.into_inner()).clone()
}
