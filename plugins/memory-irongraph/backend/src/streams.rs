//! Streams and work queues in the main project (`db`), on IronGraph's Kafka-like topics.
//!
//! ```text
//! topic `<name>`          memory.stream.*: one record per value, `{at_ms, value}` as JSON
//! topic `queue.<name>`    memory.queue.*: one record per message, the same envelope, kept 30 days
//! WORKSPACE layer, a queue's consumers and what they hold:
//! (:Queue {key: name, topic})-[:ON]->(:Machine)
//! (:QueueConsumer {key: '<queue>:<consumer>', queue, consumer, offset, updated_ms})-[:READS]->(:Queue)
//! (:QueueLease {key: '<queue>:<consumer>:<offset>', queue, consumer, offset, until_ms, attempt})-[:OF]->(:Queue)
//! ```
//!
//! **Queues.** IronGraph's own queues are AMQP queues: the embedded SDK can create them but not
//! consume them (consumers speak AMQP to a listener, which the embedded database does not open).
//! So a queue is a topic plus, per consumer, a committed offset kept in the graph, Kafka
//! consumer-group style. A consumer's `offset` is the offset after the last message handed out;
//! each message handed out and not acked yet is a `QueueLease` with its expiry and delivery count.
//! `take` hands out expired leases again first, then new messages from the offset; `ack` deletes
//! the leases. Delivery is at least once. A message id is its offset in the topic. Each consumer
//! name reads the whole queue (a consumer group); workers sharing a name share its messages.
//! Take and ack are serialised in this process, the database's only owner, which keeps each
//! consumer's state in memory after its first read and writes every change through.

use crate::db::{Graph, layer};
use crate::graphapi::GraphStore;
use crate::params;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{QueueMessage, StreamEntry, StreamReadResult, StreamTopic};
use cc_protocol::contracts::now_ms;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Records per append or publish.
pub const MAX_RECORDS: usize = 5000;
/// Bytes per record (its JSON).
const RECORD_BYTES: usize = 1 << 20;
/// Records a read returns when the caller names no limit, and at most.
const READ_RECORDS: u32 = 500;
const READ_RECORDS_MAX: u32 = 5000;
/// Bytes fetched per page.
const PAGE_BYTES: usize = 8 << 20;
/// Retention of a stream when the caller names none, and of every queue.
const RETENTION_DAYS: u32 = 30;
/// Messages a take hands out when the caller names no maximum, and at most.
const TAKE: u32 = 10;
const TAKE_MAX: u32 = 1000;
/// Lease when the caller names none, and its bounds.
const LEASE_MS: u64 = 60_000;
const LEASE_MIN_MS: u64 = 1_000;
const LEASE_MAX_MS: u64 = 24 * 60 * 60 * 1000;
/// Messages a consumer may hold unacked: past this, a take only hands out expired leases again.
const PENDING_MAX: usize = 10_000;
/// Topic prefixes the plugin writes itself.
const RESERVED_TOPICS: &[&str] = &["queue.", "session."];

/// One consumer's position in a queue.
#[derive(Debug, Clone, Default)]
struct Consumer {
    /// The offset after the last message handed out.
    offset: u64,
    /// Offset -> (lease expiry, deliveries) of messages handed out and not acked.
    pending: BTreeMap<u64, (u64, u32)>,
}

pub struct StreamStore {
    graph: Arc<Graph>,
    ig: String,
    store: Arc<GraphStore>,
    /// Topics this process created (or found).
    topics: Mutex<HashSet<String>>,
    /// Consumers read by this process, by `<queue>:<consumer>`; also serialises take and ack.
    consumers: Mutex<HashMap<String, Consumer>>,
    /// Queue nodes this process wrote.
    queues: Mutex<HashSet<String>>,
}

/// The envelope a record is stored in.
fn envelope(at_ms: u64, value: &Value) -> Result<Vec<u8>, RpcError> {
    let bytes = serde_json::to_vec(&json!({"at_ms": at_ms, "value": value})).map_err(|error| RpcError::internal(error.to_string()))?;
    if bytes.len() > RECORD_BYTES {
        return Err(RpcError::invalid_params(format!("a record of {} bytes; at most {RECORD_BYTES}", bytes.len())));
    }
    Ok(bytes)
}

/// A stored record's time and value. A record not written through the envelope (a journal
/// patch) is its own value, at the time the engine took it.
fn open_envelope(bytes: &[u8], engine_ms: u64) -> (u64, Value) {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(mut map)) if map.len() == 2 && map.get("at_ms").is_some_and(Value::is_u64) && map.contains_key("value") => {
            (map.get("at_ms").and_then(Value::as_u64).unwrap_or(engine_ms), map.remove("value").unwrap_or(Value::Null))
        }
        Ok(other) => (engine_ms, other),
        Err(_) => (engine_ms, Value::String(String::from_utf8_lossy(bytes).into_owned())),
    }
}

