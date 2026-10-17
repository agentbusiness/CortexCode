//! Where a project's container comes from: `container.attach` parameters, the project's
//! `.cortex/container.toml`, or its `.devcontainer/devcontainer.json` (`.devcontainer.json`), in
//! that order. Everything resolves to one [`Spec`]: what kind of project it is ([`Kind`]: one
//! container, a Docker Compose project, or a devcontainer with features for the Dev Containers CLI),
//! what to build or pull, how to run it, and how commands run in it.
//!
//! Attach parameters that name an image, a Dockerfile or Compose files are the whole configuration
//! (`source = "attach"`); otherwise the project's files give it and the remaining attach parameters
//! (`workdir`, `platform`, `env`, `network`, `seed_from_image`) override or extend them where the
//! kind of project allows it (an error otherwise, never silently ignored).
//!
//! `.cortex/container.toml` (every key optional, unknown keys are an error):
//!
//! ```toml
//! image = "python:3.12-slim"     # image to run; with `dockerfile`, the tag the built image gets
//! dockerfile = "Dockerfile"      # build this first (relative to the project root, or absolute)
//! context = "."                  # build context (relative to the root; default the root)
//! target = "dev"                 # build stage (`docker build --target`)
//! build_args = { PY = "3.12" }   # `docker build --build-arg`
//! workdir = "/workspace"         # where the root is mounted in the container (default /workspace)
//! platform = "linux/amd64"       # default: the image's own
//! network = true                 # false: `--network none`
//! seed_from_image = false        # copy the image's workdir to the (empty) root before mounting
//! ports = ["8000:8000"]          # published ports (`docker run -p`)
//! user = "dev"                   # user commands and terminals run as (default: the image's)
//! override_command = true        # false: keep the image's own long-running command
//! [env]                          # environment of the container (so of every command)
//! RUST_BACKTRACE = "1"
//! ```
//!
//! or, for a Docker Compose project:
//!
//! ```toml
//! compose = ["compose.yaml", "compose.dev.yaml"]   # one file or several (`-f`)
//! service = "app"                # where commands and terminals run
//! profiles = ["dev"]             # `--profile`
//! env_file = ".env.dev"          # one file or several (`--env-file`)
//! run_services = ["app", "db"]   # services to start (default: all)
//! workdir = "/app"               # default: where the root is mounted in `service`
//! user = "dev"
//! override_command = false       # true: replace `service`'s command with a keep-alive loop
//! [env]                          # added to `service`'s environment
//! [compose_env]                  # `${VAR}` interpolation in the compose files
//! ```
//!
//! `devcontainer.json` (JSON with comments): `image`, `build.{dockerfile, context, args, target,
//! options, cacheFrom}` (and the older top-level `dockerFile`/`context`), `dockerComposeFile`,
//! `service`, `runServices`, `workspaceFolder` (default `/workspaces/<root name>`, `/` for Compose),
//! `workspaceMount`, `containerEnv`, `remoteEnv` (`${containerEnv:VAR}` allowed), `remoteUser`,
//! `containerUser`, `overrideCommand`, `runArgs`, `mounts`, `privileged`, `capAdd`, `securityOpt`,
//! `init`, `appPort`, `forwardPorts` (published on 127.0.0.1), `features` (non-empty: the Dev
//! Containers CLI builds and starts the container), `initializeCommand` (on the Mac) and the
//! lifecycle commands `onCreateCommand`, `updateContentCommand`, `postCreateCommand`,
//! `postStartCommand`, `postAttachCommand`. Editor-only keys (`name`, `customizations`,
//! `portsAttributes`, ...) are ignored; unknown keys, and keys that do not apply to the kind of
//! configuration (`runArgs` with `dockerComposeFile`, ...) are an error, never silently dropped.

use crate::jsonc;
use cc_protocol::contracts::container::AttachParams;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SOURCE_ATTACH: &str = "attach";
pub const SOURCE_TOML: &str = ".cortex/container.toml";
pub const SOURCE_DEVCONTAINER: &str = "devcontainer.json";
pub const DEFAULT_WORKDIR: &str = "/workspace";

/// What to build.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Build {
    /// Absolute host path of the Dockerfile.
    pub dockerfile: String,
    /// Absolute host path of the build context.
    pub context: String,
    pub args: Vec<(String, String)>,
    pub target: Option<String>,
    /// Extra `docker build` arguments (devcontainer `build.options`).
    pub options: Vec<String>,
    pub cache_from: Vec<String>,
}

/// A lifecycle command (devcontainer semantics): a shell command line (`/bin/sh -c`), an argv run
/// directly, or named commands run in parallel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Command {
    Shell(String),
    Argv(Vec<String>),
    Parallel(Vec<(String, Command)>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hook {
    /// `postCreateCommand`, ...
    pub name: String,
    pub command: Command,
}

/// A Docker Compose project.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Compose {
    /// `cortex-<hash of root>`: stable per root, so attaching again finds the same project.
    pub project: String,
    /// Absolute paths of the compose files, in `-f` order.
    pub files: Vec<String>,
    /// The service commands and terminals run in.
    pub service: String,
    pub profiles: Vec<String>,
    /// Absolute paths of `--env-file`s.
    pub env_files: Vec<String>,
    /// Services to start; empty: all (of the enabled profiles).
    pub run_services: Vec<String>,
    /// Environment of `docker compose` (`${VAR}` interpolation).
    pub env: Vec<(String, String)>,
}

