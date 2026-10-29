//! The mesh plugin's state and its `mesh@1` methods.

use crate::borrow::Borrow;
use crate::identity::Identity;
use crate::members::{Entry, Log, log_path};
use crate::pairing::JoinerRecord;
use cc_protocol::contracts::mesh::{
    CallParams, CapacityParams, Card, Changed, ClaimParams, LendParams, Member, MembersResult, MeshStatus, NameParams, NodeParams, PairCode, PairJoinParams, Peer, PeersResult, PlaceParams, methods,
    topics,
};
use cc_protocol::contracts::model::ModelInfo;
use cc_protocol::contracts::now_ms;
use cc_protocol::{Ctx, ItemSink, RpcError, codes, parse_params, to_value};
use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::Router;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const PROVIDES: &[&str] = &[
    methods::STATUS,
    methods::PAIR_START,
    methods::PAIR_JOIN,
    methods::PAIR_CANCEL,
    methods::MEMBERS,
    methods::REVOKE,
    methods::LEAVE,
    methods::CARD,
    methods::PEERS,
    methods::CALL,
    methods::NAME,
    methods::LEND,
    methods::PLACE,
    methods::PLACE_END,
    methods::CAPACITY,
    methods::PROJECTS,
    methods::LINK,
    methods::CLAIM,
    methods::RELEASE,
];
pub const SUBSCRIBES: &[&str] = &["model.", "plugin.state", "plugin.swapped", "session.changed", "project.", "memory.knowledge.changed"];

/// An invitation lives this long; it is used once.
const PAIRING_WINDOW: Duration = Duration::from_secs(3600);
/// Wrong codes before the window closes by itself.
const PAIRING_ATTEMPTS: u32 = 3;
/// How long a card is reused before it is built again.
const CARD_TTL_MS: u64 = 20_000;
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub relays: Vec<String>,
}

impl Config {
    pub fn from_value(value: &Value) -> Config {
        Config { relays: value.get("relays").and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() }
    }
}

struct Pending {
    code: String,
    expires_ms: u64,
    invitation: String,
    attempts: u32,
}

struct EventSub {
    node: String,
    prefixes: Vec<String>,
    tx: mpsc::UnboundedSender<(String, Value)>,
}

impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh").field("node", &self.me.hex()).finish()
    }
}

pub struct Mesh {
    pub home: PathBuf,
    pub me: Identity,
    pub config: Mutex<Config>,
    name: Mutex<String>,
    log: Mutex<Log>,
    pub cards: Mutex<HashMap<String, Card>>,
    my_card: Mutex<Option<Card>>,
    conns: Mutex<HashMap<String, Connection>>,
    pub endpoint: OnceLock<Endpoint>,
    pub router: OnceLock<Router>,
    pub mdns: OnceLock<MdnsAddressLookup>,
    pub ctx: OnceLock<Ctx>,
    pairing: Mutex<Option<Pending>>,
    pub borrows: Mutex<HashMap<String, Borrow>>,
    event_subs: Mutex<Vec<EventSub>>,
    pub placements: crate::place::Placements,
    pub links: crate::projects::Links,
    pub claims: crate::claims::Claims,
}

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn computer_name() -> String {
    if let Ok(output) = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|name| name.trim().to_owned()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this computer".to_owned())
}

impl Mesh {
    pub fn new(home: &Path) -> Mesh {
        let me = Identity::load_or_create(home);
        let log = Log::load(&log_path(home));
        let name = std::fs::read_to_string(home.join("mesh").join("name")).map(|text| text.trim().to_owned()).ok().filter(|name| !name.is_empty()).unwrap_or_else(computer_name);
        Mesh {
            home: home.to_path_buf(),
            me,
            config: Mutex::new(Config::default()),
            name: Mutex::new(name),
            log: Mutex::new(log),
            cards: Mutex::new(HashMap::new()),
            my_card: Mutex::new(None),
            conns: Mutex::new(HashMap::new()),
            endpoint: OnceLock::new(),
            router: OnceLock::new(),
            mdns: OnceLock::new(),
            ctx: OnceLock::new(),
            pairing: Mutex::new(None),
            borrows: Mutex::new(HashMap::new()),
            event_subs: Mutex::new(Vec::new()),
            placements: crate::place::Placements::default(),
            links: crate::projects::Links::default(),
            claims: crate::claims::Claims::default(),
        }
    }

