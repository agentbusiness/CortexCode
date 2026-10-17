//! The docker command-line tool: finding it, running it, and the argv of every docker call this
//! plugin makes (pure functions, so they are unit-tested without Docker).

use crate::config::{Build, Kind, Spec, mount_field};
use cc_protocol::contracts::container::{ImageInfo, PathMapping, PortInfo};
use cc_protocol::{RpcError, codes};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Where Docker Desktop (and Homebrew) put the CLI on macOS; searched after `PATH`, because an app
/// started from the Dock gets a minimal `PATH`.
pub const WELL_KNOWN: &[&str] = &[
    "/usr/local/bin/docker",
    "/opt/homebrew/bin/docker",
    "~/.docker/bin/docker",
    "/Applications/Docker.app/Contents/Resources/bin/docker",
];

/// Container labels.
pub mod labels {
    pub const MANAGED: &str = "dev.cortexcode.managed";
    pub const ROOT: &str = "dev.cortexcode.root";
    pub const CONFIG: &str = "dev.cortexcode.config";
    pub const SOURCE: &str = "dev.cortexcode.source";
    pub const IMAGE: &str = "dev.cortexcode.image";
    pub const WORKDIR: &str = "dev.cortexcode.workdir";
    pub const PLATFORM: &str = "dev.cortexcode.platform";
    /// JSON [`super::ExecSettings`].
    pub const EXEC: &str = "dev.cortexcode.exec";
    /// `container` | `compose` | `devcontainer-cli` (absent on containers of older versions:
    /// `container`).
    pub const KIND: &str = "dev.cortexcode.kind";
    /// The Compose project and service (Compose projects).
    pub const PROJECT: &str = "dev.cortexcode.project";
    pub const SERVICE: &str = "dev.cortexcode.service";
    /// The devcontainer.json the Dev Containers CLI created the container from.
    pub const CONFIG_FILE: &str = "dev.cortexcode.config_file";
}

/// Environment variable that marks every process of one exec (inherited by its descendants), so a
/// cancelled exec can be found and killed inside the container.
pub const EXEC_MARKER: &str = "CORTEX_EXEC_ID";

/// The container's own process when the image's command is overridden: wait forever, exit at once on
/// `docker stop` (PID 1 ignores SIGTERM without a handler).
pub const KEEP_ALIVE: &str = "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done";

/// Kill every process of one exec inside the container: the processes whose environment carries
/// `CORTEX_EXEC_ID=<id>` and all their descendants (found through `/proc`, so also those that
/// cleared their environment). Each found process is stopped first so it cannot fork while the
/// tree is collected; then all are killed. `$1` is the exec id. (`CORTEX_PROC_ROOT` replaces
/// `/proc` only in the unit test that runs this script against a fabricated process table.)
pub const KILL_SCRIPT: &str = r#"id="$1"
proc=${CORTEX_PROC_ROOT:-/proc}
marked() { tr '\0' '\n' 2>/dev/null < "$proc/$1/environ" | grep -qxF "CORTEX_EXEC_ID=$id"; }
found=" "
while :; do
  added=0
  for d in "$proc"/[0-9]*; do
    p=${d##*/}
    [ "$p" = "$$" ] && continue
    case "$found" in *" $p "*) continue;; esac
    hit=0
    if read -r line 2>/dev/null < "$d/stat"; then
      rest=${line##*) }
      set -- $rest
      case "$found" in *" $2 "*) hit=1;; esac
    fi
    [ $hit = 0 ] && marked "$p" && hit=1
    if [ $hit = 1 ]; then kill -STOP "$p" 2>/dev/null; found="$found$p "; added=1; fi
  done
  [ $added = 0 ] && break
done
for p in $found; do kill -KILL "$p" 2>/dev/null; done
exit 0"#;

/// How commands run in a container (stored in its [`labels::EXEC`] label).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecSettings {
    #[serde(default)]
    pub user: Option<String>,
    /// Per-command environment; values may hold `${containerEnv:VAR}`.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Docker {
    pub program: PathBuf,
    /// `PATH` for docker processes: the plugin's, plus the CLI's own directory (credential helpers
    /// such as docker-credential-desktop live next to it).
    path_env: OsString,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// What docker said, for an error message: stderr, else stdout, trimmed to its last lines.
    pub fn said(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines: Vec<&str> = text.trim().lines().collect();
        let tail = &lines[lines.len().saturating_sub(30)..];
        tail.join("\n")
    }
}

