//! HTTP and WebSocket API on 127.0.0.1.
//!
//! - `GET /health` (open)
//! - `GET /ws?token=` - one JSON envelope per text message, the same protocol plugins speak
//! - `POST /api/rpc` (token) - `{method, params}` -> `{ok, result | error, items}` for scripts
//! - `/ui/` - the window (the `window@1` UI package a plugin provides), `/ui-plugins/<id>/<hash>/<file>` - UI plugins

use crate::hub::{FRAME_VERSION, Frame, PeerKind};
use axum::Router;
use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message as WsMessage, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use cc_protocol::codec::{read_message, write_message};
use cc_protocol::{Connection, Message, NoHandler, RpcError, codes};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
struct AppState {
    frame: Arc<Frame>,
    started: Instant,
}

pub fn router(frame: Arc<Frame>) -> Router {
    let state = AppState { frame, started: Instant::now() };
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/ui/") }))
        .route("/health", get(health))
        .route("/ws", get(ws_upgrade))
        .route("/api/rpc", post(api_rpc))
        .route("/ui-plugins/{id}/{hash}/{*file}", get(ui_plugin_file))
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(|State(state): State<AppState>| async move { serve_window(&state, "").await }))
        .route("/ui/{*file}", get(|State(state): State<AppState>, UrlPath(file): UrlPath<String>| async move { serve_window(&state, &file).await }))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({
        "ok": true,
        "version": FRAME_VERSION,
        "pid": std::process::id(),
        "uptime_ms": state.started.elapsed().as_millis() as u64,
        "headless": state.frame.headless,
        "ui_running": state.frame.shell.pid() != 0,
    }))
}

/// Token from `?token=`, `Authorization: Bearer`, `X-Cortex-Token` or the `cortex_token` cookie.
fn authorized(
    frame: &Frame,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Option<Option<Arc<crate::access::CommandScope>>> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(token) = query.get("token") {
        candidates.push(token.clone());
    }
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(token) = value.strip_prefix("Bearer ")
    {
        candidates.push(token.trim().to_owned());
    }
    if let Some(value) = headers.get("x-cortex-token").and_then(|v| v.to_str().ok()) {
        candidates.push(value.trim().to_owned());
    }
    if let Some(cookies) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for cookie in cookies.split(';') {
            if let Some(token) = cookie.trim().strip_prefix("cortex_token=") {
                candidates.push(token.to_owned());
            }
        }
    }
    if candidates
        .iter()
        .any(|candidate| constant_time_eq(candidate.as_bytes(), frame.token.as_bytes()))
    {
        Some(None)
    } else {
        candidates
            .iter()
            .find_map(|candidate| frame.command_access.find(candidate))
            .map(Some)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({"ok": false, "error": {"code": "unauthorized", "message": "a valid token is required"}})))
        .into_response()
}

async fn ws_upgrade(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // The token is checked before the upgrade request itself is validated.
    let Some(scope) = authorized(&state.frame, &query, &headers) else {
        return unauthorized();
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let frame = state.frame.clone();
    upgrade
        .max_message_size(cc_protocol::codec::MAX_FRAME_BYTES)
        .max_frame_size(cc_protocol::codec::MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve_ws(frame, socket, scope))
}

/// Bridge a WebSocket to a frame peer: text messages are envelopes; internally they travel over an
/// in-memory duplex stream with the protocol's framing, so WebSocket clients get exactly the
/// connection semantics plugins get (ids, stream items, cancellation, events).
async fn serve_ws(
    frame: Arc<Frame>,
    socket: WebSocket,
    scope: Option<Arc<crate::access::CommandScope>>,
) {
    let (frame_side, adapter) = tokio::io::duplex(1 << 20);
    let peer = frame.attach_scoped(frame_side, PeerKind::Client, scope);
    tracing::debug!(peer = peer.id, "websocket client connected");
    let (mut adapter_read, mut adapter_write) = tokio::io::split(adapter);
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let closing = matches!(message, WsMessage::Close(_));
            if ws_tx.send(message).await.is_err() || closing {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });
    let to_client = out_tx.clone();
    let pump = tokio::spawn(async move {
        while let Ok(Some(message)) = read_message(&mut adapter_read).await {
            match serde_json::to_string(&message) {
                Ok(text) => {
                    if to_client.send(WsMessage::Text(Utf8Bytes::from(text))).is_err() {
                        break;
                    }
                }
                Err(error) => tracing::error!(%error, "cannot encode an outgoing message"),
            }
        }
    });
    while let Some(incoming) = ws_rx.next().await {
        let text = match incoming {
            Ok(WsMessage::Text(text)) => text,
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(WsMessage::Binary(_)) => {
                let _ = out_tx.send(close_frame(1003, "binary messages are not supported; send JSON text"));
                break;
            }
            Ok(_) => continue,
        };
        match serde_json::from_str::<Message>(text.as_str()) {
            Ok(message) => {
                if write_message(&mut adapter_write, &message).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                // Answer a malformed request with an error when it carries an id; otherwise close.
                let id = serde_json::from_str::<Value>(text.as_str()).ok().and_then(|v| v.get("id").and_then(Value::as_u64));
                match id {
                    Some(id) => {
                        let reply = Message::Res {
                            id,
                            ok: false,
                            result: None,
                            error: Some(RpcError::new(codes::INVALID_PARAMS, format!("malformed envelope: {error}"))),
                        };
                        if let Ok(text) = serde_json::to_string(&reply) {
                            let _ = out_tx.send(WsMessage::Text(Utf8Bytes::from(text)));
                        }
                    }
                    None => {
                        let _ = out_tx.send(close_frame(1007, "malformed envelope"));
                        break;
                    }
                }
            }
        }
    }
    drop(adapter_write);
    pump.abort();
    let _ = pump.await;
    drop(out_tx);
    let _ = writer.await;
    tracing::debug!(peer = peer.id, "websocket client disconnected");
}

