//! `cc-model-llama`: CortexCode's model plugin for Linux (`model@1`). Qwen3.8-27B on a supervised
//! upstream llama-server (CUDA or Vulkan, DFlash 2 drafts), or a member's Qwen borrowed through the
//! mesh (see `engine`).
//!
//! Started by the frame. Without a frame, `cc-model-llama call <method> [<params json>]` serves one
//! call (items on stdout, then the result; `CORTEX_HOME` and `CORTEX_PLUGIN_DATA` must be set).

mod borrow;
mod catalog;
mod config;
mod engine;
mod gpu;
mod local;
mod progress;
mod server;
mod store;

use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport, SwapPolicy};
use cc_protocol::contracts::model::methods;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError};
use config::Config;
use engine::{Engine, Sink};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const PROVIDES: &[&str] = &[
    methods::INFO,
    methods::CHAT,
    methods::COMPLETE,
    methods::TOKENIZE,
    methods::COUNT,
    methods::SLOTS,
    methods::HIBERNATE,
    methods::HOLD,
    methods::PREWARM,
    methods::VARIANTS,
    methods::SELECT,
];

#[derive(Default)]
struct ModelPlugin {
    engine: Arc<tokio::sync::OnceCell<Arc<Engine>>>,
    ctx: Arc<Mutex<Option<Ctx>>>,
}

impl ModelPlugin {
    /// The engine, created on the first call (the frame's configuration is known by then).
    fn engine(&self, ctx: &Ctx) -> impl std::future::Future<Output = Arc<Engine>> + Send + 'static {
        if let Ok(mut slot) = self.ctx.lock()
            && slot.is_none()
        {
            *slot = Some(ctx.clone());
        }
        let shared = self.ctx.clone();
        let cell = self.engine.clone();
        let ctx = ctx.clone();
        async move {
            cell.get_or_init(|| async move {
                let config = Config::from_value(&ctx.config());
                let publish: progress::Publisher = Arc::new(move |topic: &str, data: Value| {
                    if let Ok(slot) = shared.lock()
                        && let Some(ctx) = slot.as_ref()
                    {
                        ctx.publish(topic, data);
                    }
                });
                Engine::new(config, ctx.env.home.clone(), ctx.env.data_dir.clone(), publish, Some(ctx.clone())).await
            })
            .await
            .clone()
        }
    }
}

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

/// The real self-test: a borrowed model answers `model.info`; a local one is installed (or is
/// being downloaded, which is reported, not failed), loads and adds two numbers.
async fn probe(engine: Arc<Engine>) -> ProbeReport {
    let started = Instant::now();
    if engine.is_borrowing() {
        let info = engine.info().await;
        let outcome = info
            .map(|info| format!("borrowed {} from {}", info["model_id"].as_str().unwrap_or("?"), engine.lender_name().unwrap_or_else(|| "a member".into())))
            .map_err(|e| e.message);
        return ProbeReport::from_checks(vec![check("model answers", started, outcome)]);
    }
    if engine.without_gpu() {
        let detail = Ok("no GPU with 16 GB here: a member's Qwen serves when one lends it".to_owned());
        return ProbeReport::from_checks(vec![check("no local model", started, detail)]);
    }
    if !engine.installed() {
        engine.install_in_background();
        let detail = match engine.install_progress() {
            Some(p) => format!("{}: {} {:.2} of {:.2} GB", p.variant, p.phase, p.bytes as f64 / 1e9, p.total as f64 / 1e9),
            None => "installing".into(),
        };
        return ProbeReport::from_checks(vec![check("model installing", started, Ok(detail))]);
    }
    let params = json!({
        "messages": [{"role": "user", "content": "What is 17 + 25? Reply with only the number."}],
        "sampling": {"temperature": 0.0, "seed": 1},
        "thinking": {"enabled": false},
        "priority": "foreground",
        "cache_key": "plugin-probe",
        "max_tokens": 16,
    });
    let outcome = match engine.dispatch(methods::CHAT, params, None, CancellationToken::new()).await {
        Ok(result) if result["content"].as_str().is_some_and(|c| c.contains("42")) => {
            let usage = &result["usage"];
            let decode = usage["decode_ms"].as_f64().unwrap_or(0.0).max(1.0);
            Ok(format!("{:?}, {:.0} tok/s", result["content"].as_str().unwrap_or("").trim(), usage["completion_tokens"].as_f64().unwrap_or(0.0) * 1000.0 / decode))
        }
        Ok(result) => Err(format!("expected 42, got {:?}", result["content"])),
        Err(error) => Err(error.message),
    };
    ProbeReport::from_checks(vec![check("generates a sane answer", started, outcome)])
}

