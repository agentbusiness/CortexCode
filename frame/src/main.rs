//! `cortex-frame`: the CortexCode frame.
//!
//! `cortex-frame [--config <cortex.toml>] [--headless] [--home <dir>]` (without `--config`: the app
//! bundle's `Contents/Resources/cortex.toml`)

// The frame relays requests with their JSON key order intact (serde_json `preserve_order`, see
// Cargo.toml), which makes `serde_json::Value` and so `RpcError` 136 bytes, just over clippy's
// 128-byte `result_large_err` threshold. Every frame method returns `Result<_, RpcError>`; boxing
// the error at each of them would cost an allocation per error for no measurable gain.
#![allow(clippy::result_large_err)]

mod access;
mod config;
mod fswatch;
mod home;
mod http;
mod hub;
mod journal;
mod machine;
mod package;
mod power;
mod shell;
mod state;
mod store;
mod supervisor;
mod uiplugins;
mod units;

use clap::Parser;
use config::Config;
use home::{FrameInfo, Home, LockOutcome};
use hub::{FRAME_VERSION, Frame, FrameParts, PeerKind};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(name = "cortex-frame", version, about = "CortexCode frame: plugin supervisor, router and journal owner")]
struct Cli {
    /// Frame configuration file (default: `Resources/cortex.toml` of the app bundle this binary is in).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Do not start the Electron shell.
    #[arg(long)]
    headless: bool,
    /// CortexCode home (overrides CORTEX_HOME and the config's `[frame] home`).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Safe mode: plugins use their packaged data only, never the live data in the code folder
    /// (for when an edit to a skill, guide or prompt broke the app).
    #[arg(long)]
    safe: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The app bundle's own config: `Contents/MacOS/CortexCode` reads `Contents/Resources/cortex.toml`.
    let Some(config_path) = cli.config.clone().or_else(bundle_config) else {
        eprintln!("cortex-frame: no --config given and no Resources/cortex.toml beside this binary's app bundle");
        return ExitCode::FAILURE;
    };
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cortex-frame: {error}");
            return ExitCode::from(2);
        }
    };
    // Precedence: --home, then CORTEX_HOME, then [frame] home, then ~/.cortexcode.
    let home_root = match (&cli.home, std::env::var("CORTEX_HOME").ok().filter(|v| !v.is_empty()), &config.home) {
        (Some(home), _, _) => config::absolutize(home),
        (None, Some(env), _) => config::absolutize(&config::expand_tilde(&env)),
        (None, None, Some(home)) => config::resolve(&config.base, home),
        (None, None, None) => config::expand_tilde("~/.cortexcode"),
    };
    let home = Home::new(home_root);
    if let Err(error) = home.create_layout() {
        eprintln!("cortex-frame: cannot create {}: {error}", home.root.display());
        return ExitCode::from(2);
    }
    let lock = match home::acquire_lock(&home) {
        Ok(LockOutcome::Acquired(lock)) => lock,
        Ok(LockOutcome::Held(info)) => {
            match info {
                Some(info) => eprintln!(
                    "cortex-frame: a frame is already running for {} (pid {}, port {}); refusing to start",
                    home.root.display(),
                    info.pid,
                    info.port
                ),
                None => eprintln!("cortex-frame: a frame is already running for {}; refusing to start", home.root.display()),
            }
            return ExitCode::from(3);
        }
        Err(error) => {
            eprintln!("cortex-frame: cannot lock {}: {error}", home.lock_file().display());
            return ExitCode::from(2);
        }
    };
    if let Err(error) = init_logging(&home) {
        eprintln!("cortex-frame: cannot open the frame log: {error}");
        return ExitCode::from(2);
    }
    raise_fd_limit();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("frame").build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cortex-frame: cannot start the runtime: {error}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(run(cli, config, home));
    drop(lock);
    code
}