/// A devcontainer.json with features: the Dev Containers CLI builds and starts it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DevcontainerCli {
    /// Absolute path of the devcontainer.json.
    pub config_file: String,
    /// The configuration as parsed (comments removed): the base of an override configuration.
    pub config: Value,
    /// For a Compose-based configuration: (project name, service).
    pub compose: Option<(String, String)>,
    /// SHA-256 of the Dockerfile's content, so an edited Dockerfile replaces the container.
    pub dockerfile_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Kind {
    /// One container (`docker run`) with the root bind-mounted at `workdir`.
    Container,
    Compose(Compose),
    DevcontainerCli(DevcontainerCli),
}

/// A port to publish on 127.0.0.1 (devcontainer `forwardPorts`): the same host port when it is
/// free, else one Docker picks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForwardPort {
    /// A Compose service; `None`: the container commands run in.
    pub service: Option<String>,
    pub port: u16,
}

/// The resolved configuration of a project's container.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spec {
    /// `attach` | `.cortex/container.toml` | `devcontainer.json`
    pub source: String,
    pub kind: Kind,
    /// The image to run (with `build`, the tag the build produces). Empty for Compose projects and
    /// built devcontainers with features (Compose and the CLI name their images).
    pub image: String,
    pub build: Option<Build>,
    /// Where commands run. Empty for a Compose project that does not name it: then where the root
    /// is mounted in the service (else the service's working directory).
    pub workdir: String,
    pub platform: Option<String>,
    /// Container environment (`docker run -e`, the service's `environment`): every command sees it.
    pub container_env: Vec<(String, String)>,
    /// Extra environment per command (`docker exec -e`); values may hold `${containerEnv:VAR}`.
    pub exec_env: Vec<(String, String)>,
    /// User commands and terminals run as.
    pub exec_user: Option<String>,
    /// User the container's own process runs as (`docker run --user`).
    pub container_user: Option<String>,
    pub network: bool,
    pub seed_from_image: bool,
    /// Replace the image's command with a keep-alive loop (true), or run the image's own.
    pub override_command: bool,
    /// Extra `docker run` arguments (devcontainer `runArgs`).
    pub run_args: Vec<String>,
    /// Extra mounts in `docker run --mount` syntax (devcontainer `mounts`).
    pub mounts: Vec<String>,
    pub privileged: bool,
    pub init: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    /// Published ports in `docker run -p` syntax (`ports`, devcontainer `appPort`).
    pub ports: Vec<String>,
    pub forward_ports: Vec<ForwardPort>,
    /// Replaces the bind mount of the root at `workdir` (devcontainer `workspaceMount`).
    pub workspace_mount: Option<String>,
    /// Run on the Mac, in the root, before every attach (devcontainer `initializeCommand`).
    pub initialize: Option<Command>,
    /// Run once after the container is created, in order.
    pub on_create: Vec<Hook>,
    /// Run every time the container starts.
    pub on_start: Vec<Hook>,
    /// Run on every attach.
    pub on_attach: Vec<Hook>,
}

impl Spec {
    /// Stable digest of everything that shapes the container; a running container whose label
    /// differs is replaced.
    pub fn digest(&self) -> String {
        let text = serde_json::to_string(self).unwrap_or_default();
        hex(&Sha256::digest(text.as_bytes()))[..16].to_owned()
    }

    pub fn compose(&self) -> Option<&Compose> {
        match &self.kind {
            Kind::Compose(compose) => Some(compose),
            _ => None,
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `cortex-<16 hex of sha256(root)>`: the container name of a project root.
pub fn root_hash(root: &str) -> String {
    hex(&Sha256::digest(root.as_bytes()))[..16].to_owned()
}

/// The Compose project name of a root (lowercase letters, digits and `-`, as Compose requires).
pub fn compose_project(root: &str) -> String {
    format!("cortex-{}", root_hash(root))
}

fn absolute(base: &Path, path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    crate::paths::normalize(&joined.to_string_lossy())
}

fn check_workdir(workdir: &str, root_mounted_there: bool) -> Result<(), String> {
    if !workdir.starts_with('/') {
        return Err(format!(
            "workdir `{workdir}` must be an absolute container path"
        ));
    }
    if root_mounted_there && crate::paths::normalize(workdir) == "/" {
        return Err("workdir cannot be `/`: the project root is mounted there".into());
    }
    Ok(())
}

pub fn check_platform(platform: &str) -> Result<(), String> {
    let parts: Vec<&str> = platform.split('/').collect();
    if parts.len() >= 2
        && parts.len() <= 3
        && parts[0] == "linux"
        && parts.iter().all(|p| !p.is_empty())
    {
        Ok(())
    } else {
        Err(format!(
            "platform `{platform}` must look like linux/amd64 or linux/arm64"
        ))
    }
}

fn check_env(env: &[(String, String)]) -> Result<(), String> {
    for (key, _) in env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(format!("environment variable name `{key}` is not valid"));
        }
    }
    Ok(())
}

/// A name passed to docker as one argument (service, profile): not empty, not an option.
pub fn check_name(what: &str, name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('-')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{what} `{name}` is not valid"));
    }
    Ok(())
}

