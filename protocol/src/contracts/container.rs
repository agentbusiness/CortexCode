//! `container@1`: projects whose commands run inside a container. The project's files stay on the
//! Mac (bind-mounted into the container), so the code graph, edits, language servers and the editor
//! work on them as on any project; commands (the `run` tool, terminals, verifiers) run inside the
//! container, with its installed environment. A project gets a container from its
//! `.cortex/container.toml`, its `.devcontainer/devcontainer.json`, or an explicit `attach`. A project may be one container (`docker run`), a Docker
//! Compose project (commands run in one of its services), or a devcontainer with features (created
//! by the Dev Containers CLI).
//!
//! Paths given to `exec` are host paths inside the project's bind mounts or container paths; the
//! plugin maps host paths through the container's actual bind mounts
//! ([`ContainerInfo::mounts`]).

use serde::{Deserialize, Serialize};

pub mod methods {
    /// Give a project a container (start it if needed) -> [`super::ContainerInfo`]; streams
    /// [`super::ProgressItem`]s (initializeCommand, install, build, pull, compose up, lifecycle
    /// commands) when the caller takes items.
    pub const ATTACH: &str = "container.attach";
    /// The project's container, if it has one -> `Option<ContainerInfo>`.
    pub const STATUS: &str = "container.status";
    /// Run a command inside the project's container -> [`super::ExecResult`]; streams
    /// [`super::ExecItem`]s when the caller takes items.
    pub const EXEC: &str = "container.exec";
    /// Stop (and remove) the project's container, or `docker compose down` its Compose project;
    /// the project goes back to running on the Mac. Params: [`super::DetachParams`].
    pub const DETACH: &str = "container.detach";
    /// Every container this plugin manages.
    pub const LIST: &str = "container.list";
    /// The `docker exec` argv that opens an interactive shell in the project's container (for
    /// terminals): `["docker", "exec", "-it", "-w", "/testbed", "<id>", "bash", "-l"]`.
    pub const SHELL_COMMAND: &str = "container.shell_command";
    /// Images in the local image store -> [`super::ImagesResult`]. Params: [`super::ImagesParams`].
    pub const IMAGES: &str = "container.images";
    /// Build an image from a Dockerfile -> [`super::ImageResult`]; streams
    /// [`super::ProgressItem`]s. Params: [`super::BuildParams`].
    pub const BUILD: &str = "container.build";
    /// Pull an image -> [`super::ImageResult`]; streams [`super::ProgressItem`]s. Params:
    /// [`super::PullParams`].
    pub const PULL: &str = "container.pull";
    /// Logs of the project's container or of one of its Compose services -> [`super::LogsResult`];
    /// streams [`super::LogItem`]s. Params: [`super::LogsParams`].
    pub const LOGS: &str = "container.logs";
    /// The project's containers (every service of a Compose project) -> [`super::PsResult`].
    /// Params: [`super::RootParams`].
    pub const PS: &str = "container.ps";
    /// Ports the project's containers publish on the Mac -> [`super::PortsResult`]. Params:
    /// [`super::RootParams`].
    pub const PORTS: &str = "container.ports";
}

pub mod topics {
    /// A project's container changed state: `container.changed` with [`super::ContainerInfo`].
    pub const CHANGED: &str = "container.changed";
}

