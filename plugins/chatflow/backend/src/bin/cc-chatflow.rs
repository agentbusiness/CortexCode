//! `cc-chatflow`: the CortexCode chat flow and coding agent loop (`chatflow@1`).

fn main() -> std::process::ExitCode {
    match cc_chatflow::chatflow::Chatflow::new() {
        Ok(plugin) => cc_chatflow::serve(cc_chatflow::chatflow::hello(), plugin),
        Err(error) => {
            eprintln!("cc-chatflow: the shipped data set is invalid: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
