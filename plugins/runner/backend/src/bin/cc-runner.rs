//! `cc-runner`: builds for a manager (`runner@1`).

fn main() -> std::process::ExitCode {
    cc_runner::serve(cc_runner::service::hello(), cc_runner::service::Runner::new())
}
