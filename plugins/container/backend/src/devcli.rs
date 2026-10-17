//! The Dev Containers CLI (`@devcontainers/cli`, the reference implementation of the devcontainer
//! spec) for devcontainer.json files with `features`.
//!
//! Why the CLI rather than an own implementation of features: a feature is an OCI artifact
//! (`ghcr.io/devcontainers/features/node:1`), a tarball URL or a local folder; installing one means
//! resolving semver tags against a registry (with its token flow and the user's credential
//! helpers), ordering features by `dependsOn` (fetching dependencies) and `installsAfter` and
//! `overrideFeatureInstallOrder`, generating a layered Dockerfile whose install scripts see their
//! options and `_REMOTE_USER`/`_CONTAINER_USER` (and their homes, read from the base image), and
//! merging every feature's metadata (`containerEnv`, `mounts`, `privileged`, `capAdd`,
//! `securityOpt`, `init`, `entrypoint`, lifecycle commands, which run before the configuration's)
//! into the container. The CLI does exactly that, is what VS Code and Codespaces use, and follows
//! the spec as it evolves; a second implementation would drift from it. So `devcontainer up`
//! builds, creates or reuses (by this plugin's labels, passed as `--id-label`) and starts the
//! container and runs `initializeCommand` and every lifecycle command; `devcontainer
//! read-configuration --include-merged-configuration` gives the merged `remoteUser` and
//! `remoteEnv`. Commands and terminals then run with `docker exec` like in every other project, so
//! timeouts, cancellation (the in-container kill) and output streaming behave the same.
//!
//! The CLI is installed on first use into `$CORTEX_HOME/toolchains/devcontainers-cli/<version>`
//! with `npm install --prefix` (staged in a sibling directory and renamed into place, never global),
//! or taken from the plugin config (`devcontainer_cli`). It needs Node.js (`node` on PATH, at
//! Homebrew's locations, or configured).

use crate::config::DevcontainerCli;
use crate::docker::{COMPOSE_VARIABLES, ExecSettings};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub const PACKAGE: &str = "@devcontainers/cli";
/// The pinned CLI version (plugin config `devcontainer_cli_version` changes it).
pub const DEFAULT_VERSION: &str = "0.75.0";
/// `$CORTEX_HOME/toolchains/<TOOLCHAIN>/<version>`.
pub const TOOLCHAIN: &str = "devcontainers-cli";
/// Where Homebrew puts node; searched after PATH (an app started from the Dock has a minimal PATH).
pub const NODE_WELL_KNOWN: &[&str] = &["/opt/homebrew/bin/node", "/usr/local/bin/node"];

/// A runnable CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    /// The program and its leading arguments: `[node, <script>]` for an installed CLI,
    /// `[<executable>]` for a configured one.
    pub argv0: Vec<String>,
    /// `PATH` of the CLI's processes: docker's (with its directory) plus node's directory.
    pub path_env: OsString,
    /// The installed package version (`None` for a configured executable).
    pub version: Option<String>,
}

