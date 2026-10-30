//! Shared state of the tools plugin and its configuration.

use crate::artifacts::{Artifacts, InlineLimits};
use crate::jobs::Jobs;
use crate::read::ReadConfig;
use crate::readstate::ReadState;
use crate::shell::{ShellConfig, Shells, find_bash};
use crate::workspace::Workspaces;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Config {
    pub read: ReadConfig,
    /// Inline limits for command output.
    pub run_limits: InlineLimits,
    /// Inline limits for any other tool output (reads of explicit huge ranges, outlines).
    pub read_limits: InlineLimits,
    pub default_timeout_ms: u64,
    pub bash: Option<String>,
    /// Stop a session's shell after this long without commands (state is kept on disk).
    pub shell_idle_s: u64,
    /// Explicit path of the `cx` binary.
    pub cx_path: Option<String>,
    /// Extra directories every command may write (beyond the workspace, CORTEX_HOME, temp dirs,
    /// package caches and the session venv), e.g. an IDE's build cache.
    pub sandbox_write_paths: Vec<String>,
    /// How long a read of another project (`graph://<project>/...`, `symbol://<project>/...`,
    /// `outline://<project>/...`) waits for that project's index before it answers with the
    /// indexing progress instead (`reference_wait_ms`).
    pub reference_wait: std::time::Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            read: ReadConfig::default(),
            run_limits: InlineLimits::default(),
            read_limits: InlineLimits { inline: 48 * 1024, head: 16 * 1024, tail: 16 * 1024 },
            default_timeout_ms: 600_000,
            bash: None,
            shell_idle_s: 1800,
            cx_path: None,
            sandbox_write_paths: Vec::new(),
            reference_wait: std::time::Duration::from_millis(5000),
        }
    }
}