/// `[a-z0-9][a-z0-9._-]*`, at most `max` bytes.
fn validate_name(what: &str, name: &str, max: usize) -> Result<(), RpcError> {
    let ok = !name.is_empty()
        && name.len() <= max
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if ok { Ok(()) } else { Err(RpcError::invalid_params(format!("{what} `{name}`: lowercase letters, digits, `.`, `_` and `-`, starting with a letter or digit, at most {max} bytes"))) }
}

fn queue_topic(queue: &str) -> String {
    format!("queue.{queue}")
}

impl StreamStore {
    pub fn new(graph: Arc<Graph>, store: Arc<GraphStore>) -> Self {
        let ig = store.ig().to_owned();
        Self { graph, ig, store, topics: Mutex::new(HashSet::new()), consumers: Mutex::new(HashMap::new()), queues: Mutex::new(HashSet::new()) }
    }

    /// `memory.stream.append`. Returns the first offset written.
    pub async fn append(&self, topic: &str, records: &[Value], retention_days: Option<u32>) -> Result<u64, RpcError> {
        validate_name("topic", topic, 200)?;
        if let Some(prefix) = RESERVED_TOPICS.iter().find(|prefix| topic.starts_with(**prefix)) {
            return Err(RpcError::invalid_params(format!("topics starting `{prefix}` are the plugin's own (queues use memory.queue.publish)")));
        }
        let days = retention_days.unwrap_or(RETENTION_DAYS);
        if !(1..=3650).contains(&days) {
            return Err(RpcError::invalid_params(format!("retention_days {days}: 1 to 3650")));
        }
        self.write(topic, records, days).await
    }

    /// Append to a topic (created with `days` of retention when new). Returns the first offset.
    async fn write(&self, topic: &str, records: &[Value], days: u32) -> Result<u64, RpcError> {
        if records.len() > MAX_RECORDS {
            return Err(RpcError::invalid_params(format!("{} records in one call; at most {MAX_RECORDS}", records.len())));
        }
        let at = now_ms();
        let bytes = records.iter().map(|record| envelope(at, record)).collect::<Result<Vec<_>, _>>()?;
        if !self.topics.lock().await.contains(topic) {
            self.graph.create_topic_kept(&self.ig, topic, Some(days)).await?;
            self.topics.lock().await.insert(topic.to_owned());
        }
        let end = self.graph.stream_append(&self.ig, topic, bytes).await?;
        Ok(end.saturating_sub(records.len() as u64))
    }