impl Cli {
    /// A CLI process with piped output, killed when dropped. Compose variables of the plugin's
    /// environment are cleared; `env` is added.
    pub fn command(&self, args: &[String], env: &[(String, String)]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.argv0[0]);
        for variable in COMPOSE_VARIABLES {
            command.env_remove(variable);
        }
        command
            .args(&self.argv0[1..])
            .args(args)
            .env("PATH", &self.path_env)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    pub fn describe(&self) -> String {
        match &self.version {
            Some(version) => format!(
                "{PACKAGE} {version} ({})",
                self.argv0.last().map(String::as_str).unwrap_or_default()
            ),
            None => format!("devcontainer CLI {}", self.argv0[0]),
        }
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// A version is one path segment: letters, digits, `.`, `-`, `+`, `_`, not starting with `.`.
fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && !version.starts_with('.')
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

/// `toolchains/devcontainers-cli/<version>`.
pub fn version_dir(toolchains: &Path, version: &str) -> Result<PathBuf, String> {
    if !valid_version(version) {
        return Err(format!("`{version}` is not a version of {PACKAGE}"));
    }
    Ok(toolchains.join(TOOLCHAIN).join(version))
}

/// The CLI's script and version in an npm prefix, read from the package's package.json (`bin`).
pub fn installed_script(prefix: &Path) -> Option<(PathBuf, String)> {
    let package = prefix.join("node_modules").join(PACKAGE);
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(package.join("package.json")).ok()?).ok()?;
    let version = manifest.get("version")?.as_str()?.to_owned();
    let bin = match manifest.get("bin")? {
        Value::String(path) => path.clone(),
        Value::Object(map) => map.get("devcontainer")?.as_str()?.to_owned(),
        _ => return None,
    };
    let script = package.join(bin);
    script.is_file().then_some((script, version))
}

/// `npm install --prefix <staging> --no-audit --no-fund @devcontainers/cli@<version>`.
pub fn install_argv(staging: &Path, version: &str) -> Vec<String> {
    vec![
        "install".into(),
        "--prefix".into(),
        staging.to_string_lossy().into_owned(),
        "--no-audit".into(),
        "--no-fund".into(),
        format!("{PACKAGE}@{version}"),
    ]
}

/// Where the CLI comes from.
#[derive(Debug, Clone)]
pub struct Setup {
    /// Plugin config `devcontainer_cli`: an existing `devcontainer` executable, used as is.
    pub configured: Option<String>,
    pub version: String,
    /// Plugin config `node`.
    pub node: Option<String>,
    /// `$CORTEX_HOME/toolchains`.
    pub toolchains: PathBuf,
    /// `PATH` the CLI and npm run with (docker's, which has the docker CLI's directory).
    pub path_env: OsString,
    pub home: Option<PathBuf>,
}

/// What [`Setup::locate`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Located {
    Ready(Cli),
    /// Not installed yet; installing needs node (found at this path) and npm.
    Installable {
        node: PathBuf,
        npm: PathBuf,
    },
}

impl Setup {
    /// `node`: the configured path, else on PATH, else Homebrew's locations.
    pub fn node(&self) -> Result<PathBuf, String> {
        if let Some(configured) = self.node.as_deref().filter(|c| !c.is_empty()) {
            let path = expand_home(configured, self.home.as_deref());
            return if executable(&path) {
                Ok(path)
            } else {
                Err(format!(
                    "the configured node `{}` is not an executable file",
                    path.display()
                ))
            };
        }
        std::env::split_paths(&self.path_env)
            .map(|dir| dir.join("node"))
            .chain(NODE_WELL_KNOWN.iter().map(PathBuf::from))
            .find(|p| executable(p))
            .ok_or_else(|| {
                format!(
                    "devcontainer features need the Dev Containers CLI, which needs Node.js: no `node` on PATH ({}) or in {} (install Node.js or set `node` in the plugin config)",
                    self.path_env.to_string_lossy(),
                    NODE_WELL_KNOWN.join(", ")
                )
            })
    }

