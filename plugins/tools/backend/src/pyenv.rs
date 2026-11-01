//! Python environments of a session: the project's own venv when the workspace has one, otherwise
//! an automatic venv per workspace under `<CORTEX_HOME>/venvs/<hash>` (never inside the repository),
//! created with `uv`. The session's shell activates it (VIRTUAL_ENV, PATH), so `python`, `pip` and
//! every tool installed with them resolve to it and installs land there.
//!
//! Detection is structural: a direct child directory of the workspace holding `pyvenv.cfg` and an
//! existing `bin/python` is a virtual environment (`.venv` is preferred: the uv/poetry/PEP 405
//! convention for a project's own environment).
//!
//! ```text
//! (:Venv {key: venv path, workspace, method, created_ms})-[:ON]->(:Machine)   an automatic venv
//! ```
//!
//! The venv folder is bytes and stays. A finished install is its node: the venv is ready when the
//! node is there (not retired) and `bin/python` exists. Earlier versions marked it with a
//! `.cortex-ready` file holding the method; that file is written to the node once and removed.

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

/// The marker earlier versions wrote into a finished venv (its content: the creation method).
const READY: &str = ".cortex-ready";

/// One query: while the memory plugin does not serve, the folder is used as it is (no wait).
const QUERY_ATTEMPTS: u32 = 1;

/// How the venv's node says it was made: `Ok(Some(method))` when the venv has a live node,
/// `Ok(None)` when it has none, `Err` when the graph cannot be asked.
async fn recorded_method(graph: &crate::graph::Graph, dir: &Path) -> Result<Option<String>, String> {
    let found = graph
        .query(
            "MATCH (v:Venv) WHERE v.key = $key AND v.ended_ms IS NULL RETURN v.method LIMIT 1",
            serde_json::json!({ "key": dir.to_string_lossy() }),
            1,
            QUERY_ATTEMPTS,
        )
        .await
        .map_err(|error| error.message)?;
    Ok(found
        .rows
        .first()
        .map(|row| row.first().and_then(serde_json::Value::as_str).unwrap_or_default().to_owned()))
}

/// When a file was last written, in epoch ms (now when unknown).
fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
        .unwrap_or_else(util::now_ms)
}

/// The method of a ready venv, or `None` when it must be (re)created. A `.cortex-ready` file of
/// an earlier version, with no node yet, becomes the node and is removed.
async fn ready(graph: &crate::graph::Graph, dir: &Path, workspace: &Path) -> Option<String> {
    if !dir.join("bin/python").exists() {
        return None;
    }
    let marker = dir.join(READY);
    match recorded_method(graph, dir).await {
        Ok(Some(method)) => {
            let _ = std::fs::remove_file(&marker);
            Some(method)
        }
        Ok(None) => {
            let method = std::fs::read_to_string(&marker).ok()?.trim().to_owned();
            let batch = venv_batch(dir, &util::canonical_lenient(workspace).to_string_lossy(), &method, modified_ms(&marker));
            match graph.put_wait(vec![batch]).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(&marker);
                }
                Err(error) => tracing::error!(error = %error.message, venv = %dir.display(), "the venv's record was not written to the graph; its marker file is kept"),
            }
            Some(method)
        }
        // The graph cannot be asked: a venv with its interpreter and pip is used as it is rather
        // than rebuilt.
        Err(error) => {
            tracing::warn!(error = %error, venv = %dir.display(), "the venv's record could not be read; using the folder as it is");
            dir.join("bin/pip").exists().then(|| std::fs::read_to_string(&marker).unwrap_or_default().trim().to_owned())
        }
    }
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
pub async fn ensure_auto(graph: &crate::graph::Graph, dir: &Path, workspace: &Path) -> Result<String, String> {
    if let Some(method) = ready(graph, dir, workspace).await {
        return Ok(method);
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
    // The node is what makes the venv ready: wait until the graph took it.
    let batch = venv_batch(dir, &util::canonical_lenient(workspace).to_string_lossy(), method, util::now_ms());
    if let Err(error) = graph.put_wait(vec![batch]).await {
        tracing::error!(error = %error.message, venv = %dir.display(), "the venv's record was not written to the graph");
        return Err(format!("venv created ({method}) but its record was not kept in the graph: {}", error.message));
    }
    Ok(method.to_owned())
}

/// The venv's node.
fn venv_batch(dir: &Path, workspace: &str, method: &str, created_ms: u64) -> crate::graph::Batch {
    use crate::graph;
    let key = dir.to_string_lossy();
    let node = graph::node("Venv", &key, graph::props(serde_json::json!({ "workspace": workspace, "method": method, "created_ms": created_ms })));
    graph::Batch::workspace(vec![node], vec![graph::edge(&graph::gref("Venv", &key), "ON", &graph::machine())], Vec::new())
}

/// Remove an automatic venv: the folder goes, its node is retired.
pub fn remove(graph: &crate::graph::Graph, dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    graph.retire(vec![crate::graph::gref("Venv", &dir.to_string_lossy())], "removed");
}

/// The workspace file earlier versions wrote into each venv (`.cortex-workspace`, with the method
/// in `.cortex-ready`): written to the graph as the venv's node, then both are removed.
async fn import_markers(graph: &crate::graph::Graph, home: &Path) {
    let Ok(entries) = std::fs::read_dir(home.join("venvs")) else { return };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let marker = dir.join(".cortex-workspace");
        let Ok(recorded) = std::fs::read_to_string(&marker) else { continue };
        let method = std::fs::read_to_string(dir.join(READY)).unwrap_or_default().trim().to_owned();
        let created_ms = modified_ms(&marker);
        found.push((dir.clone(), marker, venv_batch(&dir, recorded.trim_end_matches('\n'), &method, created_ms)));
    }
    if found.is_empty() {
        return;
    }
    let batches = found.iter().map(|(_, _, batch)| batch.clone()).collect();
    match graph.put_wait(batches).await {
        Ok(()) => {
            for (dir, marker, _) in found {
                let _ = std::fs::remove_file(marker);
                let _ = std::fs::remove_file(dir.join(READY));
            }
        }
        Err(error) => tracing::error!(error = %error.message, "the old venv records were not written to the graph; their files are kept"),
    }
}

/// Remove automatic venvs whose workspace no longer exists (workspaces deleted outside
/// `tools.workspace.remove`). A venv without a recorded workspace is left alone.
pub async fn remove_orphans(graph: &crate::graph::Graph, home: &Path) -> usize {
    import_markers(graph, home).await;
    let rows = match graph
        .query_all("MATCH (v:Venv) WHERE v.ended_ms IS NULL AND v.key > $after RETURN v, v.key AS k ORDER BY k LIMIT 5000", serde_json::json!({}), 60)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(error = %error.message, "the venvs could not be read from the graph");
            return 0;
        }
    };
    let venvs = home.join("venvs");
    let mut removed = 0;
    for props in rows.iter().filter_map(|row| row.first().and_then(crate::graph::properties)) {
        let dir = PathBuf::from(crate::graph::text(props, "key"));
        let workspace = PathBuf::from(crate::graph::text(props, "workspace"));
        if dir.starts_with(&venvs) && workspace.is_absolute() && !workspace.exists() {
            remove(graph, &dir);
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