/// Raise the soft open-file limit (macOS defaults to 256) toward the hard limit; plugins inherit it.
fn raise_fd_limit() {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit/setrlimit with a valid struct pointer.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let per_process: libc::rlim_t = 10_240; // OPEN_MAX on macOS: the most setrlimit accepts
        let wanted = limit.rlim_max.min(per_process);
        if wanted > limit.rlim_cur {
            limit.rlim_cur = wanted;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

fn init_logging(home: &Home) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(home.logs().join("frame.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false))
        .with(tracing_subscriber::fmt::layer().with_writer(std::sync::Mutex::new(file)).with_ansi(false).with_target(false))
        .try_init()
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn run(cli: Cli, config: Config, home: Home) -> ExitCode {
    match serve(cli, config, home).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "frame failed");
            eprintln!("cortex-frame: {error}");
            ExitCode::from(1)
        }
    }
}

async fn serve(cli: Cli, config: Config, home: Home) -> Result<(), String> {
    let machine = tokio::task::spawn_blocking(machine::measure).await.map_err(|e| e.to_string())?;
    // The frame's records live in the memory plugin's store, written through one ordered outbox.
    let memory = store::MemoryLink::new();
    let (outbox, outgoing) = store::Outbox::new();
    store::spawn_writer(memory.clone(), outgoing);
    let journal = journal::Journal::open(outbox.clone(), memory.clone());
    let state = state::StateStore::open(outbox);
    // The app's prebuilt plugins: a version this home has not taken from the app yet is installed.
    let bundled = config.packages.as_deref().map(|packages| units::queue_bundled(&home.plugins(), packages)).unwrap_or_default();
    let units = units::Units::load(&home.plugins(), &home.state(), config.overrides.clone(), machine.memory_bytes).map_err(|e| format!("plugins: {e}"))?;
    units::mark_bundled(&home.plugins(), &bundled);
    let token = home::random_token(32).map_err(|e| format!("token: {e}"))?;

    // Plugin socket. We hold the home lock, so an existing socket file is stale.
    let socket = home.socket_path();
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|e| format!("cannot bind {}: {e}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .map_err(|e| format!("cannot bind 127.0.0.1:{}: {e}", config.port))?;
    let port = tcp.local_addr().map_err(|e| e.to_string())?.port();

    let want_ui = config.ui && !cli.headless;
    let code = config.code.clone().unwrap_or_else(|| home.code());
    if cli.safe {
        tracing::warn!("safe mode: plugins read their packaged data only");
    }
    let frame = Frame::new(FrameParts { config, home: home.clone(), token: token.clone(), socket: socket.clone(), headless: cli.headless, machine, journal, state, memory, units, code, safe: cli.safe });
    frame.port.store(port, Ordering::SeqCst);
    // The Mac does not idle-sleep while the frame runs (window open or not); shutdown releases it.
    if let Err(error) = frame.power.hold(power::FRAME_HOLDER, power::FRAME_REASON, power::FRAME_PEER) {
        tracing::error!(%error, "cannot keep the Mac awake");
    }

    // Accept plugin connections.
    let accept_frame = frame.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    accept_frame.attach(stream, PeerKind::Plugin);
                }
                Err(error) => {
                    tracing::error!(%error, "plugin socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    // HTTP / WebSocket API.
    let app = http::router(frame.clone());
    let http_shutdown = frame.shutdown.clone();
    let server = tokio::spawn(async move {
        let outcome = axum::serve(tcp, app).with_graceful_shutdown(async move { http_shutdown.cancelled().await }).await;
        if let Err(error) = outcome {
            tracing::error!(%error, "http server failed");
        }
    });

    let info = FrameInfo {
        pid: std::process::id(),
        port,
        token,
        socket: socket.display().to_string(),
        version: FRAME_VERSION.into(),
    };
    home::write_frame_info(&home.frame_json(), &info).map_err(|e| format!("cannot write frame.json: {e}"))?;
    tracing::info!(home = %home.root.display(), port, socket = %socket.display(), version = FRAME_VERSION, "frame listening");

    frame.start_ui_watch();
    units::watch_incoming(&frame);

    // Plugins in order; then the UI shell.
    let start_frame = frame.clone();
    tokio::spawn(async move {
        supervisor::start_all(start_frame.clone()).await;
        if want_ui && !start_frame.is_shutting_down() {
            start_frame.shell.launch(&start_frame);
        }
    });

    tokio::select! {
        _ = wait_for_signal() => {}
        _ = frame.shell.quit.notified() => tracing::info!("the user quit the app: shutting down"),
    }
    // A second SIGINT/SIGTERM during the graceful shutdown forces an immediate exit.
    let force_frame = frame.clone();
    let force_json = home.frame_json();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::warn!("second signal: forcing exit");
        force_frame.shutting_down.store(true, Ordering::SeqCst);
        supervisor::kill_remaining(&force_frame).await;
        let _ = std::fs::remove_file(force_json);
        std::process::exit(130);
    });
    shutdown(&frame).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    let _ = std::fs::remove_file(home.frame_json());
    let _ = std::fs::remove_file(&socket);
    tracing::info!("frame stopped");
    Ok(())
}

async fn wait_for_signal() {
    let (Ok(mut interrupt), Ok(mut terminate)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
        tracing::error!("cannot install signal handlers; running until killed");
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = interrupt.recv() => tracing::info!("SIGINT: shutting down"),
        _ = terminate.recv() => tracing::info!("SIGTERM: shutting down"),
    }
}

async fn shutdown(frame: &Arc<Frame>) {
    frame.shutting_down.store(true, Ordering::SeqCst);
    frame.shutdown.cancel();
    // The shell sees the cancellation and stops Electron; give it its grace period.
    for _ in 0..60 {
        if frame.shell.pid() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Every plugin but the memory plugin first; then the records still in the outbox go to its
    // store, and it stops last.
    supervisor::stop_all_except(frame, store::MEMORY).await;
    match tokio::time::timeout(Duration::from_secs(30), frame.journal.close()).await {
        Ok(Ok(())) => {},
        Ok(Err(error)) => tracing::error!(%error, "the store writer stopped before shutdown completed"),
        Err(_) => tracing::error!("the store did not take the last changes within 30 s"),
    }
    supervisor::stop_all(frame).await;
    supervisor::kill_remaining(frame).await;
    frame.close_clients();
    frame.power.release_all();
}

/// `Contents/Resources/cortex.toml` of the app bundle this binary runs from, when there is one.
fn bundle_config() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let config = exe.parent()?.parent()?.join("Resources").join("cortex.toml");
    config.is_file().then_some(config)
}