    fn path_with(&self, dir: Option<&Path>) -> OsString {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&self.path_env).collect();
        if let Some(dir) = dir
            && !dirs.iter().any(|d| d == dir)
        {
            dirs.push(dir.to_path_buf());
        }
        std::env::join_paths(dirs).unwrap_or_else(|_| self.path_env.clone())
    }

    /// The CLI if it is configured or installed; else what installing it needs. `Err` when it can
    /// be neither used nor installed.
    pub fn locate(&self) -> Result<Located, String> {
        if let Some(configured) = self.configured.as_deref().filter(|c| !c.is_empty()) {
            let path = expand_home(configured, self.home.as_deref());
            if !executable(&path) {
                return Err(format!(
                    "the configured devcontainer CLI `{}` is not an executable file",
                    path.display()
                ));
            }
            // A node script's `#!/usr/bin/env node` needs node on PATH.
            let node_dir = self
                .node()
                .ok()
                .and_then(|n| n.parent().map(Path::to_path_buf));
            return Ok(Located::Ready(Cli {
                argv0: vec![path.to_string_lossy().into_owned()],
                path_env: self.path_with(node_dir.as_deref()),
                version: None,
            }));
        }
        let node = self.node()?;
        let node_dir = node.parent().map(Path::to_path_buf);
        let dir = version_dir(&self.toolchains, &self.version)?;
        if let Some((script, version)) = installed_script(&dir)
            && version == self.version
        {
            return Ok(Located::Ready(Cli {
                argv0: vec![
                    node.to_string_lossy().into_owned(),
                    script.to_string_lossy().into_owned(),
                ],
                path_env: self.path_with(node_dir.as_deref()),
                version: Some(version),
            }));
        }
        let npm = node_dir
            .as_ref()
            .map(|d| d.join("npm"))
            .filter(|p| executable(p))
            .or_else(|| {
                std::env::split_paths(&self.path_env)
                    .map(|d| d.join("npm"))
                    .find(|p| executable(p))
            })
            .ok_or_else(|| {
                format!(
                    "npm is not installed next to {} or on PATH; it is needed to install {PACKAGE}",
                    node.display()
                )
            })?;
        Ok(Located::Installable { node, npm })
    }

    /// A fresh staging directory next to the version directory.
    pub fn staging(&self) -> Result<PathBuf, String> {
        let parent = self.toolchains.join(TOOLCHAIN);
        std::fs::create_dir_all(&parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(parent.join(format!(".npm-{}-{nanos}", std::process::id())))
    }

    /// The npm process installing the CLI into `staging`.
    pub fn npm_command(&self, node: &Path, npm: &Path, staging: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(npm);
        command
            .args(install_argv(staging, &self.version))
            .current_dir(staging)
            .env("PATH", self.path_with(node.parent()))
            .env("npm_config_update_notifier", "false")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// After npm finished in `staging`: check it installed this version, then move it into place
    /// (a concurrent install that got there first is kept). Returns the CLI.
    pub fn commit(&self, node: &Path, staging: &Path) -> Result<Cli, String> {
        let outcome = match installed_script(staging) {
            Some((_, version)) if version == self.version => Ok(()),
            Some((_, version)) => Err(format!(
                "npm installed {PACKAGE}@{version}, not {}",
                self.version
            )),
            None => Err(format!(
                "npm finished but {PACKAGE} is not in {}",
                staging.display()
            )),
        };
        if let Err(error) = outcome {
            let _ = std::fs::remove_dir_all(staging);
            return Err(error);
        }
        let target = version_dir(&self.toolchains, &self.version)?;
        if let Err(error) = std::fs::rename(staging, &target) {
            let _ = std::fs::remove_dir_all(staging);
            if installed_script(&target).is_none_or(|(_, v)| v != self.version) {
                return Err(format!(
                    "cannot move {} to {}: {error}",
                    staging.display(),
                    target.display()
                ));
            }
        }
        let (script, version) = installed_script(&target)
            .ok_or_else(|| format!("{PACKAGE} is not in {} after installing", target.display()))?;
        Ok(Cli {
            argv0: vec![
                node.to_string_lossy().into_owned(),
                script.to_string_lossy().into_owned(),
            ],
            path_env: self.path_with(node.parent()),
            version: Some(version),
        })
    }
}

/// `devcontainer up`: build (features included), create or reuse the container carrying `labels`
/// (`--id-label`), start it, run initializeCommand and the lifecycle commands. The root itself is
/// the workspace (not its git root), as in every other kind of project.
pub fn up_argv(
    root: &str,
    config_file: &str,
    docker: &Path,
    override_config: Option<&str>,
    labels: &[(String, String)],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "up".into(),
        "--workspace-folder".into(),
        root.into(),
        "--config".into(),
        config_file.into(),
        "--docker-path".into(),
        docker.to_string_lossy().into_owned(),
        "--mount-workspace-git-root=false".into(),
    ];
    if let Some(path) = override_config {
        argv.extend(["--override-config".into(), path.into()]);
    }
    for (key, value) in labels {
        argv.extend(["--id-label".into(), format!("{key}={value}")]);
    }
    argv
}

/// `devcontainer read-configuration --include-merged-configuration` for a running container: the
/// configuration merged with every feature's metadata.
pub fn read_configuration_argv(
    root: &str,
    config_file: &str,
    docker: &Path,
    container_id: &str,
) -> Vec<String> {
    vec![
        "read-configuration".into(),
        "--workspace-folder".into(),
        root.into(),
        "--config".into(),
        config_file.into(),
        "--docker-path".into(),
        docker.to_string_lossy().into_owned(),
        "--container-id".into(),
        container_id.into(),
        "--mount-workspace-git-root=false".into(),
        "--include-merged-configuration".into(),
    ]
}

