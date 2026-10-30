//! `cc-model-bonsai`: the CortexCode model plugin process (`model@1`) on PrismML's llama.cpp.
//!
//! Started by the frame. `cc-model-bonsai install <variant> [--models-dir DIR]` installs a catalog
//! variant (download with resume, digest verification) without a frame. Without a frame there is
//! no graph: the digests are not kept, so the app hashes the files once at its next start.

use cc_model_bonsai::plugin::{ModelPlugin, PROVIDES};
use cc_model_bonsai::runtime::expand_home;
use cc_model_bonsai::variants::{self, VerifyCache};
use cc_protocol::contracts::lifecycle::{Hello, SwapPolicy};
use std::sync::Arc;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
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
        // A build tag (`CC_BUILD_TAG` at compile time) distinguishes rebuilt binaries of one version.
        let version = match option_env!("CC_BUILD_TAG") {
            Some(tag) if !tag.is_empty() => format!("{}+{tag}", env!("CARGO_PKG_VERSION")),
            _ => env!("CARGO_PKG_VERSION").to_owned(),
        };
        let mut hello = Hello::new("model-bonsai", &version, "model", "model@1");
        hello.provides = PROVIDES.iter().map(|method| (*method).to_owned()).collect();
        // Two instances (or two engines) cannot hold a model at once: drain the old one first.
        hello.swap = SwapPolicy::DrainFirst;
        // A deleted session's conversation is dropped.
        hello.subscribes = vec![cc_protocol::contracts::frame::topics::SESSION_DELETED.to_owned()];
        match cc_protocol::run_plugin(hello, Arc::new(ModelPlugin::default())).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-model-bonsai: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}

async fn install(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("usage: cc-model-bonsai install <variant> [--models-dir DIR]");
        return 2;
    };
    let Some(spec) = variants::find(id) else {
        eprintln!("unknown variant `{id}`; known: {}", variants::CATALOG.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", "));
        return 2;
    };
    let option = |name: &str| args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).map(|value| expand_home(value));
    let home = std::env::var("CORTEX_HOME").map(|home| expand_home(&home)).unwrap_or_else(|_| expand_home("~/.cortexcode"));
    let models_dir = option("--models-dir").unwrap_or_else(|| home.join("models"));
    let cache = VerifyCache::new(&models_dir);
    let report = |progress: variants::Progress| {
        eprintln!("{}", serde_json::to_string(&progress).unwrap_or_default());
    };
    match variants::install(spec, &models_dir, &cache, &report).await {
        Ok(path) => {
            println!("{}", serde_json::json!({"variant": spec.id, "path": path}));
            0
        }
        Err(error) => {
            eprintln!("install failed: {error}");
            1
        }
    }
}
