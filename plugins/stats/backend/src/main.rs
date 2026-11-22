//! `cc-stats`: serve `stats@1`. Every process of the app hands its rows here (`stats.record`) once
//! per five-minute window; they are written as day files under `<home>/stats` and queued, and once
//! every process has handed over, the rows no CortexManage has accepted yet are offered to the
//! CortexManages on the mesh, this app first, then this machine, then the members, until one that
//! manages this app's repository accepts them. One that refuses is not asked again for an hour.

use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::contracts::mesh::{Card, ServiceCallParams, ServicesResult, methods as mesh};
use cc_protocol::contracts::reports::{self, Machine, RecordParams, SendParams, SendResult, Sender};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, run_plugin};
use cc_stats_kit::deliver::Outbox;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Stats {
    /// One writer at a time: two batches of one process must not rewrite a day file together.
    writing: Arc<Mutex<()>>,
    delivering: AtomicBool,
}

fn root(ctx: &Ctx) -> PathBuf {
    ctx.stats_root()
}

impl Stats {
    async fn record(&self, ctx: &Ctx, params: Value) -> Result<Value, RpcError> {
        let batch: RecordParams = parse_params(params)?;
        let root = root(ctx);
        let writing = self.writing.clone();
        let written = tokio::task::spawn_blocking(move || {
            let _writing = writing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cc_stats_kit::files::store(&root, batch)
        })
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?
        .map_err(|error| RpcError::new(codes::INVALID_PARAMS, error))?;
        Ok(json!({"written": written}))
    }

    /// Deliver for as long as the plugin runs.
    fn deliver(&self, ctx: Ctx) {
        if self.delivering.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            let mut outbox = Outbox::default();
            loop {
                cc_stats_kit::next_delivery().await;
                let targets = targets(&ctx).await;
                if targets.is_empty() {
                    continue;
                }
                let Some(sender) = sender(&ctx).await else { continue };
                let round = outbox.deliver(&root(&ctx), &sender, &targets, |node, params| {
                    let ctx = ctx.clone();
                    async move { send(&ctx, node, params).await }
                })
                .await;
                if round.sent > 0 {
                    tracing::debug!(sent = round.sent, waiting = round.waiting, "reports delivered");
                }
            }
        });
    }
}

/// The CortexManages to ask, in order: this app, this machine, the members.
async fn targets(ctx: &Ctx) -> Vec<String> {
    let Ok(answer) = ctx.call(mesh::SERVICES, json!({})).await else { return Vec::new() };
    let Ok(services) = serde_json::from_value::<ServicesResult>(answer) else { return Vec::new() };
    let mut offers: Vec<_> = services
        .services
        .into_iter()
        .filter(|offer| offer.reachable && offer.share.service == reports::SERVICE && offer.share.method(reports::methods::SEND).is_some())
        .collect();
    offers.sort_by_key(|offer| (!offer.here, !offer.local));
    let mut nodes: Vec<String> = Vec::new();
    for offer in offers {
        if !nodes.contains(&offer.node) {
            nodes.push(offer.node);
        }
    }
    nodes
}

/// This app as it declares itself, on this computer.
async fn sender(ctx: &Ctx) -> Option<Sender> {
    let card: Card = serde_json::from_value(ctx.call(mesh::CARD, json!({})).await.ok()?).ok()?;
    Some(Sender {
        repo: cc_protocol::release::DEFAULT_REPO.to_owned(),
        software: card.software.clone(),
        version: ctx.frame_version().unwrap_or_else(|| card.version.clone()),
        machine: Machine {
            node: card.node,
            name: card.name,
            chip: card.chip,
            os_version: card.os_version,
            memory_bytes: card.memory_bytes,
            cpu_cores: card.cpu_cores,
            gpu_cores: card.gpu_cores,
        },
    })
}

async fn send(ctx: &Ctx, node: String, params: SendParams) -> Result<SendResult, String> {
    let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
    let call = ServiceCallParams { method: reports::methods::SEND.to_owned(), params, node };
    let answer = ctx.call(mesh::SERVICE_CALL, serde_json::to_value(call).map_err(|e| e.to_string())?).await.map_err(|e| e.message)?;
    serde_json::from_value(answer).map_err(|e| e.to_string())
}

impl Plugin for Stats {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.deliver(ctx.clone());
        let this = Stats { writing: self.writing.clone(), delivering: AtomicBool::new(true) };
        Box::pin(async move {
            match call.method.as_str() {
                reports::methods::RECORD => this.record(&ctx, call.params).await,
                other => Err(RpcError::not_found(format!("stats does not serve `{other}`"))),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        Box::pin(async move {
            let root = root(&ctx);
            let made = std::fs::create_dir_all(&root);
            let check = ProbeCheck { name: "the stats folder can be written".into(), ok: made.is_ok(), detail: made.err().map(|e| format!("{}: {e}", root.display())).unwrap_or_default(), ms: 0 };
            Ok(ProbeReport { ok: check.ok, checks: vec![check] })
        })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.deliver(ctx);
        Box::pin(async { Ok(()) })
    }
}

fn main() {
    cc_stats_kit::start("CORTEX_LOG", "info", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-stats: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let mut hello = Hello::new("cc-stats", env!("CARGO_PKG_VERSION"), reports::STATS, "stats@1");
        hello.provides = vec![reports::methods::RECORD.to_owned()];
        let plugin = Arc::new(Stats { writing: Arc::new(Mutex::new(())), delivering: AtomicBool::new(false) });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-stats: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
