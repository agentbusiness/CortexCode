//! A bidirectional RPC connection over any async byte stream.
//!
//! Both ends can issue requests, answer requests (with optional stream items), publish events and
//! cancel open requests. An answer too large for one frame fails only its request. One writer
//! task serializes outgoing frames; one reader task dispatches
//! incoming frames. Incoming requests are handed to a [`Handler`] on their own task so a slow
//! request never blocks the connection.

use crate::codec::{CodecError, MAX_FRAME_BYTES, read_message, write_message};
use crate::envelope::{Message, RpcError};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Handles requests and events arriving on a connection.
pub trait Handler: Send + Sync + 'static {
    /// Answer one request. The handler must eventually call exactly one of
    /// [`Responder::ok`] / [`Responder::err`]; dropping the responder answers `internal`.
    fn handle(&self, request: IncomingRequest) -> BoxFuture<'static, ()>;

    /// An event published by the peer.
    fn event(&self, _topic: String, _data: Value) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    /// The connection closed (peer exit, crash or I/O error).
    fn closed(&self) {}
}

/// A request received from the peer.
pub struct IncomingRequest {
    pub id: u64,
    pub method: String,
    pub params: Value,
    pub responder: Responder,
    /// Triggered when the peer cancels this request or the connection closes.
    pub cancel: CancellationToken,
}

/// Sends stream items and the single final response for one incoming request.
pub struct Responder {
    id: u64,
    out: mpsc::UnboundedSender<Message>,
    open: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    done: bool,
}

/// A cloneable sender of stream items for one incoming request.
#[derive(Clone)]
pub struct ItemSink {
    id: u64,
    out: mpsc::UnboundedSender<Message>,
}

impl ItemSink {
    /// Send one stream item. Returns false when the connection is gone.
    pub fn send(&self, data: Value) -> bool {
        self.out.send(Message::Item { id: self.id, data }).is_ok()
    }

    /// A sink that discards everything (for calls made without a streaming peer).
    pub fn discard() -> Self {
        let (out, _rx) = mpsc::unbounded_channel();
        Self { id: 0, out }
    }
}

impl Responder {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// A cloneable item sender for this request.
    pub fn sink(&self) -> ItemSink {
        ItemSink { id: self.id, out: self.out.clone() }
    }

    /// Send one stream item. Returns false when the connection is gone.
    pub fn item(&self, data: Value) -> bool {
        self.out.send(Message::Item { id: self.id, data }).is_ok()
    }

    pub fn ok(mut self, result: Value) {
        self.finish(Message::Res { id: self.id, ok: true, result: Some(result), error: None });
    }

    pub fn err(mut self, error: RpcError) {
        self.finish(Message::Res { id: self.id, ok: false, result: None, error: Some(error) });
    }

    pub fn respond(self, outcome: Result<Value, RpcError>) {
        match outcome {
            Ok(value) => self.ok(value),
            Err(error) => self.err(error),
        }
    }

    fn finish(&mut self, message: Message) {
        if self.done {
            return;
        }
        self.done = true;
        if let Ok(mut open) = self.open.lock() {
            open.remove(&self.id);
        }
        let _ = self.out.send(message);
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if !self.done {
            let id = self.id;
            self.finish(Message::Res {
                id,
                ok: false,
                result: None,
                error: Some(RpcError::internal("handler dropped the request without answering")),
            });
        }
    }
}

enum Pending {
    Unary(oneshot::Sender<Result<Value, RpcError>>),
    Stream { items: mpsc::UnboundedSender<Value>, done: Option<oneshot::Sender<Result<Value, RpcError>>> },
}

struct Shared {
    out: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<u64, Pending>>,
    open: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    next_id: AtomicU64,
    closed: watch::Sender<bool>,
    /// Cancelled by [`Connection::close`].
    shutdown: CancellationToken,
}