/// [`ContainerInfo::kind`] values.
pub mod kinds {
    /// One container (`docker run`) with the project root bind-mounted.
    pub const CONTAINER: &str = "container";
    /// A Docker Compose project; commands run in [`super::ContainerInfo::service`].
    pub const COMPOSE: &str = "compose";
    /// A devcontainer with features, created by the Dev Containers CLI (`devcontainer up`).
    pub const DEVCONTAINER_CLI: &str = "devcontainer-cli";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AttachParams {
    /// The project root on the Mac.
    pub root: String,
    /// Image to run (`postgres:16-alpine`). With `build`, the tag
    /// the built image gets.
    #[serde(default)]
    pub image: Option<String>,
    /// Build the image from this Dockerfile (relative to `root`, or absolute) first.
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Where the project's files appear in the container (default `/workspace`; for Compose: where
    /// the root is mounted in `service`).
    #[serde(default)]
    pub workdir: Option<String>,
    /// Copy the image's own `workdir` content out to `root` before mounting (the repository an
    /// image ships), so the files on the Mac are exactly the image's.
    #[serde(default)]
    pub seed_from_image: bool,
    /// `linux/amd64` | `linux/arm64`; default: the image's own.
    #[serde(default)]
    pub platform: Option<String>,
    /// Extra environment of the container (for Compose: of `service`).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Network access inside the container (default true).
    #[serde(default)]
    pub network: Option<bool>,
    /// Compose files (relative to `root`, or absolute): the project is a Docker Compose project.
    /// With `service`, these parameters are the whole configuration (like `image`).
    #[serde(default)]
    pub compose: Vec<String>,
    /// The Compose service commands and terminals run in.
    #[serde(default)]
    pub service: Option<String>,
    /// Compose profiles to enable (`--profile`).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Compose environment files (`--env-file`), relative to `root` or absolute.
    #[serde(default)]
    pub env_files: Vec<String>,
    /// Services to start (default: all of the enabled profiles); `service` is always started.
    #[serde(default)]
    pub run_services: Vec<String>,
    /// Variables for `${VAR}` interpolation in the Compose files (environment of `docker compose`).
    #[serde(default)]
    pub compose_env: Vec<(String, String)>,
}

/// A bind mount: a host directory or file and where it appears in the container.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PathMapping {
    pub host: String,
    pub container: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContainerInfo {
    pub root: String,
    /// Docker container id (of `service` for Compose projects).
    pub id: String,
    pub image: String,
    /// Where commands run by default.
    pub workdir: String,
    pub platform: Option<String>,
    /// `starting` | `running` | `stopped` | `failed`
    pub state: String,
    /// Why it failed, in words.
    #[serde(default)]
    pub problem: Option<String>,
    /// Where the configuration came from: `attach` | `.cortex/container.toml` | `devcontainer.json`.
    pub source: String,
    /// [`kinds`]: `container` | `compose` | `devcontainer-cli` (absent: `container`).
    #[serde(default)]
    pub kind: Option<String>,
    /// The Compose project name (Compose projects).
    #[serde(default)]
    pub project: Option<String>,
    /// The Compose service commands run in (Compose projects).
    #[serde(default)]
    pub service: Option<String>,
    /// The container's bind mounts as Docker reports them: how host paths map into it.
    #[serde(default)]
    pub mounts: Vec<PathMapping>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RootParams {
    pub root: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DetachParams {
    pub root: String,
    /// Also remove the project's volumes (`docker compose down -v`, `docker rm -v`). Default: kept.
    #[serde(default)]
    pub remove_volumes: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecParams {
    pub root: String,
    /// Run through `bash -lc` (a shell command line), like the `run` tool on the Mac.
    pub command: String,
    /// Working directory: a host path inside a bind mount (mapped) or a container path; default
    /// `workdir`.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub stdin: Option<String>,
    /// Kill the command after this long (default: no limit; cancellation also kills it).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Streamed output while a command runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecItem {
    /// `stdout` | `stderr`
    pub stream: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    /// stdout and stderr interleaved as produced.
    pub output: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListResult {
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellCommandResult {
    pub argv: Vec<String>,
}

/// Streamed progress of an attach, build or pull.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProgressItem {
    /// The step: `initializeCommand`, `install`, `build`, `pull`, `compose`, `devcontainer`,
    /// `postCreateCommand`, ...
    pub phase: String,
    /// `stdout` | `stderr` (output of the step's process) | `info` (a line from the plugin).
    pub stream: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImagesParams {
    /// Only images matching this reference pattern (`docker image ls --filter reference=...`,
    /// e.g. `python:*`).
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageInfo {
    /// `sha256:...`
    pub id: String,
    /// `<none>` for an untagged image.
    pub repository: String,
    pub tag: String,
    /// As Docker prints it (`123MB`).
    pub size: String,
    pub created: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImagesResult {
    pub images: Vec<ImageInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildParams {
    /// The project root: relative `dockerfile` and `context` are relative to it.
    pub root: String,
    /// Default `Dockerfile`.
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Build context (default: the root).
    #[serde(default)]
    pub context: Option<String>,
    /// Tag of the built image (default `cortex-build-<hash of root>:latest`).
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub build_args: Vec<(String, String)>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub no_cache: bool,
    /// Pull newer versions of the base images (`--pull`).
    #[serde(default)]
    pub pull: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PullParams {
    pub image: String,
    #[serde(default)]
    pub platform: Option<String>,
}

/// A built or pulled image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImageResult {
    /// The reference (tag).
    pub image: String,
    pub id: String,
    /// `linux/arm64`, ...
    pub platform: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LogsParams {
    pub root: String,
    /// A Compose service (default: the service commands run in).
    #[serde(default)]
    pub service: Option<String>,
    /// Lines from the end (default 1000).
    #[serde(default)]
    pub tail: Option<u64>,
    /// Only logs since this time: an RFC 3339 timestamp, a Unix timestamp or a duration (`10m`).
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub timestamps: bool,
}

/// Streamed log output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LogItem {
    /// Container name.
    pub container: String,
    #[serde(default)]
    pub service: Option<String>,
    /// `stdout` | `stderr`
    pub stream: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContainerLogs {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub service: Option<String>,
    /// stdout and stderr interleaved as read (the latest bytes when very long).
    pub output: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LogsResult {
    pub containers: Vec<ContainerLogs>,
}

/// A port a container publishes on the Mac.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PortInfo {
    pub container_id: String,
    #[serde(default)]
    pub service: Option<String>,
    pub container_port: u16,
    /// `tcp` | `udp` | `sctp`
    pub protocol: String,
    /// `0.0.0.0`, `127.0.0.1`, `::`
    pub host_ip: String,
    pub host_port: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PortsResult {
    pub ports: Vec<PortInfo>,
}

/// One container of a project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectContainer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub service: Option<String>,
    pub image: String,
    /// Docker's state: `running`, `exited`, `created`, `restarting`, `paused`, `dead`.
    pub state: String,
    /// `healthy` | `unhealthy` | `starting` for containers with a healthcheck.
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub started_at: Option<String>,
    /// Whether commands run in this container.
    pub main: bool,
    pub ports: Vec<PortInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PsResult {
    pub containers: Vec<ProjectContainer>,
}
