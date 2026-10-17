//! clangd, and the compilation database it reads.
//!
//! **Locate**: Apple's clangd first (`xcrun --find clangd`, then `/usr/bin/clangd`), then a clangd
//! on PATH, then the one installed into the toolchains directory; each accepted only when
//! `clangd --version` runs.
//!
//! **Install** (`lang-cpp.install`): nothing when a system clangd runs; otherwise the pinned clangd
//! release (github.com/clangd/clangd, SHA-256 pinned here) into `toolchains/clangd/<version>`.
//!
//! **Running clangd**: which `compile_commands.json` to read is a property of the workspace: the
//! server of each root starts ([`launch`], the pack's `server_launch`) with
//! `--compile-commands-dir=<dir>` for the database found in the root, else in its `build*`
//! directories (`build` first). When there is none, clangd guesses compile flags (and reports wrong
//! errors for anything that needs include paths or defines), so every `diagnostics` answer for that
//! root gets one warning on line 1 of each C/C++ file it covers ([`annotate`], the pack's
//! `annotate_diagnostics`) saying the database is missing and how to create it
//! (`cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` for CMake projects).

use cc_lang_kit::cc_protocol::contracts::lang::Diagnostic;
use cc_lang_kit::install::{ArchiveFormat, FoundServer, Release, Toolchains, find_existing, find_in, is_executable, run};
use cc_lang_kit::lsp::{Launch, ServerSpec};
use cc_lang_kit::pack::Toolchain;
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "clangd";
/// The code of the missing-database warning.
pub const NO_DATABASE: &str = "no-compile-commands";
pub const CLANGD_VERSION: &str = "23.1.0";
/// SHA-256 of clangd-mac-23.1.0.zip (the release asset's published digest).
const CLANGD_SHA256: &str = "1082e6638223b785ca2daf0939f13afcd0bb95c84ee9a4bbaff4745365159253";
/// The command that creates a compilation database for a CMake project.
pub const CMAKE_EXPORT: &str = "cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON";

/// What locating reads, explicit so tests can give their own.
#[derive(Debug, Clone)]
pub struct Host {
    pub path: Option<OsString>,
    /// `/usr/bin/xcrun` (absent without the Xcode command line tools).
    pub xcrun: PathBuf,
    /// `/usr/bin/clangd`.
    pub system_clangd: PathBuf,
}

impl Host {
    pub fn current() -> Self {
        Self { path: std::env::var_os("PATH").filter(|v| !v.is_empty()), xcrun: PathBuf::from("/usr/bin/xcrun"), system_clangd: PathBuf::from("/usr/bin/clangd") }
    }
}

pub fn release() -> Release {
    Release {
        version: CLANGD_VERSION.into(),
        url: format!("https://github.com/clangd/clangd/releases/download/{CLANGD_VERSION}/clangd-mac-{CLANGD_VERSION}.zip"),
        sha256: CLANGD_SHA256.into(),
        format: ArchiveFormat::Zip,
    }
}

/// `toolchains/clangd/<version>/clangd_<version>/bin` (the archive's layout).
pub fn installed_bin(toolchains: &Toolchains) -> Result<PathBuf, String> {
    Ok(toolchains.version_dir(NAME, CLANGD_VERSION)?.join(format!("clangd_{CLANGD_VERSION}")).join("bin"))
}

/// A clangd on the system (Xcode's, /usr/bin, PATH), without the installed one.
pub fn system_clangd(host: &Host) -> Result<FoundServer, String> {
    let mut candidates = Vec::new();
    if is_executable(&host.xcrun)
        && let Ok(ran) = run(&host.xcrun, &["--find".into(), NAME.into()], None, &[], Duration::from_secs(30))
        && ran.success
    {
        candidates.push(PathBuf::from(ran.stdout.trim()));
    }
    candidates.push(host.system_clangd.clone());
    candidates.extend(find_in(NAME, host.path.clone()));
    find_existing(&candidates, &["--version".into()], None, &[])
}