fn expand_home(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => home.map(|h| h.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Docker {
    /// Find the CLI: the configured path, else `docker` on `path_env`, else [`WELL_KNOWN`].
    pub fn locate(
        configured: Option<&str>,
        path_env: &OsStr,
        home: Option<&Path>,
    ) -> Result<Docker, String> {
        Self::locate_in(configured, path_env, home, WELL_KNOWN)
    }

    /// [`Docker::locate`] with an explicit list of fallback locations.
    pub fn locate_in(
        configured: Option<&str>,
        path_env: &OsStr,
        home: Option<&Path>,
        well_known: &[&str],
    ) -> Result<Docker, String> {
        let program = match configured.filter(|c| !c.is_empty()) {
            Some(configured) => {
                let path = expand_home(configured, home).unwrap_or_else(|| PathBuf::from(configured));
                if !executable(&path) {
                    return Err(format!("Docker is not available: the configured docker CLI `{}` is not an executable file", path.display()));
                }
                path
            }
            None => std::env::split_paths(path_env)
                .map(|dir| dir.join("docker"))
                .chain(well_known.iter().filter_map(|p| expand_home(p, home)))
                .find(|p| executable(p))
                .ok_or_else(|| {
                    format!(
                        "Docker is not installed: no `docker` executable on PATH ({}) or in {}. Install Docker Desktop (or set `docker` in the plugin config).",
                        path_env.to_string_lossy(),
                        well_known.join(", ")
                    )
                })?,
        };
        let mut dirs: Vec<PathBuf> = std::env::split_paths(path_env).collect();
        if let Some(dir) = program.parent()
            && !dirs.iter().any(|d| d == dir)
        {
            dirs.push(dir.to_path_buf());
        }
        let path_env = std::env::join_paths(dirs).unwrap_or_else(|_| path_env.to_owned());
        Ok(Docker { program, path_env })
    }

    pub fn path_env(&self) -> &OsStr {
        &self.path_env
    }

    /// A docker process with piped output, killed when dropped (a cancelled request stops it).
    /// Compose's own environment variables are cleared (the plugin passes files, project names and
    /// profiles explicitly), and builds print plain progress.
    pub fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        for variable in COMPOSE_VARIABLES {
            command.env_remove(variable);
        }
        command
            .args(args)
            .env("PATH", &self.path_env)
            .env("DOCKER_CLI_HINTS", "false")
            .env("BUILDKIT_PROGRESS", "plain")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Run to completion and capture the output.
    pub async fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Output, RpcError> {
        let output = self.command(args).output().await.map_err(|e| {
            RpcError::new(
                codes::UNAVAILABLE,
                format!("cannot run {}: {e}", self.program.display()),
            )
        })?;
        Ok(Output {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Run; a nonzero exit is an error naming `what` and quoting docker.
    pub async fn ok<S: AsRef<OsStr>>(&self, what: &str, args: &[S]) -> Result<String, RpcError> {
        let output = self.run(args).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(RpcError::internal(format!(
                "{what} failed (docker exit {}): {}",
                code_text(output.code),
                output.said()
            )))
        }
    }

    /// The daemon's version, or why it cannot be reached, in plain words.
    pub async fn daemon(&self) -> Result<String, String> {
        let output = self
            .run(&["version", "--format", "{{.Server.Version}}"])
            .await
            .map_err(|e| e.message)?;
        if output.success() && !output.stdout.trim().is_empty() {
            Ok(output.stdout.trim().to_owned())
        } else {
            Err(format!(
                "Docker is installed ({}) but its daemon is not running or not reachable: start Docker Desktop. Docker said: {}",
                self.program.display(),
                output.said()
            ))
        }
    }
}

/// Compose reads these from its environment; a project must not pick them up from the plugin's.
pub const COMPOSE_VARIABLES: &[&str] = &[
    "COMPOSE_FILE",
    "COMPOSE_PROJECT_NAME",
    "COMPOSE_PROFILES",
    "COMPOSE_ENV_FILES",
    "COMPOSE_PATH_SEPARATOR",
    "COMPOSE_REMOVE_ORPHANS",
];

pub fn code_text(code: Option<i32>) -> String {
    code.map(|c| c.to_string())
        .unwrap_or_else(|| "by signal".into())
}

pub fn container_name(root: &str) -> String {
    format!("cortex-{}", crate::config::root_hash(root))
}

/// `docker build`.
pub fn build_argv(build: &Build, platform: Option<&str>, tag: &str) -> Vec<String> {
    let mut argv: Vec<String> = vec!["build".into()];
    if let Some(platform) = platform {
        argv.extend(["--platform".into(), platform.into()]);
    }
    argv.extend([
        "-t".into(),
        tag.into(),
        "-f".into(),
        build.dockerfile.clone(),
    ]);
    for (key, value) in &build.args {
        argv.extend(["--build-arg".into(), format!("{key}={value}")]);
    }
    if let Some(target) = &build.target {
        argv.extend(["--target".into(), target.clone()]);
    }
    for cache in &build.cache_from {
        argv.extend(["--cache-from".into(), cache.clone()]);
    }
    argv.extend(build.options.iter().cloned());
    argv.push(build.context.clone());
    argv
}

/// The labels of a project's container (every kind): what this plugin needs to find and describe it
/// again after a restart.
pub fn labels_of(
    spec: &Spec,
    root: &str,
    digest: &str,
    platform_label: &str,
) -> Result<Vec<(String, String)>, String> {
    let exec = serde_json::to_string(&ExecSettings {
        user: spec.exec_user.clone(),
        env: spec.exec_env.clone(),
    })
    .map_err(|e| e.to_string())?;
    let kind = match &spec.kind {
        Kind::Container => cc_protocol::contracts::container::kinds::CONTAINER,
        Kind::Compose(_) => cc_protocol::contracts::container::kinds::COMPOSE,
        Kind::DevcontainerCli(_) => cc_protocol::contracts::container::kinds::DEVCONTAINER_CLI,
    };
    let mut out: Vec<(String, String)> = vec![
        (labels::MANAGED.into(), "1".into()),
        (labels::ROOT.into(), root.into()),
        (labels::CONFIG.into(), digest.into()),
        (labels::SOURCE.into(), spec.source.clone()),
        (labels::KIND.into(), kind.into()),
        (labels::IMAGE.into(), spec.image.clone()),
        (labels::WORKDIR.into(), spec.workdir.clone()),
        (labels::PLATFORM.into(), platform_label.into()),
    ];
    match &spec.kind {
        Kind::Container => out.push((labels::EXEC.into(), exec)),
        Kind::Compose(compose) => {
            out.push((labels::EXEC.into(), exec));
            out.push((labels::PROJECT.into(), compose.project.clone()));
            out.push((labels::SERVICE.into(), compose.service.clone()));
        }
        // The CLI decides the user and environment of commands (features add to them); they are
        // read from it (`read-configuration`), not labelled.
        Kind::DevcontainerCli(cli) => {
            out.push((labels::CONFIG_FILE.into(), cli.config_file.clone()));
            if let Some((project, service)) = &cli.compose {
                out.push((labels::PROJECT.into(), project.clone()));
                out.push((labels::SERVICE.into(), service.clone()));
            }
        }
    }
    Ok(out)
}

/// `docker run -d`: the root bind-mounted at the workdir (or the `workspaceMount`), labels
/// describing the project, published `ports` (`-p` values), and the keep-alive loop unless the
/// image's own command is kept.
pub fn run_argv(
    spec: &Spec,
    root: &str,
    labels: &[(String, String)],
    ports: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container_name(root),
    ];
    for (key, value) in labels {
        argv.extend(["--label".into(), format!("{key}={value}")]);
    }
    if let Some(platform) = &spec.platform {
        argv.extend(["--platform".into(), platform.clone()]);
    }
    let workspace = match &spec.workspace_mount {
        Some(mount) => mount.clone(),
        None => format!(
            "type=bind,{},{}",
            mount_field(&format!("source={root}")),
            mount_field(&format!("target={}", spec.workdir))
        ),
    };
    argv.extend([
        "--mount".into(),
        workspace,
        "-w".into(),
        spec.workdir.clone(),
    ]);
    for (key, value) in &spec.container_env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    if let Some(user) = &spec.container_user {
        argv.extend(["--user".into(), user.clone()]);
    }
    if !spec.network {
        argv.extend(["--network".into(), "none".into()]);
    }
    argv.extend(spec.run_args.iter().cloned());
    for mount in &spec.mounts {
        argv.extend(["--mount".into(), mount.clone()]);
    }
    if spec.privileged {
        argv.push("--privileged".into());
    }
    if spec.init {
        argv.push("--init".into());
    }
    for cap in &spec.cap_add {
        argv.extend(["--cap-add".into(), cap.clone()]);
    }
    for opt in &spec.security_opt {
        argv.extend(["--security-opt".into(), opt.clone()]);
    }
    for port in ports {
        argv.extend(["-p".into(), port.clone()]);
    }
    if spec.override_command {
        argv.extend([
            "--entrypoint".into(),
            "/bin/sh".into(),
            spec.image.clone(),
            "-c".into(),
            KEEP_ALIVE.into(),
        ]);
    } else {
        argv.push(spec.image.clone());
    }
    argv
}

/// `docker exec` of one command line through `<shell> -lc`, marked with the exec id.
pub fn exec_argv(
    id: &str,
    cwd: &str,
    user: Option<&str>,
    env: &[(String, String)],
    marker: &str,
    shell: &str,
    command: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["exec".into(), "-i".into(), "-w".into(), cwd.into()];
    if let Some(user) = user {
        argv.extend(["-u".into(), user.into()]);
    }
    for (key, value) in env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    argv.extend([
        "-e".into(),
        format!("{EXEC_MARKER}={marker}"),
        id.into(),
        shell.into(),
        "-lc".into(),
        command.into(),
    ]);
    argv
}

/// `docker exec` of an argv (lifecycle commands given as arrays), no shell.
pub fn exec_argv_direct(
    id: &str,
    cwd: &str,
    user: Option<&str>,
    env: &[(String, String)],
    marker: &str,
    program: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["exec".into(), "-i".into(), "-w".into(), cwd.into()];
    if let Some(user) = user {
        argv.extend(["-u".into(), user.into()]);
    }
    for (key, value) in env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    argv.extend(["-e".into(), format!("{EXEC_MARKER}={marker}"), id.into()]);
    argv.extend(program.iter().cloned());
    argv
}

/// Kill the processes of exec `marker` inside the container (as root, to reach any user's process).
pub fn kill_argv(id: &str, marker: &str) -> Vec<String> {
    vec![
        "exec".into(),
        "-u".into(),
        "0".into(),
        id.into(),
        "sh".into(),
        "-c".into(),
        KILL_SCRIPT.into(),
        "cortex-kill".into(),
        marker.into(),
    ]
}

/// Whether `shell` can be started in the container (as the exec user).
pub fn shell_check_argv(id: &str, user: Option<&str>, shell: &str) -> Vec<String> {
    let mut argv: Vec<String> = vec!["exec".into()];
    if let Some(user) = user {
        argv.extend(["-u".into(), user.into()]);
    }
    argv.extend([id.into(), shell.into(), "-c".into(), "exit 0".into()]);
    argv
}

/// The interactive shell for terminals: `docker exec -it -w <workdir> [-u user] [-e ...] <id> <shell> -l`.
pub fn shell_argv(
    program: &Path,
    id: &str,
    workdir: &str,
    user: Option<&str>,
    env: &[(String, String)],
    shell: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        program.to_string_lossy().into_owned(),
        "exec".into(),
        "-it".into(),
        "-w".into(),
        workdir.into(),
    ];
    if let Some(user) = user {
        argv.extend(["-u".into(), user.into()]);
    }
    for (key, value) in env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    argv.extend([id.into(), shell.into(), "-l".into()]);
    argv
}

/// Containers this plugin manages (all, or one root's), stopped ones included.
pub fn ps_argv(root: Option<&str>) -> Vec<String> {
    match root {
        Some(root) => ps_label_argv(&[(labels::ROOT, root)]),
        None => ps_label_argv(&[(labels::MANAGED, "1")]),
    }
}

/// Containers carrying every one of `labels` (`docker ps` ANDs its label filters), stopped ones
/// included.
pub fn ps_label_argv(labels: &[(&str, &str)]) -> Vec<String> {
    let mut argv: Vec<String> = vec!["ps".into(), "-a".into(), "-q".into(), "--no-trunc".into()];
    for (key, value) in labels {
        argv.extend(["--filter".into(), format!("label={key}={value}")]);
    }
    argv
}

/// `docker image ls`, one JSON object per line.
pub fn images_argv(reference: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "image".into(),
        "ls".into(),
        "--no-trunc".into(),
        "--format".into(),
        "{{json .}}".into(),
    ];
    if let Some(reference) = reference {
        argv.extend(["--filter".into(), format!("reference={reference}")]);
    }
    argv
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(default)]
    repository: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    created_at: String,
}

