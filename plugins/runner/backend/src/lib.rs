//! `cc-runner`: a CortexCode that builds for a manager (`runner@1`, docs/RUNNER.md).
//!
//! A job is a worktree of the source's mirror and a chat session that carries out a finished
//! plan. The plugin prepares the worktree, starts the plan, commits after each task that ended
//! done, counts the run's facts from the session's journal and the worktree, and reports them
//! (`runner.beat`). It builds into the proving home and lands the job on the main branch.
//!
//! Jobs live in the graph (`RunnerJob`, `RunnerTask`, `RunnerCheckpoint`, `RunnerRepo`); the
//! plugin keeps their copy, loaded on activation, so it can be hot-swapped at any time. What a
//! run did is read from its session's journal again.

pub mod data;
pub mod process;
pub mod service;
pub mod store;
pub mod track;
pub mod work;

/// Start a plugin process: logging to stderr (the frame captures it), then serve until shutdown.
pub fn serve<P: cc_protocol::Plugin>(hello: cc_protocol::contracts::lifecycle::Hello, plugin: P) -> std::process::ExitCode {
    cc_stats_kit::start("CORTEX_LOG", "info", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(cc_protocol::run_plugin(hello, std::sync::Arc::new(plugin)));
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("plugin stopped: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
