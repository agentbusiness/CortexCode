//! `cc-container` executable: serve `container@1` to the frame.

use cc_container::service::{Config, Containers, PROVIDES};
use cc_protocol::contracts::container::{ContainerInfo, topics};
use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

struct ContainerPlugin {
    containers: Arc<Containers>,
    ctx: Arc<OnceLock<Ctx>>,
}

impl ContainerPlugin {
    /// The frame context becomes available with the first call; configuration arrives in the hello
    /// acknowledgement, so apply it then.
    fn remember(&self, ctx: &Ctx) {
        if self.ctx.set(ctx.clone()).is_ok() {
            self.containers.configure(Config::from_value(&ctx.config()));
        }
    }
}

impl Plugin for ContainerPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.remember(&ctx);
        let containers = self.containers.clone();
        Box::pin(async move {
            containers
                .dispatch(&call.method, call.params, call.items, call.cancel)
                .await
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.remember(&ctx);
        let containers = self.containers.clone();
        Box::pin(async move { Ok(cc_container::probe::run(&containers).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        Box::pin(async { Ok(()) })
    }

    // No state export: every container is described by its Docker labels, so a successor finds the
    // running containers again by itself.

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let containers = self.containers.clone();
        Box::pin(async move { containers.shutdown().await })
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-container: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-container: {error}");
                return 2;
            }
        };
        if let Err(error) = std::fs::create_dir_all(&env.data_dir) {
            eprintln!(
                "cc-container: cannot create {}: {error}",
                env.data_dir.display()
            );
            return 1;
        }
        let ctx_slot: Arc<OnceLock<Ctx>> = Arc::new(OnceLock::new());
        let publish_slot = ctx_slot.clone();
        let publish = Arc::new(move |info: &ContainerInfo| {
            if let (Some(ctx), Ok(data)) = (publish_slot.get(), serde_json::to_value(info)) {
                ctx.publish(topics::CHANGED, data);
            }
        });
        let containers = Arc::new(Containers::new(
            env.data_dir.clone(),
            env.home.clone(),
            Config::default(),
            std::env::var_os("PATH").unwrap_or_default(),
            std::env::var_os("HOME").map(Into::into),
            Arc::new(|name: &str| std::env::var(name).ok()),
            publish,
        ));
        let mut hello = Hello::new(
            "cc-container",
            env!("CARGO_PKG_VERSION"),
            "container",
            "container@1",
        );
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        let plugin = Arc::new(ContainerPlugin {
            containers,
            ctx: ctx_slot,
        });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-container: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
