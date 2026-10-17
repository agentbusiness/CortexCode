//! Docker Compose projects (`docker compose`, the v2 CLI plugin): the argv of every compose call this
//! plugin makes, the override file it adds to the project's own files, and reading `docker compose
//! config --format json`. Pure functions, unit-tested without Docker.
//!
//! The project name is `cortex-<hash of root>` ([`crate::config::compose_project`]), so attaching
//! again addresses the same project and Compose itself decides what to reuse, recreate or start
//! (its config hash). The override file gives the service commands run in this plugin's labels
//! (how it finds the container again after a restart), the devcontainer settings that the
//! devcontainer spec applies to that service (`containerEnv`, `containerUser`, `mounts`,
//! `privileged`, `init`, `capAdd`, `securityOpt`, `overrideCommand`) and published `forwardPorts`.
//! Every string in it is escaped for Compose interpolation (`$` -> `$$`), so values reach the
//! container literally.

use crate::config::{Compose, Spec, mount_fields};
use crate::docker::KEEP_ALIVE;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Compose labels on every container of a project.
pub mod labels {
    pub const PROJECT: &str = "com.docker.compose.project";
    pub const SERVICE: &str = "com.docker.compose.service";
    pub const NUMBER: &str = "com.docker.compose.container-number";
}

/// `$` -> `$$`: Compose interpolates `${VAR}` and `$VAR` in every file it reads, overrides included.
pub fn escape(text: &str) -> String {
    text.replace('$', "$$")
}

/// The global part of every compose call on the project's files: `compose -p <project> -f ...
/// [-f <override>] [--profile ...] [--env-file ...] --ansi never`.
pub fn base_argv(compose: &Compose, override_file: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = vec!["compose".into(), "-p".into(), compose.project.clone()];
    for file in compose
        .files
        .iter()
        .map(String::as_str)
        .chain(override_file)
    {
        argv.extend(["-f".into(), file.into()]);
    }
    for profile in &compose.profiles {
        argv.extend(["--profile".into(), profile.clone()]);
    }
    for env_file in &compose.env_files {
        argv.extend(["--env-file".into(), env_file.clone()]);
    }
    argv.extend(["--ansi".into(), "never".into()]);
    argv
}

/// `docker compose ... config --format json`: the merged, interpolated model of the project.
pub fn config_argv(compose: &Compose) -> Vec<String> {
    let mut argv = base_argv(compose, None);
    argv.extend(["config".into(), "--format".into(), "json".into()]);
    argv
}

/// `docker compose ... up -d --build --wait --remove-orphans [services]`: build what has a build
/// section (Docker's cache makes an unchanged build quick), pull what is missing, create or recreate
/// what changed, and wait until every started service runs and is healthy (healthchecks).
pub fn up_argv(compose: &Compose, override_file: &str) -> Vec<String> {
    let mut argv = base_argv(compose, Some(override_file));
    argv.extend([
        "up".into(),
        "-d".into(),
        "--build".into(),
        "--wait".into(),
        "--remove-orphans".into(),
    ]);
    if !compose.run_services.is_empty() {
        let mut services = compose.run_services.clone();
        if !services.contains(&compose.service) {
            services.push(compose.service.clone());
        }
        argv.extend(services);
    }
    argv
}

/// `docker compose -p <project> down [-v]`: by project name alone (Compose finds the containers by
/// its labels), so it works after the compose files changed or disappeared. Named volumes are kept
/// unless `volumes`.
pub fn down_argv(project: &str, volumes: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "compose".into(),
        "-p".into(),
        project.into(),
        "--ansi".into(),
        "never".into(),
        "down".into(),
    ];
    if volumes {
        argv.push("-v".into());
    }
    argv
}

pub fn version_argv() -> Vec<String> {
    vec!["compose".into(), "version".into(), "--short".into()]
}

/// The service names of a `docker compose config --format json` model.
pub fn services(config: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(config.trim())
        .map_err(|e| format!("unexpected docker compose config output: {e}"))?;
    let services = value
        .get("services")
        .and_then(Value::as_object)
        .ok_or("docker compose config printed no services")?;
    Ok(services.keys().cloned().collect())
}

