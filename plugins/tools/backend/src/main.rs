//! `cc-tools`: started by the frame; serves `tools@1` until shutdown.

use cc_protocol::contracts::lifecycle::Hello;
use cc_tools::service::{COMMANDS, PROVIDES, ToolsPlugin};
use std::sync::Arc;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-tools: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let outcome = runtime.block_on(async {
        let mut hello = Hello::new("cc-tools", env!("CARGO_PKG_VERSION"), "tools", "tools@1");
        hello.provides = PROVIDES.iter().map(|method| (*method).to_owned()).collect();
        hello.commands = COMMANDS.iter().map(|method| (*method).to_owned()).collect();
        // Changed skills are asked again (the catalog and the code index).
        hello.subscribes = vec![
            cc_tools::langs::SKILLS_CHANGED.to_owned(),
            cc_protocol::contracts::frame::topics::SESSION_DELETED.to_owned(),
            cc_protocol::contracts::frame::topics::PROJECT_DELETED.to_owned(),
            cc_protocol::contracts::vcs::topics::CHANGED.to_owned(),
        ];
        cc_protocol::run_plugin(hello, Arc::new(ToolsPlugin::new())).await
    });
    if let Err(error) = outcome {
        eprintln!("cc-tools: {error}");
        std::process::exit(1);
    }
    // Background jobs run in their own sessions and keep running; shells were stopped on shutdown.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
}
