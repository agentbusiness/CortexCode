//! `model@1` over the local llama-server's HTTP API: `/v1/chat/completions` (streamed, OpenAI
//! shape, `cache_prompt`), `/completion`, `/tokenize`, `/apply-template` and `/slots`.

use crate::progress::Tracker;
use crate::server::Server;
use cc_protocol::contracts::model::{
    ChatMessage, ChatParams, ChatResult, CompleteParams, ContentPart, CountResult, PrewarmParams, Sampling, SlotState, SlotsResult, Thinking,
    TokenizeResult, ToolCall, ToolSpec, Usage,
};
use crate::engine::Sink;
use base64::Engine as _;
use cc_protocol::{RpcError, codes};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn message_json(message: &ChatMessage) -> Value {
    let mut out = Map::new();
    out.insert("role".into(), json!(message.role));
    // Images go to the vision projector as data URLs (the parts name files on this machine).
    let content = if message.parts.is_empty() {
        json!(message.content)
    } else {
        Value::Array(
            message
                .parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
                    ContentPart::Image { path, mime, .. } => match std::fs::read(path) {
                        Ok(bytes) => Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes))}})),
                        Err(error) => {
                            tracing::warn!(%path, %error, "an image of the message cannot be read; left out");
                            None
                        }
                    },
                })
                .collect(),
        )
    };
    out.insert("content".into(), content);
    if let Some(reasoning) = &message.reasoning_content
        && message.role == "assistant"
    {
        out.insert("reasoning_content".into(), json!(reasoning));
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments}}))
            .collect();
        out.insert("tool_calls".into(), Value::Array(calls));
    }
    if let Some(id) = &message.tool_call_id {
        out.insert("tool_call_id".into(), json!(id));
    }
    if let Some(name) = &message.name {
        out.insert("name".into(), json!(name));
    }
    Value::Object(out)
}

fn tools_json(tools: &[ToolSpec]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| json!({"type": "function", "function": {"name": tool.name, "description": tool.description, "parameters": tool.parameters}}))
            .collect(),
    )
}

fn sampling_into(body: &mut Map<String, Value>, sampling: &Sampling) {
    body.insert("temperature".into(), json!(sampling.temperature));
    body.insert("top_p".into(), json!(sampling.top_p));
    body.insert("top_k".into(), json!(sampling.top_k));
    body.insert("min_p".into(), json!(sampling.min_p));
    body.insert("presence_penalty".into(), json!(sampling.presence_penalty));
    if let Some(penalty) = sampling.repeat_penalty {
        body.insert("repeat_penalty".into(), json!(penalty));
    }
    if let Some(seed) = sampling.seed {
        body.insert("seed".into(), json!(seed));
    }
}

/// Qwen3.8's template turns thinking on and off with its `enable_thinking` switch, whose default is
/// on, so it is always passed: on only when the call asks for thinking.
fn thinking_into(body: &mut Map<String, Value>, thinking: &Thinking) {
    body.insert("chat_template_kwargs".into(), json!({"enable_thinking": thinking.enabled}));
    if thinking.enabled
        && let Some(budget) = thinking.budget_tokens
    {
        body.insert("reasoning_budget_tokens".into(), json!(budget));
    }
}

pub fn chat_body(params: &ChatParams) -> Value {
    let mut body = Map::new();
    body.insert("messages".into(), Value::Array(params.messages.iter().map(message_json).collect()));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    body.insert("cache_prompt".into(), json!(true));
    body.insert("return_progress".into(), json!(true));
    sampling_into(&mut body, &params.sampling);
    thinking_into(&mut body, &params.thinking);
    if let Some(max) = params.max_tokens {
        body.insert("max_tokens".into(), json!(max));
    }
    if !params.stop.is_empty() {
        body.insert("stop".into(), json!(params.stop));
    }
    if !params.tools.is_empty() {
        body.insert("tools".into(), tools_json(&params.tools));
        if let Some(choice) = &params.tool_choice {
            body.insert("tool_choice".into(), choice.clone());
        }
    }
    if let Some(grammar) = &params.grammar {
        body.insert("grammar".into(), json!(grammar));
    } else if let Some(schema) = &params.json_schema {
        body.insert("response_format".into(), json!({"type": "json_schema", "json_schema": {"name": "output", "schema": schema, "strict": true}}));
    }
    Value::Object(body)
}

async fn post_stream(server: &Server, path: &str, body: &Value) -> Result<reqwest::Response, RpcError> {
    let response = server
        .http
        .post(server.url(path))
        .json(body)
        .send()
        .await
        .map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("llama-server: {e}")).retryable())?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or(text);
    let code = if status.is_client_error() { codes::INVALID_PARAMS } else { codes::INTERNAL };
    Err(RpcError::new(code, format!("llama-server {status}: {message}")))
}

async fn post_json(server: &Server, path: &str, body: &Value) -> Result<Value, RpcError> {
    let response = post_stream(server, path, body).await?;
    response.json::<Value>().await.map_err(|e| RpcError::internal(format!("llama-server answer: {e}")))
}

