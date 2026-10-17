//! `cc-research` executable: serve `research@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin};
use cc_research::config::Config;
use cc_research::peers::FramePeers;
use cc_research::service::{Core, PROVIDES};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

struct ResearchPlugin {
    peers: FramePeers,
    core: OnceLock<Arc<Core>>,
    home: PathBuf,
    data: PathBuf,
}

impl ResearchPlugin {
    /// The frame context (and with it the configuration) arrives with the first call.
    fn core(&self, ctx: &Ctx) -> Arc<Core> {
        let _ = self.peers.ctx.set(ctx.clone());
        self.core
            .get_or_init(|| {
                Core::new(
                    Config::from_value(&ctx.config()),
                    self.home.clone(),
                    self.data.clone(),
                    Arc::new(self.peers.clone()),
                    Arc::new(|name: &str| std::env::var(name).ok()),
                )
            })
            .clone()
    }
}

impl Plugin for ResearchPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move { core.dispatch(&call.method, call.params, call.cancel).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move { Ok(cc_research::probe::run(&core).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.core(&ctx);
        Box::pin(async { Ok(()) })
    }

    /// Running deep jobs stop at their last saved step and are named for the successor.
    fn export_state(&self, ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move { Ok(json!({ "running_jobs": core.jobs.hand_off(Duration::from_secs(10)).await })) })
    }

    /// The predecessor's running jobs continue here.
    fn import_state(&self, ctx: Ctx, state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move {
            for id in state.get("running_jobs").and_then(Value::as_array).cloned().unwrap_or_default() {
                if let Some(id) = id.as_str() {
                    core.jobs.resume(id);
                }
            }
            Ok(())
        })
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-research: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-research: {error}");
                return 2;
            }
        };
        if let Err(error) = std::fs::create_dir_all(&env.data_dir) {
            eprintln!("cc-research: cannot create {}: {error}", env.data_dir.display());
            return 1;
        }
        let mut hello = Hello::new("cc-research", env!("CARGO_PKG_VERSION"), "research", "research@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        let plugin = Arc::new(ResearchPlugin { peers: FramePeers::default(), core: OnceLock::new(), home: env.home.clone(), data: env.data_dir.clone() });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-research: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
