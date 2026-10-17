//! basedpyright: the npm package `basedpyright` (pinned [`VERSION`]) installed into
//! `$CORTEX_HOME/toolchains/basedpyright/<version>/` with `npm install --prefix` (never globally), run
//! as `basedpyright-langserver --stdio`, else a `basedpyright-langserver` on PATH. The language
//! server has no version flag, so a candidate is accepted when the `basedpyright` CLI beside it
//! answers `--version` (the same package's two entry points).
//!
//! The interpreter: basedpyright uses `python.pythonPath` when it is set, else the project's `.venv`
//! (its own detection), else the `python` on PATH. The server of each root starts with
//! [`root_settings`] (the pack's `server_launch`): `python.pythonPath` is the session's interpreter
//! for that workspace ([`interpreter`]), by the tools plugin's rules
//! (plugins/tools/backend/src/pyenv.rs): the project's own venv (a direct child directory with
//! `pyvenv.cfg` and `bin/python`, `.venv` preferred), else the automatic venv
//! `$CORTEX_HOME/venvs/<sha256(canonical workspace path)[..16]>` once it is ready; with neither it
//! stays unset. The kit answers `workspace/configuration` with those settings and pushes them with
//! `workspace/didChangeConfiguration` after `initialized`. An automatic venv made after the root's
//! server started is picked up when that server next starts (idle stop, crash, install).
//!
//! A configured `server_path` is checked like a found server ([`probe`]): the `basedpyright` CLI
//! beside it answers `--version`; a wrapper without one beside it is accepted when it is
//! executable.
//!
//! Commands get the CLI through [`toolchain`]: the package's `node_modules/.bin` directory (it holds
//! only basedpyright's and pyright's entry points, never a Python interpreter; the tools plugin owns
//! the session's Python).

use cc_lang_kit::install::{FoundServer, Toolchains, find_existing, find_on_path, is_executable};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server's name and its toolchain directory.
pub const NAME: &str = "basedpyright";
/// The npm package.
pub const PACKAGE: &str = "basedpyright";
/// The pinned package version (npm, 2026-10-04).
pub const VERSION: &str = "1.40.2";
/// The language server's entry point.
pub const LANGSERVER: &str = "basedpyright-langserver";
/// The command-line checker (same package).
pub const CLI: &str = "basedpyright";

/// `toolchains/basedpyright/<VERSION>/node_modules/.bin`.
pub fn bin_dir(toolchains: &Toolchains) -> Result<PathBuf, String> {
    Ok(toolchains.version_dir(NAME, VERSION)?.join("node_modules").join(".bin"))
}

/// Settings answered to `workspace/configuration`, by section (`python`, `basedpyright`,
/// `basedpyright.analysis`), with `python` as given.
pub fn settings(python: Value) -> Value {
    json!({
        "python": python,
        "basedpyright": {
            "analysis": {
                // A project without its own pyright configuration is checked in pyright's
                // `standard` mode (basedpyright's default, `recommended`, reports every rule); a
                // pyrightconfig.json or [tool.basedpyright] / [tool.pyright] decides otherwise.
                "typeCheckingMode": "standard",
                // Diagnostics are asked for per file; the open files are the requested ones.
                "diagnosticMode": "openFilesOnly",
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
            }
        }
    })
}

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = vec!["--stdio".into()];
    spec.language_ids = vec![("py".into(), "python".into()), ("pyi".into(), "python".into())];
    spec.settings = settings(json!({}));
    spec.diagnostics_wait = Duration::from_secs(60);
    spec.query_wait = Duration::from_secs(60);
    spec
}

/// A virtual environment: `pyvenv.cfg` and an existing `bin/python`.
fn is_venv(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file() && dir.join("bin/python").exists()
}