/// A cheap, cloneable handle to a live connection.
#[derive(Clone)]
pub struct Connection {
    shared: Arc<Shared>,
}

/// An outgoing streaming call: read items with [`CallStream::next`], then the final [`CallStream::finish`].
pub struct CallStream {
    id: u64,
    items: mpsc::UnboundedReceiver<Value>,
    done: oneshot::Receiver<Result<Value, RpcError>>,
    conn: Connection,
}

impl CallStream {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The next stream item, or `None` once the request has completed.
    pub async fn next(&mut self) -> Option<Value> {
        self.items.recv().await
    }

    /// Wait for the final response (remaining items are discarded).
    pub async fn finish(mut self) -> Result<Value, RpcError> {
        self.items.close();
        (&mut self.done).await.unwrap_or_else(|_| Err(RpcError::closed()))
    }

    /// Ask the peer to cancel. The final response still arrives (usually `cancelled`).
    pub fn cancel(&self) {
        self.conn.cancel(self.id);
    }
}

impl Connection {
    /// Start reader and writer tasks for `stream`. Must be called inside a Tokio runtime.
    pub fn spawn<S>(stream: S, handler: Arc<dyn Handler>) -> Connection
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (closed_tx, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            out: out_tx,
            pending: Mutex::new(HashMap::new()),
            open: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: closed_tx,
            shutdown: CancellationToken::new(),
        });
        let conn = Connection { shared };
        tokio::spawn(write_loop(writer, out_rx, conn.clone()));
        tokio::spawn(read_loop(reader, conn.clone(), handler));
        conn
    }

    fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Close the connection: messages already queued are written, then both directions stop,
    /// pending outgoing calls fail with `connection_closed` and the handler's `closed` runs.
    pub fn close(&self) {
        self.shared.shutdown.cancel();
    }

    pub fn is_closed(&self) -> bool {
        *self.shared.closed.borrow()
    }

    /// Resolves when the connection has closed.
    pub async fn wait_closed(&self) {
        let mut rx = self.shared.closed.subscribe();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Issue a request and wait for its final response, ignoring stream items.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if self.is_closed() {
            return Err(RpcError::closed());
        }
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.lock_pending().insert(id, Pending::Unary(tx));
        if self.shared.out.send(Message::Req { id, method: method.to_owned(), params }).is_err() {
            self.lock_pending().remove(&id);
            return Err(RpcError::closed());
        }
        rx.await.unwrap_or_else(|_| Err(RpcError::closed()))
    }

    /// [`Connection::call`] with a deadline; on expiry the request is cancelled and `timeout` returned.
    pub async fn call_timeout(&self, method: &str, params: Value, limit: Duration) -> Result<Value, RpcError> {
        let mut stream = self.call_stream(method, params)?;
        let id = stream.id();
        match tokio::time::timeout(limit, async {
            while stream.next().await.is_some() {}
            stream.finish().await
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                self.cancel(id);
                self.lock_pending().remove(&id);
                Err(RpcError::new(crate::codes::TIMEOUT, format!("`{method}` exceeded {limit:?}")).retryable())
            }
        }
    }

    /// Issue a request whose stream items should be delivered.
    pub fn call_stream(&self, method: &str, params: Value) -> Result<CallStream, RpcError> {
        if self.is_closed() {
            return Err(RpcError::closed());
        }
        let id = self.next_id();
        let (items_tx, items_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        self.lock_pending().insert(id, Pending::Stream { items: items_tx, done: Some(done_tx) });
        if self.shared.out.send(Message::Req { id, method: method.to_owned(), params }).is_err() {
            self.lock_pending().remove(&id);
            return Err(RpcError::closed());
        }
        Ok(CallStream { id, items: items_rx, done: done_rx, conn: self.clone() })
    }

    /// Publish an event to the peer.
    pub fn publish(&self, topic: &str, data: Value) -> bool {
        self.shared.out.send(Message::Evt { topic: topic.to_owned(), data }).is_ok()
    }

    /// Cancel one of our open outgoing requests.
    pub fn cancel(&self, id: u64) {
        let _ = self.shared.out.send(Message::Cancel { id });
    }

    /// Send a raw message (used by the frame when relaying).
    pub fn send_raw(&self, message: Message) -> bool {
        self.shared.out.send(message).is_ok()
    }

    /// Number of incoming requests still being handled.
    pub fn open_incoming(&self) -> usize {
        self.shared.open.lock().map(|open| open.len()).unwrap_or(0)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        match self.shared.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn mark_closed(&self) {
        // send_replace: `send` would not store the value while nobody is subscribed.
        self.shared.closed.send_replace(true);
        let drained: Vec<Pending> = self.lock_pending().drain().map(|(_, pending)| pending).collect();
        for pending in drained {
            match pending {
                Pending::Unary(tx) => {
                    let _ = tx.send(Err(RpcError::closed()));
                }
                Pending::Stream { done: Some(done), .. } => {
                    let _ = done.send(Err(RpcError::closed()));
                }
                Pending::Stream { done: None, .. } => {}
            }
        }
        if let Ok(mut open) = self.shared.open.lock() {
            for (_, token) in open.drain() {
                token.cancel();
            }
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Message>, conn: Connection) {
    // The loop also ends when the peer is gone (read side closed) or on `close()`; otherwise the
    // task and the stream's write half would live as long as any `Connection` clone.
    let mut closed = conn.shared.closed.subscribe();
    let shutdown = conn.shared.shutdown.clone();
    loop {
        tokio::select! {
            // Queued messages first, so a final response queued before `close()` is delivered.
            biased;
            message = rx.recv() => match message {
                Some(message) => match write_message(&mut writer, &message).await {
                    Ok(()) => {}
                    // An answer too large for one message fails that one request; the
                    // connection, and everything else on it, goes on.
                    Err(CodecError::TooLarge(bytes)) => {
                        tracing::warn!(bytes, "a message exceeds the frame limit; its request gets an error instead");
                        if let Message::Res { id, .. } = &message {
                            let error = RpcError::internal(format!(
                                "the answer is {} MB; at most {} MB fits in one message. Ask for a smaller part: one file, one page.",
                                bytes / (1024 * 1024),
                                MAX_FRAME_BYTES / (1024 * 1024)
                            ));
                            let reply = Message::Res { id: *id, ok: false, result: None, error: Some(error) };
                            if let Err(error) = write_message(&mut writer, &reply).await {
                                tracing::debug!(%error, "connection write failed");
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "connection write failed");
                        break;
                    }
                },
                None => break,
            },
            _ = shutdown.cancelled() => break,
            _ = async { closed.wait_for(|closed| *closed).await.is_ok() } => break,
        }
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    conn.mark_closed();
}

async fn read_loop<R: AsyncRead + Unpin>(mut reader: R, conn: Connection, handler: Arc<dyn Handler>) {
    // Events are delivered to the handler one at a time, in arrival order (journal patches and
    // other streams depend on it). A slow event handler delays later events, never requests.
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<BoxFuture<'static, ()>>();
    tokio::spawn(async move {
        while let Some(delivery) = events_rx.recv().await {
            delivery.await;
        }
    });
    let shutdown = conn.shared.shutdown.clone();
    loop {
        let read = tokio::select! {
            read = read_message(&mut reader) => read,
            _ = shutdown.cancelled() => break,
        };
        let message = match read {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "connection read failed");
                break;
            }
        };
        match message {
            Message::Req { id, method, params } => {
                let cancel = CancellationToken::new();
                if let Ok(mut open) = conn.shared.open.lock() {
                    open.insert(id, cancel.clone());
                }
                let responder =
                    Responder { id, out: conn.shared.out.clone(), open: conn.shared.open.clone(), done: false };
                let request = IncomingRequest { id, method, params, responder, cancel };
                let future = handler.handle(request);
                tokio::spawn(future);
            }
            Message::Res { id, ok, result, error } => {
                let outcome = if ok {
                    Ok(result.unwrap_or(Value::Null))
                } else {
                    Err(error.unwrap_or_else(|| RpcError::internal("error response without error body")))
                };
                if let Some(pending) = conn.lock_pending().remove(&id) {
                    match pending {
                        Pending::Unary(tx) => {
                            let _ = tx.send(outcome);
                        }
                        Pending::Stream { done: Some(done), .. } => {
                            let _ = done.send(outcome);
                        }
                        Pending::Stream { done: None, .. } => {}
                    }
                }
            }
            Message::Item { id, data } => {
                if let Some(Pending::Stream { items, .. }) = conn.lock_pending().get(&id) {
                    let _ = items.send(data);
                }
            }
            Message::Evt { topic, data } => {
                let _ = events_tx.send(handler.event(topic, data));
            }
            Message::Cancel { id } => {
                let token = conn.shared.open.lock().ok().and_then(|open| open.get(&id).cloned());
                if let Some(token) = token {
                    token.cancel();
                }
            }
        }
    }
    conn.mark_closed();
    handler.closed();
}

/// A handler built from closures, convenient for tests and small peers.
pub struct FnHandler<F>
where
    F: Fn(IncomingRequest) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    f: F,
}

impl<F> FnHandler<F>
where
    F: Fn(IncomingRequest) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    pub fn new(f: F) -> Arc<Self> {
        Arc::new(Self { f })
    }
}

impl<F> Handler for FnHandler<F>
where
    F: Fn(IncomingRequest) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    fn handle(&self, request: IncomingRequest) -> BoxFuture<'static, ()> {
        (self.f)(request)
    }
}

