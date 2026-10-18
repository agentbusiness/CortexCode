//! The CPP/1 plugin: `model@1` over the runtime.

use crate::engine::Hibernated;
use crate::runtime::{Config, Paths, Publisher, Runtime, expand_home};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::model::{
    CacheKeyParams, ChatMessage, ChatParams, CompleteParams, EndpointResult, HoldParams, PrewarmParams, Priority, RenameParams,
    Sampling, SelectParams, Thinking, TokenizeParams, methods,
};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, parse_params, to_value};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub const PROVIDES: &[&str] = &[
    methods::INFO,
    methods::CHAT,
    methods::COMPLETE,
    methods::TOKENIZE,
    methods::COUNT,
    methods::SLOTS,
    methods::HIBERNATE,
    methods::HOLD,
    methods::PREWARM,
    methods::RENAME,
    methods::ENDPOINT,
    methods::VARIANTS,
    methods::SELECT,
];

/// The probe's own conversation (not handed to a successor).
const PROBE_KEY: &str = "plugin-probe";

#[derive(Default)]
pub struct ModelPlugin {
    runtime: Arc<tokio::sync::OnceCell<Arc<Runtime>>>,
    ctx: Arc<Mutex<Option<Ctx>>>,
}

/// How long the first call waits for the frame's hello acknowledgement (which carries the plugin
/// configuration) to be recorded; the frame may send `plugin.probe` before `run_plugin` stored it.
const CONFIG_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

impl ModelPlugin {
    /// The runtime, created (and its model load started) on the first call, when the frame's
    /// configuration is known.
    fn runtime(&self, ctx: &Ctx) -> impl std::future::Future<Output = Result<Arc<Runtime>, RpcError>> + Send + 'static {
        if let Ok(mut slot) = self.ctx.lock()
            && slot.is_none()
        {
            *slot = Some(ctx.clone());
        }
        let cell = self.runtime.clone();
        let shared = self.ctx.clone();
        let ctx = ctx.clone();
        async move {
            cell.get_or_try_init(|| async move {
                let started = Instant::now();
                let mut config_value = ctx.config();
                while config_value.is_null() && started.elapsed() < CONFIG_WAIT {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    config_value = ctx.config();
                }
                let config: Config = if config_value.is_null() {
                    Config::default()
                } else {
                    serde_json::from_value(config_value)
                        .map_err(|error| RpcError::invalid_params(format!("plugin config: {error}")))?
                };
                if !config.other.is_empty() {
                    tracing::info!(keys = ?config.other.keys().collect::<Vec<_>>(), "ignoring configuration keys this plugin does not use");
                }
                let home = ctx.env.home.clone();
                let models_dir = config.models_dir.as_deref().map(expand_home).unwrap_or_else(|| home.join("models"));
                let paths = Paths { home, data_dir: ctx.env.data_dir.clone(), models_dir };
                let publisher: Publisher = Arc::new(move |topic: &str, data: Value| {
                    if let Ok(slot) = shared.lock()
                        && let Some(ctx) = slot.as_ref()
                    {
                        ctx.publish(topic, data);
                    }
                });
                let runtime = Runtime::new(paths, config, publisher);
                let starter = runtime.clone();
                tokio::spawn(async move { starter.start().await });
                Ok::<_, RpcError>(runtime)
            })
            .await
            .cloned()
        }
    }
}

async fn dispatch(runtime: Arc<Runtime>, call: Call) -> Result<Value, RpcError> {
    let Call { method, params, items, cancel } = call;
    match method.as_str() {
        methods::INFO => {
            let loaded = runtime.ready().await?;
            to_value(&runtime.info(&loaded))
        }
        methods::CHAT => {
            let params: ChatParams = parse_params(params)?;
            to_value(&runtime.chat(params, Some(items), cancel).await?)
        }
        methods::COMPLETE => {
            let params: CompleteParams = parse_params(params)?;
            to_value(&runtime.complete(params, Some(items), cancel).await?)
        }
        methods::TOKENIZE => {
            let params: TokenizeParams = parse_params(params)?;
            to_value(&runtime.tokenize(params).await?)
        }
        methods::COUNT => {
            let params: ChatParams = parse_params(params)?;
            to_value(&runtime.count(params).await?)
        }
        methods::SLOTS => to_value(&runtime.slots().await?),
        methods::HIBERNATE => {
            let params: CacheKeyParams = parse_params(params)?;
            runtime.hibernate(params).await
        }
        methods::HOLD => {
            let params: HoldParams = parse_params(params)?;
            runtime.hold(params)
        }
        methods::PREWARM => {
            let params: PrewarmParams = parse_params(params)?;
            runtime.prewarm(params, cancel).await
        }
        methods::RENAME => {
            let params: RenameParams = parse_params(params)?;
            runtime.rename(params)
        }
        methods::ENDPOINT => {
            let loaded = runtime.ready().await?;
            let mut endpoint = runtime.endpoint.lock().await;
            if endpoint.is_none() {
                let port = runtime.config.endpoint_port.unwrap_or(0);
                *endpoint = Some(crate::endpoint::start(runtime.clone(), port).await.map_err(RpcError::internal)?);
            }
            let base_url = endpoint.as_ref().map(crate::endpoint::Endpoint::base_url).unwrap_or_default();
            to_value(&EndpointResult { base_url, model: loaded.variant.clone() })
        }
        methods::VARIANTS => {
            let runtime = runtime.clone();
            let result = tokio::task::spawn_blocking(move || runtime.variants())
                .await
                .map_err(|error| RpcError::internal(error.to_string()))?;
            to_value(&result)
        }
        methods::SELECT => {
            let params: SelectParams = parse_params(params)?;
            to_value(&runtime.select(&params.variant).await?)
        }
        other => Err(RpcError::unknown_method(other)),
    }
}

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.to_owned(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.to_owned(), ok: false, detail, ms },
    }
}

