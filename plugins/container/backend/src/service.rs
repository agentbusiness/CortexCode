//! `container@1`: the plugin's state and its methods.
//!
//! A project's container is labelled with its root, kind, workdir, configuration digest and exec
//! settings, so everything this plugin knows about it is recoverable from Docker itself: after a
//! plugin restart the container of a root is found again by its `dev.cortexcode.root` label and
//! reused. Three kinds of project ([`Kind`]):
//! - one container (`docker run`, named `cortex-<hash of root>`), the root bind-mounted at workdir;
//! - a Docker Compose project (`cortex-<hash of root>`): `docker compose up -d --build --wait` with
//!   an override that labels the service commands run in; the other services are found through
//!   Compose's project label;
//! - a devcontainer with features, created by the Dev Containers CLI (`devcontainer up` with this
//!   plugin's labels as `--id-label`).
//!
//! Commands run with `docker exec` in every kind; host paths map through the container's actual
//! bind mounts (`docker inspect`). Only attach failures (which may have no container) live in
//! memory alone.

use crate::compose;
use crate::config::{
    self, Command, Compose, DevcontainerCli, ForwardPort, Hook, Kind, Spec,
    container_env_substitute,
};
use crate::devcli::{self, Cli, Located};
use crate::docker::{self, Docker, ExecSettings, Inspected, labels};
use crate::exec::{self, Run, Stop};
use crate::paths::{PathMap, normalize};
use cc_protocol::contracts::container::{
    AttachParams, BuildParams, ContainerInfo, ContainerLogs, DetachParams, ExecItem, ExecParams,
    ExecResult, ImageResult, ImagesParams, ImagesResult, ListResult, LogItem, LogsParams,
    LogsResult, PortInfo, PortsResult, ProgressItem, ProjectContainer, PsResult, PullParams,
    RootParams, ShellCommandResult, kinds, methods,
};
use cc_protocol::{ItemSink, RpcError, codes, parse_params, to_value};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub const PROVIDES: &[&str] = &[
    methods::ATTACH,
    methods::STATUS,
    methods::EXEC,
    methods::DETACH,
    methods::LIST,
    methods::SHELL_COMMAND,
    methods::IMAGES,
    methods::BUILD,
    methods::PULL,
    methods::LOGS,
    methods::PS,
    methods::PORTS,
];

pub const STATE_STARTING: &str = "starting";
pub const STATE_RUNNING: &str = "running";
pub const STATE_STOPPED: &str = "stopped";
pub const STATE_FAILED: &str = "failed";

/// Lines of logs `container.logs` returns when the caller names no `tail`.
pub const DEFAULT_LOG_TAIL: u64 = 1000;

/// Output of a build, pull, compose or CLI step kept for its error message (items carry all of it).
const STEP_OUTPUT: usize = 1 << 20;

/// Plugin configuration (`[backend.config]` / `[plugin.config]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Path of the docker CLI; empty: `docker` on PATH, then the Docker Desktop locations.
    pub docker: Option<String>,
    /// Output kept in an exec result (the latest bytes); items carry all of it.
    pub max_output_bytes: usize,
    /// After a timeout or cancellation: how long to wait for the command to end after each kill.
    pub stop_wait_ms: u64,
    /// Path of node (for the Dev Containers CLI); empty: on PATH, then Homebrew's locations.
    pub node: Option<String>,
    /// An existing `devcontainer` executable to use instead of installing the CLI.
    pub devcontainer_cli: Option<String>,
    /// The `@devcontainers/cli` version installed into the toolchains directory.
    pub devcontainer_cli_version: String,
}

impl Default for Config {
    fn default() -> Self {
        // 8 MiB of text stays far below the protocol's 64 MiB frame even when every byte needs
        // JSON escaping.
        Self {
            docker: None,
            max_output_bytes: 8 * 1024 * 1024,
            stop_wait_ms: 3000,
            node: None,
            devcontainer_cli: None,
            devcontainer_cli_version: devcli::DEFAULT_VERSION.into(),
        }
    }
}

impl Config {
    pub fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Self {
            docker: text("docker"),
            max_output_bytes: value
                .get("max_output_bytes")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(defaults.max_output_bytes),
            stop_wait_ms: value
                .get("stop_wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(defaults.stop_wait_ms),
            node: text("node"),
            devcontainer_cli: text("devcontainer_cli"),
            devcontainer_cli_version: text("devcontainer_cli_version")
                .unwrap_or(defaults.devcontainer_cli_version),
        }
    }
}

pub type Publish = Arc<dyn Fn(&ContainerInfo) + Send + Sync>;
pub type HostEnv = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// Where progress items go.
pub type Emit = Arc<dyn Fn(ProgressItem) + Send + Sync>;

#[derive(Debug, Clone)]
struct Entry {
    info: ContainerInfo,
    exec: ExecSettings,
    /// The devcontainer.json of a container the Dev Containers CLI created.
    config_file: Option<String>,
}

/// Where the project root is in the container: through the bind mount with the longest source
/// containing it.
fn root_in_container(
    root: &str,
    mounts: &[cc_protocol::contracts::container::PathMapping],
) -> Option<String> {
    PathMap::new(root, root, "/", mounts)
        .to_container(root)
        .ok()
        .filter(|p| p.as_str() != normalize(root))
}

fn entry_of(inspected: &Inspected) -> Entry {
    let state = match inspected.state.status.as_str() {
        "running" => STATE_RUNNING,
        "created" | "restarting" => STATE_STARTING,
        _ => STATE_STOPPED,
    };
    let problem = (state == STATE_STOPPED).then(|| {
        let mut text = format!(
            "the container is {} (exit code {}",
            inspected.state.status, inspected.state.exit_code
        );
        if inspected.state.oom_killed {
            text.push_str(", killed for running out of memory");
        }
        if !inspected.state.error.is_empty() {
            text.push_str(&format!(", {}", inspected.state.error));
        }
        text.push_str("); attach again to start it");
        text
    });
    let label = |key: &str| Some(inspected.label(key)).filter(|v| !v.is_empty());
    let root = inspected.label(labels::ROOT);
    let mounts = inspected.binds();
    let kind = label(labels::KIND).unwrap_or_else(|| kinds::CONTAINER.to_owned());
    // A Compose service that does not name its workdir: where the root is mounted, else the
    // service's working directory.
    let workdir = label(labels::WORKDIR)
        .or_else(|| root_in_container(&root, &mounts))
        .or_else(|| Some(inspected.config.working_dir.clone()).filter(|w| !w.is_empty()))
        .unwrap_or_else(|| "/".to_owned());
    Entry {
        info: ContainerInfo {
            root,
            id: inspected.id.clone(),
            image: label(labels::IMAGE).unwrap_or_else(|| inspected.config.image.clone()),
            workdir,
            platform: label(labels::PLATFORM),
            state: state.into(),
            problem,
            source: inspected.label(labels::SOURCE),
            project: if kind == kinds::CONTAINER {
                None
            } else {
                inspected.project()
            },
            service: if kind == kinds::CONTAINER {
                None
            } else {
                inspected.service()
            },
            kind: Some(kind),
            mounts,
        },
        exec: serde_json::from_str(&inspected.label(labels::EXEC)).unwrap_or_default(),
        config_file: label(labels::CONFIG_FILE),
    }
}

fn kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Container => kinds::CONTAINER,
        Kind::Compose(_) => kinds::COMPOSE,
        Kind::DevcontainerCli(_) => kinds::DEVCONTAINER_CLI,
    }
}

/// Removes the root from the busy set when an attach or detach ends.
struct BusyGuard<'a> {
    busy: &'a Mutex<HashSet<String>>,
    root: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.busy.lock() {
            busy.remove(&self.root);
        }
    }
}

/// The result of a streamed step.
struct Streamed {
    code: Option<i32>,
    /// stdout and stderr interleaved (the latest bytes).
    output: String,
    /// stdout alone (the latest bytes).
    stdout: String,
}

impl Streamed {
    fn said(&self) -> String {
        let lines: Vec<&str> = self.output.trim().lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }

    fn ok(self, what: &str) -> Result<Streamed, RpcError> {
        if self.code == Some(0) {
            Ok(self)
        } else {
            Err(RpcError::internal(format!(
                "{what} failed (exit {}): {}",
                docker::code_text(self.code),
                self.said()
            )))
        }
    }
}

/// Run one process to completion, streaming its output as progress items of `phase`. Cancellation
/// kills it.
async fn stream(
    command: tokio::process::Command,
    phase: &str,
    emit: &Emit,
    cancel: &CancellationToken,
    stop_wait: Duration,
) -> Result<Streamed, RpcError> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let stdout = Mutex::new(exec::Collected::new(STEP_OUTPUT));
    let forward = |item: ExecItem| {
        if item.stream == exec::STDOUT
            && let Ok(mut stdout) = stdout.lock()
        {
            stdout.push(&item.text);
        }
        emit(ProgressItem {
            phase: phase.to_owned(),
            stream: item.stream,
            text: item.text,
        });
    };
    let run = Run {
        command,
        stdin: None,
        timeout: None,
        max_output: STEP_OUTPUT,
        stop_wait,
        stop_attempts: 1,
    };
    let quiet: Stop = Box::new(|_| Box::pin(async {}));
    let result = exec::run(run, &forward, cancel.clone(), quiet)
        .await
        .map_err(|e| RpcError::new(codes::UNAVAILABLE, format!("cannot run {program}: {e}")))?;
    if result.cancelled {
        return Err(RpcError::cancelled());
    }
    let stdout = stdout
        .into_inner()
        .map(exec::Collected::finish)
        .unwrap_or_default();
    Ok(Streamed {
        code: result.exit_code,
        output: result.output,
        stdout,
    })
}