/// A handler that rejects every request (for peers that only call out).
pub struct NoHandler;

impl Handler for NoHandler {
    fn handle(&self, request: IncomingRequest) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let method = request.method.clone();
            request.responder.err(RpcError::unknown_method(&method));
        })
    }
}

#[cfg(all(test, feature = "preserve_order"))]
mod tests {
    use super::*;

    /// Answers every request with its params serialized back to text, as the peer received them.
    struct Echo;

    impl Handler for Echo {
        fn handle(&self, request: IncomingRequest) -> BoxFuture<'static, ()> {
            Box::pin(async move {
                let text = serde_json::to_string(&request.params).unwrap_or_default();
                request.responder.ok(Value::String(text));
            })
        }
    }

    /// Regression (frame contract request): with the `preserve_order` feature (which the frame
    /// enables), object keys keep the order the caller wrote them in through parse, framing and
    /// serialization, so a relayed JSON schema reaches its plugin with its properties in their
    /// original order. Run with `cargo test --features preserve_order`.
    #[tokio::test]
    async fn params_keep_their_key_order_across_a_connection() {
        let written = r#"{"zeta":1,"alpha":{"properties":{"title":{"type":"string"},"answer":{"type":"string"},"because":{"type":"string"}},"required":["title","answer"]},"mid":[{"b":1,"a":2}]}"#;
        let params: Value = serde_json::from_str(written).expect("valid json");
        let (left, right) = tokio::io::duplex(1 << 16);
        let _server = Connection::spawn(left, Arc::new(Echo));
        let client = Connection::spawn(right, Arc::new(NoHandler));
        let received = client.call("echo.text", params).await.expect("call");
        assert_eq!(received, Value::String(written.to_owned()), "the peer saw the keys in the caller's order");
        // Negative control: a sorted rendering of the same object differs, so the check can fail.
        let sorted: std::collections::BTreeMap<String, Value> = serde_json::from_str(written).expect("map");
        assert_ne!(serde_json::to_string(&sorted).unwrap_or_default(), written);
    }
}
