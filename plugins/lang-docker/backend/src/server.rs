//! The language servers, hadolint and the docker CLI.
//!
//! Two servers serve Docker files: dockerfile-language-server-nodejs (`docker-langserver`) for
//! Dockerfiles and Microsoft's compose-language-service (`docker-compose-langserver`) for Compose
//! files, both npm packages run with Node.js over stdio. `lang@1`'s kit drives one server process
//! per workspace, so that process is this plugin's own executable in router mode
//! (`cc-lang-docker lsp-router`, [`crate::router`]), which starts both and routes each document to
//! its server. Both are installed with `npm install --prefix` into
//! `$CORTEX_HOME/toolchains/<name>/<version>/`; the compose server needs Node.js 22 or newer.
//!
//! hadolint (the static Dockerfile linter, one self-contained binary) is installed from its GitHub
//! release with a pinned SHA-256 into `toolchains/hadolint/<version>/`; it lints without a Docker
//! daemon, which `docker build --check` needs.
//!
//! The docker CLI is found on PATH or where Docker Desktop, OrbStack, Rancher Desktop, Colima's
//! Homebrew install or `~/.docker/bin` put it.

use cc_lang_kit::install::{ArchiveFormat, FoundServer, PROBE_TIMEOUT, Release, Toolchains, find_on_path, is_executable, npm_installed_version, run};
use cc_lang_kit::lsp::ServerSpec;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server's name in descriptions and diagnostics: the two servers behind the router.
pub const NAME: &str = "docker-langserver+compose-langserver";

/// One npm-packaged language server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NpmServer {
    /// Toolchain directory name.
    pub toolchain: &'static str,
    pub package: &'static str,
    pub version: &'static str,
    /// The package's bin script, relative to the package directory.
    pub script: &'static str,
    /// The oldest Node.js major it runs on.
    pub node_major: u32,
}

pub const DOCKERFILE_SERVER: NpmServer =
    NpmServer { toolchain: "dockerfile-language-server", package: "dockerfile-language-server-nodejs", version: "0.15.0", script: "bin/docker-langserver", node_major: 18 };

pub const COMPOSE_SERVER: NpmServer =
    NpmServer { toolchain: "compose-language-service", package: "@microsoft/compose-language-service", version: "1.0.0", script: "bin/docker-compose-langserver", node_major: 22 };

pub const HADOLINT_VERSION: &str = "2.14.0";

/// hadolint's static macOS release for this machine, with the SHA-256 GitHub publishes for it.
pub fn hadolint_release() -> Option<Release> {
    let (asset, sha256) = match std::env::consts::ARCH {
        "aarch64" => ("hadolint-macos-arm64", "3625e2e9f43dcfe7bd38738a5f5520ed50ce39ed28485266e6803dd7bc197b10"),
        "x86_64" => ("hadolint-macos-x86_64", "2b69a853433f1eca522ffb921cd490bd1321424d03331fd8390f93b7fb4a02e9"),
        _ => return None,
    };
    Some(Release {
        version: HADOLINT_VERSION.into(),
        url: format!("https://github.com/hadolint/hadolint/releases/download/v{HADOLINT_VERSION}/{asset}"),
        sha256: sha256.into(),
        format: ArchiveFormat::Binary { file_name: "hadolint".into() },
    })
}

/// The installed hadolint executable, when present.
pub fn hadolint(toolchains: &Toolchains) -> Option<PathBuf> {
    let path = toolchains.version_dir("hadolint", HADOLINT_VERSION).ok()?.join("hadolint");
    is_executable(&path).then_some(path)
}

/// Where docker CLIs live besides PATH (Docker Desktop, OrbStack, Rancher Desktop, Homebrew).
fn docker_locations() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/usr/local/bin/docker"), PathBuf::from("/opt/homebrew/bin/docker"), PathBuf::from("/Applications/Docker.app/Contents/Resources/bin/docker")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in [".docker/bin", ".orbstack/bin", ".rd/bin"] {
            out.push(home.join(dir).join("docker"));
        }
    }
    out
}

/// The docker CLI: the first on PATH, else the first at a known location. `on_path` says which.
pub fn docker_cli() -> Option<(PathBuf, bool)> {
    if let Some(found) = find_on_path("docker").into_iter().next() {
        return Some((found, true));
    }
    docker_locations().into_iter().find(|p| is_executable(p)).map(|p| (p, false))
}

/// The directory to add to commands' PATH for a docker CLI not on PATH: the directory of the
/// executable itself (symlinks resolved), which holds docker and its credential helpers only.
pub fn docker_dir() -> Option<PathBuf> {
    let (docker, on_path) = docker_cli()?;
    if on_path {
        return None;
    }
    let real = std::fs::canonicalize(&docker).unwrap_or(docker);
    real.parent().map(Path::to_path_buf)
}

fn node_candidates() -> Vec<PathBuf> {
    let mut out = find_on_path("node");
    for known in ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/opt/homebrew/opt/node@22/bin/node", "/opt/homebrew/opt/node@24/bin/node"] {
        let path = PathBuf::from(known);
        if !out.contains(&path) && is_executable(&path) {
            out.push(path);
        }
    }
    out
}

/// `v22.23.2` -> 22.
pub fn node_major(version: &str) -> Option<u32> {
    version.trim().trim_start_matches('v').split('.').next()?.parse().ok()
}