/// A `docker run -p` value: not empty and not an option.
fn check_port_spec(spec: &str) -> Result<(), String> {
    if spec.is_empty() || spec.starts_with('-') || spec.chars().any(char::is_whitespace) {
        return Err(format!("port `{spec}` is not a valid published port"));
    }
    Ok(())
}

/// Resolve the configuration for `root` (canonical, existing or not yet created).
pub fn resolve(
    params: &AttachParams,
    root: &Path,
    host_env: &dyn Fn(&str) -> Option<String>,
) -> Result<Spec, String> {
    let root_text = root.to_string_lossy().into_owned();
    let names_whole_config =
        params.image.is_some() || params.dockerfile.is_some() || !params.compose.is_empty();
    let mut spec = if names_whole_config {
        from_params(params, root)?
    } else if root.join(".cortex/container.toml").is_file() {
        let path = root.join(".cortex/container.toml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        from_toml(&text, root).map_err(|e| format!("{}: {e}", path.display()))?
    } else if let Some(path) = devcontainer_path(root) {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        from_devcontainer(&text, root, &path, host_env)
            .map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        return Err(format!(
            "no container configuration for {root_text}: pass `image`, `dockerfile` or `compose`, or add .cortex/container.toml or .devcontainer/devcontainer.json"
        ));
    };
    if !names_whole_config {
        apply_params(&mut spec, params)?;
    }
    if spec.image.is_empty() && spec.kind == Kind::Container {
        spec.image = format!("cortex-build-{}:latest", root_hash(&root_text));
    }
    match &mut spec.kind {
        Kind::Container => {
            check_workdir(&spec.workdir, spec.workspace_mount.is_none())?;
            spec.workdir = crate::paths::normalize(&spec.workdir);
        }
        Kind::Compose(compose) => {
            if !spec.workdir.is_empty() {
                check_workdir(&spec.workdir, false)?;
                spec.workdir = crate::paths::normalize(&spec.workdir);
            }
            for file in compose.files.iter().chain(&compose.env_files) {
                if !Path::new(file).is_file() {
                    return Err(format!("{file} does not exist"));
                }
            }
            check_env(&compose.env)?;
        }
        Kind::DevcontainerCli(cli) => {
            check_workdir(&spec.workdir, false)?;
            if let Some(build) = &spec.build {
                let bytes = std::fs::read(&build.dockerfile)
                    .map_err(|e| format!("cannot read {}: {e}", build.dockerfile))?;
                cli.dockerfile_digest = Some(hex(&Sha256::digest(&bytes)));
            }
        }
    }
    if let Some(platform) = &spec.platform {
        check_platform(platform)?;
    }
    check_env(&spec.container_env)?;
    check_env(&spec.exec_env)?;
    for port in &spec.ports {
        check_port_spec(port)?;
    }
    Ok(spec)
}

/// Attach parameters over a configuration from the project's files.
fn apply_params(spec: &mut Spec, params: &AttachParams) -> Result<(), String> {
    let given: Vec<&str> = [
        ("workdir", params.workdir.is_some()),
        ("platform", params.platform.is_some()),
        ("env", !params.env.is_empty()),
        ("network", params.network.is_some()),
        ("seed_from_image", params.seed_from_image),
        ("service", params.service.is_some()),
        ("profiles", !params.profiles.is_empty()),
        ("env_files", !params.env_files.is_empty()),
        ("run_services", !params.run_services.is_empty()),
        ("compose_env", !params.compose_env.is_empty()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect();
    let allowed: &[&str] = match spec.kind {
        Kind::Container => &["workdir", "platform", "env", "network", "seed_from_image"],
        Kind::Compose(_) => &["workdir", "env"],
        Kind::DevcontainerCli(_) => &[],
    };
    if let Some(refused) = given.iter().find(|name| !allowed.contains(name)) {
        let kind = match spec.kind {
            Kind::Container => "a single-container configuration",
            Kind::Compose(_) => "a Compose project (the compose files decide it)",
            Kind::DevcontainerCli(_) => {
                "a devcontainer with features (devcontainer.json decides it)"
            }
        };
        return Err(format!(
            "attach parameter `{refused}` does not apply to {kind} from {}",
            spec.source
        ));
    }
    if let Some(workdir) = &params.workdir {
        spec.workdir = workdir.clone();
    }
    if let Some(platform) = &params.platform {
        spec.platform = Some(platform.clone());
    }
    spec.container_env.extend(params.env.iter().cloned());
    if let Some(network) = params.network {
        spec.network = network;
    }
    spec.seed_from_image |= params.seed_from_image;
    Ok(())
}

/// `.devcontainer/devcontainer.json`, else `.devcontainer.json`.
pub fn devcontainer_path(root: &Path) -> Option<PathBuf> {
    [
        root.join(".devcontainer/devcontainer.json"),
        root.join(".devcontainer.json"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn base_spec(source: &str) -> Spec {
    Spec {
        source: source.into(),
        kind: Kind::Container,
        image: String::new(),
        build: None,
        workdir: DEFAULT_WORKDIR.into(),
        platform: None,
        container_env: Vec::new(),
        exec_env: Vec::new(),
        exec_user: None,
        container_user: None,
        network: true,
        seed_from_image: false,
        override_command: true,
        run_args: Vec::new(),
        mounts: Vec::new(),
        privileged: false,
        init: false,
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        ports: Vec::new(),
        forward_ports: Vec::new(),
        workspace_mount: None,
        initialize: None,
        on_create: Vec::new(),
        on_start: Vec::new(),
        on_attach: Vec::new(),
    }
}

/// Compose settings shared by attach parameters and container.toml.
struct ComposeInput<'a> {
    files: &'a [String],
    service: Option<&'a str>,
    profiles: &'a [String],
    env_files: &'a [String],
    run_services: &'a [String],
    env: Vec<(String, String)>,
}

fn compose_kind(input: ComposeInput, root: &Path, base: &Path) -> Result<Kind, String> {
    let service = input
        .service
        .ok_or("a Compose project needs `service` (where commands run)")?;
    check_name("service", service)?;
    for name in input.profiles {
        check_name("profile", name)?;
    }
    for name in input.run_services {
        check_name("service", name)?;
    }
    for file in input.files.iter().chain(input.env_files) {
        if file.is_empty() {
            return Err("an empty compose or env file path".into());
        }
    }
    Ok(Kind::Compose(Compose {
        project: compose_project(&crate::paths::normalize(&root.to_string_lossy())),
        files: input.files.iter().map(|f| absolute(base, f)).collect(),
        service: service.to_owned(),
        profiles: input.profiles.to_vec(),
        env_files: input.env_files.iter().map(|f| absolute(base, f)).collect(),
        run_services: input.run_services.to_vec(),
        env: input.env,
    }))
}

/// Attach parameters naming an image, a Dockerfile or Compose files. A Dockerfile builds with the
/// root as context.
pub fn from_params(params: &AttachParams, root: &Path) -> Result<Spec, String> {
    let mut spec = base_spec(SOURCE_ATTACH);
    if !params.compose.is_empty() {
        if params.image.is_some() || params.dockerfile.is_some() {
            return Err("`compose` cannot be combined with `image` or `dockerfile` (the compose files name the images)".into());
        }
        for (name, set) in [
            ("platform", params.platform.is_some()),
            ("network", params.network.is_some()),
            ("seed_from_image", params.seed_from_image),
        ] {
            if set {
                return Err(format!(
                    "`{name}` does not apply to a Compose project (set it in the compose file)"
                ));
            }
        }
        spec.kind = compose_kind(
            ComposeInput {
                files: &params.compose,
                service: params.service.as_deref(),
                profiles: &params.profiles,
                env_files: &params.env_files,
                run_services: &params.run_services,
                env: params.compose_env.clone(),
            },
            root,
            root,
        )?;
        spec.workdir = params.workdir.clone().unwrap_or_default();
        spec.container_env = params.env.clone();
        spec.override_command = false;
        return Ok(spec);
    }
    let compose_only = [
        ("service", params.service.is_some()),
        ("profiles", !params.profiles.is_empty()),
        ("env_files", !params.env_files.is_empty()),
        ("run_services", !params.run_services.is_empty()),
        ("compose_env", !params.compose_env.is_empty()),
    ];
    if let Some((name, _)) = compose_only.iter().find(|(_, set)| *set) {
        return Err(format!("`{name}` needs `compose` (the Compose files)"));
    }
    spec.image = params.image.clone().unwrap_or_default();
    if let Some(dockerfile) = &params.dockerfile {
        spec.build = Some(Build {
            dockerfile: absolute(root, dockerfile),
            context: crate::paths::normalize(&root.to_string_lossy()),
            args: Vec::new(),
            target: None,
            options: Vec::new(),
            cache_from: Vec::new(),
        });
    }
    if let Some(workdir) = &params.workdir {
        spec.workdir = workdir.clone();
    }
    spec.platform = params.platform.clone();
    spec.container_env = params.env.clone();
    spec.network = params.network.unwrap_or(true);
    spec.seed_from_image = params.seed_from_image;
    Ok(spec)
}

/// One string or a list of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(one) => vec![one],
            OneOrMany::Many(many) => many,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerToml {
    image: Option<String>,
    dockerfile: Option<String>,
    context: Option<String>,
    target: Option<String>,
    #[serde(default)]
    build_args: BTreeMap<String, String>,
    workdir: Option<String>,
    platform: Option<String>,
    network: Option<bool>,
    seed_from_image: Option<bool>,
    ports: Option<Vec<String>>,
    user: Option<String>,
    override_command: Option<bool>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    compose: Option<OneOrMany>,
    service: Option<String>,
    profiles: Option<Vec<String>>,
    env_file: Option<OneOrMany>,
    run_services: Option<Vec<String>>,
    compose_env: Option<BTreeMap<String, String>>,
}

/// `.cortex/container.toml`; relative paths are relative to the project root.
pub fn from_toml(text: &str, root: &Path) -> Result<Spec, String> {
    let file: ContainerToml = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut spec = base_spec(SOURCE_TOML);
    spec.exec_user = file.user;
    spec.container_env = file.env.into_iter().collect();
    if let Some(compose) = file.compose {
        let not_for_compose = [
            ("image", file.image.is_some()),
            ("dockerfile", file.dockerfile.is_some()),
            ("context", file.context.is_some()),
            ("target", file.target.is_some()),
            ("build_args", !file.build_args.is_empty()),
            ("platform", file.platform.is_some()),
            ("network", file.network.is_some()),
            ("seed_from_image", file.seed_from_image.is_some()),
            ("ports", file.ports.is_some()),
        ];
        if let Some((key, _)) = not_for_compose.iter().find(|(_, set)| *set) {
            return Err(format!(
                "`{key}` does not apply with `compose`: the compose files define the services (set it there)"
            ));
        }
        let files = compose.into_vec();
        if files.is_empty() {
            return Err("`compose` names no file".into());
        }
        spec.kind = compose_kind(
            ComposeInput {
                files: &files,
                service: file.service.as_deref(),
                profiles: &file.profiles.unwrap_or_default(),
                env_files: &file.env_file.map(OneOrMany::into_vec).unwrap_or_default(),
                run_services: &file.run_services.unwrap_or_default(),
                env: file.compose_env.unwrap_or_default().into_iter().collect(),
            },
            root,
            root,
        )?;
        spec.workdir = file.workdir.unwrap_or_default();
        spec.override_command = file.override_command.unwrap_or(false);
        return Ok(spec);
    }
    let compose_only = [
        ("service", file.service.is_some()),
        ("profiles", file.profiles.is_some()),
        ("env_file", file.env_file.is_some()),
        ("run_services", file.run_services.is_some()),
        ("compose_env", file.compose_env.is_some()),
    ];
    if let Some((key, _)) = compose_only.iter().find(|(_, set)| *set) {
        return Err(format!("`{key}` needs `compose`"));
    }
    if file.image.is_none() && file.dockerfile.is_none() {
        return Err("needs `image`, `dockerfile` or `compose`".into());
    }
    if file.dockerfile.is_none()
        && (file.context.is_some() || file.target.is_some() || !file.build_args.is_empty())
    {
        return Err("`context`, `target` and `build_args` need `dockerfile`".into());
    }
    spec.image = file.image.unwrap_or_default();
    if let Some(dockerfile) = &file.dockerfile {
        spec.build = Some(Build {
            dockerfile: absolute(root, dockerfile),
            context: absolute(root, file.context.as_deref().unwrap_or(".")),
            args: file.build_args.into_iter().collect(),
            target: file.target,
            options: Vec::new(),
            cache_from: Vec::new(),
        });
    }
    if let Some(workdir) = file.workdir {
        spec.workdir = workdir;
    }
    spec.platform = file.platform;
    spec.network = file.network.unwrap_or(true);
    spec.seed_from_image = file.seed_from_image.unwrap_or(false);
    spec.ports = file.ports.unwrap_or_default();
    spec.override_command = file.override_command.unwrap_or(true);
    Ok(spec)
}

/// devcontainer.json keys that only concern an editor; they do not change the environment
/// commands run in.
const IGNORED_KEYS: &[&str] = &[
    "$schema",
    "name",
    "customizations",
    "extensions",
    "settings",
    "portsAttributes",
    "otherPortsAttributes",
    "shutdownAction",
    "hostRequirements",
    "userEnvProbe",
    "updateRemoteUserUID",
    "waitFor",
];

const SUPPORTED_KEYS: &[&str] = &[
    "image",
    "build",
    "dockerFile",
    "context",
    "dockerComposeFile",
    "service",
    "runServices",
    "workspaceFolder",
    "workspaceMount",
    "containerEnv",
    "remoteEnv",
    "remoteUser",
    "containerUser",
    "overrideCommand",
    "runArgs",
    "mounts",
    "privileged",
    "capAdd",
    "securityOpt",
    "init",
    "appPort",
    "forwardPorts",
    "features",
    "overrideFeatureInstallOrder",
    "initializeCommand",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
];

/// Keys of image/Dockerfile configurations only (the devcontainer spec); with `dockerComposeFile`
/// the compose file says these things.
const IMAGE_ONLY_KEYS: &[&str] = &[
    "image",
    "build",
    "dockerFile",
    "context",
    "runArgs",
    "appPort",
    "workspaceMount",
];

/// Keys of Compose configurations only.
const COMPOSE_ONLY_KEYS: &[&str] = &["service", "runServices"];

struct Vars<'a> {
    root: String,
    workspace: Option<String>,
    host_env: &'a dyn Fn(&str) -> Option<String>,
}

fn basename(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Replace `${...}` variables. `${containerEnv:...}` is kept for [`container_env_substitute`] when
/// `keep_container_env`, an error otherwise.
fn substitute(text: &str, vars: &Vars, keep_container_env: bool) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated variable in `{text}`"))?;
        let name = &after[..end];
        let whole = &rest[start..start + 2 + end + 1];
        let value = match name {
            "localWorkspaceFolder" => vars.root.clone(),
            "localWorkspaceFolderBasename" => basename(&vars.root),
            "containerWorkspaceFolder" => vars
                .workspace
                .clone()
                .ok_or("containerWorkspaceFolder cannot be used in workspaceFolder")?,
            "containerWorkspaceFolderBasename" => basename(
                vars.workspace
                    .as_deref()
                    .ok_or("containerWorkspaceFolderBasename cannot be used in workspaceFolder")?,
            ),
            _ => {
                if let Some(spec) = name
                    .strip_prefix("localEnv:")
                    .or_else(|| name.strip_prefix("env:"))
                {
                    let (var, default) = spec
                        .split_once(':')
                        .map(|(v, d)| (v, Some(d)))
                        .unwrap_or((spec, None));
                    (vars.host_env)(var)
                        .or(default.map(str::to_owned))
                        .unwrap_or_default()
                } else if name.starts_with("containerEnv:") {
                    if !keep_container_env {
                        return Err(format!("`{whole}` can only be used in remoteEnv"));
                    }
                    whole.to_owned()
                } else {
                    return Err(format!("unsupported variable `{whole}`"));
                }
            }
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolve `${containerEnv:VAR}` / `${containerEnv:VAR:default}` against the container's
/// environment (a missing variable without default becomes empty, as in the devcontainer spec).
pub fn container_env_substitute(text: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${containerEnv:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "${containerEnv:".len()..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let spec = &after[..end];
        let (var, default) = spec
            .split_once(':')
            .map(|(v, d)| (v, Some(d)))
            .unwrap_or((spec, None));
        out.push_str(env.get(var).map(String::as_str).or(default).unwrap_or(""));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn string(value: &Value, key: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("`{key}` must be a string"))
}

fn strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("`{key}` must be an array of strings"))?
        .iter()
        .map(|v| string(v, key))
        .collect()
}

fn string_or_strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    match value {
        Value::String(s) => Ok(vec![s.clone()]),
        _ => strings(value, key)
            .map_err(|_| format!("`{key}` must be a string or an array of strings")),
    }
}

fn string_map(
    value: &Value,
    key: &str,
    vars: &Vars,
    keep_container_env: bool,
) -> Result<Vec<(String, String)>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("`{key}` must be an object"))?;
    let mut out = Vec::new();
    for (name, v) in object {
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => {
                return Err(format!(
                    "`{key}.{name}` is null (unsetting a variable is not supported)"
                ));
            }
            _ => return Err(format!("`{key}.{name}` must be a string")),
        };
        out.push((name.clone(), substitute(&text, vars, keep_container_env)?));
    }
    Ok(out)
}

fn lifecycle(value: &Value, key: &str, vars: &Vars, nested: bool) -> Result<Command, String> {
    match value {
        Value::String(s) => Ok(Command::Shell(substitute(s, vars, false)?)),
        Value::Array(items) => {
            let argv = items
                .iter()
                .map(|v| string(v, key).and_then(|s| substitute(&s, vars, false)))
                .collect::<Result<Vec<_>, _>>()?;
            if argv.is_empty() {
                return Err(format!("`{key}` is an empty command"));
            }
            Ok(Command::Argv(argv))
        }
        Value::Object(map) if !nested => {
            let mut parallel = Vec::new();
            for (name, v) in map {
                parallel.push((
                    name.clone(),
                    lifecycle(v, &format!("{key}.{name}"), vars, true)?,
                ));
            }
            Ok(Command::Parallel(parallel))
        }
        _ => Err(format!(
            "`{key}` must be a string, an array of strings{}",
            if nested {
                ""
            } else {
                " or an object of commands"
            }
        )),
    }
}

fn bool_key(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn port_number(value: &Value, key: &str) -> Result<u16, String> {
    value
        .as_u64()
        .filter(|p| (1..=65535).contains(p))
        .map(|p| p as u16)
        .ok_or_else(|| format!("`{key}`: `{value}` is not a port number"))
}

/// devcontainer `appPort`: a number (published on 127.0.0.1 as the same port, like the Dev
/// Containers CLI) or a `docker run -p` string, or an array of those.
fn app_ports(value: &Value) -> Result<Vec<String>, String> {
    let items = match value {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(s) => Ok(s.clone()),
            Value::Number(_) => port_number(item, "appPort").map(|p| format!("127.0.0.1:{p}:{p}")),
            _ => Err("`appPort` entries must be port numbers or strings".into()),
        })
        .collect()
}

/// devcontainer `forwardPorts`: numbers (the container commands run in) and `"host:port"` strings,
/// where host is a Compose service, or `localhost`/`127.0.0.1` for the main container.
fn forward_ports(value: &Value, compose: bool) -> Result<Vec<ForwardPort>, String> {
    let items = value.as_array().ok_or("`forwardPorts` must be an array")?;
    let mut out = Vec::new();
    for item in items {
        out.push(match item {
            Value::Number(_) => ForwardPort { service: None, port: port_number(item, "forwardPorts")? },
            Value::String(s) => {
                let (host, port) = s.rsplit_once(':').ok_or_else(|| format!("`forwardPorts`: `{s}` must be a number or \"host:port\""))?;
                let port = port.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("`forwardPorts`: `{s}` has no valid port"))?;
                if host == "localhost" || host == "127.0.0.1" {
                    ForwardPort { service: None, port }
                } else if compose {
                    check_name("service", host)?;
                    ForwardPort { service: Some(host.to_owned()), port }
                } else {
                    return Err(format!("`forwardPorts`: `{s}` names host `{host}`; without dockerComposeFile only localhost ports can be published"));
                }
            }
            _ => return Err("`forwardPorts` entries must be port numbers or \"host:port\" strings".into()),
        });
    }
    Ok(out)
}