/// The workspace's own venv: `.venv`, else the first (by name) direct child directory that is one.
pub fn project_env(workspace: &Path) -> Option<PathBuf> {
    let preferred = workspace.join(".venv");
    if is_venv(&preferred) {
        return Some(preferred);
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(workspace)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| is_venv(path))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// The automatic venv directory of a workspace: `<home>/venvs/<first 16 hex of SHA-256 of the
/// canonical workspace path>` (the tools plugin's `pyenv::auto_dir`).
pub fn auto_dir(home: &Path, workspace: &Path) -> PathBuf {
    let canonical = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    home.join("venvs").join(&hash[..16])
}

/// The tools plugin marks a finished automatic venv with `.cortex-ready`.
fn auto_ready(dir: &Path) -> bool {
    dir.join(".cortex-ready").is_file() && dir.join("bin/python").exists()
}

/// The interpreter of a workspace's session: the project's venv, else the ready automatic venv.
pub fn interpreter(home: &Path, workspace: &Path) -> Option<PathBuf> {
    let venv = project_env(workspace).or_else(|| Some(auto_dir(home, workspace)).filter(|dir| auto_ready(dir)))?;
    Some(venv.join("bin").join("python"))
}

/// The settings for a server of `workspace`: `python.pythonPath` is the session's interpreter when
/// there is one, else unset (basedpyright then looks for `.venv` itself, then PATH).
pub fn root_settings(home: &Path, workspace: &Path) -> Value {
    match interpreter(home, workspace) {
        Some(python) => settings(json!({"pythonPath": python.to_string_lossy()})),
        None => settings(json!({})),
    }
}

/// The first runnable language server of `candidates`: one whose sibling `basedpyright` CLI answers
/// `--version` (the language server itself has no version flag).
pub fn locate_in(candidates: &[PathBuf], cwd: Option<&Path>) -> Result<FoundServer, String> {
    let mut problems = Vec::new();
    for candidate in candidates.iter().filter(|c| is_executable(c)) {
        let Some(cli) = candidate.parent().map(|dir| dir.join(CLI)) else { continue };
        match find_existing(std::slice::from_ref(&cli), &["--version".into()], cwd, &[]) {
            Ok(found) => return Ok(FoundServer { path: candidate.clone(), version: found.version }),
            Err(problem) => problems.push(format!("{}: {problem}", candidate.display())),
        }
    }
    if problems.is_empty() {
        Err(format!("{NAME} is not installed (not in the toolchains directory or on PATH); `lang-python.install` installs {PACKAGE}@{VERSION} with npm"))
    } else {
        Err(format!("{NAME} is not installed: {}", problems.join("; ")))
    }
}

/// The version of a configured server executable: its sibling CLI's `--version`, else (a wrapper
/// with no CLI beside it) accepted when executable, with the version unknown.
pub fn probe(path: &Path, cwd: Option<&Path>) -> Result<String, String> {
    if !is_executable(path) {
        return Err(format!("{} is not an executable", path.display()));
    }
    let cli = path.parent().map(|dir| dir.join(CLI));
    match cli.filter(|cli| is_executable(cli)) {
        Some(cli) => find_existing(std::slice::from_ref(&cli), &["--version".into()], cwd, &[]).map(|found| found.version),
        None => Ok(format!("unknown version (no {CLI} beside it to ask)")),
    }
}

/// The installed server, else one on PATH.
pub fn locate(toolchains: &Toolchains, root: Option<&Path>) -> Result<FoundServer, String> {
    let mut candidates = vec![bin_dir(toolchains)?.join(LANGSERVER)];
    candidates.extend(find_on_path(LANGSERVER));
    locate_in(&candidates, root)
}

/// `npm install --prefix toolchains/basedpyright/<VERSION> basedpyright@<VERSION>`.
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    let bin = toolchains.npm_install(NAME, PACKAGE, VERSION, log)?;
    if !is_executable(&bin.join(LANGSERVER)) {
        return Err(format!("{PACKAGE}@{VERSION} is installed but {} is missing", bin.join(LANGSERVER).display()));
    }
    Ok(())
}

/// The CLI's directory for commands' PATH, once installed.
pub fn toolchain(toolchains: &Toolchains) -> Toolchain {
    match bin_dir(toolchains) {
        Ok(bin) if is_executable(&bin.join(CLI)) => Toolchain { path: vec![bin.to_string_lossy().into_owned()], env: Vec::new() },
        _ => Toolchain::default(),
    }
}
