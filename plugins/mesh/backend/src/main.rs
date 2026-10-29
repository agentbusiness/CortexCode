//! `cc-mesh` executable: serve `mesh@1` to the frame.

use cc_mesh::service::{Mesh, PROVIDES, SUBSCRIBES};
use cc_protocol::contracts::lifecycle::{Hello, ProbeCheck, ProbeReport};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, run_plugin};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

struct MeshPlugin {
    mesh: Arc<OnceLock<Arc<Mesh>>>,
}

impl MeshPlugin {
    fn mesh(&self, ctx: &Ctx) -> Arc<Mesh> {
        let mesh = self.mesh.get_or_init(|| Arc::new(Mesh::new(&ctx.env.home))).clone();
        mesh.attach(ctx);
        mesh
    }
}

impl Plugin for MeshPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let mesh = self.mesh(&ctx);
        Box::pin(async move {
            let Call { method, params, items, cancel } = call;
            mesh.handle(&method, params, items, cancel).await
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let mesh = self.mesh(&ctx);
        Box::pin(async move {
            let started = std::time::Instant::now();
            let node = mesh.me.hex();
            let checks = vec![ProbeCheck { name: "identity".into(), ok: node.len() == 64, detail: format!("node {}", &node[..node.len().min(10)]), ms: started.elapsed().as_millis() as u64 }];
            Ok(ProbeReport::from_checks(checks))
        })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let mesh = self.mesh(&ctx);
        Box::pin(async move {
            if mesh.endpoint.get().is_none()
                && let Err(error) = mesh.start_net().await
            {
                tracing::warn!(%error, "the mesh endpoint did not start; pairing and members are unavailable until it does");
            }
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        let mesh = self.mesh(&ctx);
        Box::pin(async move {
            if topic == cc_protocol::contracts::frame::topics::PLUGIN_STATE || topic == cc_protocol::contracts::frame::topics::PLUGIN_SWAPPED {
                // A model plugin here may have started, stopped or swapped.
                mesh.forget_card();
                mesh.models_changed();
            } else if topic == "model.state" {
                // This computer's model loaded, unloaded or now borrows another app's: the card
                // changes, and the apps on this machine should know at once.
                mesh.forget_card();
                mesh.models_changed();
                mesh.event(&topic, &data);
                mesh.push_card().await;
            } else if topic.starts_with("project.")
                || (topic == cc_protocol::contracts::memory::topics::KNOWLEDGE_CHANGED && data.get("kind").and_then(Value::as_str) == Some(cc_protocol::contracts::memory::knowledge::DESCRIPTION))
            {
                mesh.projects_changed().await;
            } else {
                mesh.event(&topic, &data);
            }
        })
    }

    fn shutdown(&self, ctx: Ctx) -> BoxFuture<'static, ()> {
        let mesh = self.mesh(&ctx);
        Box::pin(async move {
            cc_mesh::local::withdraw(cc_mesh::SOFTWARE, &mesh.me.hex());
            if let Some(endpoint) = mesh.endpoint.get() {
                endpoint.close().await;
            }
        })
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
            eprintln!("cc-mesh: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let mut hello = Hello::new("cc-mesh", env!("CARGO_PKG_VERSION"), "mesh", "mesh@1");
    hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
    hello.subscribes = SUBSCRIBES.iter().map(|t| (*t).to_owned()).collect();
    let plugin = Arc::new(MeshPlugin { mesh: Arc::new(OnceLock::new()) });
    if let Err(error) = runtime.block_on(run_plugin(hello, plugin)) {
        eprintln!("cc-mesh: {error}");
        std::process::exit(1);
    }
}
