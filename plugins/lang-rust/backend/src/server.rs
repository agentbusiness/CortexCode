//! rust-analyzer: the server of the workspace's own toolchain (`rustup which rust-analyzer` run in
//! the workspace, so a `rust-toolchain.toml` there picks it), else one on PATH or in Cargo's bin
//! directory, each accepted only when `rust-analyzer --version` runs (a rustup proxy without the
//! component exists but does not run). Installed with `rustup component add rust-analyzer` when
//! rustup is present; nothing else is touched.
//!
//! rustup is always run with `RUSTUP_AUTO_INSTALL=0`: finding the server must never download a
//! toolchain a workspace pins.

use cc_lang_kit::install::{FoundServer, PROBE_TIMEOUT, cargo_home, find_existing, find_on_path, is_executable, run, run_logged, tail};
use cc_lang_kit::lsp::ServerSpec;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "rust-analyzer";

/// No implicit toolchain installs by rustup (proxies included).
const RUSTUP_ENV: &[(&str, &str)] = &[("RUSTUP_AUTO_INSTALL", "0")];

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.language_ids = vec![("rs".into(), "rust".into())];
    spec.env = RUSTUP_ENV.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    // rust-analyzer's own target directory (target/rust-analyzer): its `cargo check` and build
    // scripts then never wait on, or make wait, the agent's and the user's cargo builds for the
    // build directory lock.
    let settings = json!({"cargo": {"targetDir": true}});
    spec.initialization_options = settings.clone();
    spec.settings = json!({"rust-analyzer": settings});
    // `experimental/serverStatus`: rust-analyzer says when its workspace is loaded.
    spec.experimental_capabilities = json!({"serverStatusNotification": true});
    // The first analysis of a workspace (cargo metadata, build scripts, proc macros, indexing, the
    // on-save `cargo check`) takes long on a cold start.
    spec.diagnostics_wait = Duration::from_secs(120);
    spec.query_wait = Duration::from_secs(90);
    spec
}

fn rustup() -> Option<PathBuf> {
    find_on_path("rustup").into_iter().next().or_else(|| cargo_home().map(|home| home.join("bin").join("rustup")).filter(|p| is_executable(p)))
}

/// A runnable rust-analyzer for `root` (or the plugin's working directory).
pub fn locate(root: Option<&Path>) -> Result<FoundServer, String> {
    let mut problems = Vec::new();
    let mut candidates = Vec::new();
    if let Some(rustup) = rustup() {
        match run(&rustup, &["which".into(), NAME.into()], root, RUSTUP_ENV, PROBE_TIMEOUT) {
            Ok(ran) if ran.success => candidates.push(PathBuf::from(ran.stdout.trim())),
            Ok(ran) => problems.push(format!("`rustup which {NAME}`: {}", tail(&ran.stderr, 2))),
            Err(error) => problems.push(error),
        }
    }
    candidates.extend(find_on_path(NAME));
    if let Some(home) = cargo_home() {
        candidates.push(home.join("bin").join(NAME));
    }
    match find_existing(&candidates, &["--version".into()], root, RUSTUP_ENV) {
        Ok(found) => Ok(found),
        Err(problem) => {
            if problem != "no executable found" {
                problems.push(problem);
            }
            if problems.is_empty() {
                Err(format!("{NAME} is not installed (not found through rustup, on PATH or in Cargo's bin directory)"))
            } else {
                Err(format!("{NAME} is not installed: {}", problems.join("; ")))
            }
        }
    }
}

/// `rustup component add rust-analyzer` for the toolchain active in the plugin's working directory.
pub fn install(log: &mut Vec<String>) -> Result<(), String> {
    let rustup = rustup().ok_or_else(|| {
        format!("rustup is not installed; {NAME} comes with rustup (`rustup component add {NAME}`), or put a {NAME} binary on PATH")
    })?;
    let toolchain = run_logged(&rustup, &["show".into(), "active-toolchain".into()], None, RUSTUP_ENV, PROBE_TIMEOUT, log)?;
    log.push(format!("adding {NAME} to {}", toolchain.trim()));
    run_logged(&rustup, &["component".into(), "add".into(), NAME.into()], None, RUSTUP_ENV, Duration::from_secs(900), log)?;
    Ok(())
}