fn info_line(emit: &Emit, phase: &str, text: String) {
    emit(ProgressItem {
        phase: phase.into(),
        stream: "info".into(),
        text: format!("{text}\n"),
    });
}

pub struct Containers {
    config: Mutex<Config>,
    data_dir: PathBuf,
    /// `$CORTEX_HOME/toolchains`.
    toolchains: PathBuf,
    /// `PATH` searched for the docker CLI (the plugin's own environment).
    path_env: OsString,
    home: Option<PathBuf>,
    host_env: HostEnv,
    entries: Mutex<HashMap<String, Entry>>,
    /// Container id -> the shell commands run through (`bash`, or `sh` when the image has no bash).
    shells: Mutex<HashMap<String, String>>,
    /// Container id -> how commands run: user and per-command environment (resolved).
    settings: Mutex<HashMap<String, ExecSettings>>,
    busy: Mutex<HashSet<String>>,
    publish: Publish,
    /// Cancelled at shutdown: stops every running exec.
    stopping: CancellationToken,
    tasks: TaskTracker,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message)
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, message)
}

fn sink(items: &ItemSink) -> Emit {
    let items = items.clone();
    Arc::new(move |item: ProgressItem| {
        if let Ok(value) = serde_json::to_value(&item) {
            items.send(value);
        }
    })
}

impl Containers {
    pub fn new(
        data_dir: PathBuf,
        cortex_home: PathBuf,
        config: Config,
        path_env: OsString,
        home: Option<PathBuf>,
        host_env: HostEnv,
        publish: Publish,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            data_dir,
            toolchains: cortex_home.join("toolchains"),
            path_env,
            home,
            host_env,
            entries: Mutex::new(HashMap::new()),
            shells: Mutex::new(HashMap::new()),
            settings: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            publish,
            stopping: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    pub fn configure(&self, config: Config) {
        if let Ok(mut current) = self.config.lock() {
            *current = config;
        }
    }

    fn config(&self) -> Config {
        self.config.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn stop_wait(&self) -> Duration {
        Duration::from_millis(self.config().stop_wait_ms)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The docker CLI, or why there is none.
    pub fn docker(&self) -> Result<Docker, String> {
        Docker::locate(
            self.config().docker.as_deref(),
            &self.path_env,
            self.home.as_deref(),
        )
    }

    fn docker_or_unavailable(&self) -> Result<Docker, RpcError> {
        self.docker().map_err(unavailable)
    }

    /// Where the Dev Containers CLI comes from (docker's PATH, so the CLI finds docker's helpers).
    pub fn devcli_setup(&self, docker: Option<&Docker>) -> devcli::Setup {
        let config = self.config();
        devcli::Setup {
            configured: config.devcontainer_cli,
            version: config.devcontainer_cli_version,
            node: config.node,
            toolchains: self.toolchains.clone(),
            path_env: docker
                .map(|d| d.path_env().to_owned())
                .unwrap_or_else(|| self.path_env.clone()),
            home: self.home.clone(),
        }
    }

    /// The Compose plugin's version, or why `docker compose` cannot be used.
    pub async fn compose_version(&self, docker: &Docker) -> Result<String, String> {
        let output = docker
            .run(&compose::version_argv())
            .await
            .map_err(|e| e.message)?;
        if output.success() && !output.stdout.trim().is_empty() {
            Ok(output.stdout.trim().trim_start_matches('v').to_owned())
        } else {
            Err(format!(
                "Docker Compose (the `docker compose` plugin, v2) is not available with {}: {}. It ships with Docker Desktop.",
                docker.program.display(),
                output.said()
            ))
        }
    }

    pub async fn dispatch(
        &self,
        method: &str,
        params: Value,
        items: ItemSink,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            methods::ATTACH => to_value(
                &self
                    .attach_streaming(parse_params(params)?, &items, &cancel)
                    .await?,
            ),
            methods::STATUS => to_value(
                &self
                    .status(&parse_params::<RootParams>(params)?.root)
                    .await?,
            ),
            methods::EXEC => to_value(&self.exec(parse_params(params)?, items, cancel).await?),
            methods::DETACH => to_value(&self.detach_with(parse_params(params)?).await?),
            methods::LIST => to_value(&self.list().await?),
            methods::SHELL_COMMAND => to_value(
                &self
                    .shell_command(&parse_params::<RootParams>(params)?.root)
                    .await?,
            ),
            methods::IMAGES => to_value(&self.images(parse_params(params)?).await?),
            methods::BUILD => to_value(&self.build(parse_params(params)?, &items, &cancel).await?),
            methods::PULL => to_value(&self.pull(parse_params(params)?, &items, &cancel).await?),
            methods::LOGS => to_value(&self.logs(parse_params(params)?, &items, &cancel).await?),
            methods::PS => to_value(
                &self
                    .ps_project(&parse_params::<RootParams>(params)?.root)
                    .await?,
            ),
            methods::PORTS => to_value(
                &self
                    .ports(&parse_params::<RootParams>(params)?.root)
                    .await?,
            ),
            other => Err(RpcError::unknown_method(other)),
        }
    }

    fn expand(&self, root: &str) -> Result<PathBuf, RpcError> {
        let path = match (root.strip_prefix("~/"), &self.home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(root),
        };
        if !path.is_absolute() {
            return Err(invalid(format!("root `{root}` must be an absolute path")));
        }
        Ok(path)
    }

    /// The canonical root when it exists; else its normalized spelling (a container may outlive its
    /// directory).
    fn root_key(&self, root: &str) -> Result<String, RpcError> {
        let path = self.expand(root)?;
        Ok(match std::fs::canonicalize(&path) {
            Ok(canonical) => canonical.to_string_lossy().into_owned(),
            Err(_) => normalize(&path.to_string_lossy()),
        })
    }

    /// An existing project directory, canonical.
    fn existing_root(&self, root: &str) -> Result<PathBuf, RpcError> {
        let path = self.expand(root)?;
        if !path.is_dir() {
            return Err(RpcError::not_found(format!(
                "project root {} is not an existing directory",
                path.display()
            )));
        }
        std::fs::canonicalize(&path)
            .map_err(|e| RpcError::internal(format!("{}: {e}", path.display())))
    }

    fn busy(&self, root: &str) -> Result<BusyGuard<'_>, RpcError> {
        let mut busy = self
            .busy
            .lock()
            .map_err(|_| RpcError::internal("state lock poisoned"))?;
        if !busy.insert(root.to_owned()) {
            return Err(RpcError::new(
                codes::BUSY,
                format!("an attach or detach of {root} is in progress"),
            )
            .retryable());
        }
        Ok(BusyGuard {
            busy: &self.busy,
            root: root.to_owned(),
        })
    }

    /// Remember `entry` (or forget the root) and publish `container.changed` when it changed.
    fn store(&self, root: &str, entry: Option<Entry>) {
        let previous = self
            .entries
            .lock()
            .ok()
            .and_then(|mut entries| match &entry {
                Some(entry) => entries.insert(root.to_owned(), entry.clone()),
                None => entries.remove(root),
            });
        match (previous, entry) {
            (Some(previous), Some(entry)) if previous.info == entry.info => {}
            (_, Some(entry)) => (self.publish)(&entry.info),
            (Some(previous), None) if previous.info.state != STATE_STOPPED => {
                let mut info = previous.info;
                info.state = STATE_STOPPED.into();
                (self.publish)(&info);
            }
            _ => {}
        }
    }

    fn remembered(&self, root: &str) -> Option<Entry> {
        self.entries
            .lock()
            .ok()
            .and_then(|entries| entries.get(root).cloned())
    }

    async fn inspect(&self, docker: &Docker, ids: &[String]) -> Result<Vec<Inspected>, RpcError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv = vec!["inspect".to_owned(), "--type".into(), "container".into()];
        argv.extend(ids.iter().cloned());
        let output = docker.run(&argv).await?;
        // A container removed between listing and inspecting makes the exit code nonzero; the
        // others are still printed.
        match docker::parse_inspect(&output.stdout) {
            Ok(found) => Ok(found),
            Err(_) if !output.success() => {
                Err(self.docker_failure(docker, "docker inspect", &output).await)
            }
            Err(error) => Err(RpcError::internal(format!(
                "unexpected docker inspect output: {error}"
            ))),
        }
    }

    /// An error for a failed docker call: the daemon's absence in plain words when that is the
    /// cause, else what docker said.
    async fn docker_failure(
        &self,
        docker: &Docker,
        what: &str,
        output: &docker::Output,
    ) -> RpcError {
        match docker.daemon().await {
            Err(message) => unavailable(message),
            Ok(_) => RpcError::internal(format!(
                "{what} failed (docker exit {}): {}",
                docker::code_text(output.code),
                output.said()
            )),
        }
    }

