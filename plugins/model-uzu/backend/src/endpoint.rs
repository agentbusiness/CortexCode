//! Local OpenAI-compatible HTTP endpoint (`model.endpoint`) for baseline harnesses (aider,
//! mini-swe-agent). It runs inside the plugin process on the same engine instance and goes through
//! the same scheduler and per-conversation cache as plugin traffic.
//!
//! Routes: `GET /v1/models`, `POST /v1/chat/completions` (streaming and non-streaming, tools,
//! `response_format` JSON schemas), `GET /health`. Optional extensions: header
//! `X-Cortex-Priority: foreground|agent|background` (default agent) and `X-Cortex-Cache-Key`
//! (default: derived from the conversation start). Thinking follows `chat_template_kwargs`
//! (`enable_thinking`, `reasoning_effort`) or the same top-level fields; on by default as in the
//! model's template. Message content may carry `image_url` parts (when the model takes images):
//! `data:` URLs (stored content-addressed under the plugin's data directory, `images/`) and local
//! file paths (`file://` or absolute); remote URLs are refused (the plugin runs offline).

use crate::parse::Piece;
use crate::runtime::Runtime;
use bytes::Bytes;
use base64::Engine as _;
use cc_protocol::contracts::model::{ChatMessage, ChatParams, ContentPart, Priority, Sampling, Thinking, ToolCall, ToolSpec};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, StatusCode};
use serde_json::{Map, Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type Body = BoxBody<Bytes, Infallible>;

pub struct Endpoint {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start(runtime: Arc<Runtime>, port: u16) -> Result<Endpoint, String> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).await.map_err(|error| format!("cannot bind 127.0.0.1:{port}: {error}"))?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let runtime = runtime.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let runtime = runtime.clone();
                    async move { Ok::<_, Infallible>(route(runtime, request).await) }
                });
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok(Endpoint { port, task })
}

fn full(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> hyper::Response<Body> {
    let mut response = hyper::Response::new(Full::new(body.into()).boxed());
    *response.status_mut() = status;
    if let Ok(value) = hyper::header::HeaderValue::from_str(content_type) {
        response.headers_mut().insert(hyper::header::CONTENT_TYPE, value);
    }
    response
}

fn error(status: StatusCode, message: &str) -> hyper::Response<Body> {
    let body = json!({"error": {"code": status.as_u16(), "message": message, "type": "invalid_request_error"}});
    full(status, "application/json", body.to_string())
}

async fn route(runtime: Arc<Runtime>, request: Request<Incoming>) -> hyper::Response<Body> {
    let path = request.uri().path().trim_end_matches('/').to_owned();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/health") => full(StatusCode::OK, "application/json", json!({"status": "ok"}).to_string()),
        (&Method::GET, "/v1/models") | (&Method::GET, "/models") => match runtime.ready().await {
            Ok(loaded) => {
                let body = json!({"object": "list", "data": [{"id": loaded.variant, "object": "model", "owned_by": "cortexcode", "created": 0, "meta": {"engine": crate::runtime::ENGINE_REVISION, "ctx": loaded.plan.ctx_total}}]});
                full(StatusCode::OK, "application/json", body.to_string())
            }
            Err(failure) => error(StatusCode::SERVICE_UNAVAILABLE, &failure.message),
        },
        (&Method::POST, "/v1/chat/completions") | (&Method::POST, "/chat/completions") => {
            chat_completions(runtime, request).await
        }
        _ => error(StatusCode::NOT_FOUND, "unknown route"),
    }
}

fn header(request: &Request<Incoming>, name: &str) -> Option<String> {
    request.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned)
}

/// An OpenAI message `content` (a string, or an array of text and `image_url` parts): its text
/// and, when it has images, the content parts in order (none for text-only content, which stays
/// exactly as before).
fn content(value: &Value, images_dir: Option<&Path>) -> Result<(String, Vec<ContentPart>), String> {
    match value {
        Value::Null => Ok((String::new(), Vec::new())),
        Value::String(text) => Ok((text.clone(), Vec::new())),
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") | None => {
                        parts.push(ContentPart::Text { text: item.get("text").and_then(Value::as_str).unwrap_or("").to_owned() })
                    }
                    Some("image_url") => {
                        let url = match &item["image_url"] {
                            Value::String(url) => url.as_str(),
                            other => other["url"].as_str().ok_or("an image_url part needs a url")?,
                        };
                        parts.push(image_part(url, images_dir)?);
                    }
                    Some(other) => return Err(format!("content part type `{other}` is not supported (text and image_url)")),
                }
            }
            let text: String =
                parts.iter().filter_map(|part| if let ContentPart::Text { text } = part { Some(text.as_str()) } else { None }).collect();
            if parts.iter().any(|part| matches!(part, ContentPart::Image { .. })) {
                Ok((text, parts))
            } else {
                Ok((text, Vec::new()))
            }
        }
        other => Err(format!("unsupported content {other}")),
    }
}