/// Server-sent events: each `data:` line as JSON, until `[DONE]` or the end.
async fn each_event(
    response: reqwest::Response,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(Value),
) -> Result<bool, RpcError> {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel.cancelled() => return Ok(false),
        };
        let Some(chunk) = chunk else { return Ok(true) };
        let chunk = chunk.map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("llama-server stream: {e}")).retryable())?;
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else { continue };
            if data == "[DONE]" {
                return Ok(true);
            }
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                if let Some(error) = value.get("error") {
                    return Err(RpcError::internal(format!("llama-server: {}", error["message"].as_str().unwrap_or("error"))));
                }
                on_event(value);
            }
        }
    }
}

fn usage_from(timings: &Value, queue_ms: f64) -> Usage {
    let n = |key: &str| timings[key].as_u64().unwrap_or(0) as u32;
    Usage {
        prompt_tokens: n("prompt_n") + n("cache_n"),
        cached_tokens: n("cache_n"),
        completion_tokens: n("predicted_n"),
        prefill_ms: timings["prompt_ms"].as_f64().unwrap_or(0.0),
        decode_ms: timings["predicted_ms"].as_f64().unwrap_or(0.0),
        queue_ms,
        draft_accepted: n("draft_n_accepted"),
        draft_total: n("draft_n"),
        slot: None,
    }
}

fn finish_of(reason: &str) -> String {
    match reason {
        "length" => "length",
        "tool_calls" => "tool_calls",
        _ => "stop",
    }
    .to_owned()
}