impl Plugin for ModelPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let engine = self.engine(&ctx);
        Box::pin(async move {
            let engine = engine.await;
            let Call { method, params, items, cancel } = call;
            let sink = Sink::Frame(items);
            engine.dispatch(&method, params, Some(&sink), cancel).await
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let engine = self.engine(&ctx);
        Box::pin(async move { Ok(probe(engine.await).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let engine = self.engine(&ctx);
        Box::pin(async move {
            engine.await;
            Ok(())
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let engine = self.engine.get().cloned();
        Box::pin(async move {
            if let Some(engine) = engine {
                engine.shutdown().await;
            }
        })
    }
}

/// SIGTERM / SIGINT: stop the llama-server child first, then exit.
fn on_signals(data_dir: Option<PathBuf>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        if let Some(dir) = &data_dir {
            server::kill_recorded(dir);
        }
        std::process::exit(0);
    });
}

/// `call <method> [<params>] [<method> [<params>]]...`: the calls in order on one engine;
/// `sleep {"ms": N}` waits in between.
async fn call_once(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("usage: cc-model-llama call <method> [<params json>] ...");
        return 2;
    }
    let mut calls: Vec<(String, Value)> = Vec::new();
    let mut rest = args.iter().peekable();
    while let Some(method) = rest.next() {
        let params = match rest.peek() {
            Some(text) if text.trim_start().starts_with('{') => match serde_json::from_str(rest.next().map(String::as_str).unwrap_or("{}")) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("params of {method}: {error}");
                    return 2;
                }
            },
            _ => json!({}),
        };
        calls.push((method.clone(), params));
    }
    let (Some(home), Some(data)) = (std::env::var_os("CORTEX_HOME"), std::env::var_os("CORTEX_PLUGIN_DATA")) else {
        eprintln!("set CORTEX_HOME and CORTEX_PLUGIN_DATA");
        return 2;
    };
    let config = std::env::var("CORTEX_PLUGIN_CONFIG").ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null);
    let publish: progress::Publisher = Arc::new(|topic: &str, data: Value| eprintln!("{topic} {data}"));
    let engine = Engine::new(Config::from_value(&config), PathBuf::from(home), PathBuf::from(data), publish, None).await;
    let mut code = 0;
    for (method, params) in calls {
        if method == "sleep" {
            tokio::time::sleep(std::time::Duration::from_millis(params["ms"].as_u64().unwrap_or(1000))).await;
            continue;
        }
        let started = Instant::now();
        let outcome = engine.dispatch(&method, params, Some(&Sink::Print), CancellationToken::new()).await;
        eprintln!("{method} took {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
        match outcome {
            Ok(result) => println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default()),
            Err(error) => {
                eprintln!("{method} error: {error}");
                code = 1;
            }
        }
    }
    engine.shutdown().await;
    code
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = std::env::var_os("CORTEX_PLUGIN_DATA").map(PathBuf::from);
    let code = runtime.block_on(async move {
        on_signals(data_dir.clone());
        if args.first().map(String::as_str) == Some("call") {
            return call_once(&args[1..]).await;
        }
        let mut hello = Hello::new("model-llama", env!("CARGO_PKG_VERSION"), "model", "model@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        // Two instances cannot hold a model at once: drain the old one first.
        hello.swap = SwapPolicy::DrainFirst;
        let code = match cc_protocol::run_plugin(hello, Arc::new(ModelPlugin::default())).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-model-llama: {error}");
                1
            }
        };
        // Never leave a llama-server behind (frame gone, shutdown, error).
        if let Some(dir) = &data_dir {
            server::kill_recorded(dir);
        }
        code
    });
    std::process::exit(code);
}