    async fn ps_argv(&self, docker: &Docker, argv: &[String]) -> Result<Vec<String>, RpcError> {
        let output = docker.run(argv).await?;
        if !output.success() {
            return Err(self.docker_failure(docker, "docker ps", &output).await);
        }
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }

    async fn ps(&self, docker: &Docker, root: Option<&str>) -> Result<Vec<String>, RpcError> {
        self.ps_argv(docker, &docker::ps_argv(root)).await
    }

    /// The root's main containers (those carrying its label) as Docker reports them now.
    async fn root_containers(
        &self,
        docker: &Docker,
        root: &str,
    ) -> Result<Vec<Inspected>, RpcError> {
        let ids = self.ps(docker, Some(root)).await?;
        self.inspect(docker, &ids).await
    }

    /// The root's container as Docker reports it.
    async fn lookup(&self, docker: &Docker, root: &str) -> Result<Option<Entry>, RpcError> {
        Ok(self
            .root_containers(docker, root)
            .await?
            .first()
            .map(entry_of))
    }

    // ---- attach ------------------------------------------------------------------------------

    /// [`Containers::attach_streaming`] without progress items or cancellation.
    pub async fn attach(&self, params: AttachParams) -> Result<ContainerInfo, RpcError> {
        self.attach_streaming(params, &ItemSink::discard(), &CancellationToken::new())
            .await
    }

    pub async fn attach_streaming(
        &self,
        params: AttachParams,
        items: &ItemSink,
        cancel: &CancellationToken,
    ) -> Result<ContainerInfo, RpcError> {
        let path = self.expand(&params.root)?;
        if !path.exists() {
            if params.seed_from_image {
                std::fs::create_dir_all(&path).map_err(|e| {
                    RpcError::internal(format!("cannot create {}: {e}", path.display()))
                })?;
            } else {
                return Err(RpcError::not_found(format!(
                    "project root {} does not exist",
                    path.display()
                )));
            }
        }
        if !path.is_dir() {
            return Err(invalid(format!(
                "project root {} is not a directory",
                path.display()
            )));
        }
        let root_path = std::fs::canonicalize(&path)
            .map_err(|e| RpcError::internal(format!("{}: {e}", path.display())))?;
        let root = root_path.to_string_lossy().into_owned();
        let _guard = self.busy(&root)?;
        let host_env = self.host_env.clone();
        let spec = config::resolve(&params, &root_path, &|name| host_env(name)).map_err(invalid)?;
        let docker = self.docker_or_unavailable()?;
        docker.daemon().await.map_err(unavailable)?;
        let uses_compose = match &spec.kind {
            Kind::Compose(_) => true,
            Kind::DevcontainerCli(cli) => cli.compose.is_some(),
            Kind::Container => false,
        };
        if uses_compose {
            self.compose_version(&docker).await.map_err(unavailable)?;
        }
        let (project, service) = match &spec.kind {
            Kind::Compose(c) => (Some(c.project.clone()), Some(c.service.clone())),
            Kind::DevcontainerCli(DevcontainerCli {
                compose: Some((p, s)),
                ..
            }) => (Some(p.clone()), Some(s.clone())),
            _ => (None, None),
        };
        let starting = ContainerInfo {
            root: root.clone(),
            id: String::new(),
            image: spec.image.clone(),
            workdir: spec.workdir.clone(),
            platform: spec.platform.clone(),
            state: STATE_STARTING.into(),
            problem: None,
            source: spec.source.clone(),
            kind: Some(kind_name(&spec.kind).into()),
            project,
            service,
            mounts: Vec::new(),
        };
        self.store(
            &root,
            Some(Entry {
                info: starting.clone(),
                exec: ExecSettings::default(),
                config_file: None,
            }),
        );
        let emit = sink(items);
        match self.bring_up(&docker, &spec, &root, &emit, cancel).await {
            Ok(entry) => {
                self.store(&root, Some(entry.clone()));
                Ok(entry.info)
            }
            Err(error) => {
                let mut failed = starting;
                failed.state = STATE_FAILED.into();
                failed.problem = Some(error.message.clone());
                self.store(
                    &root,
                    Some(Entry {
                        info: failed,
                        exec: ExecSettings::default(),
                        config_file: None,
                    }),
                );
                Err(error)
            }
        }
    }