/// A `docker run --mount` value as a Compose long-syntax volume.
pub fn volume(mount: &str) -> Result<Value, String> {
    let mut out = Map::new();
    let mut bind = Map::new();
    let mut volume = Map::new();
    let mut tmpfs = Map::new();
    let mut kind = "volume".to_owned();
    for field in mount_fields(mount)? {
        let (key, value) = match field.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), Some(v.to_owned())),
            None => (field.trim().to_ascii_lowercase(), None),
        };
        let flag = |value: &Option<String>| -> Result<bool, String> {
            match value.as_deref() {
                None | Some("true") | Some("1") => Ok(true),
                Some("false") | Some("0") => Ok(false),
                Some(other) => Err(format!(
                    "mount `{mount}`: `{key}={other}` is not true or false"
                )),
            }
        };
        let required = |value: Option<String>| {
            value
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("mount `{mount}`: `{key}` needs a value"))
        };
        match key.as_str() {
            "type" => kind = required(value)?,
            "source" | "src" => {
                out.insert("source".into(), Value::String(escape(&required(value)?)));
            }
            "target" | "destination" | "dst" => {
                out.insert("target".into(), Value::String(escape(&required(value)?)));
            }
            "readonly" | "ro" => {
                out.insert("read_only".into(), Value::Bool(flag(&value)?));
            }
            "consistency" => {
                out.insert("consistency".into(), Value::String(required(value)?));
            }
            "bind-propagation" => {
                bind.insert("propagation".into(), Value::String(required(value)?));
            }
            "volume-nocopy" => {
                volume.insert("nocopy".into(), Value::Bool(flag(&value)?));
            }
            "volume-subpath" => {
                volume.insert("subpath".into(), Value::String(escape(&required(value)?)));
            }
            "tmpfs-size" => {
                let size = required(value)?;
                let bytes: u64 = size.parse().map_err(|_| {
                    format!("mount `{mount}`: tmpfs-size `{size}` must be a number of bytes")
                })?;
                tmpfs.insert("size".into(), json!(bytes));
            }
            "tmpfs-mode" => {
                let mode = required(value)?;
                let bits = u32::from_str_radix(&mode, 8)
                    .map_err(|_| format!("mount `{mount}`: tmpfs-mode `{mode}` is not octal"))?;
                tmpfs.insert("mode".into(), json!(bits));
            }
            other => {
                return Err(format!(
                    "mount `{mount}`: `{other}` cannot be expressed on a Compose service (define the volume in the compose file)"
                ));
            }
        }
    }
    if !["bind", "volume", "tmpfs"].contains(&kind.as_str()) {
        return Err(format!(
            "mount `{mount}`: type `{kind}` is not bind, volume or tmpfs"
        ));
    }
    if !out.contains_key("target") {
        return Err(format!("mount `{mount}` has no target"));
    }
    if kind == "bind" && !out.contains_key("source") {
        return Err(format!("bind mount `{mount}` has no source"));
    }
    out.insert("type".into(), Value::String(kind));
    for (key, section) in [("bind", bind), ("volume", volume), ("tmpfs", tmpfs)] {
        if !section.is_empty() {
            out.insert(key.into(), Value::Object(section));
        }
    }
    Ok(Value::Object(out))
}

/// The override file: `labels` and the devcontainer settings for `compose.service`, and published
/// ports (`ports`: service -> `docker run -p` values) for any service.
pub fn override_file(
    spec: &Spec,
    compose: &Compose,
    labels: &[(String, String)],
    ports: &BTreeMap<String, Vec<String>>,
) -> Result<Value, String> {
    let mut main = Map::new();
    main.insert(
        "labels".into(),
        Value::Array(
            labels
                .iter()
                .map(|(k, v)| Value::String(escape(&format!("{k}={v}"))))
                .collect(),
        ),
    );
    if !spec.container_env.is_empty() {
        let env: Map<String, Value> = spec
            .container_env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(escape(v))))
            .collect();
        main.insert("environment".into(), Value::Object(env));
    }
    if let Some(user) = &spec.container_user {
        main.insert("user".into(), Value::String(escape(user)));
    }
    if spec.override_command {
        main.insert(
            "entrypoint".into(),
            json!(["/bin/sh", "-c", escape(KEEP_ALIVE)]),
        );
    }
    if !spec.mounts.is_empty() {
        main.insert(
            "volumes".into(),
            Value::Array(
                spec.mounts
                    .iter()
                    .map(|m| volume(m))
                    .collect::<Result<_, _>>()?,
            ),
        );
    }
    if spec.privileged {
        main.insert("privileged".into(), Value::Bool(true));
    }
    if spec.init {
        main.insert("init".into(), Value::Bool(true));
    }
    if !spec.cap_add.is_empty() {
        main.insert(
            "cap_add".into(),
            json!(spec.cap_add.iter().map(|c| escape(c)).collect::<Vec<_>>()),
        );
    }
    if !spec.security_opt.is_empty() {
        main.insert(
            "security_opt".into(),
            json!(
                spec.security_opt
                    .iter()
                    .map(|c| escape(c))
                    .collect::<Vec<_>>()
            ),
        );
    }
    let mut services = Map::new();
    for (service, published) in ports {
        let entry = if *service == compose.service {
            &mut main
        } else {
            services
                .entry(service.clone())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("service entry")?
        };
        entry.insert(
            "ports".into(),
            json!(published.iter().map(|p| escape(p)).collect::<Vec<_>>()),
        );
    }
    services.insert(compose.service.clone(), Value::Object(main));
    Ok(json!({ "services": services }))
}
