//! `cc-vcs` executable: serve `vcs@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::contracts::vcs::topics;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin, to_value};
use cc_vcs::service::{Config, PROVIDES, Vcs};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

struct VcsPlugin {
    vcs: Arc<Vcs>,
    ctx: Arc<OnceLock<Ctx>>,
}

impl VcsPlugin {
    /// The frame context becomes available with the first call; configuration arrives in the hello
    /// acknowledgement, so apply it then.
    fn remember(&self, ctx: &Ctx) {
        if self.ctx.set(ctx.clone()).is_ok() {
            self.vcs.configure(&Config::from_value(&ctx.config()));
        }
    }
}

impl Plugin for VcsPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.remember(&ctx);
        let vcs = self.vcs.clone();
        Box::pin(async move { vcs.dispatch(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.remember(&ctx);
        let vcs = self.vcs.clone();
        Box::pin(async move { Ok(cc_vcs::probe::run(&vcs).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        let vcs = self.vcs.clone();
        // The GitHub account comes from the store, whose plugin may start later.
        tokio::spawn(async move {
            if let Err(error) = vcs.github.load(&ctx).await {
                tracing::error!(error = %error.message, "the GitHub settings could not be read from the store");
            }
        });
        Box::pin(async { Ok(()) })
    }

    fn export_state(&self, _ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        let roots = self.vcs.watches.roots();
        Box::pin(async move { to_value(&json!({ "watched": roots })) })
    }

    fn import_state(&self, ctx: Ctx, state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        let vcs = self.vcs.clone();
        Box::pin(async move {
            let roots: Vec<String> = state
                .get("watched")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            for root in roots {
                // A root that disappeared meanwhile is simply not watched again.
                if let Ok(repo) = cc_vcs::repo::require(&vcs.git, &root).await {
                    vcs.watches.ensure(&repo, &root);
                }
            }
            Ok(())
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        self.vcs.watches.clear();
        Box::pin(async {})
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
            eprintln!("cc-vcs: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-vcs: {error}");
                return 2;
            }
        };
        let ctx_slot: Arc<OnceLock<Ctx>> = Arc::new(OnceLock::new());
        let publish_slot = ctx_slot.clone();
        let publish = Arc::new(move |event: cc_protocol::contracts::vcs::ChangedEvent| {
            if let (Some(ctx), Ok(data)) = (publish_slot.get(), serde_json::to_value(&event)) {
                ctx.publish(topics::CHANGED, data);
            }
        });
        let vcs = match Vcs::new(env.data_dir.clone(), &Config::default(), publish) {
            Ok(vcs) => Arc::new(vcs),
            Err(error) => {
                eprintln!("cc-vcs: {error}");
                return 1;
            }
        };
        let mut hello = Hello::new("cc-vcs", env!("CARGO_PKG_VERSION"), "vcs", "vcs@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        let plugin = Arc::new(VcsPlugin { vcs, ctx: ctx_slot });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-vcs: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
