//! `plugin.probe`: exercise the real implementation. Configuration parsing (a container, a Compose
//! project, a devcontainer with features), path mapping through bind mounts and the exec engine
//! (streaming, exit code, timeout with stop) run without Docker. The `docker` check reaches the CLI
//! and the daemon and lists the managed containers, `compose` asks for the Compose plugin's version
//! and `devcontainer_cli` says whether the Dev Containers CLI is ready or what installing it needs.
//! When one of them is missing it reports that as a problem in its detail but passes, because the
//! plugin stays useful without it (`container.status` and `container.list` answer, attach and exec
//! explain what is missing) and a failed probe would keep it out of service altogether.

use crate::config::{self, Kind, SOURCE_DEVCONTAINER, SOURCE_TOML};
use crate::devcli::Located;
use crate::exec::{self, Run, Stop};
use crate::paths::PathMap;
use crate::service::Containers;
use cc_protocol::contracts::container::{AttachParams, ExecItem, PathMapping};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck {
            name: name.into(),
            ok: true,
            detail,
            ms,
        },
        Err(detail) => ProbeCheck {
            name: name.into(),
            ok: false,
            detail,
            ms,
        },
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn configuration(dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let root = std::fs::canonicalize(dir).map_err(|e| e.to_string())?;
    write(
        &root.join(".cortex/container.toml"),
        "image = \"alpine:3.20\"\nworkdir = \"/src\"\n[env]\nPROBE = \"1\"\n",
    )?;
    write(
        &root.join(".devcontainer/devcontainer.json"),
        "{\n  // probe\n  \"image\": \"debian:12-slim\",\n  \"remoteEnv\": {\"P\": \"${containerEnv:PATH}:/x\"},\n  \"postCreateCommand\": [\"true\"],\n}\n",
    )?;
    let params = AttachParams {
        root: root.to_string_lossy().into_owned(),
        ..AttachParams::default()
    };
    let no_env = |_: &str| None;
    let toml = config::resolve(&params, &root, &no_env)?;
    if (
        toml.source.as_str(),
        toml.image.as_str(),
        toml.workdir.as_str(),
    ) != (SOURCE_TOML, "alpine:3.20", "/src")
        || toml.container_env != [("PROBE".to_owned(), "1".to_owned())]
    {
        return Err(format!("container.toml resolved to {toml:?}"));
    }
    std::fs::remove_file(root.join(".cortex/container.toml")).map_err(|e| e.to_string())?;
    let dev = config::resolve(&params, &root, &no_env)?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dev.source != SOURCE_DEVCONTAINER
        || dev.image != "debian:12-slim"
        || dev.workdir != format!("/workspaces/{name}")
        || dev.exec_env != [("P".to_owned(), "${containerEnv:PATH}:/x".to_owned())]
        || dev.on_create.len() != 1
    {
        return Err(format!("devcontainer.json resolved to {dev:?}"));
    }
    write(
        &root.join("compose.yaml"),
        "services:\n  app:\n    image: alpine:3.20\n",
    )?;
    write(
        &root.join(".cortex/container.toml"),
        "compose = \"compose.yaml\"\nservice = \"app\"\n",
    )?;
    let compose = config::resolve(&params, &root, &no_env)?;
    match compose.compose() {
        Some(c)
            if c.service == "app"
                && c.files == [root.join("compose.yaml").to_string_lossy()]
                && compose.workdir.is_empty() => {}
        _ => {
            return Err(format!(
                "the Compose container.toml resolved to {compose:?}"
            ));
        }
    }
    std::fs::remove_file(root.join(".cortex/container.toml")).map_err(|e| e.to_string())?;
    write(
        &root.join(".devcontainer/devcontainer.json"),
        "{\"image\": \"debian:12-slim\", \"features\": {\"ghcr.io/devcontainers/features/git:1\": {}}, \"forwardPorts\": [3000]}",
    )?;
    let features = config::resolve(&params, &root, &no_env)?;
    if !matches!(features.kind, Kind::DevcontainerCli(_)) || features.forward_ports.len() != 1 {
        return Err(format!(
            "the devcontainer with features resolved to {features:?}"
        ));
    }
    Ok("container.toml (container and Compose) and devcontainer.json (with and without features) resolved".into())
}

fn paths() -> Result<String, String> {
    let mount = |host: &str, container: &str| PathMapping {
        host: host.into(),
        container: container.into(),
        read_only: false,
    };
    let map = PathMap::new(
        "/tmp/probe-root",
        "/private/tmp/probe-root",
        "/testbed",
        &[
            mount("/private/tmp/probe-root", "/testbed"),
            mount("/private/tmp/probe-root/data", "/srv/data"),
        ],
    );
    let cases = [
        (map.to_container("/tmp/probe-root/a/b"), "/testbed/a/b"),
        (map.to_container("/private/tmp/probe-root"), "/testbed"),
        (
            map.to_container("/private/tmp/probe-root/data/x"),
            "/srv/data/x",
        ),
        (
            map.to_container("/private/tmp/probe-rootx"),
            "/private/tmp/probe-rootx",
        ),
        (map.to_container("src/../lib"), "/testbed/lib"),
    ];
    for (got, want) in cases {
        if got.as_deref() != Ok(want) {
            return Err(format!("mapped to {got:?}, expected {want}"));
        }
    }
    if map.to_host("/testbed/a") != Some(PathBuf::from("/private/tmp/probe-root/a"))
        || map.to_host("/etc").is_some()
    {
        return Err("container paths do not map back".into());
    }
    let unmounted = PathMap::new("/p", "/p", "/app", &[mount("/p/src", "/app")]);
    if unmounted.to_container("/p/docs").is_ok() {
        return Err("a root path outside every mount was mapped".into());
    }
    Ok("host <-> container mapping through bind mounts".into())
}