#[derive(Debug, Clone, PartialEq)]
pub struct Up {
    pub container_id: String,
    pub remote_user: Option<String>,
    pub remote_workspace_folder: Option<String>,
}

/// The JSON line `devcontainer up` prints last on stdout.
pub fn parse_up(stdout: &str) -> Result<Up, String> {
    let line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or("devcontainer up printed no result")?;
    let value: Value = serde_json::from_str(line)
        .map_err(|e| format!("unexpected devcontainer up result `{line}`: {e}"))?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    match text("outcome").as_deref() {
        Some("success") => Ok(Up {
            container_id: text("containerId")
                .filter(|id| !id.is_empty())
                .ok_or("devcontainer up succeeded without a containerId")?,
            remote_user: text("remoteUser"),
            remote_workspace_folder: text("remoteWorkspaceFolder"),
        }),
        _ => Err(format!(
            "devcontainer up failed: {}{}",
            text("message").unwrap_or_else(|| line.to_owned()),
            text("description")
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        )),
    }
}

/// `remoteUser` and `remoteEnv` of `devcontainer read-configuration --include-merged-configuration`.
pub fn parse_merged(stdout: &str) -> Result<ExecSettings, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("unexpected devcontainer read-configuration output: {e}"))?;
    let merged = value
        .get("mergedConfiguration")
        .and_then(Value::as_object)
        .ok_or("read-configuration printed no mergedConfiguration")?;
    let text = |key: &str| merged.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut env = Vec::new();
    for (key, v) in merged
        .get("remoteEnv")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        match v {
            Value::String(s) => env.push((key.clone(), s.clone())),
            Value::Null => {
                return Err(format!(
                    "remoteEnv `{key}` is null (unsetting a variable is not supported)"
                ));
            }
            other => env.push((key.clone(), other.to_string())),
        }
    }
    Ok(ExecSettings {
        user: text("remoteUser").or_else(|| text("containerUser")),
        env,
    })
}

/// The configuration with `forwardPorts` published, for `--override-config` (the CLI itself does not
/// publish forwardPorts): `published` maps a Compose service (`None`: the container commands run in)
/// to `docker run -p` values. An image/Dockerfile configuration gets them as `appPort` entries; a
/// Compose one gets `ports_file` (a compose file publishing them) appended to `dockerComposeFile`.
/// Paths are made absolute, so they resolve the same whichever file the CLI resolves them against.
pub fn override_config(
    cli: &DevcontainerCli,
    ports_file: Option<&str>,
    main_ports: &[String],
) -> Value {
    let mut config = cli.config.clone();
    let dir = Path::new(&cli.config_file)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let absolute = |p: &str| {
        if Path::new(p).is_absolute() {
            p.to_owned()
        } else {
            crate::paths::normalize(&dir.join(p).to_string_lossy())
        }
    };
    let Some(object) = config.as_object_mut() else {
        return config;
    };
    if let Some(build) = object.get_mut("build").and_then(Value::as_object_mut) {
        for key in ["dockerfile", "dockerFile", "context"] {
            if let Some(Value::String(p)) = build.get_mut(key) {
                *p = absolute(p);
            }
        }
    }
    for key in ["dockerFile", "context"] {
        if let Some(Value::String(p)) = object.get_mut(key) {
            *p = absolute(p);
        }
    }
    if let Some(files) = object.get("dockerComposeFile").cloned() {
        let mut list: Vec<Value> = match files {
            Value::Array(items) => items,
            other => vec![other],
        };
        for item in &mut list {
            if let Value::String(p) = item {
                *p = absolute(p);
            }
        }
        if let Some(file) = ports_file {
            list.push(Value::String(file.to_owned()));
        }
        object.insert("dockerComposeFile".into(), Value::Array(list));
    } else if !main_ports.is_empty() {
        let mut list: Vec<Value> = match object.remove("appPort") {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        list.extend(main_ports.iter().map(|p| json!(p)));
        object.insert("appPort".into(), Value::Array(list));
    }
    config
}
