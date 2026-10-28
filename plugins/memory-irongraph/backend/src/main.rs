//! `cc-memory`: the memory@1 plugin process.

use cc_memory::service::{COMMANDS, Memory, MemoryPlugin, PROVIDES};
use cc_protocol::contracts::lifecycle::{Hello, SwapPolicy};
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-memory: cannot start runtime: {error}");
            std::process::exit(1);
        }
    };
    let data = std::env::var_os("CORTEX_PLUGIN_DATA").map(PathBuf::from);
    // <CORTEX_HOME>/irongraph; CORTEX_HOME, else derived from <home>/plugins-data/<id>.
    let home = std::env::var_os("CORTEX_HOME")
        .map(PathBuf::from)
        .or_else(|| data.as_ref().and_then(|d| d.parent()).and_then(|p| p.parent()).map(PathBuf::from));
    let (Some(home), Some(data)) = (home, data) else {
        eprintln!("cc-memory: CORTEX_HOME / CORTEX_PLUGIN_DATA are not set; plugins are started by the frame");
        std::process::exit(2);
    };
    let memory = Memory::new(home, data);
    let code = runtime.block_on(async {
        // Open the database while the frame finishes the handshake (model load takes seconds).
        let warm = memory.clone();
        tokio::spawn(async move {
            if let Err(error) = warm.ensure_main().await {
                tracing::error!(%error, "irongraph open failed");
            }
        });
        let mut hello = Hello::new("cc-memory", env!("CARGO_PKG_VERSION"), "memory", "memory@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        hello.commands = COMMANDS.iter().map(|m| (*m).to_owned()).collect();
        // Language plugins starting, stopping and swapping change which plugin extracts which files;
        hello.subscribes = vec![
            cc_protocol::contracts::frame::topics::PLUGIN_STATE.to_owned(),
            cc_protocol::contracts::frame::topics::PLUGIN_SWAPPED.to_owned(),
            // A project the frame opens is opened here too.
            cc_protocol::contracts::frame::topics::PROJECT_CHANGED.to_owned(),
            // A project the person deleted is dropped here too.
            cc_protocol::contracts::frame::topics::PROJECT_DELETED.to_owned(),
        ];
        // One embedded IronGraph per data directory: two instances cannot coexist.
        hello.swap = SwapPolicy::DrainFirst;
        let plugin = Arc::new(MemoryPlugin { inner: memory.clone() });
        let result = cc_protocol::run_plugin(hello, plugin).await;
        memory.close().await;
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-memory: {error}");
                1
            }
        }
    });
    // The runtime's tasks hold the database too: once they are gone, the last holder closes it.
    // A running statement gets a moment to finish.
    let started = std::time::Instant::now();
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    let tasks_ms = started.elapsed().as_millis() as u64;
    let closed = match Arc::try_unwrap(memory) {
        Ok(memory) => memory.into_graph().map(|graph| graph.close()),
        Err(_) => Some(Ok(false)),
    };
    let ms = started.elapsed().as_millis() as u64;
    match closed {
        None => tracing::info!(tasks_ms, "irongraph was never opened"),
        Some(Ok(true)) => tracing::info!(tasks_ms, ms, "irongraph closed"),
        Some(Ok(false)) => tracing::warn!(tasks_ms, ms, "irongraph still held at exit; it was flushed, the next open repairs it"),
        Some(Err(error)) => tracing::warn!(tasks_ms, ms, error = %error.message, "closing irongraph failed"),
    }
    std::process::exit(code);
}