/// A Node.js at least `major`, with its version.
pub fn find_node(major: u32) -> Result<(PathBuf, String), String> {
    let mut seen = Vec::new();
    for node in node_candidates() {
        match run(&node, &["--version".into()], None, &[], PROBE_TIMEOUT) {
            Ok(ran) if ran.success => {
                let version = ran.stdout.trim().to_owned();
                if node_major(&version).is_some_and(|m| m >= major) {
                    return Ok((node, version));
                }
                seen.push(format!("{} is {version}", node.display()));
            }
            Ok(ran) => seen.push(format!("{} --version failed ({})", node.display(), ran.status)),
            Err(error) => seen.push(error),
        }
    }
    if seen.is_empty() {
        Err(format!("Node.js {major} or newer is needed and none was found (PATH, Homebrew)"))
    } else {
        Err(format!("Node.js {major} or newer is needed: {}", seen.join("; ")))
    }
}

impl NpmServer {
    /// The installed bin script, when this exact version is installed.
    pub fn script_path(&self, toolchains: &Toolchains) -> Result<PathBuf, String> {
        let prefix = toolchains.version_dir(self.toolchain, self.version)?;
        if npm_installed_version(&prefix, self.package).as_deref() != Some(self.version) {
            return Err(format!("{}@{} is not installed in {}", self.package, self.version, prefix.display()));
        }
        let script = prefix.join("node_modules").join(self.package).join(self.script);
        if !script.is_file() {
            return Err(format!("{} is missing", script.display()));
        }
        Ok(script)
    }
}

/// What the router runs: Node.js and the two servers' scripts.
#[derive(Debug, Clone, PartialEq)]
pub struct Children {
    pub node: PathBuf,
    pub node_version: String,
    pub dockerfile: PathBuf,
    pub compose: PathBuf,
}

impl Children {
    pub fn version(&self) -> String {
        format!("docker-langserver {}, docker-compose-langserver {} (node {})", DOCKERFILE_SERVER.version, COMPOSE_SERVER.version, self.node_version)
    }
}

/// Both servers installed and a Node.js that runs them.
pub fn children(toolchains: &Toolchains) -> Result<Children, String> {
    let mut problems = Vec::new();
    let dockerfile = DOCKERFILE_SERVER.script_path(toolchains).map_err(|e| problems.push(e)).ok();
    let compose = COMPOSE_SERVER.script_path(toolchains).map_err(|e| problems.push(e)).ok();
    let node = find_node(DOCKERFILE_SERVER.node_major.max(COMPOSE_SERVER.node_major)).map_err(|e| problems.push(e)).ok();
    match (node, dockerfile, compose) {
        (Some((node, node_version)), Some(dockerfile), Some(compose)) => Ok(Children { node, node_version, dockerfile, compose }),
        _ => Err(problems.join("; ")),
    }
}

pub fn spec() -> ServerSpec {
    let mut spec = ServerSpec::new(NAME);
    spec.args = vec!["lsp-router".into()];
    spec.version_args = vec!["lsp-router".into(), "--version".into()];
    // The plugin's own file names and patterns (`Dockerfile`, `Dockerfile.dev`, `compose.yaml`,
    // `docker-compose.prod.yml`), so other YAML is never opened; `api.Dockerfile` also by extension.
    let ids = |names: &[&str], id: &str| names.iter().map(|name| ((*name).to_owned(), id.to_owned())).collect::<Vec<_>>();
    spec.file_names = [ids(crate::DOCKERFILE_NAMES, "dockerfile"), ids(crate::DOCKERFILE_PATTERNS, "dockerfile"), ids(crate::STANDARD_COMPOSE, "dockercompose"), ids(crate::COMPOSE_PATTERNS, "dockercompose")].concat();
    spec.language_ids = ids(&["Dockerfile", "dockerfile", "Containerfile", "containerfile"], "dockerfile");
    // dockerfile-language-server asks for `docker.languageserver.diagnostics` per document; the
    // defaults (deprecated MAINTAINER, casing, empty continuation lines... as warnings) are kept.
    spec.settings = json!({"docker": {"languageserver": {"diagnostics": {}, "formatter": {}}}});
    // Both servers validate in-process and publish within about a second of a change.
    spec.diagnostics_wait = Duration::from_secs(20);
    spec.query_wait = Duration::from_secs(15);
    spec.init_timeout = Duration::from_secs(30);
    spec
}

/// The router (this executable) when both servers can run.
pub fn locate(toolchains: &Toolchains) -> Result<FoundServer, String> {
    let children = children(toolchains)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this plugin's executable: {e}"))?;
    Ok(FoundServer { path: exe, version: children.version() })
}

/// Install both servers (npm, into the toolchains directory), then hadolint (optional: a failure
/// is logged and does not fail the install).
pub fn install(toolchains: &Toolchains, log: &mut Vec<String>) -> Result<(), String> {
    let (_, node_version) = find_node(COMPOSE_SERVER.node_major)?;
    log.push(format!("node {node_version}"));
    for server in [DOCKERFILE_SERVER, COMPOSE_SERVER] {
        toolchains.npm_install(server.toolchain, server.package, server.version, log)?;
    }
    match hadolint_release() {
        Some(release) => {
            if let Err(error) = toolchains.download_release("hadolint", &release, log) {
                log.push(format!("hadolint was not installed (optional): {error}"));
            }
        }
        None => log.push(format!("hadolint has no macOS release for {}", std::env::consts::ARCH)),
    }
    Ok(())
}
