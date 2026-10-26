//! `cc-updater` executable: serve `update@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin};
use cc_updater::PROVIDES;
use cc_updater::service::Updater;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct UpdaterPlugin {
    updater: Arc<Updater>,
    stop: CancellationToken,
}

impl Plugin for UpdaterPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.updater.attach(&ctx);
        let updater = self.updater.clone();
        Box::pin(async move { updater.handle(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.updater.attach(&ctx);
        let report = self.updater.probe();
        Box::pin(async move { Ok(report) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.updater.attach(&ctx);
        self.updater.spawn_loop(self.stop.clone());
        Box::pin(async { Ok(()) })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        self.stop.cancel();
        Box::pin(async {})
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-updater: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-updater: {error}");
                return 2;
            }
        };
        if let Err(error) = std::fs::create_dir_all(&env.data_dir) {
            eprintln!("cc-updater: cannot create {}: {error}", env.data_dir.display());
            return 1;
        }
        let mut hello = Hello::new("cc-updater", env!("CARGO_PKG_VERSION"), "update", "update@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        let plugin = Arc::new(UpdaterPlugin { updater: Updater::new(env.data_dir.clone()), stop: CancellationToken::new() });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-updater: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
