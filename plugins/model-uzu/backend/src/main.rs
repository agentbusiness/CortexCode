//! `cc-model-uzu`: the CortexCode model plugin process (`model@1`) on uzu.
//!
//! Started by the frame. `cc-model-uzu install <variant> [--models-dir DIR]` installs a catalog
//! variant with its vision tower (download with resume, digest verification) without a frame;
//! `cc-model-uzu install-vision [variant] [--models-dir DIR]` fetches the variant's vision tower
//! (only the vision tensors of the upstream checkpoint, by HTTP range requests). Without a frame
//! there is no graph: the digests and the vision install record are not kept, so the app hashes
//! the files once at its next start and records the install at its next vision install.
// `cc_protocol::RpcError` (128 bytes) is the protocol's error type for every contract method; the
// toolchain this plugin needs (1.99) lints large `Err` variants, the repository's (1.94) does not.
#![allow(clippy::result_large_err)]

use cc_model_uzu::plugin::{ModelPlugin, PROVIDES};
use cc_model_uzu::runtime::{Paths, expand_home};
use cc_model_uzu::variants::{self, VerifyCache};
use cc_protocol::contracts::lifecycle::{Hello, SwapPolicy};
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = runtime.block_on(async move {
        if args.first().map(String::as_str) == Some("install") {
            return install(&args[1..]).await;
        }
        if args.first().map(String::as_str) == Some("install-vision") {
            return install_vision(&args[1..]).await;
        }
        // A build tag (`CC_BUILD_TAG` at compile time) distinguishes rebuilt binaries of one version.
        let version = match option_env!("CC_BUILD_TAG") {
            Some(tag) if !tag.is_empty() => format!("{}+{tag}", env!("CARGO_PKG_VERSION")),
            _ => env!("CARGO_PKG_VERSION").to_owned(),
        };
        let mut hello = Hello::new("model-uzu", &version, "model", "model@1");
        hello.provides = PROVIDES.iter().map(|method| (*method).to_owned()).collect();
        // Two instances cannot hold the model at once: drain the old one first.
        hello.swap = SwapPolicy::DrainFirst;
        // A deleted session's conversation is dropped, in memory and on disk.
        hello.subscribes = vec![cc_protocol::contracts::frame::topics::SESSION_DELETED.to_owned()];
        match cc_protocol::run_plugin(hello, Arc::new(ModelPlugin::default())).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-model-uzu: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}

async fn install(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("usage: cc-model-uzu install <variant> [--models-dir DIR]");
        return 2;
    };
    let Some(spec) = variants::find(id) else {
        eprintln!(
            "unknown variant `{id}`; known: {}",
            variants::CATALOG.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", ")
        );
        return 2;
    };
    let option = |name: &str| args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).map(|value| expand_home(value));
    let home = std::env::var("CORTEX_HOME").map(|home| expand_home(&home)).unwrap_or_else(|_| expand_home("~/.cortexcode"));
    let models_dir = Paths::new(home, PathBuf::new(), option("--models-dir")).models_dir;
    let cache = VerifyCache::new(&models_dir, None);
    let report = |progress: variants::Progress| {
        eprintln!("{}", serde_json::to_string(&progress).unwrap_or_default());
    };
    let path = match variants::install(spec, &models_dir, &cache, &report).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    // The variant's vision tower comes with it, as in the app.
    let vision = match spec.vision {
        Some(tower) => match cc_model_uzu::vision::install::install(tower, spec.id, &spec.local_dir(&models_dir), &cache, &report).await {
            Ok(vision) => Some(vision),
            Err(error) => {
                eprintln!("the model is installed, but its vision tower failed: {error} (retry with `cc-model-uzu install-vision {}`)", spec.id);
                return 1;
            }
        },
        None => None,
    };
    println!("{}", serde_json::json!({"variant": spec.id, "path": path, "vision": vision}));
    0
}

async fn install_vision(args: &[String]) -> i32 {
    let id = args.first().filter(|arg| !arg.starts_with("--")).map(String::as_str).unwrap_or(variants::DEFAULT_VARIANT);
    let Some(spec) = variants::find(id) else {
        eprintln!("unknown variant `{id}`");
        return 2;
    };
    let Some(tower) = spec.vision else {
        eprintln!("variant `{id}` has no vision tower in this plugin's catalog");
        return 2;
    };
    let option = |name: &str| args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).map(|value| expand_home(value));
    let home = std::env::var("CORTEX_HOME").map(|home| expand_home(&home)).unwrap_or_else(|_| expand_home("~/.cortexcode"));
    let models_dir = Paths::new(home, PathBuf::new(), option("--models-dir")).models_dir;
    let cache = VerifyCache::new(&models_dir, None);
    let report = |progress: variants::Progress| {
        eprintln!("{}", serde_json::to_string(&progress).unwrap_or_default());
    };
    match cc_model_uzu::vision::install::install(tower, spec.id, &spec.local_dir(&models_dir), &cache, &report).await {
        Ok(path) => {
            println!("{}", serde_json::json!({"variant": spec.id, "vision": path}));
            0
        }
        Err(error) => {
            eprintln!("vision install failed: {error}");
            1
        }
    }
}
