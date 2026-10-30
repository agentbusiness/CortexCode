//! `cc-chatflow`: CortexCode's conversation and coding agent loop (`chatflow@1`).
//!
//! The foreground turn follows a router-first flow recast for coding: router -> parallel host reads
//! from the code graph -> DECIDE with `missing` first -> answer, act (the coding agent loop) or
//! delegate. The same agent loop serves background agents through `chatflow.agent_run`.
//!
//! The plugin keeps no private session state: every decision, read, tool call, steer and status is
//! a journal entry (frame), so it can be hot-swapped mid-run and its successor continues from the
//! journal.

pub mod afteraction;
pub mod agent_loop;
pub mod attachments;
pub mod chatflow;
pub mod checks;
pub mod context;
pub mod contract;
pub mod data;
pub mod fit;
pub mod graph;
pub mod flow;
pub mod guides;
pub mod host;
pub mod impact;
pub mod journal;
pub mod learn;
pub mod live;
pub mod llm;
pub mod manage;
pub mod night;
pub mod plan;
pub mod prewarm;
pub mod locks;
pub mod projection;
pub mod proposal;
pub mod prompts;
pub mod reads;
pub mod references;
pub mod replay;
pub mod repetition;
pub mod schemas;
pub mod skillsel;
pub mod typeerrors;
pub mod verifiers;

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
