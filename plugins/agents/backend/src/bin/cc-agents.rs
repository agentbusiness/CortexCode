//! `cc-agents`: delegation, named agents, scheduler, verification and merge steward (`agents@1`).

fn main() -> std::process::ExitCode {
    // `cc-agents frontier-mcp <socket>`: a frontier CLI's MCP server, a pipe to its run here.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(cc_agents::lend::RELAY_ARG) {
        return cc_agents::lend::relay(&args.next().unwrap_or_default());
    }
    match cc_agents::agents::Agents::new() {
        Ok(plugin) => cc_agents::serve(cc_agents::agents::hello(), plugin),
        Err(error) => {
            eprintln!("cc-agents: the shipped data set is invalid: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