/// CSV-quote one `--mount` field when it contains a comma or a quote (docker parses the value as a
/// CSV record).
pub fn mount_field(field: &str) -> String {
    if field.contains(',') || field.contains('"') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

/// The fields of a `--mount` value (a CSV record, as docker parses it).
pub fn mount_fields(text: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_start = true;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && at_start {
            quoted = true;
            at_start = false;
        } else if c == ',' {
            fields.push(std::mem::take(&mut field));
            at_start = true;
        } else {
            field.push(c);
            at_start = false;
        }
    }
    if quoted {
        return Err(format!("mount `{text}` has an unterminated quote"));
    }
    fields.push(field);
    Ok(fields)
}

/// devcontainer.json (JSONC) at `config_file`; `root` is the project root.
pub fn from_devcontainer(
    text: &str,
    root: &Path,
    config_file: &Path,
    host_env: &dyn Fn(&str) -> Option<String>,
) -> Result<Spec, String> {
    let config_dir = config_file.parent().unwrap_or(root);
    let value = jsonc::parse(text)?;
    let object = value.as_object().ok_or("must be a JSON object")?;
    let unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| !SUPPORTED_KEYS.contains(key) && !IGNORED_KEYS.contains(key))
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unsupported keys: {}", unknown.join(", ")));
    }
    let compose = object.contains_key("dockerComposeFile");
    let misplaced: Vec<&str> = if compose {
        IMAGE_ONLY_KEYS
            .iter()
            .copied()
            .filter(|k| object.contains_key(*k))
            .collect()
    } else {
        COMPOSE_ONLY_KEYS
            .iter()
            .copied()
            .filter(|k| object.contains_key(*k))
            .collect()
    };
    if !misplaced.is_empty() {
        return Err(if compose {
            format!(
                "{} only apply to image or Dockerfile configurations; with dockerComposeFile the compose file defines them",
                misplaced.join(", ")
            )
        } else {
            format!("{} need dockerComposeFile", misplaced.join(", "))
        });
    }
    let features = match object.get("features") {
        None => false,
        Some(Value::Object(map)) => !map.is_empty(),
        Some(_) => return Err("`features` must be an object".into()),
    };
    if object.contains_key("overrideFeatureInstallOrder") && !features {
        return Err("`overrideFeatureInstallOrder` needs `features`".into());
    }

    let root_text = crate::paths::normalize(&root.to_string_lossy());
    let mut vars = Vars {
        root: root_text.clone(),
        workspace: None,
        host_env,
    };
    let workspace = match object.get("workspaceFolder") {
        Some(v) => substitute(&string(v, "workspaceFolder")?, &vars, false)?,
        None if compose => "/".to_owned(),
        None if object.contains_key("workspaceMount") => {
            return Err("`workspaceMount` needs `workspaceFolder`".into());
        }
        None => format!("/workspaces/{}", basename(&root_text)),
    };
    vars.workspace = Some(workspace.clone());

    let mut spec = base_spec(SOURCE_DEVCONTAINER);
    spec.workdir = workspace;
    if compose {
        let files = string_or_strings(&object["dockerComposeFile"], "dockerComposeFile")?
            .iter()
            .map(|f| substitute(f, &vars, false))
            .collect::<Result<Vec<_>, _>>()?;
        if files.is_empty() {
            return Err("`dockerComposeFile` names no file".into());
        }
        let service = object
            .get("service")
            .map(|s| string(s, "service"))
            .transpose()?;
        let run_services = object
            .get("runServices")
            .map(|s| strings(s, "runServices"))
            .transpose()?
            .unwrap_or_default();
        spec.kind = compose_kind(
            ComposeInput {
                files: &files,
                service: service.as_deref(),
                profiles: &[],
                env_files: &[],
                run_services: &run_services,
                env: Vec::new(),
            },
            root,
            config_dir,
        )
        .map_err(|e| e.replace("`service`", "`service` (with dockerComposeFile)"))?;
        // The devcontainer spec: Compose services keep their own command unless asked.
        spec.override_command = bool_key(object, "overrideCommand")?.unwrap_or(false);
    } else {
        if let Some(image) = object.get("image") {
            spec.image = substitute(&string(image, "image")?, &vars, false)?;
        }
        let build = object
            .get("build")
            .map(|b| b.as_object().ok_or("`build` must be an object"))
            .transpose()?;
        let dockerfile =
            match build.and_then(|b| b.get("dockerfile").or_else(|| b.get("dockerFile"))) {
                Some(v) => Some(string(v, "build.dockerfile")?),
                None => object
                    .get("dockerFile")
                    .map(|v| string(v, "dockerFile"))
                    .transpose()?,
            };
        let context = match build.and_then(|b| b.get("context")) {
            Some(v) => Some(string(v, "build.context")?),
            None => object
                .get("context")
                .map(|v| string(v, "context"))
                .transpose()?,
        };
        if let Some(build) = build {
            for key in build.keys() {
                if ![
                    "dockerfile",
                    "dockerFile",
                    "context",
                    "args",
                    "target",
                    "options",
                    "cacheFrom",
                ]
                .contains(&key.as_str())
                {
                    return Err(format!("unsupported key `build.{key}`"));
                }
            }
        }
        match dockerfile {
            Some(dockerfile) => {
                let build = build.cloned().unwrap_or_default();
                let dockerfile = substitute(&dockerfile, &vars, false)?;
                let context = substitute(context.as_deref().unwrap_or("."), &vars, false)?;
                spec.build = Some(Build {
                    dockerfile: absolute(config_dir, &dockerfile),
                    context: absolute(config_dir, &context),
                    args: build
                        .get("args")
                        .map(|a| string_map(a, "build.args", &vars, false))
                        .transpose()?
                        .unwrap_or_default(),
                    target: build
                        .get("target")
                        .map(|t| string(t, "build.target"))
                        .transpose()?,
                    options: build
                        .get("options")
                        .map(|o| strings(o, "build.options"))
                        .transpose()?
                        .unwrap_or_default(),
                    cache_from: match build.get("cacheFrom") {
                        None => Vec::new(),
                        Some(Value::String(s)) => vec![s.clone()],
                        Some(other) => strings(other, "build.cacheFrom")?,
                    },
                });
            }
            None => {
                if build.is_some() || context.is_some() {
                    return Err("`build` needs `dockerfile`".into());
                }
                if spec.image.is_empty() {
                    return Err("needs `image`, `build.dockerfile` or `dockerComposeFile`".into());
                }
            }
        }
        spec.override_command = bool_key(object, "overrideCommand")?.unwrap_or(true);
        if let Some(args) = object.get("runArgs") {
            for arg in strings(args, "runArgs")? {
                spec.run_args.push(substitute(&arg, &vars, false)?);
            }
        }
        if let Some(ports) = object.get("appPort") {
            spec.ports = app_ports(ports)?;
        }
        if let Some(mount) = object.get("workspaceMount") {
            let mount = substitute(&string(mount, "workspaceMount")?, &vars, false)?;
            if mount.trim().is_empty() {
                return Err("`workspaceMount` is empty".into());
            }
            spec.workspace_mount = Some(mount);
        }
    }
    if let Some(env) = object.get("containerEnv") {
        spec.container_env = string_map(env, "containerEnv", &vars, false)?;
    }
    if let Some(env) = object.get("remoteEnv") {
        spec.exec_env = string_map(env, "remoteEnv", &vars, true)?;
    }
    spec.container_user = object
        .get("containerUser")
        .map(|v| string(v, "containerUser"))
        .transpose()?;
    // remoteUser defaults to containerUser (devcontainer spec).
    spec.exec_user = object
        .get("remoteUser")
        .map(|v| string(v, "remoteUser"))
        .transpose()?
        .or_else(|| spec.container_user.clone());
    if let Some(mounts) = object.get("mounts") {
        for mount in mounts.as_array().ok_or("`mounts` must be an array")? {
            let text = match mount {
                Value::String(s) => substitute(s, &vars, false)?,
                Value::Object(m) => {
                    let mut fields = Vec::new();
                    for key in ["type", "source", "target"] {
                        if let Some(v) = m.get(key) {
                            fields.push(mount_field(&format!(
                                "{key}={}",
                                substitute(&string(v, "mounts")?, &vars, false)?
                            )));
                        }
                    }
                    if let Some(extra) = m
                        .keys()
                        .find(|k| !["type", "source", "target"].contains(&k.as_str()))
                    {
                        return Err(format!("unsupported mount key `{extra}`"));
                    }
                    fields.join(",")
                }
                _ => return Err("`mounts` entries must be strings or objects".into()),
            };
            if compose {
                // Validated now: a Compose project gets it as a long-syntax volume.
                crate::compose::volume(&text)?;
            }
            spec.mounts.push(text);
        }
    }
    spec.privileged = bool_key(object, "privileged")? == Some(true);
    spec.init = bool_key(object, "init")? == Some(true);
    if let Some(v) = object.get("capAdd") {
        spec.cap_add = strings(v, "capAdd")?;
    }
    if let Some(v) = object.get("securityOpt") {
        spec.security_opt = strings(v, "securityOpt")?;
    }
    if let Some(ports) = object.get("forwardPorts") {
        spec.forward_ports = forward_ports(ports, compose)?;
    }
    if let Some(v) = object.get("initializeCommand") {
        spec.initialize = Some(lifecycle(v, "initializeCommand", &vars, false)?);
    }
    for (key, phase) in [
        ("onCreateCommand", 0),
        ("updateContentCommand", 0),
        ("postCreateCommand", 0),
        ("postStartCommand", 1),
        ("postAttachCommand", 2),
    ] {
        if let Some(v) = object.get(key) {
            let hook = Hook {
                name: key.into(),
                command: lifecycle(v, key, &vars, false)?,
            };
            match phase {
                0 => spec.on_create.push(hook),
                1 => spec.on_start.push(hook),
                _ => spec.on_attach.push(hook),
            }
        }
    }
    if features {
        // The Dev Containers CLI builds the features and runs every lifecycle command (also the
        // features' own); this plugin only runs commands in the container it creates.
        let compose_target = spec
            .compose()
            .map(|c| (c.project.clone(), c.service.clone()));
        spec.kind = Kind::DevcontainerCli(DevcontainerCli {
            config_file: crate::paths::normalize(&config_file.to_string_lossy()),
            config: value.clone(),
            compose: compose_target,
            dockerfile_digest: None,
        });
    }
    Ok(spec)
}