/// The plugin's real self-test: the model is loaded, generates, and answers sanely.
async fn probe(runtime: Arc<Runtime>) -> ProbeReport {
    let mut checks = Vec::new();
    let started = Instant::now();
    let loaded = runtime.ready().await;
    checks.push(check(
        "model loaded",
        started,
        loaded
            .as_ref()
            .map(|loaded| {
                format!("{} on {} ({}-token shared pool, up to {} conversations resident)", loaded.variant, loaded.engine.backend, loaded.plan.ctx_total, runtime.resident_capacity(&loaded.plan))
            })
            .map_err(|error| error.message.clone()),
    ));
    let Ok(loaded) = loaded else { return ProbeReport::from_checks(checks) };

    // Generation: 17 + 25 computed here, not by the model.
    let (a, b) = (17u32, 25u32);
    let expected = (a + b).to_string();
    let started = Instant::now();
    let params = ChatParams {
        messages: vec![ChatMessage::new("user", format!("What is {a} + {b}? Reply with only the number."))],
        tools: Vec::new(),
        tool_choice: None,
        grammar: None,
        json_schema: None,
        sampling: Sampling { temperature: 0.0, seed: Some(1), ..Sampling::default() },
        thinking: Thinking { enabled: false, effort: None, budget_tokens: None },
        priority: Priority::Foreground,
        cache_key: Some(PROBE_KEY.into()),
        stop: Vec::new(),
        // A self-test bound, not a cap on product output: a sane answer is a few tokens.
        max_tokens: Some(32),
    };
    let generated = runtime.chat(params, None, CancellationToken::new()).await;
    checks.push(check(
        "generates a sane answer",
        started,
        match generated {
            Ok(result) if result.usage.completion_tokens > 0 && result.content.contains(&expected) => Ok(format!(
                "{:?} in {} tokens, {:.1} tok/s",
                result.content.trim(),
                result.usage.completion_tokens,
                f64::from(result.usage.completion_tokens) / (result.usage.decode_ms.max(1.0) / 1000.0)
            )),
            Ok(result) => Err(format!("expected {expected}, got {:?} ({})", result.content, result.finish_reason)),
            Err(error) => Err(error.message),
        },
    ));
    let started = Instant::now();
    let text = "fn main() { println!(\"hello\"); }";
    let roundtrip = async {
        let tokens = runtime.tokenize(TokenizeParams { text: text.into() }).await.map_err(|error| error.message)?.tokens;
        if tokens.is_empty() {
            return Err("no tokens".to_owned());
        }
        let back = runtime.detokenize(&tokens).await.map_err(|error| error.message)?;
        if back == text { Ok(format!("{} tokens round-trip", tokens.len())) } else { Err(format!("round-trip gave {back:?}")) }
    }
    .await;
    checks.push(check("tokenizer round-trip", started, roundtrip));
    drop(loaded);
    ProbeReport::from_checks(checks)
}

impl Plugin for ModelPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let runtime = self.runtime(&ctx);
        Box::pin(async move { dispatch(runtime.await?, call).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let runtime = self.runtime(&ctx);
        Box::pin(async move { Ok(probe(runtime.await?).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let runtime = self.runtime(&ctx);
        Box::pin(async move { runtime.await.map(|_| ()) })
    }

    fn event(&self, _ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        if topic == cc_protocol::contracts::frame::topics::SESSION_DELETED
            && let Ok(deleted) = serde_json::from_value::<cc_protocol::contracts::frame::SessionDeleted>(data)
            && let Some(runtime) = self.runtime.get()
        {
            runtime.forget(deleted.session_id);
        }
        Box::pin(async {})
    }

    /// Hand the conversations to a successor: every idle one is hibernated to the plugin's data
    /// directory (stable across versions) and the records travel in the state.
    fn export_state(&self, _ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        let runtime = self.runtime.get().cloned();
        Box::pin(async move {
            let Some(runtime) = runtime else { return Ok(Value::Null) };
            let (records, probe): (Vec<Hibernated>, Vec<Hibernated>) =
                runtime.hibernate_all().await.into_iter().partition(|record| record.key != PROBE_KEY);
            for record in probe {
                let _ = std::fs::remove_file(&record.file);
            }
            Ok(json!({"hibernated": records}))
        })
    }

    fn import_state(&self, ctx: Ctx, state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        let runtime = self.runtime(&ctx);
        Box::pin(async move {
            let runtime = runtime.await?;
            let records: Vec<Hibernated> = serde_json::from_value(state["hibernated"].clone()).unwrap_or_default();
            if !records.is_empty() {
                tracing::info!(conversations = records.len(), "adopting conversations hibernated by the previous instance");
                runtime.adopt(records);
            }
            Ok(())
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let runtime = self.runtime.get().cloned();
        Box::pin(async move {
            if let Some(runtime) = runtime {
                runtime.shutdown().await;
            }
        })
    }
}