fn close_frame(code: u16, reason: &str) -> WsMessage {
    WsMessage::Close(Some(axum::extract::ws::CloseFrame { code, reason: Utf8Bytes::from(reason.to_owned()) }))
}

#[derive(Deserialize)]
struct RpcBody {
    method: String,
    #[serde(default)]
    params: Value,
}

/// One call over HTTP, through an ephemeral in-memory peer (same routing as a WebSocket client).
async fn api_rpc(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(scope) = authorized(&state.frame, &query, &headers) else {
        return unauthorized();
    };
    let request: RpcBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"ok": false, "error": {"code": "invalid_params", "message": error.to_string()}})))
                .into_response();
        }
    };
    let (frame_side, client_side) = tokio::io::duplex(1 << 20);
    let _peer = state
        .frame
        .attach_scoped(frame_side, PeerKind::Client, scope);
    let client = Connection::spawn(client_side, Arc::new(NoHandler));
    let mut stream = match client.call_stream(&request.method, request.params) {
        Ok(stream) => stream,
        Err(error) => {
            client.close();
            return axum::Json(json!({"ok": false, "error": error, "items": []})).into_response();
        }
    };
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    let body = match stream.finish().await {
        Ok(result) => json!({"ok": true, "result": result, "items": items}),
        Err(error) => json!({"ok": false, "error": error, "items": items}),
    };
    client.close();
    axum::Json(body).into_response()
}

/// A file of the window (the `window@1` package); a path that names no file is the page itself (the
/// window routes inside the page). Revalidated on every load: a new window version shows at once.
/// Until a plugin provides a window, a page that waits for it.
async fn serve_window(state: &AppState, file: &str) -> Response {
    let Some(window) = state.frame.ui_plugins.window() else {
        let mut response = Response::new(Body::from(WAITING_PAGE));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    };
    let named = crate::uiplugins::safe_relative(file).map(|relative| window.dir.join(relative)).filter(|path| path.is_file());
    let path = named.unwrap_or_else(|| window.dir.join("index.html"));
    let inside = std::fs::canonicalize(&path).ok().zip(std::fs::canonicalize(&window.dir).ok()).is_some_and(|(path, root)| path.starts_with(root));
    if !inside {
        return (StatusCode::NOT_FOUND, "no such window file").into_response();
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(crate::uiplugins::content_type(&path)));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            response
        }
        Err(error) => (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    }
}

/// What `/ui/` shows while no plugin provides the window (the workbench plugin is installing or
/// disabled). Every second it shows the updater's round (`update.status`: one row per plugin, with
/// its download's percent) as a bar, and loads the window as soon as `/ui/` serves it.
const WAITING_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>CortexCode</title><style>
body{margin:0;height:100vh;display:grid;place-items:center;background:#0d0f12;color:#9aa3ad;font:14px -apple-system,system-ui,sans-serif}
main{width:min(420px,80vw);text-align:center}
#bar{height:4px;margin:16px 0 10px;border-radius:2px;background:#22262d;overflow:hidden;visibility:hidden}
#fill{height:100%;width:0;background:#5b9dff;transition:width .4s}
#detail{min-height:1.4em;font-size:12px;color:#6b737d}
</style></head><body><main><div id="title">Waiting for the window (the Workbench plugin)</div>
<div id="bar"><div id="fill"></div></div><div id="detail"></div></main><script>
const token = new URLSearchParams(location.search).get('token') || '';
const finished = { done: 100, at_restart: 100, failed: 100, installing: 100 };
async function tick() {
  const page = await fetch(location.href, { method: 'HEAD', cache: 'no-store' }).catch(() => null);
  if (page && page.status !== 503) return location.reload();
  const answer = await fetch('/api/rpc?token=' + encodeURIComponent(token), {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ method: 'update.status', params: {} }),
  }).then((r) => r.json()).catch(() => null);
  const status = answer && answer.ok ? answer.result : null;
  const round = status && status.state !== 'idle' ? status.round || [] : [];
  document.getElementById('bar').style.visibility = round.length ? 'visible' : 'hidden';
  if (round.length) {
    const percent = round.reduce((sum, row) => sum + (finished[row.state] ?? row.percent ?? 0), 0) / round.length;
    const ready = round.filter((row) => finished[row.state] !== undefined).length;
    document.getElementById('title').textContent = `Updating the plugins (${ready} of ${round.length})`;
    document.getElementById('fill').style.width = percent.toFixed(1) + '%';
  }
  document.getElementById('detail').textContent = (status && status.detail) || '';
}
setInterval(tick, 1000);
tick();
</script></body></html>"#;

async fn ui_plugin_file(State(state): State<AppState>, UrlPath((id, hash, file)): UrlPath<(String, String, String)>) -> Response {
    let Some(path) = state.frame.ui_plugins.resolve(&id, &hash, &file) else {
        return (StatusCode::NOT_FOUND, "no such UI plugin file (the bundle may have been rebuilt; re-list UI plugins)").into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(crate::uiplugins::content_type(&path)));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
            response
        }
        Err(error) => (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    }
}
