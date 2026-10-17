//! `cc-agents`: delegation, named agents, scheduler, verification and merge steward (`agents@1`).

fn main() -> std::process::ExitCode {
    match cc_agents::agents::Agents::new() {
        Ok(plugin) => cc_agents::serve(cc_agents::agents::hello(), plugin),
        Err(error) => {
            eprintln!("cc-agents: the shipped data set is invalid: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
