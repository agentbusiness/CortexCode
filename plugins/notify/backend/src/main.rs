//! `cc-notify` executable: serve `notify@1` to the frame.

use cc_notify::native::{MacNative, NativeDelivery, locate_helper};
use cc_notify::service::{Config, CtxBus, Notifier, PROVIDES, SUBSCRIBES};
use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, run_plugin};
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

struct NotifyPlugin {
    notifier: OnceLock<Arc<Notifier>>,
}

impl NotifyPlugin {
    /// Built on first contact: the configuration arrives in the frame's hello acknowledgement.
    fn notifier(&self, ctx: &Ctx) -> Arc<Notifier> {
        self.notifier
            .get_or_init(|| {
                let config = Config::from_value(&ctx.config());
                let data_dir = ctx.env.data_dir.clone();
                let (activations_tx, activations_rx) = mpsc::unbounded_channel();
                let helper = locate_helper(config.helper_app.clone());
                if let Err(reason) = &helper {
                    tracing::warn!(%reason, "the CortexCode notification app is unavailable; notifications stay in CortexCode");
                }
                let native = Arc::new(MacNative::new(helper, config.request_authorization, data_dir.join("activations.jsonl"), activations_tx));
                // Like any app: ask for permission to send notifications when it starts, if macOS
                // has not had an answer yet.
                {
                    let native = native.clone();
                    tokio::spawn(async move { native.ask_if_undecided().await });
                }
                let notifier = Notifier::new(native, activations_rx, data_dir);
                notifier.attach(Arc::new(CtxBus(ctx.clone())));
                notifier
            })
            .clone()
    }
}

impl Plugin for NotifyPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move { notifier.handle(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move { Ok(notifier.probe().await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move {
            notifier.drain_spool().await;
            Ok(())
        })
    }

    fn export_state(&self, ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move { Ok(notifier.export_state()) })
    }

    fn import_state(&self, ctx: Ctx, state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move {
            notifier.import_state(state);
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move { notifier.event(&topic, data).await })
    }

    fn shutdown(&self, ctx: Ctx) -> BoxFuture<'static, ()> {
        let notifier = self.notifier(&ctx);
        Box::pin(async move { notifier.shutdown().await })
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
            eprintln!("cc-notify: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let mut hello = Hello::new("cc-notify", env!("CARGO_PKG_VERSION"), "notify", "notify@1");
    hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
    hello.subscribes = SUBSCRIBES.iter().map(|t| (*t).to_owned()).collect();
    let plugin = Arc::new(NotifyPlugin { notifier: OnceLock::new() });
    if let Err(error) = runtime.block_on(run_plugin(hello, plugin)) {
        eprintln!("cc-notify: {error}");
        std::process::exit(1);
    }
}