pub async fn chat(server: &Server, params: &ChatParams, items: Option<&Sink>, cancel: &CancellationToken, tracker: &mut Tracker) -> Result<ChatResult, RpcError> {
    let started = Instant::now();
    let body = chat_body(params);
    let response = post_stream(server, "/v1/chat/completions", &body).await?;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut finish = String::from("stop");
    let mut timings = Value::Null;
    let mut queue_ms: Option<f64> = None;
    let send = |kind: &str, text: &str, index: Option<u32>| {
        if let Some(items) = items {
            items.send(json!({"kind": kind, "text": text, "index": index}));
        }
    };
    let completed = each_event(response, cancel, |event| {
        if let Some(progress) = event.get("prompt_progress") {
            queue_ms.get_or_insert(started.elapsed().as_secs_f64() * 1000.0);
            let n = |key: &str| progress[key].as_u64().unwrap_or(0) as u32;
            tracker.prefill(n("total"), n("cache"), n("processed"));
        }
        if event.get("timings").is_some() {
            timings = event["timings"].clone();
        }
        let Some(choice) = event["choices"].get(0) else { return };
        let delta = &choice["delta"];
        if let Some(text) = delta["reasoning_content"].as_str().filter(|t| !t.is_empty()) {
            reasoning.push_str(text);
            tracker.token();
            send("reasoning", text, None);
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            content.push_str(text);
            tracker.token();
            send("content", text, None);
        }
        if let Some(list) = delta["tool_calls"].as_array() {
            for call in list {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                while calls.len() <= index {
                    calls.push(ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
                }
                let slot = &mut calls[index];
                if let Some(id) = call["id"].as_str() {
                    slot.id = id.to_owned();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    slot.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str().filter(|t| !t.is_empty()) {
                    slot.arguments.push_str(arguments);
                    tracker.token();
                    send("tool_call", arguments, Some(index as u32));
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = finish_of(reason);
        }
    })
    .await?;
    if !completed {
        finish = "cancelled".into();
    }
    let usage = usage_from(&timings, queue_ms.unwrap_or(0.0));
    let ended = if !completed { "stopped" } else if params.max_tokens == Some(0) { "prefilled" } else { finish.as_str() };
    tracker.done(ended, Some(usage.completion_tokens));
    for (index, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("call_{index}");
        }
    }
    Ok(ChatResult { content, reasoning, tool_calls: calls, finish_reason: finish, usage, probs: None })
}

pub async fn complete(server: &Server, params: &CompleteParams, items: Option<&Sink>, cancel: &CancellationToken, tracker: &mut Tracker) -> Result<ChatResult, RpcError> {
    let started = Instant::now();
    let mut body = Map::new();
    body.insert("prompt".into(), json!(params.prompt));
    body.insert("stream".into(), json!(true));
    body.insert("cache_prompt".into(), json!(true));
    body.insert("return_progress".into(), json!(true));
    body.insert("n_predict".into(), json!(params.max_tokens.map_or(-1, i64::from)));
    sampling_into(&mut body, &params.sampling);
    if !params.stop.is_empty() {
        body.insert("stop".into(), json!(params.stop));
    }
    if let Some(grammar) = &params.grammar {
        body.insert("grammar".into(), json!(grammar));
    }
    if let Some(n) = params.n_probs {
        body.insert("n_probs".into(), json!(n));
    }
    let response = post_stream(server, "/completion", &Value::Object(body)).await?;
    let mut content = String::new();
    let mut finish = String::from("stop");
    let mut timings = Value::Null;
    let mut probs: Vec<Value> = Vec::new();
    let mut queue_ms: Option<f64> = None;
    let completed = each_event(response, cancel, |event| {
        if let Some(progress) = event.get("prompt_progress") {
            queue_ms.get_or_insert(started.elapsed().as_secs_f64() * 1000.0);
            let n = |key: &str| progress[key].as_u64().unwrap_or(0) as u32;
            tracker.prefill(n("total"), n("cache"), n("processed"));
        }
        if let Some(text) = event["content"].as_str().filter(|t| !t.is_empty()) {
            content.push_str(text);
            tracker.token();
            if let Some(items) = items {
                items.send(json!({"kind": "content", "text": text, "index": null}));
            }
        }
        if let Some(list) = event["completion_probabilities"].as_array() {
            probs.extend(list.iter().cloned());
        }
        if event["stop"].as_bool() == Some(true) {
            if event.get("timings").is_some() {
                timings = event["timings"].clone();
            }
            if event["stop_type"] == "limit" {
                finish = "length".into();
            }
        }
    })
    .await?;
    if !completed {
        finish = "cancelled".into();
    }
    let usage = usage_from(&timings, queue_ms.unwrap_or(0.0));
    tracker.done(if completed { &finish } else { "stopped" }, Some(usage.completion_tokens));
    let probs = (params.n_probs.is_some() && !probs.is_empty()).then_some(Value::Array(probs));
    Ok(ChatResult { content, reasoning: String::new(), tool_calls: Vec::new(), finish_reason: finish, usage, probs })
}

pub async fn tokenize(server: &Server, text: &str) -> Result<TokenizeResult, RpcError> {
    let answer = post_json(server, "/tokenize", &json!({"content": text})).await?;
    let tokens = answer["tokens"].as_array().map(|list| list.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    Ok(TokenizeResult { tokens })
}

/// Tokens of the templated prompt (what the model would read for this request).
pub async fn count(server: &Server, params: &ChatParams) -> Result<CountResult, RpcError> {
    let mut body = chat_body(params);
    if let Some(object) = body.as_object_mut() {
        for key in ["stream", "stream_options", "return_progress", "cache_prompt"] {
            object.remove(key);
        }
    }
    let templated = post_json(server, "/apply-template", &body).await?;
    let prompt = templated["prompt"].as_str().unwrap_or_default();
    let answer = post_json(server, "/tokenize", &json!({"content": prompt, "add_special": true, "parse_special": true})).await?;
    Ok(CountResult { tokens: answer["tokens"].as_array().map_or(0, Vec::len) as u32 })
}

pub async fn slots(server: &Server) -> Result<SlotsResult, RpcError> {
    let response = server
        .http
        .get(server.url("/slots"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("llama-server: {e}")))?;
    let list: Value = response.json().await.map_err(|e| RpcError::internal(e.to_string()))?;
    let slots: Vec<SlotState> = list
        .as_array()
        .map(|list| {
            list.iter()
                .map(|slot| SlotState {
                    slot: slot["id"].as_u64().unwrap_or(0) as u32,
                    cache_key: None,
                    busy: slot["is_processing"].as_bool().unwrap_or(false),
                    n_tokens: slot["n_prompt_tokens"].as_u64().unwrap_or(0) as u32,
                    priority: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let busy = slots.iter().filter(|slot| slot.busy).count() as u32;
    let pool: u64 = list.as_array().map(|l| l.iter().map(|s| s["n_ctx"].as_u64().unwrap_or(0)).sum()).unwrap_or(0);
    Ok(SlotsResult {
        slots,
        queued: 0,
        hibernated: Vec::new(),
        queued_by_priority: Default::default(),
        busy,
        pool_tokens: pool,
        reserved_tokens: 0,
        metrics: None,
    })
}

/// Prefill without generating (`max_tokens: 0`, `cache_prompt`): the slot keeps the prefix.
pub async fn prewarm(server: &Server, params: &PrewarmParams, cancel: &CancellationToken, tracker: &mut Tracker) -> Result<Value, RpcError> {
    let started = Instant::now();
    let messages: Vec<ChatMessage> = if params.shared_only {
        params.messages.iter().take_while(|m| m.role == "system").take(1).cloned().collect()
    } else {
        params.messages.clone()
    };
    let chat = ChatParams {
        messages,
        tools: params.tools.clone(),
        tool_choice: None,
        grammar: None,
        json_schema: None,
        sampling: Sampling::default(),
        thinking: params.thinking.clone(),
        priority: cc_protocol::contracts::model::Priority::Background,
        cache_key: Some(params.cache_key.clone()),
        stop: Vec::new(),
        max_tokens: Some(0),
        idle_only: params.idle_only,
    };
    let result = chat_inner(server, &chat, cancel, tracker).await?;
    Ok(json!({
        "cache_key": params.cache_key,
        "prompt_tokens": result.prompt_tokens,
        "cached_tokens": result.cached_tokens,
        "prefill_ms": result.prefill_ms,
        "ms": started.elapsed().as_secs_f64() * 1000.0,
    }))
}

async fn chat_inner(server: &Server, params: &ChatParams, cancel: &CancellationToken, tracker: &mut Tracker) -> Result<Usage, RpcError> {
    let result = chat(server, params, None, cancel, tracker).await?;
    Ok(result.usage)
}
