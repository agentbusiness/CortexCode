use appcfg::config::load_from_file;
use appcfg::retry::retry;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: appcfg <config-file>");
        return ExitCode::from(2);
    };
    match retry(3, Duration::from_millis(100), |_| load_from_file(&path)) {
        Ok(cfg) => {
            println!("{} listens on port {} ({} retries)", cfg.name, cfg.port, cfg.retries);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
