//! `cc-agents`: CortexCode's background agents (`agents@1`): delegation of a goal into a task tree
//! with contracts (planned by the model over the code graph), named agents in copy-on-write
//! workspaces, the scheduler, host-run contract verification, the project board and the merge
//! steward.
//!
//! Agents, tasks and board messages live in the graph (memory) and the agents' journals (frame);
//! the plugin holds only its scheduler queue, rebuilt on activation from the agent records.
//!
//! This plugin is self-contained (its own workspace, only the protocol as a dependency); a few
//! helpers (`host`, `journal`, `llm`, `contract`, `schemas`, the `data` loader) are its own copies of the chat flow's.

pub mod agents;
pub mod contract;
pub mod control;
pub mod data;
pub mod graph;
pub mod host;
pub mod journal;
pub mod llm;
pub mod plan;
pub mod scheduler;
pub mod schemas;

/// Start a plugin process: logging to stderr (the frame captures it), then serve until shutdown.
pub fn serve<P: cc_protocol::Plugin>(hello: cc_protocol::contracts::lifecycle::Hello, plugin: P) -> std::process::ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(cc_protocol::run_plugin(hello, std::sync::Arc::new(plugin)));
    // Background runs hold no state worth waiting for: everything is in the journal.
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("plugin stopped: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
