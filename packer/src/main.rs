//! `cortex-pack`: compile a CortexCode plugin folder into one portable `.ccplugin`.
//!
//! ```text
//! cortex-pack build <plugin-dir> [--install] [--release] [-o <file.ccplugin>] [--no-build]
//! cortex-pack info <plugin-dir>
//! cortex-pack release next|newer|check <version> ...
//! ```
//!
//! `build` reads `plugin.toml`, runs the backend's build (`cargo build --release` in `backend/`)
//! and the UI's (`npm ci` when the npm root has no install, then `npm run build`), checks that every
//! declared output exists, and writes a gzip-compressed tar holding `manifest.json`, the backend
//! executable with its assets (`backend/`), every UI package's built bundle (`ui/<name>/`), the
//! shell modules (`shell/<id>/`), `plugin.toml` and the plugin's data folders (`[data] dirs`, at
//! their own paths; the files git tracks or would track in them). No source code is packed.
//!
//! A build is a dev build, versioned `<version>+dev.<UTC date>.<time>`, unless `--release` keeps
//! plugin.toml's version or `--build-id <id>` versions it `<version>+<id>`. `--install` hands the package to the frame (`CORTEX_HOME`, default
//! `~/.cortexcode`): a running frame hot-swaps to it and this command prints the outcome; a stopped
//! frame installs it at its next start. The source manager then merges the package's data into the
//! live folder (`<home>/live/plugins/<id>/`), where plugins read it. `info` prints the validated
//! plugin.toml as JSON.

mod install;
mod pack;
mod release;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "cortex-pack", version, about = "Build a CortexCode plugin folder into a portable .ccplugin")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build the plugin folder and pack it.
    Build {
        /// The plugin folder (holds plugin.toml).
        dir: PathBuf,
        /// Install the package into the frame (a running frame hot-swaps to it).
        #[arg(long)]
        install: bool,
        /// Keep plugin.toml's version (default: a dev build, `<version>+dev.<date>.<time>`).
        #[arg(long)]
        release: bool,
        /// Output file (default: <dir>/target/<id>-dev-<target>.ccplugin, or with --release
        /// <dir>/target/<id>-<version>-<target>.ccplugin).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Version the package `<version>+<id>` (the app builder passes a digest of the plugin's
        /// source, so an unchanged plugin keeps its version and is not installed again).
        #[arg(long)]
        build_id: Option<String>,
        /// Pack the existing build outputs without running the build commands.
        #[arg(long)]
        no_build: bool,
    },
    /// Print the plugin folder's validated plugin.toml as JSON.
    Info { dir: PathBuf },
    /// The version rule (for scripts/release-*.sh).
    #[command(subcommand)]
    Release(release::Release),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Build { dir, install, release, out, build_id, no_build } => {
            pack::build(&pack::Options { dir, out, run_build: !no_build, release, build_id }).and_then(|packed| {
                println!("packed {} {} ({} bytes) -> {}", packed.manifest.plugin.id, packed.manifest.plugin.version, packed.bytes, packed.path.display());
                if install { install::install(&packed.path, &install::home()) } else { Ok(()) }
            })
        }
        Command::Info { dir } => pack::read_plugin_toml(&dir).and_then(|toml| {
            serde_json::to_string_pretty(&toml).map(|json| println!("{json}")).map_err(|e| e.to_string())
        }),
        Command::Release(command) => release::run(command),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cortex-pack: {error}");
            ExitCode::FAILURE
        }
    }
}