    async fn bring_up(
        &self,
        docker: &Docker,
        spec: &Spec,
        root: &str,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<Entry, RpcError> {
        match &spec.kind {
            Kind::Container => {
                self.initialize(spec, root, emit, cancel).await?;
                self.up_container(docker, spec, root, emit, cancel).await
            }
            Kind::Compose(compose) => {
                self.initialize(spec, root, emit, cancel).await?;
                self.up_compose(docker, spec, compose, root, emit, cancel)
                    .await
            }
            // The CLI runs initializeCommand itself.
            Kind::DevcontainerCli(cli) => self.up_cli(docker, spec, cli, root, emit, cancel).await,
        }
    }

    /// devcontainer `initializeCommand`: on the Mac, in the root, before anything else.
    async fn initialize(
        &self,
        spec: &Spec,
        root: &str,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<(), RpcError> {
        let Some(command) = &spec.initialize else {
            return Ok(());
        };
        let commands: Vec<(String, Command)> = match command {
            Command::Parallel(named) => named
                .iter()
                .map(|(name, c)| (format!("initializeCommand.{name}"), c.clone()))
                .collect(),
            single => vec![("initializeCommand".to_owned(), single.clone())],
        };
        let mut running = tokio::task::JoinSet::new();
        for (name, command) in commands {
            let argv: Vec<String> = match command {
                Command::Shell(line) => vec!["/bin/sh".into(), "-c".into(), line],
                Command::Argv(argv) => argv,
                Command::Parallel(_) => {
                    return Err(RpcError::internal(format!(
                        "{name}: nested parallel commands"
                    )));
                }
            };
            let mut process = tokio::process::Command::new(&argv[0]);
            process.args(&argv[1..]).current_dir(root);
            let emit = emit.clone();
            let cancel = cancel.clone();
            let stop_wait = self.stop_wait();
            running.spawn(async move {
                let result = stream(process, &name, &emit, &cancel, stop_wait)
                    .await
                    .and_then(|s| s.ok(&name));
                result.map(|_| ())
            });
        }
        while let Some(done) = running.join_next().await {
            done.map_err(|e| RpcError::internal(e.to_string()))??;
        }
        Ok(())
    }

    /// Remove a container of this root that belongs to another configuration: its whole Compose
    /// project (volumes kept) when it is part of one, else the container.
    async fn remove_existing(
        &self,
        docker: &Docker,
        existing: &Inspected,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<(), RpcError> {
        match existing.project() {
            Some(project) => {
                info_line(
                    emit,
                    "compose",
                    format!("removing Compose project {project} of an older configuration"),
                );
                let mut command = docker.command(&compose::down_argv(&project, false));
                command.current_dir(&self.data_dir);
                stream(command, "compose", emit, cancel, self.stop_wait())
                    .await?
                    .ok("docker compose down")?;
            }
            None => {
                docker
                    .ok(
                        "removing the container of an older configuration",
                        &["rm".to_owned(), "-f".into(), existing.id.clone()],
                    )
                    .await?;
            }
        }
        self.forget_container(&existing.id);
        Ok(())
    }

    /// One container: build or pull the image, then reuse, restart or create the container and run
    /// its hooks.
    async fn up_container(
        &self,
        docker: &Docker,
        spec: &Spec,
        root: &str,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<Entry, RpcError> {
        if let Some(build) = &spec.build {
            let command = docker.command(&docker::build_argv(
                build,
                spec.platform.as_deref(),
                &spec.image,
            ));
            stream(command, "build", emit, cancel, self.stop_wait())
                .await?
                .ok(&format!("building {}", build.dockerfile))?;
        }
        let (image_id, image_platform) = self.ensure_image(docker, spec, emit, cancel).await?;
        // The digest covers the configuration and the exact image, so a rebuilt or re-pulled image
        // replaces the container.
        let digest = config::root_hash(&format!("{}:{image_id}", spec.digest()));
        let mut restarted = false;
        for existing in self.root_containers(docker, root).await? {
            if existing.label(labels::CONFIG) == digest && existing.project().is_none() {
                if existing.state.running {
                    let entry = entry_of(&existing);
                    self.run_hooks(docker, &entry, &spec.on_attach, emit, cancel)
                        .await?;
                    return Ok(entry);
                }
                docker
                    .ok(
                        "starting the container",
                        &["start".to_owned(), existing.id.clone()],
                    )
                    .await?;
                restarted = true;
            } else {
                self.remove_existing(docker, &existing, emit, cancel)
                    .await?;
            }
        }
        if !restarted {
            if spec.seed_from_image {
                self.seed(docker, spec, root, &image_id).await?;
            }
            let mut ports = spec.ports.clone();
            ports.extend(
                publish_forwarded(&spec.forward_ports, &[])
                    .remove(&None)
                    .unwrap_or_default(),
            );
            let labels = docker::labels_of(spec, root, &digest, &image_platform)
                .map_err(RpcError::internal)?;
            docker
                .ok(
                    "starting the container",
                    &docker::run_argv(spec, root, &labels, &ports),
                )
                .await?;
        }
        let Some(created) = self.root_containers(docker, root).await?.into_iter().next() else {
            return Err(RpcError::internal(
                "the container disappeared right after it was started",
            ));
        };
        let entry = entry_of(&created);
        if !created.state.running {
            let _ = docker.run(&["rm", "-f", created.id.as_str()]).await;
            let hint = if spec.override_command {
                ""
            } else {
                "; the image's own command ended, so keep override_command (overrideCommand) true"
            };
            return Err(RpcError::internal(format!(
                "the container stopped right after starting ({}){hint}",
                entry.info.problem.clone().unwrap_or_default()
            )));
        }
        let hooks: Vec<Hook> = if restarted {
            spec.on_start
                .iter()
                .chain(&spec.on_attach)
                .cloned()
                .collect()
        } else {
            spec.on_create
                .iter()
                .chain(&spec.on_start)
                .chain(&spec.on_attach)
                .cloned()
                .collect()
        };
        if let Err(error) = self.run_hooks(docker, &entry, &hooks, emit, cancel).await {
            // A container whose setup failed is not reused by the next attach.
            let _ = docker.run(&["rm", "-f", created.id.as_str()]).await;
            self.forget_container(&created.id);
            return Err(error);
        }
        Ok(entry)
    }

    /// A process of `docker compose` on the project's files: in the root, with the interpolation
    /// environment.
    fn compose_command(
        &self,
        docker: &Docker,
        compose: &Compose,
        root: &str,
        argv: &[String],
    ) -> tokio::process::Command {
        let mut command = docker.command(argv);
        command
            .current_dir(root)
            .envs(compose.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        command
    }

    /// The containers of a Compose project (all services).
    async fn project_containers(
        &self,
        docker: &Docker,
        project: &str,
    ) -> Result<Vec<Inspected>, RpcError> {
        let ids = self
            .ps_argv(
                docker,
                &docker::ps_label_argv(&[(compose::labels::PROJECT, project)]),
            )
            .await?;
        self.inspect(docker, &ids).await
    }

    /// The container of `service` in the root's Compose project (the lowest replica number,
    /// preferring a running one).
    async fn main_container(
        &self,
        docker: &Docker,
        root: &str,
        project: &str,
        service: &str,
    ) -> Result<Option<Inspected>, RpcError> {
        let ids = self
            .ps_argv(
                docker,
                &docker::ps_label_argv(&[
                    (labels::ROOT, root),
                    (compose::labels::PROJECT, project),
                    (compose::labels::SERVICE, service),
                ]),
            )
            .await?;
        let mut found = self.inspect(docker, &ids).await?;
        found.sort_by_key(|c| {
            (
                !c.state.running,
                c.label(compose::labels::NUMBER)
                    .parse::<u32>()
                    .unwrap_or(u32::MAX),
            )
        });
        Ok(found.into_iter().next())
    }

    /// Ports the project's containers were created to publish, so an unchanged forwardPorts entry
    /// keeps its host port (and Compose sees an unchanged configuration).
    async fn held_ports(&self, docker: &Docker, project: &str) -> Result<Vec<PortInfo>, RpcError> {
        Ok(self
            .project_containers(docker, project)
            .await?
            .iter()
            .flat_map(Inspected::bindings)
            .collect())
    }

    /// A Docker Compose project: `up -d --build --wait` with the override, then the lifecycle
    /// commands in `service` (create/start/attach decided by the container's id and start time).
    async fn up_compose(
        &self,
        docker: &Docker,
        spec: &Spec,
        compose: &Compose,
        root: &str,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<Entry, RpcError> {
        for existing in self.root_containers(docker, root).await? {
            if existing.project().as_deref() != Some(compose.project.as_str()) {
                self.remove_existing(docker, &existing, emit, cancel)
                    .await?;
            }
        }
        let config = self
            .compose_command(docker, compose, root, &compose::config_argv(compose))
            .output()
            .await;
        let config = config
            .map_err(|e| unavailable(format!("cannot run {}: {e}", docker.program.display())))?;
        let config_out = docker::Output {
            code: config.status.code(),
            stdout: String::from_utf8_lossy(&config.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&config.stderr).into_owned(),
        };
        if !config_out.success() {
            return Err(invalid(format!(
                "the compose files are not valid (docker compose config): {}",
                config_out.said()
            )));
        }
        let services = compose::services(&config_out.stdout).map_err(RpcError::internal)?;
        let named = std::iter::once(&compose.service)
            .chain(&compose.run_services)
            .chain(spec.forward_ports.iter().filter_map(|p| p.service.as_ref()));
        for service in named {
            if !services.contains(service) {
                return Err(invalid(format!(
                    "service `{service}` is not in the compose files (services: {})",
                    services.join(", ")
                )));
            }
        }
        let before = self
            .main_container(docker, root, &compose.project, &compose.service)
            .await?;
        let held = self.held_ports(docker, &compose.project).await?;
        let ports: BTreeMap<String, Vec<String>> = publish_forwarded(&spec.forward_ports, &held)
            .into_iter()
            .map(|(service, specs)| (service.unwrap_or_else(|| compose.service.clone()), specs))
            .fold(BTreeMap::new(), |mut all, (service, specs)| {
                all.entry(service).or_insert_with(Vec::new).extend(specs);
                all
            });
        let digest = spec.digest();
        let labels = docker::labels_of(
            spec,
            root,
            &digest,
            spec.platform.as_deref().unwrap_or_default(),
        )
        .map_err(RpcError::internal)?;
        let override_value =
            compose::override_file(spec, compose, &labels, &ports).map_err(invalid)?;
        let override_path = self.data_file("compose", root)?;
        std::fs::write(
            &override_path,
            serde_json::to_string_pretty(&override_value).unwrap_or_default(),
        )
        .map_err(|e| {
            RpcError::internal(format!("cannot write {}: {e}", override_path.display()))
        })?;
        let up = compose::up_argv(compose, &override_path.to_string_lossy());
        stream(
            self.compose_command(docker, compose, root, &up),
            "compose",
            emit,
            cancel,
            self.stop_wait(),
        )
        .await?
        .ok("docker compose up")?;
        let Some(main) = self
            .main_container(docker, root, &compose.project, &compose.service)
            .await?
        else {
            return Err(RpcError::internal(format!(
                "docker compose up finished, but service `{}` has no container",
                compose.service
            )));
        };
        if !main.state.running {
            return Err(RpcError::internal(format!(
                "service `{}` is not running after docker compose up ({}){}",
                compose.service,
                entry_of(&main).info.problem.unwrap_or_default(),
                if spec.override_command {
                    ""
                } else {
                    "; if its command ends at once, set override_command (overrideCommand) true"
                }
            )));
        }
        let entry = entry_of(&main);
        let hooks: Vec<Hook> = match &before {
            Some(b)
                if b.id == main.id
                    && b.state.running
                    && b.state.started_at == main.state.started_at =>
            {
                spec.on_attach.clone()
            }
            Some(b) if b.id == main.id => spec
                .on_start
                .iter()
                .chain(&spec.on_attach)
                .cloned()
                .collect(),
            _ => spec
                .on_create
                .iter()
                .chain(&spec.on_start)
                .chain(&spec.on_attach)
                .cloned()
                .collect(),
        };
        if let Err(error) = self.run_hooks(docker, &entry, &hooks, emit, cancel).await {
            // Removing the service's container makes the next attach create it again, so its
            // creation commands run again (the other services keep running).
            let _ = docker.run(&["rm", "-f", main.id.as_str()]).await;
            self.forget_container(&main.id);
            return Err(error);
        }
        Ok(entry)
    }

    /// `<data>/<what>/<hash of root>.json`.
    fn data_file(&self, what: &str, root: &str) -> Result<PathBuf, RpcError> {
        let dir = self.data_dir.join(what);
        std::fs::create_dir_all(&dir)
            .map_err(|e| RpcError::internal(format!("cannot create {}: {e}", dir.display())))?;
        Ok(dir.join(format!("{}.json", config::root_hash(root))))
    }

    /// The Dev Containers CLI, installed first when needed.
    async fn ensure_cli(
        &self,
        docker: &Docker,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<Cli, RpcError> {
        let setup = self.devcli_setup(Some(docker));
        match setup.locate().map_err(unavailable)? {
            Located::Ready(cli) => Ok(cli),
            Located::Installable { node, npm } => {
                let staging = setup.staging().map_err(RpcError::internal)?;
                std::fs::create_dir_all(&staging).map_err(|e| {
                    RpcError::internal(format!("cannot create {}: {e}", staging.display()))
                })?;
                info_line(
                    emit,
                    "install",
                    format!(
                        "installing {}@{} into {}",
                        devcli::PACKAGE,
                        setup.version,
                        setup.toolchains.display()
                    ),
                );
                let installed = stream(
                    setup.npm_command(&node, &npm, &staging),
                    "install",
                    emit,
                    cancel,
                    self.stop_wait(),
                )
                .await;
                let installed = installed.and_then(|s| {
                    s.ok(&format!(
                        "npm install {}@{}",
                        devcli::PACKAGE,
                        setup.version
                    ))
                });
                if let Err(error) = installed {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err(error);
                }
                setup.commit(&node, &staging).map_err(RpcError::internal)
            }
        }
    }

    /// A devcontainer with features: `devcontainer up`, then the merged exec settings.
    async fn up_cli(
        &self,
        docker: &Docker,
        spec: &Spec,
        cli: &DevcontainerCli,
        root: &str,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<Entry, RpcError> {
        let tool = self.ensure_cli(docker, emit, cancel).await?;
        let digest = spec.digest();
        let mut held = Vec::new();
        for existing in self.root_containers(docker, root).await? {
            let same = match &cli.compose {
                // Compose recreates the service when its labels change.
                Some((project, _)) => existing.project().as_deref() == Some(project.as_str()),
                None => existing.label(labels::CONFIG) == digest && existing.project().is_none(),
            };
            if same {
                held.extend(existing.bindings());
            } else {
                self.remove_existing(docker, &existing, emit, cancel)
                    .await?;
            }
        }
        if let Some((project, _)) = &cli.compose {
            held = self.held_ports(docker, project).await?;
        }
        let labels = docker::labels_of(spec, root, &digest, "").map_err(RpcError::internal)?;
        let override_path = if spec.forward_ports.is_empty() {
            None
        } else {
            let mut published = publish_forwarded(&spec.forward_ports, &held);
            let main_ports = published.remove(&None).unwrap_or_default();
            let ports_file = match &cli.compose {
                Some((_, service)) => {
                    let mut services = serde_json::Map::new();
                    let all = published
                        .into_iter()
                        .map(|(s, p)| (s.unwrap_or_default(), p))
                        .chain(std::iter::once((service.clone(), main_ports.clone())));
                    for (name, specs) in all.filter(|(_, p)| !p.is_empty()) {
                        let entry = services
                            .entry(name)
                            .or_insert_with(|| serde_json::json!({"ports": []}));
                        if let Some(list) = entry.get_mut("ports").and_then(Value::as_array_mut) {
                            list.extend(specs.iter().map(|p| Value::String(compose::escape(p))));
                        }
                    }
                    let path = self.data_file("devcontainer-ports", root)?;
                    std::fs::write(
                        &path,
                        serde_json::json!({ "services": services }).to_string(),
                    )
                    .map_err(|e| {
                        RpcError::internal(format!("cannot write {}: {e}", path.display()))
                    })?;
                    Some(path.to_string_lossy().into_owned())
                }
                None => None,
            };
            let config = devcli::override_config(cli, ports_file.as_deref(), &main_ports);
            let path = self.data_file("devcontainer", root)?;
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&config).unwrap_or_default(),
            )
            .map_err(|e| RpcError::internal(format!("cannot write {}: {e}", path.display())))?;
            Some(path.to_string_lossy().into_owned())
        };
        let argv = devcli::up_argv(
            root,
            &cli.config_file,
            &docker.program,
            override_path.as_deref(),
            &labels,
        );
        let env: Vec<(String, String)> = cli
            .compose
            .iter()
            .map(|(project, _)| ("COMPOSE_PROJECT_NAME".to_owned(), project.clone()))
            .collect();
        let mut command = tool.command(&argv, &env);
        command.current_dir(root);
        let streamed = stream(command, "devcontainer", emit, cancel, self.stop_wait()).await?;
        let up = devcli::parse_up(&streamed.stdout).map_err(|e| {
            if streamed.code == Some(0) {
                RpcError::internal(e)
            } else {
                RpcError::internal(format!("{e}\n{}", streamed.said()))
            }
        })?;
        let Some(created) = self
            .inspect(docker, std::slice::from_ref(&up.container_id))
            .await?
            .into_iter()
            .next()
        else {
            return Err(RpcError::internal(format!(
                "devcontainer up reported container {}, which does not exist",
                up.container_id
            )));
        };
        if created.label(labels::ROOT) != root {
            return Err(RpcError::internal(format!(
                "devcontainer up returned container {} without this project's label {}={root}",
                up.container_id,
                labels::ROOT
            )));
        }
        let entry = entry_of(&created);
        if let Some(folder) = &up.remote_workspace_folder
            && normalize(folder) != normalize(&spec.workdir)
        {
            return Err(RpcError::internal(format!(
                "the Dev Containers CLI uses workspace folder {folder}, but this plugin resolved {} from {}",
                spec.workdir, cli.config_file
            )));
        }
        if !created.state.running {
            return Err(RpcError::internal(format!(
                "the container is not running after devcontainer up ({})",
                entry.info.problem.clone().unwrap_or_default()
            )));
        }
        self.settings_of(docker, &entry).await?;
        Ok(entry)
    }

    fn forget_container(&self, id: &str) {
        if let Ok(mut shells) = self.shells.lock() {
            shells.remove(id);
        }
        if let Ok(mut settings) = self.settings.lock() {
            settings.remove(id);
        }
    }

    /// The image's id and platform, pulling it when it is missing or has another platform.
    async fn ensure_image(
        &self,
        docker: &Docker,
        spec: &Spec,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<(String, String), RpcError> {
        let matches = |platform: &str| match &spec.platform {
            None => true,
            Some(wanted) => platform == wanted || platform.starts_with(&format!("{wanted}/")),
        };
        if let Some((id, platform)) = self.image_identity(docker, &spec.image).await?
            && matches(&platform)
        {
            return Ok((id, platform));
        }
        if spec.build.is_some() {
            return Err(RpcError::internal(format!(
                "the built image {} is not available for {}",
                spec.image,
                spec.platform.as_deref().unwrap_or("this machine")
            )));
        }
        self.pull_image(docker, &spec.image, spec.platform.as_deref(), emit, cancel)
            .await?;
        let (id, platform) = self
            .image_identity(docker, &spec.image)
            .await?
            .ok_or_else(|| {
                RpcError::internal(format!("{} is missing after pulling it", spec.image))
            })?;
        if !matches(&platform) {
            return Err(RpcError::internal(format!(
                "{} is {platform}, not {}",
                spec.image,
                spec.platform.as_deref().unwrap_or_default()
            )));
        }
        Ok((id, platform))
    }

    /// `(id, os/arch[/variant])` of a local image; `None` when it is not there.
    async fn image_identity(
        &self,
        docker: &Docker,
        image: &str,
    ) -> Result<Option<(String, String)>, RpcError> {
        let inspect = [
            "image",
            "inspect",
            "--format",
            "{{.Id}}|{{.Os}}/{{.Architecture}}{{if .Variant}}/{{.Variant}}{{end}}",
            image,
        ];
        let output = docker.run(&inspect).await?;
        if !output.success() {
            return Ok(None);
        }
        output
            .stdout
            .trim()
            .split_once('|')
            .map(|(id, platform)| Some((id.to_owned(), platform.to_owned())))
            .ok_or_else(|| {
                RpcError::internal(format!(
                    "unexpected docker image inspect output: {}",
                    output.stdout
                ))
            })
    }

    async fn pull_image(
        &self,
        docker: &Docker,
        image: &str,
        platform: Option<&str>,
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<(), RpcError> {
        let mut pull = vec!["pull".to_owned()];
        if let Some(platform) = platform {
            pull.extend(["--platform".into(), platform.to_owned()]);
        }
        pull.push(image.to_owned());
        stream(
            docker.command(&pull),
            "pull",
            emit,
            cancel,
            self.stop_wait(),
        )
        .await?
        .ok(&format!("pulling {image}"))?;
        Ok(())
    }

    /// Copy the image's workdir to the (empty) root. A root that was seeded from the same image
    /// before keeps its files (they may have been edited since).
    async fn seed(
        &self,
        docker: &Docker,
        spec: &Spec,
        root: &str,
        image_id: &str,
    ) -> Result<(), RpcError> {
        let marker_dir = self.data_dir.join("seeds");
        let marker = marker_dir.join(format!("{}.json", config::root_hash(root)));
        let empty = std::fs::read_dir(root)
            .map_err(|e| RpcError::internal(format!("{root}: {e}")))?
            .next()
            .is_none();
        if !empty {
            let seeded: Option<Value> = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            if seeded
                .as_ref()
                .is_some_and(|s| s["root"] == root && s["image_id"] == image_id)
            {
                return Ok(());
            }
            return Err(invalid(format!(
                "seed_from_image needs an empty project root, and {root} already has files that were not seeded from {}",
                spec.image
            )));
        }
        let name = format!(
            "cortex-seed-{}-{}",
            config::root_hash(root),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let mut create = vec!["create".to_owned(), "--name".into(), name.clone()];
        if let Some(platform) = &spec.platform {
            create.extend(["--platform".into(), platform.clone()]);
        }
        create.extend([spec.image.clone(), "true".into()]);
        docker
            .ok(
                "creating a container to copy the image's files from",
                &create,
            )
            .await?;
        let copied = docker
            .ok(
                &format!("copying {} out of {}", spec.workdir, spec.image),
                &[
                    "cp".to_owned(),
                    format!("{name}:{}/.", spec.workdir),
                    root.to_owned(),
                ],
            )
            .await;
        let _ = docker.run(&["rm", "-f", name.as_str()]).await;
        copied?;
        std::fs::create_dir_all(&marker_dir).map_err(|e| RpcError::internal(e.to_string()))?;
        std::fs::write(
            &marker,
            serde_json::json!({"root": root, "image_id": image_id, "image": spec.image})
                .to_string(),
        )
        .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(())
    }

    /// Lifecycle commands, in the container, as the exec user, in its workdir; a failing one fails.
    async fn run_hooks(
        &self,
        docker: &Docker,
        entry: &Entry,
        hooks: &[Hook],
        emit: &Emit,
        cancel: &CancellationToken,
    ) -> Result<(), RpcError> {
        if hooks.is_empty() {
            return Ok(());
        }
        let settings = self.settings_of(docker, entry).await?;
        for hook in hooks {
            let commands: Vec<(String, Command)> = match &hook.command {
                Command::Parallel(named) => named
                    .iter()
                    .map(|(name, c)| (format!("{}.{name}", hook.name), c.clone()))
                    .collect(),
                single => vec![(hook.name.clone(), single.clone())],
            };
            let mut running = tokio::task::JoinSet::new();
            for (name, command) in commands {
                let program: Vec<String> = match command {
                    Command::Shell(line) => vec!["/bin/sh".into(), "-c".into(), line],
                    Command::Argv(argv) => argv,
                    Command::Parallel(_) => {
                        return Err(RpcError::internal(format!(
                            "{name}: nested parallel commands"
                        )));
                    }
                };
                let marker = uuid::Uuid::new_v4().simple().to_string();
                let argv = docker::exec_argv_direct(
                    &entry.info.id,
                    &entry.info.workdir,
                    settings.user.as_deref(),
                    &settings.env,
                    &marker,
                    &program,
                );
                let command = docker.command(&argv);
                let emit = emit.clone();
                let cancel = cancel.clone();
                let stop_wait = self.stop_wait();
                running.spawn(async move {
                    stream(command, &name, &emit, &cancel, stop_wait)
                        .await
                        .and_then(|s| s.ok(&name))
                        .map(|_| ())
                });
            }
            while let Some(done) = running.join_next().await {
                done.map_err(|e| RpcError::internal(e.to_string()))??;
            }
        }
        Ok(())
    }

    // ---- status / list -------------------------------------------------------------------------

    pub async fn status(&self, root: &str) -> Result<Option<ContainerInfo>, RpcError> {
        let root = self.root_key(root)?;
        let docker = match self.docker() {
            Ok(docker) => docker,
            Err(message) => return Ok(self.unreachable(&root, &message)),
        };
        let found = match self.lookup(&docker, &root).await {
            Ok(found) => found,
            Err(error) if error.code == codes::UNAVAILABLE => {
                return Ok(self.unreachable(&root, &error.message));
            }
            Err(error) => return Err(error),
        };
        Ok(self.settle(&root, found))
    }

    /// Without Docker, a remembered container is reported stopped, with the reason.
    fn unreachable(&self, root: &str, message: &str) -> Option<ContainerInfo> {
        self.remembered(root).map(|entry| {
            let mut info = entry.info;
            if info.state != STATE_FAILED {
                info.state = STATE_STOPPED.into();
            }
            info.problem = Some(message.to_owned());
            info
        })
    }

    /// Reconcile memory with what Docker reported for `root`.
    fn settle(&self, root: &str, found: Option<Entry>) -> Option<ContainerInfo> {
        match found {
            Some(entry) => {
                self.store(root, Some(entry.clone()));
                Some(entry.info)
            }
            None => match self.remembered(root) {
                // An attach that failed or is still starting has no container yet.
                Some(entry)
                    if entry.info.state == STATE_FAILED || entry.info.state == STATE_STARTING =>
                {
                    Some(entry.info)
                }
                Some(_) => {
                    self.store(root, None);
                    None
                }
                None => None,
            },
        }
    }

    pub async fn list(&self) -> Result<ListResult, RpcError> {
        let docker = match self.docker() {
            Ok(docker) => docker,
            Err(message) => return Ok(self.list_unreachable(&message)),
        };
        let ids = match self.ps(&docker, None).await {
            Ok(ids) => ids,
            Err(error) if error.code == codes::UNAVAILABLE => {
                return Ok(self.list_unreachable(&error.message));
            }
            Err(error) => return Err(error),
        };
        let found: Vec<Entry> = self
            .inspect(&docker, &ids)
            .await?
            .iter()
            .map(entry_of)
            .collect();
        let mut containers = Vec::new();
        let seen: HashSet<String> = found.iter().map(|e| e.info.root.clone()).collect();
        for entry in found {
            self.store(&entry.info.root.clone(), Some(entry.clone()));
            containers.push(entry.info);
        }
        let remembered: Vec<String> = self
            .entries
            .lock()
            .map(|e| e.keys().cloned().collect())
            .unwrap_or_default();
        for root in remembered.iter().filter(|root| !seen.contains(*root)) {
            // Failed or still-starting attaches have no container; anything else is gone.
            if let Some(info) = self.settle(root, None) {
                containers.push(info);
            }
        }
        containers.sort_by(|a, b| a.root.cmp(&b.root));
        Ok(ListResult { containers })
    }

    fn list_unreachable(&self, message: &str) -> ListResult {
        let roots: Vec<String> = self
            .entries
            .lock()
            .map(|e| e.keys().cloned().collect())
            .unwrap_or_default();
        let mut containers: Vec<ContainerInfo> = roots
            .iter()
            .filter_map(|root| self.unreachable(root, message))
            .collect();
        containers.sort_by(|a, b| a.root.cmp(&b.root));
        ListResult { containers }
    }

    // ---- detach --------------------------------------------------------------------------------

    pub async fn detach(&self, root: &str) -> Result<Option<ContainerInfo>, RpcError> {
        self.detach_with(DetachParams {
            root: root.to_owned(),
            remove_volumes: false,
        })
        .await
    }

    /// Remove the root's container, or `docker compose down` its Compose project (also one whose
    /// service container is already gone, found by the project name derived from the root).
    pub async fn detach_with(
        &self,
        params: DetachParams,
    ) -> Result<Option<ContainerInfo>, RpcError> {
        let root = self.root_key(&params.root)?;
        let _guard = self.busy(&root)?;
        let docker = self.docker_or_unavailable()?;
        let remembered = self.remembered(&root);
        let found = self.root_containers(&docker, &root).await?;
        let mut last = found
            .first()
            .map(|c| entry_of(c).info)
            .or(remembered.map(|e| e.info));
        let mut projects: BTreeSet<String> = found.iter().filter_map(Inspected::project).collect();
        let derived = config::compose_project(&root);
        if !projects.contains(&derived)
            && !self
                .ps_argv(
                    &docker,
                    &docker::ps_label_argv(&[(compose::labels::PROJECT, &derived)]),
                )
                .await?
                .is_empty()
        {
            projects.insert(derived);
        }
        for project in &projects {
            let mut command = docker.command(&compose::down_argv(project, params.remove_volumes));
            command.current_dir(&self.data_dir);
            let output = command.output().await.map_err(|e| {
                unavailable(format!("cannot run {}: {e}", docker.program.display()))
            })?;
            if !output.status.success() {
                return Err(RpcError::internal(format!(
                    "docker compose down of {project} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        for container in found.iter().filter(|c| c.project().is_none()) {
            let mut rm = vec!["rm".to_owned(), "-f".into()];
            if params.remove_volumes {
                rm.push("-v".into());
            }
            rm.push(container.id.clone());
            docker.ok("removing the container", &rm).await?;
        }
        for container in &found {
            self.forget_container(&container.id);
        }
        if let Some(info) = &mut last {
            info.state = STATE_STOPPED.into();
            info.problem = None;
        }
        let had_entry = self
            .entries
            .lock()
            .map(|mut e| e.remove(&root).is_some())
            .unwrap_or(false);
        if let Some(info) = &last
            && (had_entry || !found.is_empty())
        {
            (self.publish)(info);
        }
        Ok(last)
    }

    // ---- exec ----------------------------------------------------------------------------------

    /// The running container of `root`, from memory or Docker.
    async fn running(&self, docker: &Docker, root: &str) -> Result<Entry, RpcError> {
        let entry = match self.remembered(root) {
            Some(entry) if entry.info.state == STATE_RUNNING => entry,
            _ => {
                let found = self.lookup(docker, root).await?;
                match (found, self.settle(root, None)) {
                    (Some(found), _) => {
                        self.store(root, Some(found.clone()));
                        found
                    }
                    // A failed or still-running attach: say that rather than "no container".
                    (None, Some(info)) => {
                        return Err(unavailable(format!(
                            "the container of {root} is {}{}",
                            info.state,
                            info.problem
                                .as_deref()
                                .map(|p| format!(": {p}"))
                                .unwrap_or_default()
                        )));
                    }
                    (None, None) => {
                        return Err(RpcError::not_found(format!(
                            "{root} has no container; call container.attach first"
                        )));
                    }
                }
            }
        };
        if entry.info.state != STATE_RUNNING {
            return Err(unavailable(format!(
                "the container of {root} is {}{}",
                entry.info.state,
                entry
                    .info
                    .problem
                    .as_deref()
                    .map(|p| format!(": {p}"))
                    .unwrap_or_default()
            )));
        }
        Ok(entry)
    }

    /// After a failed docker call on a container: if it is no longer running, say so (and update
    /// the state); `None` when it is still running.
    async fn gone(&self, docker: &Docker, root: &str) -> Option<RpcError> {
        match self.lookup(docker, root).await {
            Ok(Some(entry)) if entry.info.state == STATE_RUNNING => None,
            Ok(found) => {
                let message = match &found {
                    Some(entry) => format!(
                        "the container of {root} stopped: {}",
                        entry.info.problem.clone().unwrap_or_default()
                    ),
                    None => format!("the container of {root} no longer exists; attach again"),
                };
                if let Some(entry) = &found {
                    self.forget_container(&entry.info.id);
                }
                self.store(root, found);
                Some(unavailable(message))
            }
            Err(error) => Some(error),
        }
    }

    /// The shell commands run through: bash when the image has it, else sh (checked once per
    /// container).
    async fn shell(
        &self,
        docker: &Docker,
        root: &str,
        entry: &Entry,
        user: Option<&str>,
    ) -> Result<String, RpcError> {
        if let Some(shell) = self
            .shells
            .lock()
            .ok()
            .and_then(|s| s.get(&entry.info.id).cloned())
        {
            return Ok(shell);
        }
        for shell in ["bash", "sh"] {
            let output = docker
                .run(&docker::shell_check_argv(&entry.info.id, user, shell))
                .await?;
            if output.success() {
                if let Ok(mut shells) = self.shells.lock() {
                    shells.insert(entry.info.id.clone(), shell.to_owned());
                }
                return Ok(shell.to_owned());
            }
        }
        if let Some(error) = self.gone(docker, root).await {
            return Err(error);
        }
        Err(RpcError::internal(format!(
            "the image {} has neither bash nor sh, so commands cannot run in it",
            entry.info.image
        )))
    }

    /// The exec user and per-command environment, with `${containerEnv:VAR}` resolved (once per
    /// container). A container the Dev Containers CLI created gets them from the CLI's merged
    /// configuration (features contribute to them).
    async fn settings_of(&self, docker: &Docker, entry: &Entry) -> Result<ExecSettings, RpcError> {
        if let Some(settings) = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.get(&entry.info.id).cloned())
        {
            return Ok(settings);
        }
        let mut settings = if entry.info.kind.as_deref() == Some(kinds::DEVCONTAINER_CLI) {
            let config_file = entry.config_file.clone().ok_or_else(|| {
                RpcError::internal(format!(
                    "container {} has no {} label",
                    entry.info.id,
                    labels::CONFIG_FILE
                ))
            })?;
            let tool = match self
                .devcli_setup(Some(docker))
                .locate()
                .map_err(unavailable)?
            {
                Located::Ready(cli) => cli,
                Located::Installable { .. } => {
                    return Err(unavailable(format!(
                        "the Dev Containers CLI that created {} is not installed any more; attach again to install it",
                        entry.info.id
                    )));
                }
            };
            let argv = devcli::read_configuration_argv(
                &entry.info.root,
                &config_file,
                &docker.program,
                &entry.info.id,
            );
            let mut command = tool.command(&argv, &[]);
            if Path::new(&entry.info.root).is_dir() {
                command.current_dir(&entry.info.root);
            }
            let output = command
                .output()
                .await
                .map_err(|e| unavailable(format!("cannot run the Dev Containers CLI: {e}")))?;
            if !output.status.success() {
                return Err(RpcError::internal(format!(
                    "devcontainer read-configuration failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            devcli::parse_merged(&String::from_utf8_lossy(&output.stdout))
                .map_err(RpcError::internal)?
        } else {
            entry.exec.clone()
        };
        if settings
            .env
            .iter()
            .any(|(_, v)| v.contains("${containerEnv:"))
        {
            let inspected = self
                .inspect(docker, std::slice::from_ref(&entry.info.id))
                .await?;
            let container_env = inspected.first().map(Inspected::env).unwrap_or_default();
            settings.env = settings
                .env
                .iter()
                .map(|(k, v)| (k.clone(), container_env_substitute(v, &container_env)))
                .collect();
        }
        if let Ok(mut all) = self.settings.lock() {
            all.insert(entry.info.id.clone(), settings.clone());
        }
        Ok(settings)
    }

    pub async fn exec(
        &self,
        params: ExecParams,
        items: ItemSink,
        cancel: CancellationToken,
    ) -> Result<ExecResult, RpcError> {
        if params.command.trim().is_empty() {
            return Err(invalid("command is empty"));
        }
        for (key, _) in &params.env {
            if key.is_empty() || key.contains('=') {
                return Err(invalid(format!(
                    "environment variable name `{key}` is not valid"
                )));
            }
        }
        let root = self.root_key(&params.root)?;
        let docker = self.docker_or_unavailable()?;
        let entry = self.running(&docker, &root).await?;
        let settings = self.settings_of(&docker, &entry).await?;
        let shell = self
            .shell(&docker, &root, &entry, settings.user.as_deref())
            .await?;
        let mut env = settings.env.clone();
        env.extend(params.env.iter().cloned());
        let map = PathMap::new(&params.root, &root, &entry.info.workdir, &entry.info.mounts);
        let cwd = match params.cwd.as_deref() {
            Some(cwd) => map.to_container(cwd).map_err(invalid)?,
            None => map.workdir().to_owned(),
        };
        let marker = uuid::Uuid::new_v4().simple().to_string();
        let argv = docker::exec_argv(
            &entry.info.id,
            &cwd,
            settings.user.as_deref(),
            &env,
            &marker,
            &shell,
            &params.command,
        );
        let config = self.config();
        let stop_wait = Duration::from_millis(config.stop_wait_ms);
        let run = Run {
            command: docker.command(&argv),
            stdin: params.stdin.clone(),
            timeout: params.timeout_ms.map(Duration::from_millis),
            max_output: config.max_output_bytes,
            stop_wait,
            stop_attempts: 3,
        };
        let kill = docker::kill_argv(&entry.info.id, &marker);
        let killer = docker.clone();
        let stop: Stop = Box::new(move |_| {
            let killer = killer.clone();
            let kill = kill.clone();
            Box::pin(async move {
                // Bounded: a wedged daemon must not hold the stop forever; the engine then kills
                // the client.
                let _ = tokio::time::timeout(stop_wait * 3, killer.run(&kill)).await;
            })
        });
        // The command runs on its own task: when the caller cancels, the request future is dropped,
        // but the task still sees the cancellation and kills the command inside the container.
        let token = self.stopping.child_token();
        let done = CancellationToken::new();
        {
            let token = token.clone();
            let done = done.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => token.cancel(),
                    _ = done.cancelled() => {}
                    _ = token.cancelled() => {}
                }
            });
        }
        let task = self.tasks.spawn(async move {
            let emit = move |item: ExecItem| {
                if let Ok(value) = serde_json::to_value(&item) {
                    items.send(value);
                }
            };
            let result = exec::run(run, &emit, token, stop).await;
            done.cancel();
            result
        });
        let result = task
            .await
            .map_err(|e| RpcError::internal(format!("exec task: {e}")))?
            .map_err(|e| unavailable(format!("cannot run {}: {e}", docker.program.display())))?;
        if !result.timed_out
            && !result.cancelled
            && result.exit_code != Some(0)
            && let Some(error) = self.gone(&docker, &root).await
        {
            return Err(error);
        }
        Ok(result)
    }

    pub async fn shell_command(&self, root: &str) -> Result<ShellCommandResult, RpcError> {
        let root = self.root_key(root)?;
        let docker = self.docker_or_unavailable()?;
        let entry = self.running(&docker, &root).await?;
        let settings = self.settings_of(&docker, &entry).await?;
        let shell = self
            .shell(&docker, &root, &entry, settings.user.as_deref())
            .await?;
        Ok(ShellCommandResult {
            argv: docker::shell_argv(
                &docker.program,
                &entry.info.id,
                &entry.info.workdir,
                settings.user.as_deref(),
                &settings.env,
                &shell,
            ),
        })
    }

    // ---- images, build, pull -----------------------------------------------------------------

    async fn reachable_docker(&self) -> Result<Docker, RpcError> {
        let docker = self.docker_or_unavailable()?;
        docker.daemon().await.map_err(unavailable)?;
        Ok(docker)
    }

    pub async fn images(&self, params: ImagesParams) -> Result<ImagesResult, RpcError> {
        if let Some(reference) = &params.reference
            && (reference.is_empty() || reference.starts_with('-'))
        {
            return Err(invalid(format!("reference `{reference}` is not valid")));
        }
        let docker = self.docker_or_unavailable()?;
        let output = docker
            .run(&docker::images_argv(params.reference.as_deref()))
            .await?;
        if !output.success() {
            return Err(self
                .docker_failure(&docker, "docker image ls", &output)
                .await);
        }
        Ok(ImagesResult {
            images: docker::parse_images(&output.stdout).map_err(RpcError::internal)?,
        })
    }

    fn check_image_name(image: &str) -> Result<(), RpcError> {
        if image.is_empty()
            || image.starts_with('-')
            || image.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid(format!("image `{image}` is not a valid reference")));
        }
        Ok(())
    }

    pub async fn build(
        &self,
        params: BuildParams,
        items: &ItemSink,
        cancel: &CancellationToken,
    ) -> Result<ImageResult, RpcError> {
        let root = self.existing_root(&params.root)?;
        let root_text = root.to_string_lossy().into_owned();
        let absolute = |p: &str| {
            if Path::new(p).is_absolute() {
                normalize(p)
            } else {
                normalize(&root.join(p).to_string_lossy())
            }
        };
        let dockerfile = absolute(params.dockerfile.as_deref().unwrap_or("Dockerfile"));
        if !Path::new(&dockerfile).is_file() {
            return Err(RpcError::not_found(format!(
                "Dockerfile {dockerfile} does not exist"
            )));
        }
        let context = params
            .context
            .as_deref()
            .map(absolute)
            .unwrap_or_else(|| normalize(&root_text));
        if !Path::new(&context).is_dir() {
            return Err(RpcError::not_found(format!(
                "build context {context} is not a directory"
            )));
        }
        let tag = params
            .tag
            .clone()
            .unwrap_or_else(|| format!("cortex-build-{}:latest", config::root_hash(&root_text)));
        Self::check_image_name(&tag)?;
        if let Some(platform) = &params.platform {
            config::check_platform(platform).map_err(invalid)?;
        }
        for (key, _) in &params.build_args {
            if key.is_empty() || key.contains('=') {
                return Err(invalid(format!("build argument name `{key}` is not valid")));
            }
        }
        if let Some(target) = &params.target {
            config::check_name("target", target).map_err(invalid)?;
        }
        let mut options = Vec::new();
        if params.no_cache {
            options.push("--no-cache".to_owned());
        }
        if params.pull {
            options.push("--pull".to_owned());
        }
        let build = config::Build {
            dockerfile,
            context,
            args: params.build_args.clone(),
            target: params.target.clone(),
            options,
            cache_from: Vec::new(),
        };
        let docker = self.reachable_docker().await?;
        let started = Instant::now();
        let emit = sink(items);
        stream(
            docker.command(&docker::build_argv(
                &build,
                params.platform.as_deref(),
                &tag,
            )),
            "build",
            &emit,
            cancel,
            self.stop_wait(),
        )
        .await?
        .ok(&format!("building {}", build.dockerfile))?;
        let (id, platform) = self
            .image_identity(&docker, &tag)
            .await?
            .ok_or_else(|| RpcError::internal(format!("{tag} is missing after building it")))?;
        Ok(ImageResult {
            image: tag,
            id,
            platform,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub async fn pull(
        &self,
        params: PullParams,
        items: &ItemSink,
        cancel: &CancellationToken,
    ) -> Result<ImageResult, RpcError> {
        Self::check_image_name(&params.image)?;
        if let Some(platform) = &params.platform {
            config::check_platform(platform).map_err(invalid)?;
        }
        let docker = self.reachable_docker().await?;
        let started = Instant::now();
        self.pull_image(
            &docker,
            &params.image,
            params.platform.as_deref(),
            &sink(items),
            cancel,
        )
        .await?;
        let (id, platform) = self
            .image_identity(&docker, &params.image)
            .await?
            .ok_or_else(|| {
                RpcError::internal(format!("{} is missing after pulling it", params.image))
            })?;
        Ok(ImageResult {
            image: params.image,
            id,
            platform,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    // ---- ps, ports, logs -----------------------------------------------------------------------

    /// Every container of the root's project: its labelled containers and, for a Compose project,
    /// every service's (main ones first).
    async fn project(
        &self,
        docker: &Docker,
        root: &str,
    ) -> Result<Vec<(bool, Inspected)>, RpcError> {
        let main = self.root_containers(docker, root).await?;
        let mut projects: BTreeSet<String> = main.iter().filter_map(Inspected::project).collect();
        projects.insert(config::compose_project(root));
        let main_ids: HashSet<String> = main.iter().map(|c| c.id.clone()).collect();
        let mut all: Vec<(bool, Inspected)> = main.into_iter().map(|c| (true, c)).collect();
        for project in projects {
            for container in self.project_containers(docker, &project).await? {
                if !main_ids.contains(&container.id) {
                    all.push((false, container));
                }
            }
        }
        all.sort_by(|(a_main, a), (b_main, b)| {
            (!a_main, a.service(), &a.name).cmp(&(!b_main, b.service(), &b.name))
        });
        Ok(all)
    }

    pub async fn ps_project(&self, root: &str) -> Result<PsResult, RpcError> {
        let root = self.root_key(root)?;
        let docker = self.docker_or_unavailable()?;
        let containers = self
            .project(&docker, &root)
            .await?
            .into_iter()
            .map(|(main, c)| ProjectContainer {
                id: c.id.clone(),
                name: c.name.trim_start_matches('/').to_owned(),
                service: c.service(),
                image: c.config.image.clone(),
                state: c.state.status.clone(),
                health: c
                    .state
                    .health
                    .as_ref()
                    .map(|h| h.status.clone())
                    .filter(|s| !s.is_empty()),
                exit_code: (!c.state.running).then_some(c.state.exit_code),
                started_at: Some(c.state.started_at.clone())
                    .filter(|s| !s.is_empty() && !s.starts_with("0001-")),
                main,
                ports: c.published(),
            })
            .collect();
        Ok(PsResult { containers })
    }

    pub async fn ports(&self, root: &str) -> Result<PortsResult, RpcError> {
        Ok(PortsResult {
            ports: self
                .ps_project(root)
                .await?
                .containers
                .into_iter()
                .flat_map(|c| c.ports)
                .collect(),
        })
    }

    pub async fn logs(
        &self,
        params: LogsParams,
        items: &ItemSink,
        cancel: &CancellationToken,
    ) -> Result<LogsResult, RpcError> {
        if let Some(since) = &params.since
            && (since.is_empty()
                || since.starts_with('-')
                || since.chars().any(char::is_whitespace))
        {
            return Err(invalid(format!(
                "since `{since}` is not a timestamp or duration"
            )));
        }
        let root = self.root_key(&params.root)?;
        let docker = self.docker_or_unavailable()?;
        let all = self.project(&docker, &root).await?;
        let selected: Vec<&Inspected> = match &params.service {
            Some(service) => all
                .iter()
                .filter(|(_, c)| c.service().as_deref() == Some(service.as_str()))
                .map(|(_, c)| c)
                .collect(),
            None => all
                .iter()
                .filter(|(main, _)| *main)
                .map(|(_, c)| c)
                .collect(),
        };
        if selected.is_empty() {
            return Err(RpcError::not_found(match &params.service {
                Some(service) => {
                    let services: BTreeSet<String> =
                        all.iter().filter_map(|(_, c)| c.service()).collect();
                    format!(
                        "{root} has no container of service `{service}` (services: {})",
                        if services.is_empty() {
                            "none".to_owned()
                        } else {
                            services.into_iter().collect::<Vec<_>>().join(", ")
                        }
                    )
                }
                None => format!("{root} has no container; call container.attach first"),
            }));
        }
        let config = self.config();
        let mut containers = Vec::new();
        for container in selected {
            let name = container.name.trim_start_matches('/').to_owned();
            let service = container.service();
            let argv = docker::logs_argv(
                &container.id,
                params.tail.unwrap_or(DEFAULT_LOG_TAIL),
                params.since.as_deref(),
                params.timestamps,
            );
            let run = Run {
                command: docker.command(&argv),
                stdin: None,
                timeout: None,
                max_output: config.max_output_bytes,
                stop_wait: Duration::from_millis(config.stop_wait_ms),
                stop_attempts: 1,
            };
            let emit = |item: ExecItem| {
                let item = LogItem {
                    container: name.clone(),
                    service: service.clone(),
                    stream: item.stream,
                    text: item.text,
                };
                if let Ok(value) = serde_json::to_value(&item) {
                    items.send(value);
                }
            };
            let result = exec::run(run, &emit, cancel.clone(), Box::new(|_| Box::pin(async {})))
                .await
                .map_err(|e| {
                    unavailable(format!("cannot run {}: {e}", docker.program.display()))
                })?;
            if result.cancelled {
                return Err(RpcError::cancelled());
            }
            if result.exit_code != Some(0) {
                return Err(RpcError::internal(format!(
                    "docker logs {name} failed (exit {}): {}",
                    docker::code_text(result.exit_code),
                    result.output.trim()
                )));
            }
            containers.push(ContainerLogs {
                id: container.id.clone(),
                name,
                service,
                output: result.output,
            });
        }
        Ok(LogsResult { containers })
    }

    /// Stop every running exec (killing its command in the container) and wait for them, bounded.
    pub async fn shutdown(&self) {
        self.stopping.cancel();
        self.tasks.close();
        let limit = Duration::from_millis(self.config().stop_wait_ms * 4);
        let _ = tokio::time::timeout(limit, self.tasks.wait()).await;
    }
}

/// `forwardPorts` as `docker run -p` values per service (`None`: the container commands run in):
/// the binding the project already has for that port (so its configuration stays the same), else
/// the same port on 127.0.0.1 when it is free on the Mac, else a port Docker picks (reported by
/// `container.ports`).
pub fn publish_forwarded(
    wanted: &[ForwardPort],
    held: &[PortInfo],
) -> BTreeMap<Option<String>, Vec<String>> {
    let mut out: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    for port in wanted {
        let existing = held.iter().find(|h| {
            h.container_port == port.port
                && h.protocol == "tcp"
                && (port.service.is_none() || h.service == port.service)
        });
        let spec = match existing {
            Some(h) => format!("{}:{}:{}", h.host_ip, h.host_port, port.port),
            None if std::net::TcpListener::bind(("127.0.0.1", port.port)).is_ok() => {
                format!("127.0.0.1:{0}:{0}", port.port)
            }
            None => format!("127.0.0.1::{}", port.port),
        };
        out.entry(port.service.clone()).or_default().push(spec);
    }
    out
}