    pub fn attach(&self, ctx: &Ctx) {
        if self.ctx.set(ctx.clone()).is_ok() {
            *lock(&self.config) = Config::from_value(&ctx.config());
        }
    }

    pub fn ctx(&self) -> Result<Ctx, RpcError> {
        self.ctx.get().cloned().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "not connected to the frame yet"))
    }

    pub fn notify(&self, what: &str) {
        if let Some(ctx) = self.ctx.get() {
            ctx.publish(topics::CHANGED, serde_json::to_value(Changed { what: what.to_owned() }).unwrap_or(Value::Null));
        }
    }

    pub fn name(&self) -> String {
        lock(&self.name).clone()
    }

    // --- membership ----------------------------------------------------------------------------

    pub fn members(&self) -> BTreeMap<String, Member> {
        lock(&self.log).members()
    }

    pub fn member(&self, node: &str) -> Option<Member> {
        lock(&self.log).member(node)
    }

    /// A member's full id from a prefix (`cx` shows the first ten characters); the id itself when
    /// it is one. None when no member or more than one matches.
    pub fn resolve_node(&self, prefix: &str) -> Option<String> {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return None;
        }
        let members = self.members();
        if members.contains_key(prefix) {
            return Some(prefix.to_owned());
        }
        let mut found = members.keys().filter(|node| node.starts_with(prefix));
        let first = found.next()?.clone();
        found.next().is_none().then_some(first)
    }

    pub fn is_member(&self, node: &str) -> bool {
        lock(&self.log).is_member(node)
    }

    pub fn log_entries(&self) -> Vec<Entry> {
        lock(&self.log).entries.clone()
    }

    fn save_log(&self) {
        lock(&self.log).save(&log_path(&self.home));
    }

    /// This node founds its own cortex (the first member, with every grant) when it has none.
    fn ensure_founded(&self) {
        let mut log = lock(&self.log);
        if !log.founded() {
            let addrs = self.my_addr().map(|addr| serde_json::to_string(&addr).unwrap_or_default()).into_iter().collect();
            log.append(&self.me, "add", &self.me.hex(), &self.name(), addrs);
            log.save(&log_path(&self.home));
        }
    }

    /// The inviter's admission of a joiner: an `add` entry signed here, the whole log back.
    pub fn admit(&self, record: &JoinerRecord) -> Result<Vec<Entry>, String> {
        self.ensure_founded();
        let mut log = lock(&self.log);
        if !log.is_member(&self.me.hex()) {
            return Err("this node is not a member of its own cortex".to_owned());
        }
        if !log.is_member(&record.node) {
            log.append(&self.me, "add", &record.node, &record.name, record.addrs.clone());
            log.save(&log_path(&self.home));
        }
        let entries = log.entries.clone();
        drop(log);
        self.notify("members");
        Ok(entries)
    }

    /// A joiner takes the inviter's log as its own cortex.
    pub fn adopt_log(&self, entries: Vec<Entry>) -> Result<(), String> {
        let mut candidate = Log { entries };
        candidate.validate()?;
        if !candidate.is_member(&self.me.hex()) {
            return Err("the other side did not admit this node".to_owned());
        }
        {
            let mut log = lock(&self.log);
            let alone = log.members().len() <= 1;
            if log.founded() && !alone && log.entries.first() != candidate.entries.first() {
                return Err("this computer is already in a cortex with other members; revoke it there first".to_owned());
            }
            *log = candidate;
            log.save(&log_path(&self.home));
        }
        self.notify("members");
        Ok(())
    }

    /// The network this node is on, as the log stands: the founder's name and the first entry's
    /// hash as its id; none while alone.
    pub fn network(&self) -> Option<cc_protocol::contracts::mesh::Network> {
        let log = lock(&self.log);
        let first = log.entries.first()?;
        let members = log.members();
        if members.len() < 2 {
            return None;
        }
        Some(cc_protocol::contracts::mesh::Network { id: first.hash()[..10].to_owned(), founder: first.name.clone(), founded_ms: first.ts_ms, members: members.len() as u32 })
    }

    /// Forget the network: the log goes, every connection closes, nothing borrowed or placed
    /// stays. The node is alone again and can be invited into another network.
    fn forget_network(&self) {
        {
            let mut log = lock(&self.log);
            *log = Log::default();
            let _ = std::fs::remove_file(log_path(&self.home));
        }
        let conns: Vec<(String, Connection)> = lock(&self.conns).drain().collect();
        for (_, connection) in conns {
            connection.close(4u32.into(), b"left");
        }
        lock(&self.event_subs).clear();
        lock(&self.cards).clear();
        let borrowed: Vec<String> = lock(&self.borrows).keys().cloned().collect();
        for category in borrowed {
            self.stop_borrow(&category);
        }
        let placed: Vec<String> = lock(&self.placements.placed).values().cloned().collect();
        for node in placed {
            self.member_gone(&node);
        }
        self.notify("members");
    }

    /// This node leaves its network: a revoke of itself, told to the members, then nothing of
    /// the network kept here.
    pub async fn leave(&self) {
        if self.is_member(&self.me.hex()) && self.members().len() > 1 {
            let name = self.name();
            lock(&self.log).append(&self.me, "revoke", &self.me.hex(), &name, Vec::new());
            self.save_log();
            self.push_log().await;
        }
        self.forget_network();
    }

    /// A member's copy of the log arrived.
    pub fn log_received(&self, entries: Vec<Entry>) -> Result<bool, String> {
        let changed = lock(&self.log).merge(entries)?;
        if changed && !self.is_member(&self.me.hex()) {
            // Taken out by a member: this node is alone again.
            tracing::info!("this computer was removed from its network");
            self.forget_network();
            return Ok(true);
        }
        if changed {
            self.save_log();
            self.notify("members");
            // A revoked member loses its connection.
            let members = self.members();
            let gone: Vec<String> = lock(&self.conns).keys().filter(|node| !members.contains_key(*node)).cloned().collect();
            for node in gone {
                if let Some(connection) = lock(&self.conns).remove(&node) {
                    connection.close(3u32.into(), b"revoked");
                }
            }
        }
        Ok(changed)
    }

    // --- connections ---------------------------------------------------------------------------

    pub fn connection(&self, node: &str) -> Option<Connection> {
        lock(&self.conns).get(node).cloned()
    }

    pub fn connections(&self) -> HashMap<String, Connection> {
        lock(&self.conns).clone()
    }

    pub fn connected(&self, node: &str, connection: Connection) {
        lock(&self.conns).insert(node.to_owned(), connection);
        self.models_changed();
        self.notify("peers");
    }

    pub fn disconnected(&self, node: &str, connection: &Connection) {
        let mut conns = lock(&self.conns);
        if conns.get(node).is_some_and(|current| current.stable_id() == connection.stable_id()) {
            conns.remove(node);
            drop(conns);
            lock(&self.event_subs).retain(|sub| sub.node != node);
            self.member_gone(node);
            self.requester_gone(node);
            self.models_changed();
            self.notify("peers");
        }
    }

    // --- cards and events ----------------------------------------------------------------------

    pub fn card_received(&self, node: &str, card: Card) {
        lock(&self.cards).insert(node.to_owned(), card);
        self.models_changed();
        self.notify("cards");
    }

    /// The card is built again on its next use.
    pub fn forget_card(&self) {
        *lock(&self.my_card) = None;
    }

    pub async fn my_card(&self) -> Card {
        let cached = lock(&self.my_card).clone();
        if let Some(mut card) = cached
            && now_ms().saturating_sub(card.updated_ms) < CARD_TTL_MS
        {
            // The slow parts are reused; the model's load is what placement decides on, so it is
            // read live every time.
            if let (Some(ctx), Some(model)) = (self.ctx.get(), card.model.as_mut()) {
                crate::card::refresh_load(ctx, model).await;
            }
            if !self.lends() {
                card.model = None;
            }
            return card;
        }
        let card = match self.ctx.get() {
            Some(ctx) => crate::card::build(ctx, &self.me.hex(), &self.name(), &self.home).await,
            None => Card { node: self.me.hex(), name: self.name(), software: "cortexcode".to_owned(), version: env!("CARGO_PKG_VERSION").to_owned(), updated_ms: now_ms(), ..Card::default() },
        };
        let mut card = card;
        // A model this computer does not lend is not shown to the members.
        if !self.lends() {
            card.model = None;
        }
        *lock(&self.my_card) = Some(card.clone());
        card
    }

    pub fn subscribe_events(&self, node: &str, prefixes: Vec<String>, tx: mpsc::UnboundedSender<(String, Value)>) {
        lock(&self.event_subs).push(EventSub { node: node.to_owned(), prefixes, tx });
    }

    /// An event of this frame: to every member subscribed to its prefix.
    pub fn event(&self, topic: &str, data: &Value) {
        let mut subs = lock(&self.event_subs);
        subs.retain(|sub| !sub.tx.is_closed());
        for sub in subs.iter() {
            if crate::net::event_matches(&sub.prefixes, topic) {
                let _ = sub.tx.send((topic.to_owned(), data.clone()));
            }
        }
    }

    /// The methods a category's plugin serves here, from the contract the frame knows: what a
    /// borrower's hello says it provides.
    pub async fn provides(&self, category: &str) -> Vec<String> {
        use cc_protocol::contracts as c;
        let Some(ctx) = self.ctx.get() else { return Vec::new() };
        let listed: Value = ctx.call("frame.plugins.list", json!({})).await.unwrap_or(Value::Null);
        let active = listed
            .get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|plugins| plugins.iter().any(|plugin| plugin.get("category").and_then(Value::as_str) == Some(category) && plugin.get("state").and_then(Value::as_str) == Some("active")));
        if !active || !self.lends() {
            return Vec::new();
        }
        if category != "model" {
            return Vec::new();
        }
        [c::model::methods::INFO, c::model::methods::CHAT, c::model::methods::COMPLETE, c::model::methods::TOKENIZE, c::model::methods::COUNT, c::model::methods::SLOTS, c::model::methods::HIBERNATE, c::model::methods::HOLD, c::model::methods::PREWARM, c::model::methods::VARIANTS]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    // --- pairing -------------------------------------------------------------------------------

    pub fn pending_code(&self) -> Option<String> {
        let mut pairing = lock(&self.pairing);
        match pairing.as_ref() {
            Some(pending) if pending.expires_ms > now_ms() => Some(pending.code.clone()),
            Some(_) => {
                *pairing = None;
                None
            }
            None => None,
        }
    }

    pub fn pairing_done(&self) {
        *lock(&self.pairing) = None;
        self.notify("pairing");
    }

    pub fn pairing_attempt_failed(&self) {
        let mut pairing = lock(&self.pairing);
        if let Some(pending) = pairing.as_mut() {
            pending.attempts += 1;
            if pending.attempts >= PAIRING_ATTEMPTS {
                *pairing = None;
            }
        }
        drop(pairing);
        self.notify("pairing");
    }

    fn pair_start(&self) -> Result<PairCode, RpcError> {
        self.ensure_founded();
        let code = crate::words::code();
        let ticket = self.my_addr().map(|addr| crate::net::encode_ticket(&addr)).unwrap_or_default();
        let invitation = if ticket.is_empty() { code.clone() } else { format!("{code}@{ticket}") };
        let expires_ms = now_ms() + PAIRING_WINDOW.as_millis() as u64;
        *lock(&self.pairing) = Some(Pending { code: code.clone(), expires_ms, invitation: invitation.clone(), attempts: 0 });
        self.notify("pairing");
        Ok(PairCode { code, expires_ms, invitation })
    }

    // --- status --------------------------------------------------------------------------------

    fn status(&self) -> MeshStatus {
        let members = self.members();
        let conns = lock(&self.conns);
        let pairing = lock(&self.pairing).as_ref().filter(|pending| pending.expires_ms > now_ms()).map(|pending| PairCode { code: pending.code.clone(), expires_ms: pending.expires_ms, invitation: pending.invitation.clone() });
        let serving: Vec<(String, String)> = lock(&self.borrows).values().map(|borrow| (borrow.category.clone(), borrow.node.clone())).collect();
        let addrs: Vec<String> = self.my_addr().map(|addr| addr.addrs.iter().map(|a| format!("{a:?}")).collect()).unwrap_or_default();
        MeshStatus {
            node: self.me.hex(),
            name: self.name(),
            addrs,
            members: members.len() as u32,
            reachable: conns.keys().filter(|node| members.contains_key(*node)).count() as u32,
            network: self.network(),
            pairing,
            serving,
            placed: self.placed(),
            lend: self.lends(),
        }
    }

    /// Members may use this computer's model (off while `<home>/mesh/no-lend` exists).
    pub fn lends(&self) -> bool {
        !self.home.join("mesh").join("no-lend").exists()
    }

    fn peers(&self) -> PeersResult {
        let me = self.me.hex();
        let cards = lock(&self.cards);
        let conns = lock(&self.conns);
        let borrows = lock(&self.borrows);
        let peers = self
            .members()
            .into_values()
            .filter(|member| member.node != me)
            .map(|member| {
                let connection = conns.get(&member.node);
                let (path, rtt_ms) = connection.map(Self::path_of).unwrap_or_default();
                Peer {
                    card: cards.get(&member.node).cloned(),
                    reachable: connection.is_some(),
                    path,
                    rtt_ms,
                    serving: borrows.values().filter(|borrow| borrow.node == member.node).map(|borrow| borrow.category.clone()).collect(),
                    member,
                }
            })
            .collect();
        PeersResult { peers }
    }

    // --- dispatch ------------------------------------------------------------------------------

    pub async fn handle(self: &std::sync::Arc<Self>, method: &str, params: Value, items: ItemSink, cancel: CancellationToken) -> Result<Value, RpcError> {
        match method {
            methods::STATUS => to_value(&self.status()),
            methods::CARD => to_value(&self.my_card().await),
            methods::MEMBERS => to_value(&MembersResult { members: self.members().into_values().collect() }),
            methods::PEERS => to_value(&self.peers()),
            methods::PAIR_START => to_value(&self.pair_start()?),
            methods::PAIR_CANCEL => {
                *lock(&self.pairing) = None;
                self.notify("pairing");
                Ok(json!({}))
            }
            methods::PAIR_JOIN => {
                let params: PairJoinParams = parse_params(params)?;
                let name = params.name.filter(|name| !name.trim().is_empty()).unwrap_or_else(|| self.name());
                // One network at a time: joining another means leaving this one.
                if self.members().len() > 1 {
                    self.leave().await;
                }
                self.join(&params.code, &name).await.map_err(RpcError::invalid_params)?;
                let member = self.member(&self.me.hex()).ok_or_else(|| RpcError::internal("joined, but this node is not in the log"))?;
                to_value(&member)
            }
            methods::LEAVE => {
                self.leave().await;
                Ok(json!({}))
            }
            methods::REVOKE => {
                let mut params: NodeParams = parse_params(params)?;
                params.node = self.resolve_node(&params.node).ok_or_else(|| RpcError::not_found("no such member"))?;
                if params.node == self.me.hex() {
                    return Err(RpcError::invalid_params("this node cannot revoke itself; revoke it from another member"));
                }
                let member = self.member(&params.node).ok_or_else(|| RpcError::not_found("no such member"))?;
                lock(&self.log).append(&self.me, "revoke", &params.node, &member.name, Vec::new());
                self.save_log();
                let borrowed: Vec<String> = lock(&self.borrows).values().filter(|borrow| borrow.node == params.node).map(|borrow| borrow.category.clone()).collect();
                for category in borrowed {
                    self.stop_borrow(&category);
                }
                self.member_gone(&params.node);
                // Told before the connection closes, so the revoked node learns it.
                self.push_log().await;
                if let Some(connection) = lock(&self.conns).remove(&params.node) {
                    connection.close(3u32.into(), b"revoked");
                }
                self.notify("members");
                Ok(json!({}))
            }
            methods::NAME => {
                let params: NameParams = parse_params(params)?;
                let name = params.name.trim().to_owned();
                if name.is_empty() {
                    return Err(RpcError::invalid_params("a name is needed"));
                }
                *lock(&self.name) = name.clone();
                let _ = std::fs::create_dir_all(self.home.join("mesh"));
                let _ = std::fs::write(self.home.join("mesh").join("name"), &name);
                if self.is_member(&self.me.hex()) {
                    lock(&self.log).append(&self.me, "rename", &self.me.hex(), &name, Vec::new());
                    self.save_log();
                    self.push_log().await;
                }
                *lock(&self.my_card) = None;
                self.notify("members");
                Ok(json!({}))
            }
            methods::LEND => {
                let params: LendParams = parse_params(params)?;
                let marker = self.home.join("mesh").join("no-lend");
                let written = if params.lend {
                    std::fs::remove_file(&marker).or_else(|error| if error.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(error) })
                } else {
                    std::fs::create_dir_all(self.home.join("mesh")).and_then(|_| std::fs::write(&marker, ""))
                };
                written.map_err(|error| RpcError::internal(format!("cannot record the lending choice: {error}")))?;
                *lock(&self.my_card) = None;
                self.notify("members");
                to_value(&self.status())
            }
            methods::CALL => {
                let mut params: CallParams = parse_params(params)?;
                params.node = self.resolve_node(&params.node).ok_or_else(|| RpcError::not_found("no such member"))?;
                let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
                let pump = tokio::spawn(async move {
                    while let Some(item) = rx.recv().await {
                        items.send(item);
                    }
                });
                let outcome = self.call_member(&params.node, &params.method, params.params, Some(tx), cancel).await;
                let _ = pump.await;
                outcome
            }
            methods::PLACE => {
                let params: PlaceParams = parse_params(params)?;
                to_value(&self.place(params, cancel).await?)
            }
            methods::PLACE_END => {
                let params: PlaceParams = parse_params(params)?;
                self.claims_end(&params.session_id).await;
                self.place_end(&params.session_id).await;
                Ok(json!({}))
            }
            methods::CAPACITY => {
                let params: CapacityParams = parse_params(params)?;
                to_value(&self.capacity(params).await)
            }
            methods::PROJECTS => to_value(&self.projects().await),
            methods::CLAIM => {
                let params: ClaimParams = parse_params(params)?;
                self.start_granting();
                to_value(&self.claim(params, cancel).await?)
            }
            methods::RELEASE => {
                let params: ClaimParams = parse_params(params)?;
                self.release(&params.session_id).await;
                Ok(json!({}))
            }
            methods::LINK => {
                let mut params: NodeParams = parse_params(params)?;
                params.node = self.resolve_node(&params.node).ok_or_else(|| RpcError::not_found("no such member"))?;
                to_value(&self.link(&params.node).await?)
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    /// Replace this member's recorded addresses (its endpoint address and relay URL).
    pub fn append_addrs(&self, node: &str, addrs: Vec<String>) {
        let name = self.member(node).map(|member| member.name).unwrap_or_default();
        lock(&self.log).append(&self.me, "addrs", node, &name, addrs);
        self.save_log();
    }

    /// Automatic placement of the model (`docs/MESH-IMPLEMENTATION-PLAN.md` §6, §8): when this
    /// node has no model plugin installed and a reachable member lends one with a free slot, that
    /// member's model serves here; when the member goes away, the borrow stops. The best model with a free slot is taken: the strongest (the
    /// larger weights), then the nearest, then the fastest. Nothing to configure.
    /// (A node with a model of its own places single sessions instead: `place.rs`.)
    pub async fn auto_place(self: &std::sync::Arc<Self>) {
        let Some(ctx) = self.ctx.get().cloned() else { return };
        let borrowed = lock(&self.borrows).get("model").map(|borrow| borrow.node.clone());
        if let Some(node) = &borrowed {
            // A borrowed model whose lender is gone: back to this node's own.
            if self.connection(node).is_none() {
                tracing::info!(node = %&node[..node.len().min(10)], "the lender of the model is not reachable; this node's own model serves again");
                self.stop_borrow("model");
            }
            return;
        }
        // Only a node without a model plugin of its own borrows one as a whole; a node with one
        // keeps it, even while it downloads or loads, and sends single sessions to free members
        // (`place.rs`).
        let listed: Value = ctx.call("frame.plugins.list", json!({})).await.unwrap_or(Value::Null);
        let own_model = listed
            .get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|plugins| plugins.iter().any(|plugin| plugin.get("category").and_then(Value::as_str) == Some("model") && plugin.get("transport").and_then(Value::as_str) != Some("websocket")));
        if own_model {
            return;
        }
        let cards = lock(&self.cards).clone();
        let mut candidates = Vec::new();
        // The borrowed model serves every call here; coding is the one kind of work today.
        for mut candidate in self.candidates(None, cc_protocol::contracts::model::tags::CODING) {
            let Some(node) = candidate.node.clone() else { continue };
            let free = cards.get(&node).and_then(|card| card.model.as_ref()).is_some_and(|model| model.slots > model.slots_busy);
            let info = self.call_member(&node, "model.info", json!({}), None, CancellationToken::new()).await.ok().and_then(|value| serde_json::from_value::<ModelInfo>(value).ok());
            if let (true, Some(info)) = (free, info) {
                candidate.weights = info.file_bytes;
                candidates.push(candidate);
            }
        }
        candidates.sort_by(crate::place::best_first);
        let Some(node) = candidates.into_iter().next().and_then(|candidate| candidate.node) else { return };
        match self.start_borrow("model", &node).await {
            Ok(_) => tracing::info!(node = %&node[..node.len().min(10)], "no model of its own here: a member's model serves"),
            Err(error) => tracing::debug!(%error, "could not borrow a member's model"),
        }
    }

}