/// The clangd to run: a system one, else the installed one.
pub fn find_clangd(toolchains: &Toolchains, host: &Host) -> Result<FoundServer, String> {
    match system_clangd(host) {
        Ok(found) => Ok(found),
        Err(system) => {
            let installed = installed_bin(toolchains)?.join(NAME);
            find_existing(std::slice::from_ref(&installed), &["--version".into()], None, &[]).map_err(|_| {
                let why = if system == "no executable found" { "not found through xcrun, at /usr/bin/clangd or on PATH".to_owned() } else { system };
                format!("{NAME} is not installed ({why}); `lang-cpp.install` installs clangd {CLANGD_VERSION} into {}", toolchains.root().display())
            })
        }
    }
}

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = clangd_args(None);
    spec.language_ids = ["c", "h"].iter().map(|e| ((*e).to_owned(), "c".to_owned())).chain(["cc", "cpp", "cxx", "hpp", "hh", "hxx"].iter().map(|e| ((*e).to_owned(), "cpp".to_owned()))).collect();
    // clangd's own extension: report file status, so a first index build is visible.
    spec.initialization_options = json!({"clangdFileStatus": true});
    spec
}

/// The clangd that serves (see [`find_clangd`]).
pub fn locate(toolchains: &Toolchains, host: &Host) -> Result<FoundServer, String> {
    find_clangd(toolchains, host)
}

/// Use a system clangd when one runs, else install the pinned release.
pub fn install(toolchains: &Toolchains, host: &Host, log: &mut Vec<String>) -> Result<(), String> {
    if let Ok(found) = system_clangd(host) {
        log.push(format!("using {} at {}; nothing installed", found.version, found.path.display()));
        return Ok(());
    }
    toolchains.download_release(NAME, &release(), log)?;
    let exe = installed_bin(toolchains)?.join(NAME);
    if !is_executable(&exe) {
        return Err(format!("the clangd release has no {}", exe.display()));
    }
    Ok(())
}

/// The installed clangd's directory, when one is installed.
pub fn toolchain(toolchains: &Toolchains) -> Toolchain {
    match installed_bin(toolchains) {
        Ok(bin) if is_executable(&bin.join(NAME)) => Toolchain { path: vec![bin.to_string_lossy().into_owned()], env: Vec::new() },
        _ => Toolchain::default(),
    }
}

/// The directory holding the workspace's `compile_commands.json`: `root`, else its `build*`
/// directories (`build` first, then by name).
pub fn compile_commands_dir(root: &Path) -> Option<PathBuf> {
    if root.join("compile_commands.json").is_file() {
        return Some(root.to_path_buf());
    }
    let mut builds: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("build")))
        .map(|e| e.path())
        .filter(|dir| dir.join("compile_commands.json").is_file())
        .collect();
    builds.sort_by_key(|d| (d.file_name().is_none_or(|n| n != "build"), d.clone()));
    builds.into_iter().next()
}

/// What the missing-database warning says, for a workspace.
pub fn missing_database_message(root: &Path) -> String {
    let how = if root.join("CMakeLists.txt").is_file() {
        format!("`{CMAKE_EXPORT}` creates build/compile_commands.json")
    } else {
        "generate one with the build system (CMake: -DCMAKE_EXPORT_COMPILE_COMMANDS=ON; Make: `bear -- make`)".to_owned()
    };
    format!("no compile_commands.json in the workspace root or its build* directories: clangd guesses compile flags, so include and macro errors may be wrong; {how}")
}

/// The clangd arguments for a workspace.
pub fn clangd_args(database: Option<&Path>) -> Vec<String> {
    let mut args = vec!["--log=error".to_owned()];
    if let Some(dir) = database {
        args.push(format!("--compile-commands-dir={}", dir.display()));
    }
    args
}

/// How clangd starts for `root`: pointed at the workspace's compilation database.
pub fn launch(root: &Path) -> Launch {
    Launch { args: Some(clangd_args(compile_commands_dir(root).as_deref())), ..Launch::default() }
}

/// Without a compilation database: one warning on line 1 of every C/C++ file the answer covers.
pub fn annotate(root: &Path, files: &[String], diagnostics: &mut Vec<Diagnostic>) {
    if compile_commands_dir(root).is_some() {
        return;
    }
    let spec = spec();
    let note = missing_database_message(root);
    for file in files.iter().filter(|file| spec.language_id(Path::new(file)).is_some()) {
        diagnostics.push(Diagnostic {
            path: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 1,
            severity: "warning".into(),
            message: note.clone(),
            code: Some(NO_DATABASE.into()),
            source: Some("lang-cpp".into()),
        });
    }
}