    /// `memory.stream.read`: records from `from_offset` (the oldest kept when absent), those
    /// appended at or after `since_ms` (found by halving: records are in time order).
    pub async fn read(&self, topic: &str, from_offset: Option<u64>, since_ms: Option<u64>, limit: Option<u32>) -> Result<StreamReadResult, RpcError> {
        validate_name("topic", topic, 200)?;
        let limit = limit.unwrap_or(READ_RECORDS).clamp(1, READ_RECORDS_MAX) as usize;
        let (_, _, end) = self.graph.stream_page(&self.ig, topic, 0, 1, PAGE_BYTES).await?;
        let mut offset = from_offset.unwrap_or(0);
        if let Some(since) = since_ms {
            let (mut low, mut high) = (offset, end.max(offset));
            while low < high {
                let middle = low + (high - low) / 2;
                let (records, _, _) = self.graph.stream_page(&self.ig, topic, middle, 1, PAGE_BYTES).await?;
                match records.first().map(|(at, time, bytes)| (*at, open_envelope(bytes, *time).0)) {
                    // Retention removed what came before: the first record kept is the next one.
                    Some((at, time)) if time < since => low = at.max(middle) + 1,
                    Some(_) => high = middle,
                    None => low = middle + 1,
                }
            }
            offset = low;
        }
        let mut out = StreamReadResult { records: Vec::new(), next_offset: offset, end: false };
        'pages: while out.records.len() < limit && out.next_offset < end {
            let from = out.next_offset;
            let (records, next, _) = self.graph.stream_page(&self.ig, topic, from, (limit - out.records.len()).min(1000), PAGE_BYTES).await?;
            if records.is_empty() || next <= from {
                break;
            }
            for (at, time, bytes) in records {
                out.next_offset = at + 1;
                let (at_ms, value) = open_envelope(&bytes, time);
                if since_ms.is_none_or(|since| at_ms >= since) {
                    out.records.push(StreamEntry { offset: at, at_ms, value });
                }
                if out.records.len() == limit {
                    break 'pages;
                }
            }
            out.next_offset = next;
        }
        out.end = out.next_offset >= end;
        Ok(out)
    }

    /// `memory.stream.topics`.
    pub async fn topics(&self, prefix: &str) -> Result<Vec<StreamTopic>, RpcError> {
        let mut topics: Vec<StreamTopic> = self
            .graph
            .topics(&self.ig)
            .await?
            .into_iter()
            .filter(|(name, ..)| name.starts_with(prefix))
            .map(|(name, records, next_offset, retention_ms)| StreamTopic {
                name,
                records,
                next_offset,
                retention_days: retention_ms.map(|ms| (ms / 86_400_000) as u32),
            })
            .collect();
        topics.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(topics)
    }

    /// `memory.queue.publish`. Returns how many were added.
    pub async fn publish(&self, queue: &str, messages: &[Value]) -> Result<u32, RpcError> {
        validate_name("queue", queue, 190)?;
        self.write(&queue_topic(queue), messages, RETENTION_DAYS).await?;
        Ok(messages.len() as u32)
    }

    /// `memory.queue.take`: expired leases again, then new messages, oldest first.
    pub async fn take(&self, queue: &str, consumer: &str, max: Option<u32>, lease_ms: Option<u64>) -> Result<Vec<QueueMessage>, RpcError> {
        validate_name("queue", queue, 190)?;
        validate_consumer(consumer)?;
        let max = max.unwrap_or(TAKE).clamp(1, TAKE_MAX) as usize;
        let lease = lease_ms.unwrap_or(LEASE_MS).clamp(LEASE_MIN_MS, LEASE_MAX_MS);
        let topic = queue_topic(queue);
        let id = format!("{queue}:{consumer}");
        let mut consumers = self.consumers.lock().await;
        let mut state = match consumers.get(&id) {
            Some(state) => state.clone(),
            None => self.load(queue, consumer).await?,
        };
        let now = now_ms();
        let mut out = Vec::new();
        let mut gone = Vec::new();
        let expired: Vec<u64> = state.pending.iter().filter(|(_, (until, _))| *until <= now).map(|(offset, _)| *offset).take(max).collect();
        for offset in expired {
            let (records, _, _) = self.graph.stream_page(&self.ig, &topic, offset, 1, PAGE_BYTES).await?;
            match records.into_iter().next() {
                Some((at, time, bytes)) if at == offset => {
                    let entry = state.pending.entry(offset).or_insert((0, 0));
                    *entry = (now + lease, entry.1 + 1);
                    let (at_ms, value) = open_envelope(&bytes, time);
                    out.push(QueueMessage { id: offset, value, at_ms, attempt: entry.1 });
                }
                // Gone with the topic's retention: nothing left to deliver.
                _ => {
                    state.pending.remove(&offset);
                    gone.push(offset);
                }
            }
        }
        let start = state.offset;
        let room = max.saturating_sub(out.len()).min(PENDING_MAX.saturating_sub(state.pending.len()));
        if room > 0 {
            let (records, next, _) = self.graph.stream_page(&self.ig, &topic, state.offset, room, PAGE_BYTES).await?;
            let new = records.is_empty();
            for (at, time, bytes) in records {
                let (at_ms, value) = open_envelope(&bytes, time);
                state.pending.insert(at, (now + lease, 1));
                out.push(QueueMessage { id: at, value, at_ms, attempt: 1 });
                state.offset = at + 1;
            }
            // Retention may have removed messages never delivered: start after them.
            if new && next > state.offset {
                state.offset = next;
            }
        }
        let leased: Vec<u64> = out.iter().map(|message| message.id).collect();
        if !leased.is_empty() || !gone.is_empty() || state.offset != start || !consumers.contains_key(&id) {
            self.save(queue, consumer, &state, &leased, &gone).await?;
        }
        consumers.insert(id, state);
        out.sort_by_key(|message| message.id);
        Ok(out)
    }

    /// `memory.queue.ack`. Returns how many were pending.
    pub async fn ack(&self, queue: &str, consumer: &str, ids: &[u64]) -> Result<u32, RpcError> {
        validate_name("queue", queue, 190)?;
        validate_consumer(consumer)?;
        let id = format!("{queue}:{consumer}");
        let mut consumers = self.consumers.lock().await;
        let mut state = match consumers.get(&id) {
            Some(state) => state.clone(),
            None => self.load(queue, consumer).await?,
        };
        let mut acked: Vec<u64> = ids.iter().copied().filter(|offset| state.pending.remove(offset).is_some()).collect();
        acked.sort_unstable();
        if !acked.is_empty() {
            self.save(queue, consumer, &state, &[], &acked).await?;
        }
        consumers.insert(id, state);
        Ok(acked.len() as u32)
    }

    /// A consumer's stored offset and leases (a new consumer starts at the queue's beginning).
    async fn load(&self, queue: &str, consumer: &str) -> Result<Consumer, RpcError> {
        let rows = self
            .graph
            .run(&self.ig, &format!("{} MATCH (c:QueueConsumer {{key: $key}}) RETURN c.offset", layer::WORKSPACE_READ), params! { "key" => format!("{queue}:{consumer}") })
            .await?;
        let offset = rows.rows.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0);
        let rows = self
            .graph
            .run(
                &self.ig,
                &format!("{} MATCH (l:QueueLease)-[:OF]->(:Queue {{key: $queue}}) WHERE l.consumer = $consumer RETURN l.offset, l.until_ms, l.attempt", layer::WORKSPACE_READ),
                params! { "queue" => queue, "consumer" => consumer },
            )
            .await?;
        let number = |row: &[Value], at: usize| row.get(at).and_then(Value::as_u64).unwrap_or(0);
        let pending = rows.rows.iter().map(|row| (number(row, 0), (number(row, 1), number(row, 2) as u32))).collect();
        Ok(Consumer { offset, pending })
    }

    /// Write a consumer's offset, the leases in `leased` (as `state` holds them) and delete those
    /// in `released`.
    async fn save(&self, queue: &str, consumer: &str, state: &Consumer, leased: &[u64], released: &[u64]) -> Result<(), RpcError> {
        let write = |cypher: &str| format!("{} {cypher}", layer::WORKSPACE_WRITE);
        if !self.queues.lock().await.contains(queue) {
            for label in ["Queue", "QueueConsumer", "QueueLease"] {
                self.store.ensure_keyed(label).await?;
            }
            self.graph
                .run(
                    &self.ig,
                    &write("MATCH (m:Machine {key: $machine}) MERGE (q:Queue {key: $queue}) SET q.topic = $topic MERGE (q)-[:ON]->(m)"),
                    params! { "machine" => self.store.machine().key, "queue" => queue, "topic" => queue_topic(queue) },
                )
                .await?;
            self.queues.lock().await.insert(queue.to_owned());
        }
        let prefix = format!("{queue}:{consumer}");
        self.graph
            .run(
                &self.ig,
                &write(
                    "MATCH (q:Queue {key: $queue}) MERGE (c:QueueConsumer {key: $key}) \
                     SET c.queue = $queue, c.consumer = $consumer, c.offset = $offset, c.updated_ms = $now MERGE (c)-[:READS]->(q)",
                ),
                params! { "queue" => queue, "key" => prefix, "consumer" => consumer, "offset" => state.offset, "now" => now_ms() },
            )
            .await?;
        if !leased.is_empty() {
            let leases: Vec<Value> = leased
                .iter()
                .filter_map(|offset| state.pending.get(offset).map(|(until, attempt)| json!({"key": format!("{prefix}:{offset}"), "offset": offset, "until": until, "attempt": attempt})))
                .collect();
            self.graph
                .run(
                    &self.ig,
                    &write(
                        "MATCH (q:Queue {key: $queue}) UNWIND $leases AS l MERGE (x:QueueLease {key: l.key}) \
                         SET x.queue = $queue, x.consumer = $consumer, x.offset = l.offset, x.until_ms = l.until, x.attempt = l.attempt MERGE (x)-[:OF]->(q)",
                    ),
                    params! { "queue" => queue, "consumer" => consumer, "leases" => leases },
                )
                .await?;
        }
        if !released.is_empty() {
            let keys: Vec<String> = released.iter().map(|offset| format!("{prefix}:{offset}")).collect();
            self.graph.run(&self.ig, &write("UNWIND $keys AS k MATCH (x:QueueLease {key: k}) DETACH DELETE x"), params! { "keys" => keys }).await?;
        }
        Ok(())
    }
}

/// `[A-Za-z0-9._-]`, at most 100 bytes.
fn validate_consumer(consumer: &str) -> Result<(), RpcError> {
    let ok = !consumer.is_empty() && consumer.len() <= 100 && consumer.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok { Ok(()) } else { Err(RpcError::invalid_params(format!("consumer `{consumer}`: letters, digits, `.`, `_` and `-`, at most 100 bytes"))) }
}