/// An image part from an OpenAI `image_url`: a `data:` URL (stored under `images_dir` by its
/// SHA-256) or a local file path.
fn image_part(url: &str, images_dir: Option<&Path>) -> Result<ContentPart, String> {
    let (bytes, path) = if let Some(rest) = url.strip_prefix("data:") {
        let (header, data) = rest.split_once(',').ok_or("a data: URL needs a comma")?;
        if !header.ends_with(";base64") || !header.starts_with("image/") {
            return Err("image data URLs must be base64 images (data:image/<type>;base64,...)".into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|error| format!("the image data URL is not valid base64: {error}"))?;
        (bytes, None)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        return Err("remote image URLs are not fetched (the model runs offline); send a data: URL or a local file path".into());
    } else {
        let path = crate::runtime::expand_home(url.strip_prefix("file://").unwrap_or(url));
        if !path.is_absolute() {
            return Err(format!("image path `{url}` must be absolute"));
        }
        let bytes = std::fs::read(&path).map_err(|error| format!("cannot read image {}: {error}", path.display()))?;
        (bytes, Some(path))
    };
    let format = image::guess_format(&bytes).map_err(|_| "unrecognized image format (PNG, JPEG, WebP, GIF or BMP expected)".to_owned())?;
    let (width, height) = crate::vision::preprocess::dimensions(&bytes)?;
    let sha256: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    let path = match path {
        Some(path) => path,
        None => {
            let dir = images_dir.ok_or("image data needs the plugin's data directory")?;
            std::fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            let extension = format.extensions_str().first().copied().unwrap_or("img");
            let path: PathBuf = dir.join(format!("{sha256}.{extension}"));
            if !path.exists() {
                let temporary = dir.join(format!("{sha256}.{}.tmp", std::process::id()));
                std::fs::write(&temporary, &bytes).map_err(|error| format!("cannot store the image: {error}"))?;
                std::fs::rename(&temporary, &path).map_err(|error| format!("cannot store the image: {error}"))?;
            }
            path
        }
    };
    Ok(ContentPart::Image {
        sha256,
        path: path.display().to_string(),
        mime: format.to_mime_type().to_owned(),
        width: Some(width),
        height: Some(height),
    })
}

/// An OpenAI request body as contract parameters (`images_dir` stores images sent as data URLs).
pub fn chat_params(
    body: &Value,
    cache_key: Option<String>,
    priority: Priority,
    images_dir: Option<&Path>,
) -> Result<(ChatParams, bool), String> {
    let messages = body["messages"].as_array().ok_or("messages must be an array")?;
    let messages = messages
        .iter()
        .map(|message| {
            let role = message["role"].as_str().ok_or("every message needs a role")?.to_owned();
            let (text, parts) = content(&message["content"], images_dir)?;
            let mut out = ChatMessage::new(&role, text);
            out.parts = parts;
            out.reasoning_content = message["reasoning_content"].as_str().map(str::to_owned);
            out.tool_call_id = message["tool_call_id"].as_str().map(str::to_owned);
            out.name = message["name"].as_str().map(str::to_owned);
            if let Some(calls) = message["tool_calls"].as_array() {
                out.tool_calls = calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call["id"].as_str().unwrap_or("").to_owned(),
                        name: call["function"]["name"].as_str().unwrap_or("").to_owned(),
                        arguments: match &call["function"]["arguments"] {
                            Value::String(text) => text.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        },
                    })
                    .collect();
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let tools = body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    let function = &tool["function"];
                    ToolSpec {
                        name: function["name"].as_str().unwrap_or("").to_owned(),
                        description: function["description"].as_str().unwrap_or("").to_owned(),
                        parameters: function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let kwargs = &body["chat_template_kwargs"];
    let enabled = kwargs["enable_thinking"].as_bool().or_else(|| body["enable_thinking"].as_bool()).unwrap_or(true);
    let effort = kwargs["reasoning_effort"].as_str().or_else(|| body["reasoning_effort"].as_str()).map(str::to_owned);
    let mut sampling = Sampling::default();
    if let Some(value) = body["temperature"].as_f64() {
        sampling.temperature = value as f32;
    }
    if let Some(value) = body["top_p"].as_f64() {
        sampling.top_p = value as f32;
    }
    if let Some(value) = body["top_k"].as_u64() {
        sampling.top_k = value as u32;
    }
    if let Some(value) = body["min_p"].as_f64() {
        sampling.min_p = value as f32;
    }
    if let Some(value) = body["presence_penalty"].as_f64() {
        sampling.presence_penalty = value as f32;
    }
    if let Some(value) = body["repetition_penalty"].as_f64().or_else(|| body["repeat_penalty"].as_f64()) {
        sampling.repeat_penalty = Some(value as f32);
    }
    sampling.seed = body["seed"].as_u64();
    let json_schema = match body["response_format"]["type"].as_str() {
        Some("json_schema") => Some(
            body["response_format"]["json_schema"]["schema"]
                .clone()
                .as_object()
                .map(|schema| Value::Object(schema.clone()))
                .ok_or("response_format.json_schema.schema must be an object")?,
        ),
        Some("json_object") => Some(json!({"type": "object"})),
        Some("text") | None => None,
        Some(other) => return Err(format!("unsupported response_format `{other}`")),
    };
    let stop = match &body["stop"] {
        Value::String(stop) => vec![stop.clone()],
        Value::Array(stops) => stops.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    let max_tokens = body["max_completion_tokens"].as_u64().or_else(|| body["max_tokens"].as_u64()).map(|value| value as u32);
    let stream = body["stream"].as_bool().unwrap_or(false);
    Ok((
        ChatParams {
            messages,
            tools,
            tool_choice: body.get("tool_choice").cloned().filter(|value| !value.is_null()),
            grammar: body["grammar"].as_str().map(str::to_owned),
            json_schema,
            sampling,
            thinking: Thinking { enabled, effort, budget_tokens: None },
            priority,
            cache_key,
            stop,
            max_tokens,
        },
        stream,
    ))
}

enum Out {
    Piece(Piece),
    Done(Result<cc_protocol::contracts::model::ChatResult, cc_protocol::RpcError>),
}

fn chunk(id: &str, created: u64, model: &str, delta: Value, finish: Option<&str>) -> String {
    let body = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    format!("data: {body}\n\n")
}

fn usage_json(usage: &cc_protocol::contracts::model::Usage) -> Value {
    json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.prompt_tokens + usage.completion_tokens,
        "prompt_tokens_details": {"cached_tokens": usage.cached_tokens},
    })
}

fn timings_json(usage: &cc_protocol::contracts::model::Usage) -> Value {
    json!({
        "cache_n": usage.cached_tokens,
        "prompt_n": usage.prompt_tokens - usage.cached_tokens,
        "prompt_ms": usage.prefill_ms,
        "predicted_n": usage.completion_tokens,
        "predicted_ms": usage.decode_ms,
        "queue_ms": usage.queue_ms,
        "draft_n": usage.draft_total,
        "draft_n_accepted": usage.draft_accepted,
    })
}

/// OpenAI has no "repeated": a stream the repetition guard ended reports `length`.
fn openai_finish(reason: &str) -> &str {
    if reason == "repeated" { "length" } else { reason }
}

async fn chat_completions(runtime: Arc<Runtime>, request: Request<Incoming>) -> hyper::Response<Body> {
    let priority = match header(&request, "x-cortex-priority").as_deref() {
        Some("foreground") => Priority::Foreground,
        Some("background") => Priority::Background,
        _ => Priority::Agent,
    };
    let cache_key = header(&request, "x-cortex-cache-key");
    let bytes = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(failure) => return error(StatusCode::BAD_REQUEST, &failure.to_string()),
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => return error(StatusCode::BAD_REQUEST, "the body must be a JSON object"),
    };
    let images_dir = runtime.paths.data_dir.join("images");
    let (params, stream) = match chat_params(&body, cache_key, priority, Some(&images_dir)) {
        Ok(parsed) => parsed,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let model = match runtime.ready().await {
        Ok(loaded) => loaded.variant.clone(),
        Err(failure) => return error(StatusCode::SERVICE_UNAVAILABLE, &failure.message),
    };
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = cc_protocol::contracts::now_ms() / 1000;
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    tokio::spawn(async move {
        let pieces = tx.clone();
        let mut sink = move |piece: &Piece| {
            let _ = pieces.send(Out::Piece(piece.clone()));
        };
        let result = runtime.chat_with(params, &mut sink, worker_cancel).await;
        let _ = tx.send(Out::Done(result));
    });
    let cancel_on_drop = CancelOnDrop(cancel);
    if !stream {
        let _guard = cancel_on_drop;
        while let Some(out) = rx.recv().await {
            if let Out::Done(result) = out {
                let result = match result {
                    Ok(result) => result,
                    Err(failure) => {
                        let status = match failure.code.as_str() {
                            cc_protocol::codes::INVALID_PARAMS => StatusCode::BAD_REQUEST,
                            cc_protocol::codes::UNAVAILABLE => StatusCode::SERVICE_UNAVAILABLE,
                            _ => StatusCode::INTERNAL_SERVER_ERROR,
                        };
                        return error(status, &failure.message);
                    }
                };
                let tool_calls: Vec<Value> = result
                    .tool_calls
                    .iter()
                    .map(|call| json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments}}))
                    .collect();
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                message.insert(
                    "content".into(),
                    if result.content.is_empty() && !tool_calls.is_empty() { Value::Null } else { Value::String(result.content.clone()) },
                );
                if !result.reasoning.is_empty() {
                    message.insert("reasoning_content".into(), Value::String(result.reasoning.clone()));
                }
                if !tool_calls.is_empty() {
                    message.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                let response = json!({
                    "id": id,
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "message": message, "finish_reason": openai_finish(&result.finish_reason)}],
                    "usage": usage_json(&result.usage),
                    "timings": timings_json(&result.usage),
                });
                return full(StatusCode::OK, "application/json", response.to_string());
            }
        }
        return error(StatusCode::INTERNAL_SERVER_ERROR, "the generation ended without a result");
    }
    let (out_tx, out_rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(64);
    tokio::spawn(async move {
        let _guard = cancel_on_drop;
        let send = |text: String| {
            let out_tx = out_tx.clone();
            async move { out_tx.send(Ok(Frame::data(Bytes::from(text)))).await.is_ok() }
        };
        if !send(chunk(&id, created, &model, json!({"role": "assistant", "content": ""}), None)).await {
            return;
        }
        while let Some(out) = rx.recv().await {
            let text = match out {
                Out::Piece(Piece::Reasoning(text)) => chunk(&id, created, &model, json!({"reasoning_content": text}), None),
                Out::Piece(Piece::Content(text)) => chunk(&id, created, &model, json!({"content": text}), None),
                Out::Piece(Piece::ToolStart { index, id: call_id, name }) => chunk(
                    &id,
                    created,
                    &model,
                    json!({"tool_calls": [{"index": index, "id": call_id, "type": "function", "function": {"name": name, "arguments": ""}}]}),
                    None,
                ),
                Out::Piece(Piece::ToolArgs { index, text }) => {
                    chunk(&id, created, &model, json!({"tool_calls": [{"index": index, "function": {"arguments": text}}]}), None)
                }
                Out::Done(Ok(result)) => {
                    let mut text = chunk(&id, created, &model, json!({}), Some(openai_finish(&result.finish_reason)));
                    let usage = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [], "usage": usage_json(&result.usage), "timings": timings_json(&result.usage)});
                    text.push_str(&format!("data: {usage}\n\ndata: [DONE]\n\n"));
                    text
                }
                Out::Done(Err(failure)) => {
                    let failure = json!({"error": {"message": failure.message, "code": failure.code, "type": "server_error"}});
                    format!("data: {failure}\n\ndata: [DONE]\n\n")
                }
            };
            if !send(text).await {
                return;
            }
        }
    });
    let mut response = hyper::Response::new(StreamBody::new(stream_from(out_rx)).boxed());
    response.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/event-stream"));
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, hyper::header::HeaderValue::from_static("no-cache"));
    response
}

/// Cancels the generation when the HTTP client goes away.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn stream_from(
    mut rx: mpsc::Receiver<Result<Frame<Bytes>, Infallible>>,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, Infallible>> {
    futures_util::stream::poll_fn(move |context| rx.poll_recv(context))
}
