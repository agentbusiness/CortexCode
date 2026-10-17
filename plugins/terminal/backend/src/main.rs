//! `cc-terminal` executable: serve `terminal@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::contracts::terminal::methods;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, run_plugin};
use cc_terminal::Terminals;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

struct TerminalPlugin {
    terms: Arc<Terminals>,
    ctx: Arc<OnceLock<Ctx>>,
}

impl Plugin for TerminalPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let _ = self.ctx.set(ctx);
        let terms = self.terms.clone();
        Box::pin(async move { terms.dispatch(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let _ = self.ctx.set(ctx);
        Box::pin(async { Ok(cc_terminal::probe::run().await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let _ = self.ctx.set(ctx);
        Box::pin(async { Ok(()) })
    }

    // No state export: terminals are interactive sessions; the UI reopens them after a swap.

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let terms = self.terms.clone();
        Box::pin(async move { terms.close_all().await })
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
            eprintln!("cc-terminal: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        if let Err(error) = PluginEnv::from_env() {
            eprintln!("cc-terminal: {error}");
            return 2;
        }
        let ctx_slot: Arc<OnceLock<Ctx>> = Arc::new(OnceLock::new());
        let publish_slot = ctx_slot.clone();
        let emit = Arc::new(move |topic: &str, data: Value| {
            if let Some(ctx) = publish_slot.get() {
                ctx.publish(topic, data);
            }
        });
        let terms = Arc::new(Terminals::new(emit));
        let mut hello = Hello::new("cc-terminal", env!("CARGO_PKG_VERSION"), "terminal", "terminal@1");
        hello.provides = [methods::OPEN, methods::WRITE, methods::RESIZE, methods::CLOSE, methods::LIST, methods::SNAPSHOT]
            .iter()
            .map(|m| (*m).to_owned())
            .collect();
        let plugin = Arc::new(TerminalPlugin { terms: terms.clone(), ctx: ctx_slot });
        let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("cc-terminal: cannot handle SIGTERM: {error}");
                return 1;
            }
        };
        let outcome = tokio::select! {
            outcome = run_plugin(hello, plugin) => outcome,
            _ = terminate.recv() => Ok(()),
            _ = tokio::signal::ctrl_c() => Ok(()),
        };
        // No terminal outlives this instance, however it ends (shutdown, frame gone, SIGTERM).
        terms.close_all().await;
        match outcome {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-terminal: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