/// The output of [`images_argv`].
pub fn parse_images(stdout: &str) -> Result<Vec<ImageInfo>, String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let image: ImageLine = serde_json::from_str(line)
                .map_err(|e| format!("unexpected docker image ls line `{line}`: {e}"))?;
            Ok(ImageInfo {
                id: image.id,
                repository: image.repository,
                tag: image.tag,
                size: image.size,
                created: image.created_at,
            })
        })
        .collect()
}

/// `docker logs`.
pub fn logs_argv(id: &str, tail: u64, since: Option<&str>, timestamps: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["logs".into(), "--tail".into(), tail.to_string()];
    if let Some(since) = since {
        argv.extend(["--since".into(), since.into()]);
    }
    if timestamps {
        argv.push("--timestamps".into());
    }
    argv.push(id.into());
    argv
}

/// `docker inspect --type container` output, the parts this plugin reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Inspected {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The image id.
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub state: InspectState,
    #[serde(default)]
    pub config: InspectConfig,
    #[serde(default)]
    pub mounts: Option<Vec<InspectMount>>,
    #[serde(default)]
    pub network_settings: InspectNetwork,
    #[serde(default)]
    pub host_config: InspectHost,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default, rename = "OOMKilled")]
    pub oom_killed: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub health: Option<InspectHealth>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectHealth {
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectConfig {
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// The image as named when the container was created.
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub working_dir: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectMount {
    #[serde(default, rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub destination: String,
    #[serde(default, rename = "RW")]
    pub rw: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Binding {
    #[serde(default, rename = "HostIp")]
    pub host_ip: String,
    #[serde(default, rename = "HostPort")]
    pub host_port: String,
}

type PortMap = Option<HashMap<String, Option<Vec<Binding>>>>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectNetwork {
    /// Ports published now (running containers).
    #[serde(default)]
    pub ports: PortMap,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InspectHost {
    /// Ports the container was created to publish (also known while it is stopped).
    #[serde(default)]
    pub port_bindings: PortMap,
}

/// Docker Desktop shares the Mac's files with its VM under this directory; a bind source reported
/// with it is the Mac path without it.
pub const DESKTOP_HOST_MOUNT: &str = "/host_mnt";

impl Inspected {
    pub fn label(&self, key: &str) -> String {
        self.config
            .labels
            .as_ref()
            .and_then(|l| l.get(key))
            .cloned()
            .unwrap_or_default()
    }

    pub fn env(&self) -> BTreeMap<String, String> {
        self.config
            .env
            .iter()
            .flatten()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
            })
            .collect()
    }

    /// The bind mounts: host path -> container path.
    pub fn binds(&self) -> Vec<PathMapping> {
        self.mounts
            .iter()
            .flatten()
            .filter(|m| {
                m.kind == "bind" && m.source.starts_with('/') && m.destination.starts_with('/')
            })
            .map(|m| {
                let host = match m.source.strip_prefix(DESKTOP_HOST_MOUNT) {
                    Some(rest) if rest.starts_with('/') => rest.to_owned(),
                    _ => m.source.clone(),
                };
                PathMapping {
                    host,
                    container: m.destination.clone(),
                    read_only: !m.rw,
                }
            })
            .collect()
    }

    /// The Compose service, when the container belongs to a Compose project.
    pub fn service(&self) -> Option<String> {
        Some(self.label(crate::compose::labels::SERVICE)).filter(|s| !s.is_empty())
    }

    pub fn project(&self) -> Option<String> {
        Some(self.label(crate::compose::labels::PROJECT)).filter(|s| !s.is_empty())
    }

    /// Published ports now (`NetworkSettings.Ports`).
    pub fn published(&self) -> Vec<PortInfo> {
        ports(&self.network_settings.ports, &self.id, self.service())
    }

    /// Ports the container was created with (`HostConfig.PortBindings`).
    pub fn bindings(&self) -> Vec<PortInfo> {
        ports(&self.host_config.port_bindings, &self.id, self.service())
    }
}

fn ports(map: &PortMap, id: &str, service: Option<String>) -> Vec<PortInfo> {
    let mut out = Vec::new();
    for (key, bindings) in map.iter().flatten() {
        let (port, protocol) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
        let Ok(container_port) = port.parse::<u16>() else {
            continue;
        };
        for binding in bindings.iter().flatten() {
            // An unassigned host port (`""` in PortBindings) is Docker's pick; reported once known.
            let Ok(host_port) = binding.host_port.parse::<u16>() else {
                continue;
            };
            out.push(PortInfo {
                container_id: id.to_owned(),
                service: service.clone(),
                container_port,
                protocol: protocol.to_owned(),
                host_ip: if binding.host_ip.is_empty() {
                    "0.0.0.0".to_owned()
                } else {
                    binding.host_ip.clone()
                },
                host_port,
            });
        }
    }
    out.sort_by(|a, b| {
        (a.container_port, &a.protocol, &a.host_ip).cmp(&(
            b.container_port,
            &b.protocol,
            &b.host_ip,
        ))
    });
    out
}

/// Parse `docker inspect` output.
pub fn parse_inspect(stdout: &str) -> Result<Vec<Inspected>, String> {
    serde_json::from_str(stdout.trim()).map_err(|e| e.to_string())
}