impl Config {
    /// Read overrides from the plugin's `[plugin.config]` table.
    pub fn from_value(value: &Value) -> Config {
        let mut config = Config::default();
        let number = |key: &str| value.get(key).and_then(Value::as_u64).map(|n| n as usize);
        if let Some(lines) = number("outline_lines") {
            config.read.outline_lines = lines.max(1);
        }
        if let Some(bytes) = number("outline_bytes") {
            config.read.outline_bytes = bytes.max(1);
        }
        if let Some(inline) = number("inline_output_bytes") {
            config.run_limits.inline = inline.max(256);
            config.run_limits.head = (inline / 4).max(64);
            config.run_limits.tail = (inline / 2).max(64);
        }
        if let Some(ms) = value.get("default_timeout_ms").and_then(Value::as_u64) {
            config.default_timeout_ms = ms.max(1);
        }
        if let Some(seconds) = value.get("shell_idle_s").and_then(Value::as_u64) {
            config.shell_idle_s = seconds.max(1);
        }
        if let Some(ms) = value.get("reference_wait_ms").and_then(Value::as_u64) {
            config.reference_wait = std::time::Duration::from_millis(ms);
        }
        config.bash = value.get("bash").and_then(Value::as_str).map(str::to_owned);
        config.cx_path = value.get("cx_path").and_then(Value::as_str).map(str::to_owned);
        config.sandbox_write_paths = value
            .get("sandbox_write_paths")
            .and_then(Value::as_array)
            .map(|paths| paths.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        config
    }
}

pub struct Core {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub config: Config,
    pub read_state: ReadState,
    pub shells: Shells,
    pub jobs: Arc<Jobs>,
    pub artifacts: Artifacts,
    pub workspaces: Workspaces,
    /// The step log of every workspace: what each step changed, for undo (see [`crate::undo`]).
    pub steps: crate::undo::StepLog,
    /// One tree, many hands: file reservations, running commands, check copies (see [`crate::tree`]).
    pub tree: crate::tree::Tree,
    file_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// What each session's user let its commands do: `Approval` nodes, held here while the plugin
    /// runs (see [`Core::load_approvals`]).
    pub approvals: Mutex<HashMap<String, BTreeSet<String>>>,
    /// The plugin's records in the app's graph (see [`crate::graph`]).
    pub graph: Arc<crate::graph::Graph>,
    /// Per session: edits of project files since its last run (an edit result names the next step).
    pub edits_since_run: Mutex<HashMap<String, usize>>,
    /// Per session: the files `edit` created in the running turn (cleared when it ends). The model
    /// may write those whole again: nothing of the user's is in them.
    pub created: Mutex<HashMap<String, BTreeSet<PathBuf>>>,
    /// Per session: each file `edit` changed in the running turn, as it was before the turn's first
    /// change (None: the turn created it). The baseline check runs the tests on these (cleared when
    /// the turn ends).
    pub originals: Mutex<HashMap<String, std::collections::BTreeMap<PathBuf, Option<String>>>>,
    /// Per session: the last refused call (tool and arguments) and how it was refused, so the same
    /// call refused again the same way says so.
    pub refused: Mutex<HashMap<String, (String, String)>>,
    /// Where the `cx` binary was found (linked into `<home>/bin`).
    pub cx: Option<PathBuf>,
    /// The write sandbox's machine-wide policy.
    pub sandbox: crate::sandbox::Policy,
}

/// How a session's commands run: its sandbox, its Python environment and the writes approved for it.
pub struct Runtime {
    pub sandbox: crate::sandbox::Sandbox,
    pub command_access: cc_protocol::contracts::frame::CommandAccessResult,
    pub venv: Option<crate::pyenv::PyEnv>,
    pub grants: Vec<PathBuf>,
    /// Why there is no Python environment (creation failed), for the tool output.
    pub venv_error: Option<String>,
}

/// Whether `path` lies under `root` (both resolved).
fn inside(root: &Path, path: &Path) -> bool {
    crate::util::canonical_lenient(path).starts_with(crate::util::canonical_lenient(root))
}

/// Locate the `cx` binary: the one this plugin ships next to its executable (built from the same
/// source), else the `cx_path` plugin config or the `CORTEX_CX_BIN` environment variable. The plugin
/// never assumes a repository layout.
pub fn find_cx(configured: Option<&str>) -> Option<PathBuf> {
    let beside = std::env::current_exe().and_then(|exe| exe.canonicalize()).ok().and_then(|exe| Some(exe.parent()?.join("cx")));
    if let Some(beside) = beside.filter(|path| path.is_file()) {
        return Some(beside);
    }
    find_program("cx", configured, "CORTEX_CX_BIN")
}

fn find_program(name: &str, configured: Option<&str>, env: &str) -> Option<PathBuf> {
    let explicit = configured.map(str::to_owned).or_else(|| std::env::var(env).ok().filter(|value| !value.is_empty()));
    if let Some(path) = explicit {
        // Absolute, so the link in the shells' bin directory resolves (a relative configured path
        // is relative to where the frame runs, not to that directory).
        return PathBuf::from(path).canonicalize().ok().filter(|path| path.is_file());
    }
    let exe = std::env::current_exe().and_then(|exe| exe.canonicalize()).ok()?;
    let beside = exe.parent()?.join(name);
    beside.is_file().then_some(beside)
}

/// Put `target` on the shells' PATH as `<bin_dir>/<name>`: a copy, so it keeps working when the
/// app bundle it came from moves or a build folder is cleaned (written beside and renamed over, so
/// it is never missing for a running shell). An identical copy is left as it is.
fn link_program(bin_dir: &Path, name: &str, target: &Path) {
    let _ = std::fs::create_dir_all(bin_dir);
    let place = bin_dir.join(name);
    let Ok(wanted) = std::fs::read(target) else { return };
    if std::fs::symlink_metadata(&place).is_ok_and(|meta| meta.is_file()) && std::fs::read(&place).is_ok_and(|have| have == wanted) {
        return;
    }
    let temp = bin_dir.join(format!(".{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let written = std::fs::write(&temp, &wanted).and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
    });
    if written.is_err() || std::fs::rename(&temp, &place).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

impl Core {
    pub fn new(home: &Path, data_dir: &Path, config: Config) -> Arc<Core> {
        let bash = find_bash(config.bash.as_deref());
        let bin_dir = home.join("bin");
        let cx = find_cx(config.cx_path.as_deref());
        if let Some(cx) = &cx {
            link_program(&bin_dir, "cx", cx);
        }
        // CortexCode's code is not changed from its shells: the packer earlier versions put here goes.
        let _ = std::fs::remove_file(bin_dir.join("cortex-pack"));
        let graph = crate::graph::Graph::new();
        let core = Arc::new(Core {
            home: home.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            shells: Shells::new(ShellConfig { state_root: data_dir.join("shells"), bin_dir: Some(bin_dir), bash: bash.clone() }),
            jobs: Jobs::new(home, bash, graph.clone()),
            artifacts: Artifacts::new(home, graph.clone()),
            workspaces: Workspaces::new(home, graph.clone()),
            steps: crate::undo::StepLog::new(data_dir, graph.clone()),
            tree: crate::tree::Tree::new(),
            read_state: ReadState::default(),
            file_locks: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            graph,
            edits_since_run: Mutex::new(HashMap::new()),
            created: Mutex::new(HashMap::new()),
            originals: Mutex::new(HashMap::new()),
            refused: Mutex::new(HashMap::new()),
            sandbox: crate::sandbox::Policy::new(home, data_dir, &config.sandbox_write_paths),
            config,
            cx,
        });
        // Reap idle shells in the background (when running inside a Tokio runtime).
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&core);
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let Some(core) = weak.upgrade() else { return };
                    core.shells.reap_idle(std::time::Duration::from_secs(core.config.shell_idle_s)).await;
                }
            });
        }
        core
    }

    /// Serialize edits to one file across sessions.
    pub fn file_lock(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.file_locks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks.entry(path.to_path_buf()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    fn approvals(&self) -> std::sync::MutexGuard<'_, HashMap<String, BTreeSet<String>>> {
        self.approvals.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn approve(&self, session: &str, classes: &[String]) -> Vec<String> {
        let mut approvals = self.approvals();
        let entry = approvals.entry(session.to_owned()).or_default();
        let new: Vec<String> = classes.iter().filter(|class| entry.insert((*class).clone())).cloned().collect();
        self.graph.put_many(new.iter().map(|class| approval_batch(session, class)).collect());
        entry.iter().cloned().collect()
    }

    /// The approvals kept in the graph, and once those an earlier version kept in the settings
    /// store (`tools.approvals`) or in `approvals.json`: written to the graph, then removed there.
    pub async fn load_approvals(&self) {
        let mut legacy: HashMap<String, BTreeSet<String>> = HashMap::new();
        let file = self.data_dir.join("approvals.json");
        if let Some(old) = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<HashMap<String, BTreeSet<String>>>(&bytes).ok()) {
            legacy.extend(old);
        }
        let setting = self.graph.call_waiting(cc_protocol::contracts::memory::methods::SETTINGS_GET, serde_json::json!({ "path": LEGACY_APPROVALS })).await;
        let had_setting = match setting {
            Ok(value) => {
                if let Some(old) = value.get("value").cloned().and_then(|value| serde_json::from_value::<HashMap<String, BTreeSet<String>>>(value).ok()) {
                    for (session, classes) in old {
                        legacy.entry(session).or_default().extend(classes);
                    }
                }
                true
            }
            Err(error) if error.code == cc_protocol::codes::NOT_FOUND => false,
            Err(error) => {
                tracing::error!(error = %error.message, "the approvals kept in the settings could not be read");
                false
            }
        };
        let imported = legacy.is_empty() || {
            let batches = legacy.iter().flat_map(|(session, classes)| classes.iter().map(|class| approval_batch(session, class))).collect();
            match self.graph.put_wait(batches).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(&file);
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error.message, "the old approvals were not written to the graph; they are kept where they were");
                    false
                }
            }
        };
        if imported
            && had_setting
            && let Err(error) = self.graph.call_waiting(cc_protocol::contracts::memory::methods::SETTINGS_DELETE, serde_json::json!({ "path": LEGACY_APPROVALS })).await
        {
            tracing::warn!(error = %error.message, "the old approvals setting could not be removed");
        }
        let stored = self
            .graph
            .query_all("MATCH (a:Approval) WHERE a.ended_ms IS NULL AND a.key > $after RETURN a, a.key AS k ORDER BY k LIMIT 5000", serde_json::json!({}), 60)
            .await;
        let mut approvals = self.approvals();
        for (session, classes) in legacy {
            approvals.entry(session).or_default().extend(classes);
        }
        match stored {
            Ok(rows) => {
                for props in rows.iter().filter_map(|row| row.first().and_then(crate::graph::properties)) {
                    let (session, class) = (crate::graph::text(props, "session_id"), crate::graph::text(props, "class"));
                    if !session.is_empty() && !class.is_empty() {
                        approvals.entry(session).or_default().insert(class);
                    }
                }
            }
            Err(error) => tracing::error!(error = %error.message, "the approvals could not be read from the graph"),
        }
    }

    /// The sandbox and Python environment for commands of `session` in `workspace`. With
    /// `python_env`, the project's venv is used, or the automatic one is created on first use.
    pub async fn runtime(
        &self,
        host: &dyn crate::host::Host,
        session: &str,
        workspace: &Path,
        python_env: bool,
    ) -> Result<Runtime, String> {
        if let Some(missing) = self.sandbox.unavailable() {
            return Err(format!("commands are not run: {missing}. Work with read and edit."));
        }
        let mut venv_error = None;
        let venv = if !python_env {
            None
        } else if let Some(project) = crate::pyenv::project_env(workspace) {
            Some(crate::pyenv::PyEnv::Project(project))
        } else {
            let dir = crate::pyenv::auto_dir(&self.home, workspace);
            let lock = self.file_lock(&dir);
            let _guard = lock.lock().await;
            match crate::pyenv::ensure_auto(&self.graph, &dir, workspace).await {
                Ok(_) => Some(crate::pyenv::PyEnv::Auto(dir)),
                Err(error) => {
                    venv_error = Some(error);
                    None
                }
            }
        };
        let access = host
            .call(
                cc_protocol::contracts::frame::methods::COMMAND_ACCESS,
                serde_json::json!({"session_id": session, "workspace": workspace}),
            )
            .await
            .map_err(|error| format!("cannot obtain command access: {}", error.message))?;
        let command_access: cc_protocol::contracts::frame::CommandAccessResult =
            serde_json::from_value(access).map_err(|error| format!("invalid command access: {error}"))?;
        self.sandbox.set_socket(Path::new(&command_access.socket));
        let grants = crate::sandbox::grants(&self.approved(session));
        let sandbox = self
            .sandbox
            .sandbox(workspace, venv.as_ref().map(|venv| venv.path()), &grants)
            .ok_or_else(|| "the write sandbox is unavailable".to_owned())?;
        Ok(Runtime { sandbox, command_access, venv, grants, venv_error })
    }

    /// A deleted session: its shell, background jobs, approvals and, when it worked in a workspace
    /// clone of its own, that clone and its venv go.
    pub async fn forget_session(&self, session: &str, workspace: &Path) {
        self.tree.release_session(session);
        self.shells.drop_session(session).await;
        for job in self.jobs.list(Some(session)) {
            self.jobs.kill(&job.job_id).await;
        }
        self.approvals().remove(session);
        self.graph.retire_matching(
            "Approval",
            "MATCH (a:Approval) WHERE a.session_id = $session AND a.ended_ms IS NULL RETURN a.key LIMIT 5000",
            serde_json::json!({ "session": session }),
            "session deleted",
        );
        self.jobs.forget_session(session);
        if inside(self.workspaces.root(), workspace) {
            self.forget_workspace(workspace);
        }
    }

    /// A deleted project: its workspace clones and the venv of its folder go (the folder stays).
    pub async fn forget_project(&self, project_id: &str, root: &Path) {
        for workspace in self.workspaces.list(Some(project_id)).await {
            self.forget_workspace(Path::new(&workspace.path));
        }
        self.steps.forget(root);
        let _ = std::fs::remove_dir_all(self.workspaces.root().join(project_id));
        crate::pyenv::remove(&self.graph, &crate::pyenv::auto_dir(&self.home, root));
    }

    fn forget_workspace(&self, workspace: &Path) {
        self.steps.forget(workspace);
        if let Err(error) = self.workspaces.remove(&workspace.to_string_lossy()) {
            tracing::debug!(workspace = %workspace.display(), error = %error.message, "removing a deleted session's workspace");
        }
        crate::pyenv::remove(&self.graph, &crate::pyenv::auto_dir(&self.home, workspace));
    }

    /// Use up a one-time approval (a destructive git action the user allowed once): true when it was
    /// given, and it is gone afterwards.
    pub fn take_approval(&self, session: &str, class: &str) -> bool {
        let taken = self.approvals().get_mut(session).is_some_and(|entry| entry.remove(class));
        if taken {
            self.graph.retire(vec![crate::graph::gref("Approval", &approval_key(session, class))], "used");
        }
        taken
    }

    pub fn approved(&self, session: &str) -> BTreeSet<String> {
        self.approvals().get(session).cloned().unwrap_or_default()
    }
}

/// Where earlier versions kept the approvals in the settings store.
const LEGACY_APPROVALS: &str = "tools.approvals";

fn approval_key(session: &str, class: &str) -> String {
    format!("{session}#{class}")
}

/// `(:Approval {key: '<session>#<class>', class, session_id})-[:FOR]->(:Session)`
fn approval_batch(session: &str, class: &str) -> crate::graph::Batch {
    let key = approval_key(session, class);
    let node = crate::graph::node("Approval", &key, crate::graph::props(serde_json::json!({ "class": class, "session_id": session })));
    let edge = crate::graph::edge(&crate::graph::gref("Approval", &key), "FOR", &crate::graph::gref("Session", session));
    crate::graph::Batch::workspace(vec![node], vec![edge], Vec::new())
}
