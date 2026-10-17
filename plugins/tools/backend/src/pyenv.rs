//! Python environments of a session: the project's own venv when the workspace has one, otherwise
//! an automatic venv per workspace under `<CORTEX_HOME>/venvs/<hash>` (never inside the repository),
//! created with `uv`. The session's shell activates it (VIRTUAL_ENV, PATH), so `python`, `pip` and
//! every tool installed with them resolve to it and installs land there.
//!
//! Detection is structural: a direct child directory of the workspace holding `pyvenv.cfg` and an
//! existing `bin/python` is a virtual environment (`.venv` is preferred: the uv/poetry/PEP 405
//! convention for a project's own environment).

use crate::util;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a session's Python comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyEnv {
    /// A venv inside the workspace (the project's own environment).
    Project(PathBuf),
    /// The automatic per-workspace venv under CORTEX_HOME.
    Auto(PathBuf),
}

impl PyEnv {
    pub fn path(&self) -> &Path {
        match self {
            PyEnv::Project(path) | PyEnv::Auto(path) => path,
        }
    }
}

fn is_venv(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file() && dir.join("bin/python").exists()
}

/// The workspace's own venv, if it has one.
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

/// Directory of the automatic venv of a workspace.
pub fn auto_dir(home: &Path, workspace: &Path) -> PathBuf {
    let canonical = util::canonical_lenient(workspace);
    let hash = util::content_hash(canonical.to_string_lossy().as_bytes());
    home.join("venvs").join(&hash[..16])
}

const READY: &str = ".cortex-ready";

fn ready(dir: &Path) -> bool {
    dir.join(READY).is_file() && dir.join("bin/python").exists()
}

async fn run(program: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("VIRTUAL_ENV")
        .env_remove("PYTHONHOME")
        .env_remove("CORTEX_VENV")
        .env("UV_NO_PROGRESS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|error| format!("{program}: {error}"))?;
    match tokio::time::timeout(Duration::from_secs(180), child.wait_with_output()).await {
        Err(_) => Err(format!("{program} {} timed out", args.join(" "))),
        Ok(Err(error)) => Err(format!("{program}: {error}")),
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).lines().take(6).collect::<Vec<_>>().join(" | ")
        )),
    }
}

/// Create the automatic venv of `workspace` unless it is ready. Steps, offline first:
/// `uv venv --seed --offline` (pip from uv's cache); else `uv venv` and pip from the interpreter's
/// bundled `ensurepip`; `uv venv` is tried in the workspace (honours `.python-version` and
/// `requires-python`) and then without project discovery; `python3 -m venv` only when uv is missing
/// or fails. Returns the method used.
pub async fn ensure_auto(dir: &Path, workspace: &Path) -> Result<String, String> {
    if ready(dir) {
        return Ok(std::fs::read_to_string(dir.join(READY)).unwrap_or_default().trim().to_owned());
    }
    let parent = dir.parent().ok_or("venv directory has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let target = dir.to_string_lossy().into_owned();
    let cwd = if workspace.is_dir() { workspace.to_path_buf() } else { parent.to_path_buf() };
    let reset = || {
        let _ = std::fs::remove_dir_all(dir);
    };
    let mut errors = Vec::new();
    reset();
    let mut method = None;
    match run("uv", &["venv", "--seed", "--offline", "--quiet", "--no-python-downloads", &target], &cwd).await {
        Ok(()) => method = Some("uv venv --seed (offline)"),
        Err(error) => errors.push(error),
    }
    if method.is_none() {
        let attempts: [(&[&str], &Path, &str); 2] = [
            (&["venv", "--quiet", "--no-python-downloads", &target], &cwd, "uv venv + ensurepip"),
            (&["venv", "--quiet", "--no-python-downloads", "--no-project", &target], parent, "uv venv --no-project + ensurepip"),
        ];
        for (args, dir_for, label) in attempts {
            reset();
            match run("uv", args, dir_for).await {
                Ok(()) => {
                    method = Some(label);
                    break;
                }
                Err(error) => errors.push(error),
            }
        }
    }
    if method.is_none() {
        reset();
        match run("python3", &["-m", "venv", &target], parent).await {
            Ok(()) => method = Some("python3 -m venv"),
            Err(error) => errors.push(error),
        }
    }
    let Some(method) = method else {
        reset();
        return Err(format!("could not create a Python venv: {}", errors.join("; ")));
    };
    if !dir.join("bin/pip").exists() {
        let python = dir.join("bin/python").to_string_lossy().into_owned();
        if let Err(error) = run(&python, &["-m", "ensurepip", "--default-pip"], parent).await {
            reset();
            return Err(format!("venv created ({method}) but pip could not be installed: {error}"));
        }
    }
    let _ = std::fs::write(dir.join(".cortex-workspace"), format!("{}\n", util::canonical_lenient(workspace).display()));
    std::fs::write(dir.join(READY), format!("{method}\n")).map_err(|error| format!("{}: {error}", dir.display()))?;
    Ok(method.to_owned())
}

/// Remove automatic venvs whose workspace no longer exists (workspaces deleted outside
/// `tools.workspace.remove`). A venv without a recorded workspace is left alone.
pub fn remove_orphans(home: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(home.join("venvs")) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(recorded) = std::fs::read_to_string(dir.join(".cortex-workspace")) else { continue };
        let workspace = PathBuf::from(recorded.trim_end_matches('\n'));
        if workspace.is_absolute() && !workspace.exists() && std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Bash that activates `venv` in the session shell. It switches to `venv` when the shell's managed
/// venv (`CORTEX_VENV`) is another one and the model has not activated a venv of its own, and keeps
/// `venv/bin` first on PATH while it is the active one; a venv the model activated or deactivated
/// itself is left alone.
pub fn activation(venv: &Path) -> String {
    let v = util::shell_quote(&venv.to_string_lossy());
    format!(
        "__cx_v={v}; \
if [ \"${{CORTEX_VENV-}}\" != \"$__cx_v\" ]; then \
if [ -z \"${{VIRTUAL_ENV-}}\" ] || [ \"${{VIRTUAL_ENV-}}\" = \"${{CORTEX_VENV-}}\" ]; then \
if [ -n \"${{CORTEX_VENV-}}\" ]; then __cx_o=\":$CORTEX_VENV/bin:\"; PATH=\":$PATH:\"; PATH=${{PATH//\"$__cx_o\"/:}}; PATH=${{PATH#:}}; PATH=${{PATH%:}}; unset __cx_o; fi; \
case \":$PATH:\" in *\":$__cx_v/bin:\"*) ;; *) PATH=\"$__cx_v/bin:$PATH\" ;; esac; \
export VIRTUAL_ENV=\"$__cx_v\" CORTEX_VENV=\"$__cx_v\" PATH; unset PYTHONHOME; hash -r; \
fi; \
elif [ \"${{VIRTUAL_ENV-}}\" = \"$__cx_v\" ]; then \
case \":$PATH:\" in *\":$__cx_v/bin:\"*) ;; *) export PATH=\"$__cx_v/bin:$PATH\" ;; esac; \
fi; unset __cx_v\n"
    )
}