async fn engine() -> Result<String, String> {
    let items = Mutex::new(Vec::new());
    let emit = |item: ExecItem| {
        if let Ok(mut items) = items.lock() {
            items.push(item);
        }
    };
    let mut command = tokio::process::Command::new("/bin/sh");
    command.args(["-c", "echo out-$((6*7)); echo err-$((1+1)) >&2; exit 3"]);
    let quiet: Stop = Box::new(|_| Box::pin(async {}));
    let run = Run {
        command,
        stdin: None,
        timeout: None,
        max_output: 1 << 20,
        stop_wait: Duration::from_secs(2),
        stop_attempts: 2,
    };
    let done = exec::run(run, &emit, CancellationToken::new(), quiet).await?;
    let streamed: Vec<(String, String)> = items
        .lock()
        .map(|i| {
            i.iter()
                .map(|i| (i.stream.clone(), i.text.clone()))
                .collect()
        })
        .unwrap_or_default();
    if done.exit_code != Some(3)
        || !streamed.contains(&("stdout".into(), "out-42\n".into()))
        || !streamed.contains(&("stderr".into(), "err-2\n".into()))
    {
        return Err(format!("unexpected result {done:?}, items {streamed:?}"));
    }
    let kill: Stop = Box::new(|pid| {
        if let Some(pid) = pid {
            // SAFETY: kill(2) on the process this run started.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        Box::pin(async {})
    });
    let mut command = tokio::process::Command::new("/bin/sh");
    command.args(["-c", "echo started; exec sleep 30"]);
    let run = Run {
        command,
        stdin: None,
        timeout: Some(Duration::from_millis(300)),
        max_output: 1 << 20,
        stop_wait: Duration::from_secs(2),
        stop_attempts: 2,
    };
    let timed = exec::run(run, &|_: ExecItem| {}, CancellationToken::new(), kill).await?;
    if !timed.timed_out
        || timed.exit_code.is_some()
        || timed.output != "started\n"
        || timed.duration_ms > 5000
    {
        return Err(format!("timeout: {timed:?}"));
    }
    Ok(format!(
        "streamed stdout/stderr, exit 3; timeout stopped after {} ms",
        timed.duration_ms
    ))
}

pub async fn run(containers: &Containers) -> ProbeReport {
    let mut checks = Vec::new();

    let started = Instant::now();
    let scratch = containers
        .data_dir()
        .join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let outcome = configuration(&scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    checks.push(check("config", started, outcome));

    let started = Instant::now();
    checks.push(check("paths", started, paths()));

    let started = Instant::now();
    checks.push(check("exec_engine", started, engine().await));

    let started = Instant::now();
    let detail = match containers.docker() {
        Err(message) => format!("problem: {message}"),
        Ok(docker) => match docker.daemon().await {
            Err(message) => format!("problem: {message}"),
            Ok(version) => match containers.list().await {
                Ok(list) => format!(
                    "docker {version} at {}; {} managed container(s)",
                    docker.program.display(),
                    list.containers.len()
                ),
                Err(error) => {
                    format!(
                        "problem: docker {version} at {} cannot list containers: {}",
                        docker.program.display(),
                        error.message
                    )
                }
            },
        },
    };
    checks.push(ProbeCheck {
        name: "docker".into(),
        ok: true,
        detail,
        ms: started.elapsed().as_millis() as u64,
    });

    let started = Instant::now();
    let docker = containers.docker();
    let detail = match &docker {
        Err(message) => format!("problem: {message}"),
        Ok(docker) => match containers.compose_version(docker).await {
            Ok(version) => format!("docker compose {version}"),
            Err(message) => format!("problem: {message}"),
        },
    };
    checks.push(ProbeCheck {
        name: "compose".into(),
        ok: true,
        detail,
        ms: started.elapsed().as_millis() as u64,
    });

    let started = Instant::now();
    let setup = containers.devcli_setup(docker.as_ref().ok());
    let detail = match setup.locate() {
        Ok(Located::Ready(cli)) => format!("{} ready", cli.describe()),
        Ok(Located::Installable { node, npm }) => format!(
            "{}@{} is installed into {} on the first attach of a devcontainer with features (node {}, npm {})",
            crate::devcli::PACKAGE,
            setup.version,
            setup.toolchains.join(crate::devcli::TOOLCHAIN).display(),
            node.display(),
            npm.display()
        ),
        Err(message) => format!("problem: {message}"),
    };
    checks.push(ProbeCheck {
        name: "devcontainer_cli".into(),
        ok: true,
        detail,
        ms: started.elapsed().as_millis() as u64,
    });
    ProbeReport::from_checks(checks)
}
