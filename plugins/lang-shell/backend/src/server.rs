//! bash-language-server with ShellCheck. The npm package `bash-language-server` (pinned
//! [`VERSION`]) is installed into `$CORTEX_HOME/toolchains/bash-language-server/<version>/` with
//! `npm install --prefix` (never globally) and run as `bash-language-server start`; ShellCheck is
//! the official static release binary (pinned [`SHELLCHECK_VERSION`], SHA-256 checked) unpacked into
//! `$CORTEX_HOME/toolchains/shellcheck/<version>/`. Either one already on PATH serves too, each
//! accepted only when it runs.
//!
//! bash-language-server's diagnostics are ShellCheck's: the server is pointed at the ShellCheck
//! executable through its `bashIde.shellcheckPath` setting (answered to `workspace/configuration`)
//! and the `SHELLCHECK_PATH` environment variable (read before the configuration arrives). Without
//! ShellCheck it would publish no diagnostics at all, so the server counts as available only when
//! both are.
//!
//! Commands get ShellCheck through [`toolchain`]: the installed binary's directory on PATH.

use cc_lang_kit::install::{ArchiveFormat, FoundServer, Release, Toolchains, find_existing, find_on_path, is_executable};
use cc_lang_kit::lsp::ServerSpec;
use cc_lang_kit::pack::Toolchain;
use serde_json::json;
use std::path::{Path, PathBuf};

/// The server's name and its toolchain directory.
pub const NAME: &str = "bash-language-server";
/// The npm package.
pub const PACKAGE: &str = "bash-language-server";
/// The pinned package version (npm, 2026-09-19).
pub const VERSION: &str = "5.8.1";
/// ShellCheck's toolchain directory and executable name.
pub const SHELLCHECK: &str = "shellcheck";
/// The pinned ShellCheck release (GitHub koalaman/shellcheck, 2025-08-04).
pub const SHELLCHECK_VERSION: &str = "0.11.0";

/// Files bash-language-server analyses in the background (its default plus `.zsh`).
const GLOB_PATTERN: &str = "**/*@(.sh|.inc|.bash|.command|.zsh)";

/// The official ShellCheck release for this machine, with its published SHA-256.
pub fn shellcheck_release() -> Result<Release, String> {
    let (arch, sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin.aarch64", "339b930feb1ea764467013cc1f72d09cd6b869ebf1013296ba9055ab2ffbd26f"),
        ("macos", "x86_64") => ("darwin.x86_64", "c2c15e08df0e8fbc374c335b230a7ee958c313fa5714817a59aa59f1aa594f51"),
        (os, arch) => return Err(format!("no pinned ShellCheck release for {os} {arch}; put a shellcheck binary on PATH")),
    };
    Ok(Release {
        version: SHELLCHECK_VERSION.into(),
        url: format!("https://github.com/koalaman/shellcheck/releases/download/v{SHELLCHECK_VERSION}/shellcheck-v{SHELLCHECK_VERSION}.{arch}.tar.gz"),
        sha256: sha256.into(),
        format: ArchiveFormat::TarGz,
    })
}

/// `toolchains/bash-language-server/<VERSION>/node_modules/.bin`.
pub fn bin_dir(toolchains: &Toolchains) -> Result<PathBuf, String> {
    Ok(toolchains.version_dir(NAME, VERSION)?.join("node_modules").join(".bin"))
}

/// Where the installed ShellCheck executable is: the release archive holds
/// `shellcheck-v<version>/shellcheck`.
pub fn installed_shellcheck(toolchains: &Toolchains) -> Result<PathBuf, String> {
    Ok(toolchains.version_dir(SHELLCHECK, SHELLCHECK_VERSION)?.join(format!("shellcheck-v{SHELLCHECK_VERSION}")).join(SHELLCHECK))
}

/// The ShellCheck the server uses: the installed one, else one on PATH; None when neither exists.
pub fn find_shellcheck(toolchains: &Toolchains) -> Option<PathBuf> {
    installed_shellcheck(toolchains).ok().filter(|p| is_executable(p)).or_else(|| find_on_path(SHELLCHECK).into_iter().next())
}

