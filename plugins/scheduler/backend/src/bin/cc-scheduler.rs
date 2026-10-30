//! `cc-scheduler`: routines, recurring and timed work (`schedule@1`).

fn main() -> std::process::ExitCode {
    cc_scheduler::serve(cc_scheduler::service::hello(), cc_scheduler::service::Scheduler::new())
}
