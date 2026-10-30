//! `cc-scheduler`: CortexCode's routines (`schedule@1`, docs/SCHEDULES-PLAN.md): recurring and
//! timed work that wakes a session, hands a brief to an agent or runs a command, on a clock.
//!
//! Routines live in the graph (memory, `memory.routine.*`); the plugin holds only their copy for
//! the clock, loaded on activation, so it can be hot-swapped at any time.

pub mod data;
pub mod service;
pub mod when;

/// Start a plugin process: logging to stderr (the frame captures it), then serve until shutdown.
pub fn serve<P: cc_protocol::Plugin>(hello: cc_protocol::contracts::lifecycle::Hello, plugin: P) -> std::process::ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
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