/// The server, pointed at `shellcheck`.
pub fn spec(shellcheck: &Path) -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = vec!["start".into()];
    spec.language_ids = ["sh", "bash", "zsh"].iter().map(|ext| ((*ext).to_owned(), "shellscript".to_owned())).collect();
    let shellcheck = shellcheck.to_string_lossy().into_owned();
    spec.env = vec![("SHELLCHECK_PATH".into(), shellcheck.clone()), ("GLOB_PATTERN".into(), GLOB_PATTERN.into())];
    spec.settings = json!({
        "bashIde": {
            "shellcheckPath": shellcheck,
            "globPattern": GLOB_PATTERN,
            // Never reach out to explainshell (offline).
            "explainshellEndpoint": "",
        }
    });
    spec
}

/// The spec for this home: the ShellCheck found now, else where the install puts it (the install
/// restarts the servers, which then find it there).
pub fn spec_for(toolchains: &Toolchains) -> ServerSpec {
    let shellcheck = find_shellcheck(toolchains).or_else(|| installed_shellcheck(toolchains).ok()).unwrap_or_else(|| PathBuf::from(SHELLCHECK));
    spec(&shellcheck)
}

/// The first of `candidates` that answers `--version`, when a ShellCheck runs too.
pub fn locate_in(candidates: &[PathBuf], shellcheck: Option<&Path>, cwd: Option<&Path>) -> Result<FoundServer, String> {
    let found = find_existing(candidates, &["--version".into()], cwd, &[]).map_err(|problem| {
        if problem == "no executable found" {
            format!("{NAME} is not installed (not in the toolchains directory or on PATH); `lang-shell.install` installs {PACKAGE}@{VERSION} with npm and ShellCheck {SHELLCHECK_VERSION}")
        } else {
            format!("{NAME} is not installed: {problem}")
        }
    })?;
    let Some(shellcheck) = shellcheck else {
        return Err(format!("ShellCheck is not installed, so {NAME} would report no diagnostics; `lang-shell.install` installs ShellCheck {SHELLCHECK_VERSION}"));
    };
    find_existing(&[shellcheck.to_path_buf()], &["--version".into()], cwd, &[]).map_err(|problem| format!("ShellCheck at {} does not run: {problem}", shellcheck.display()))?;
    Ok(FoundServer { path: found.path, version: format!("{} (ShellCheck at {})", found.version, shellcheck.display()) })
}

/// The installed server, else one on PATH, with a ShellCheck.
pub fn locate(toolchains: &Toolchains, root: Option<&Path>) -> Result<FoundServer, String> {
    let mut candidates = vec![bin_dir(toolchains)?.join(NAME)];
    candidates.extend(find_on_path(NAME));
    locate_in(&candidates, find_shellcheck(toolchains).as_deref(), root)
}

/// Install ShellCheck (download, SHA-256, unpack) and the npm package.
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    let release = shellcheck_release()?;
    toolchains.download_release(SHELLCHECK, &release, log)?;
    let shellcheck = installed_shellcheck(toolchains)?;
    if !is_executable(&shellcheck) {
        return Err(format!("ShellCheck {SHELLCHECK_VERSION} is unpacked but {} is missing", shellcheck.display()));
    }
    let bin = toolchains.npm_install(NAME, PACKAGE, VERSION, log)?;
    if !is_executable(&bin.join(NAME)) {
        return Err(format!("{PACKAGE}@{VERSION} is installed but {} is missing", bin.join(NAME).display()));
    }
    Ok(())
}

/// The installed ShellCheck's directory for commands' PATH.
pub fn toolchain(toolchains: &Toolchains) -> Toolchain {
    match installed_shellcheck(toolchains) {
        Ok(shellcheck) if is_executable(&shellcheck) => {
            let dir = shellcheck.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            Toolchain { path: vec![dir], env: Vec::new() }
        }
        _ => Toolchain::default(),
    }
}
