//! One model call: `model.chat` streamed into the journal (thinking -> `reasoning`, text ->
//! `assistant_message`), its usage journaled, cancellation honoured mid-generation.
//!
//! Sampling comes from the data set (shipped with the Qwen3.8 guidance). Nothing caps output or reasoning (`max_tokens` is never set); runaway generations
//! are ended by the model plugin's repetition stop (`finish_reason = repeated`).

use crate::journal::{Journal, TextStream};
use cc_protocol::contracts::frame::kinds;
use cc_protocol::contracts::model::{
    ChatDelta, ChatMessage, ChatParams, ChatResult, Priority, Sampling, Thinking, ToolSpec, methods as model_methods,
};
use cc_protocol::{RpcError, codes, parse_params};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Thinking for a model-chosen effort (`none` = thinking off).
pub fn thinking(effort: &str) -> Thinking {
    match effort {
        "none" | "" => Thinking { enabled: false, effort: None, budget_tokens: None },
        level => Thinking { enabled: true, effort: Some(level.to_owned()), budget_tokens: None },
    }
}


/// Where a call's streamed output goes.
#[derive(Clone, Debug)]
pub struct StreamTo {
    pub run_id: String,
    pub step: u64,
    /// Stream text into an `assistant_message` (false for constrained decisions, whose JSON is
    /// journaled as a `route` entry instead).
    pub content: bool,
}

pub struct CallSpec {
    pub stage: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub json_schema: Option<Value>,
    /// GBNF grammar constraining the output (how decisions are sent; see `schemas`).
    pub grammar: Option<String>,
    /// `None` = the default (`auto` with tools; `none` for constrained decisions).
    pub tool_choice: Option<Value>,
    pub effort: String,
    /// From the data set.
    pub sampling: Sampling,
    pub priority: Priority,
    pub cache_key: String,
    pub stream: StreamTo,
}

/// A finished call with the journal entries it produced.
pub struct Called {
    pub result: ChatResult,
    pub content_seq: Option<u64>,
    pub reasoning_seq: Option<u64>,
    pub wall_ms: u64,
}

/// How long to wait for the model plugin's answer after cancelling a generation.
const CANCEL_GRACE: Duration = Duration::from_millis(300);
/// Refusals (model plugin restarting or swapping) are retried this long.
const REFUSED_WINDOW: Duration = Duration::from_secs(600);

pub async fn chat(journal: &mut Journal, spec: CallSpec, cancel: &CancellationToken) -> Result<Called, RpcError> {
    let thinking = thinking(&spec.effort);
    let constrained = spec.json_schema.is_some() || spec.grammar.is_some();
    let params = ChatParams {
        messages: spec.messages,
        tool_choice: spec.tool_choice.or_else(|| (constrained && !spec.tools.is_empty()).then(|| json!("none"))),
        tools: spec.tools,
        grammar: spec.grammar,
        json_schema: spec.json_schema,
        sampling: spec.sampling,
        thinking,
        priority: spec.priority,
        cache_key: Some(spec.cache_key),
        stop: Vec::new(),
        max_tokens: None,
    };
    let params = serde_json::to_value(&params)?;
    dump(&spec.stage, &params);
    let started = Instant::now();
    let mut delay = Duration::from_millis(200);
    loop {
        if cancel.is_cancelled() {
            return Err(RpcError::cancelled());
        }
        match stream_once(journal, &params, &spec.stream, &spec.stage, cancel).await {
            Ok(mut called) => {
                called.wall_ms = started.elapsed().as_millis() as u64;
                let usage = &called.result.usage;
                journal
                    .append(
                        kinds::USAGE,
                        json!({
                            "stage": spec.stage,
                            "run_id": spec.stream.run_id,
                            "step": spec.stream.step,
                            "prompt_tokens": usage.prompt_tokens,
                            "cached_tokens": usage.cached_tokens,
                            "completion_tokens": usage.completion_tokens,
                            "prefill_ms": usage.prefill_ms,
                            "decode_ms": usage.decode_ms,
                            "queue_ms": usage.queue_ms,
                            "wall_ms": called.wall_ms,
                            "finish_reason": called.result.finish_reason,
                            "instance": journal.host().ctx().env.instance,
                        }),
                    )
                    .await?;
                return Ok(called);
            }
            // Nothing was generated: the model plugin refused the call (restart, swap, busy).
            Err(error) if crate::host::refused(&error) && started.elapsed() < REFUSED_WINDOW => {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return Err(RpcError::cancelled()),
                }
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Development aid: with `CORTEX_DUMP_PROMPTS=<dir>` every request is written there as JSON, so
/// prompts can be replayed and compared against the model by hand.
fn dump(stage: &str, params: &Value) {
    let Ok(dir) = std::env::var("CORTEX_DUMP_PROMPTS") else { return };
    let dir = std::path::PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_ok() {
        let name = format!("{}-{stage}-{}.json", cc_protocol::contracts::now_ms(), uuid::Uuid::new_v4().simple());
        let _ = std::fs::write(dir.join(name), params.to_string());
    }
}

async fn stream_once(
    journal: &mut Journal,
    params: &Value,
    target: &StreamTo,
    stage: &str,
    cancel: &CancellationToken,
) -> Result<Called, RpcError> {
    let base = json!({"run_id": target.run_id, "step": target.step, "stage": stage});
    let mut content = TextStream::new(kinds::ASSISTANT_MESSAGE, base.clone());
    let mut reasoning = TextStream::new(kinds::REASONING, base);
    let mut stream = journal.host().ctx().call_stream(model_methods::CHAT, params.clone())?;
    loop {
        let item = tokio::select! {
            item = stream.next() => item,
            _ = cancel.cancelled() => {
                stream.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, async { while stream.next().await.is_some() {} }).await;
                let _ = content.finish(journal, None, &[("interrupted", json!(true))]).await;
                let _ = reasoning.finish(journal, None, &[("interrupted", json!(true))]).await;
                return Err(RpcError::cancelled());
            }
        };
        let Some(item) = item else { break };
        let Ok(delta) = serde_json::from_value::<ChatDelta>(item) else { continue };
        match delta.kind.as_str() {
            "reasoning" => reasoning.push(journal, &delta.text).await?,
            "content" if target.content => content.push(journal, &delta.text).await?,
            _ => {}
        }
    }
    let outcome = stream.finish().await;
    let result: ChatResult = match outcome {
        Ok(value) => parse_params(value)?,
        Err(error) => {
            let interrupted = [("interrupted", json!(true))];
            content.finish(journal, None, &interrupted).await?;
            reasoning.finish(journal, None, &interrupted).await?;
            if error.code == codes::CANCELLED && !cancel.is_cancelled() {
                // The model plugin cancelled on its own (e.g. preempted while being swapped).
                return Err(RpcError::new(codes::UNAVAILABLE, error.message).retryable());
            }
            return Err(error);
        }
    };
    let final_content = if target.content { Some(result.content.as_str()) } else { None };
    content.finish(journal, final_content, &[]).await?;
    let final_reasoning = (!result.reasoning.is_empty()).then_some(result.reasoning.as_str());
    reasoning.finish(journal, final_reasoning, &[]).await?;
    Ok(Called { content_seq: content.seq(), reasoning_seq: reasoning.seq(), result, wall_ms: 0 })
}
