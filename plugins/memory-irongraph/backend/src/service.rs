//! The memory@1 plugin: project registry, method dispatch, events, probe and lifecycle.

use crate::db::{Graph, layer};
use crate::engine::{Engine, EnginePaths};
use crate::host::{CtxHost, Host};
use crate::langs::Languages;
use crate::params;
use crate::tree::{self, CodeTree, SyncReport};
use crate::watch::{self, TreeWatcher};
use crate::settings::Settings;
use crate::work::WorkStore;
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::{
    ExtractProblem, LanguagePlugin, UncoveredFiles,
    AppProjectDeleteParams, AppProjectPutParams, AppProjectsResult, AppSessionPutParams, AppSessionsDeleteParams, AppSessionsResult, JournalAppendParams, JournalAppendResult, JournalDropParams, JournalReadParams, JournalReadResult,
    AgentListParams, AgentListResult, AgentRecord, BoardListParams, BoardListResult, BoardPostParams, DescribeParams, EdgesParams, FilesParams, ImpactParams, LinesParams,
    KnowledgeBriefParams, KnowledgeCountParams, KnowledgeGetParams, KnowledgeItem, KnowledgeLinkParams, KnowledgeLinkedParams, KnowledgeListParams, KnowledgeListResult, KnowledgeSearchParams, KnowledgeSupersedeParams, KnowledgeUpsertParams, KnowledgeWithdrawParams,
    NoteAddParams, NoteListParams, NoteListResult, NoteRemoveParams, NoteRemoveResult, NoteSearchParams, OutlineParams, PathParams, ProjectCloseParams, ProjectCloseResult, ProjectInfo, ProjectListResult, ProjectOpenParams, ProjectStatus, RepoMapParams, Rule, RuleListParams, RuleListResult, RunRecordParams,
    RunRecordResult, Scope, SearchParams, SettingsDeleteParams, SettingsGetParams, SettingsListParams, SettingsListResult, SettingsSetParams, StyleEntry, StyleListParams, StyleListResult, SymbolParams, TaskCreateParams, TaskGetParams, TaskListParams,
    TaskListResult, TaskUpdateParams, UndescribedParams, WorkspaceParams, methods, topics,
};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, codes, parse_params, to_value};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::{Mutex, OnceCell, RwLock};

/// At most this many extraction problems are listed in a project's status (all are counted).
const EXTRACT_PROBLEMS_SHOWN: usize = 100;

/// The main IronGraph project: everything that belongs to no repository (the app's projects,
/// sessions and their journals, settings, global rules and style, the project registry). It is
/// never one of the app's projects, so it never shows in the project or session lists.
pub const MAIN_PROJECT: &str = "db";

pub struct Project {
    pub id: String,
    pub root: PathBuf,
    pub tree: Arc<CodeTree>,
    pub work: Arc<WorkStore>,
    workspaces: Mutex<HashMap<PathBuf, Arc<CodeTree>>>,
    watchers: Mutex<HashMap<PathBuf, TreeWatcher>>,
}

pub struct Memory {
    home: PathBuf,
    data: PathBuf,
    /// IronGraph's embedding model (installed under CORTEX_HOME before the database opens).
    pub engine: Arc<Engine>,
    graph: OnceCell<Arc<Graph>>,
    global: OnceCell<Arc<WorkStore>>,
    settings: OnceCell<Arc<Settings>>,
    app: OnceCell<Arc<crate::app::AppStore>>,
    projects: RwLock<HashMap<String, Arc<Project>>>,
    /// Serialises opening of each project.
    opening: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Task id -> project id cache (task ids are global).
    task_owner: Mutex<HashMap<String, String>>,
    ctx: OnceLock<Ctx>,
    /// The language plugins that extract the code graph's facts.
    pub langs: Arc<Languages>,
    shutting_down: std::sync::atomic::AtomicBool,
    /// Serialises registry writes, so two projects never get the same name.
    registry: Mutex<()>,
    /// Reference projects whose background open runs now, and the last failure of each.
    indexing: std::sync::Mutex<HashSet<String>>,
    failed: std::sync::Mutex<HashMap<String, String>>,
    /// Trees whose open (cold index) runs now, by project id: their progress, without waiting.
    opening_trees: std::sync::Mutex<HashMap<String, Arc<CodeTree>>>,
    /// References closed, or projects deleted, while their background index ran: forgotten once it stops.
    forgotten: std::sync::Mutex<HashSet<String>>,
    /// The background opens started, by project id.
    background: std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// A tree listed in [`Memory::opening_trees`] while its open runs (removed however it ends).
struct OpeningTree<'m> {
    trees: &'m std::sync::Mutex<HashMap<String, Arc<CodeTree>>>,
    id: String,
}

impl Drop for OpeningTree<'_> {
    fn drop(&mut self) {
        if let Ok(mut trees) = self.trees.lock() {
            trees.remove(&self.id);
        }
    }
}

/// A project registry entry (`ProjectEntry` in the main project).
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    name: String,
    root: String,
    reference: bool,
}

/// Names a project cannot take: graph URI verbs (`graph://<project>/search/...` must stay
/// unambiguous) and the work-state scopes.
const RESERVED_NAMES: &[&str] = &["search", "callers", "callees", "impact", "path", "global", "main", "db"];

/// The id the frame gives a canonical root (`frame/src/state.rs`, `project_id`): a reference opened
/// by path has the id the frame would give it, so opening it later as a session project reuses
/// its index.
pub fn root_project_id(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("p_{}", &hex[..16])
}

/// A project's name before de-duplication: its folder name, restricted to `A-Z a-z 0-9 . _ -`.
pub fn base_name(root: &Path) -> String {
    let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cleaned: String = folder.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() { "project".to_owned() } else { cleaned }
}

/// `base`, else `base-2`, `base-3`, ...: the first that no other project has and is not reserved.
pub fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    let free = |name: &str| !taken.contains(name) && !RESERVED_NAMES.contains(&name);
    if free(base) {
        return base.to_owned();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|name| free(name)).unwrap_or_default()
}

/// Every method this plugin serves.
pub const PROVIDES: &[&str] = &[
    methods::PROJECT_OPEN,
    methods::PROJECT_STATUS,
    methods::PROJECT_LIST,
    methods::PROJECT_CLOSE,
    methods::REFRESH,
    methods::WORKSPACE_ATTACH,
    methods::WORKSPACE_DETACH,
    methods::SEARCH,
    methods::SYMBOL,
    methods::CALLERS,
    methods::CALLEES,
    methods::IMPACT,
    methods::PATH,
    methods::OUTLINE,
    methods::REPO_MAP,
    methods::FILES,
    methods::SKILLS,
    methods::SKILL,
    crate::queries::QUERY,
    methods::DESCRIBE,
    methods::UNDESCRIBED,
    methods::TASK_CREATE,
    methods::TASK_UPDATE,
    methods::TASK_GET,
    methods::TASK_LIST,
    methods::BOARD_POST,
    methods::BOARD_LIST,
    methods::AGENT_UPSERT,
    methods::AGENT_LIST,
    methods::RULE_LIST,
    methods::RULE_UPSERT,
    methods::STYLE_LIST,
    methods::STYLE_UPSERT,
    methods::RUN_RECORD,
    methods::LINES,
    methods::SETTINGS_GET,
    methods::SETTINGS_SET,
    methods::SETTINGS_LIST,
    methods::SETTINGS_DELETE,
    methods::ENGINE_STATUS,
    methods::APP_PROJECTS,
    methods::APP_PROJECT_PUT,
    methods::APP_PROJECT_DELETE,
    methods::APP_SESSIONS,
    methods::APP_SESSION_PUT,
    methods::APP_SESSIONS_DELETE,
    methods::JOURNAL_APPEND,
    methods::JOURNAL_READ,
    methods::JOURNAL_DROP,
    methods::NOTE_ADD,
    methods::NOTE_SEARCH,
    methods::NOTE_LIST,
    methods::NOTE_REMOVE,
    methods::KNOWLEDGE_UPSERT,
    methods::KNOWLEDGE_GET,
    methods::KNOWLEDGE_LIST,
    methods::KNOWLEDGE_SEARCH,
    methods::KNOWLEDGE_LINK,
    methods::KNOWLEDGE_UNLINK,
    methods::KNOWLEDGE_WITHDRAW,
    methods::KNOWLEDGE_SUPERSEDE,
    methods::KNOWLEDGE_LINKED,
    methods::KNOWLEDGE_BRIEF,
    methods::KNOWLEDGE_COUNT,
];

fn short_hash(text: &str) -> String {
    crate::facts::hash_hex(text.as_bytes())[..24].to_owned()
}

fn project_ig_name(project_id: &str) -> String {
    format!("cc_p_{}", short_hash(project_id))
}

fn workspace_ig_name(project_id: &str, workspace: &Path) -> String {
    format!("cc_w_{}", short_hash(&format!("{project_id}\u{0}{}", workspace.display())))
}

fn canonical(path: &str) -> Result<PathBuf, RpcError> {
    std::fs::canonicalize(path).map_err(|e| RpcError::invalid_params(format!("{path}: {e}")))
}

impl Memory {
    /// Paths from the plugin environment: `<CORTEX_HOME>/irongraph` holds the database.
    pub fn new(home: PathBuf, data: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            engine: Engine::new(EnginePaths::under(&home)),
            home,
            data,
            graph: OnceCell::new(),
            global: OnceCell::new(),
            settings: OnceCell::new(),
            app: OnceCell::new(),
            projects: RwLock::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
            task_owner: Mutex::new(HashMap::new()),
            ctx: OnceLock::new(),
            langs: Languages::new(),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            registry: Mutex::new(()),
            indexing: std::sync::Mutex::new(HashSet::new()),
            failed: std::sync::Mutex::new(HashMap::new()),
            opening_trees: std::sync::Mutex::new(HashMap::new()),
            forgotten: std::sync::Mutex::new(HashSet::new()),
            background: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Reach the language plugins (and every other category) through `host`: the frame once the
    /// plugin has a context, an in-test peer in the tests.
    pub fn set_host(&self, host: Arc<dyn Host>) {
        self.langs.set_host(host);
    }

    fn attach_ctx(&self, ctx: &Ctx) {
        self.engine.attach(ctx);
        if self.ctx.set(ctx.clone()).is_ok() {
            self.set_host(Arc::new(CtxHost(ctx.clone())));
        }
    }

    /// The environment the frame started this process with (known from the first request on).
    fn plugin_env(&self) -> Result<&PluginEnv, RpcError> {
        self.ctx.get().map(|ctx| &ctx.env).ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "memory: not connected to the frame yet"))
    }

    /// An event this plugin subscribed to: a project the frame opened is opened here too; a
    /// language plugin that changed state or was swapped changes which plugin extracts which files,
    /// so every open tree is brought in line.
    pub async fn on_event(self: Arc<Self>, topic: &str, data: Value) {
        use cc_protocol::contracts::{contract_of, frame, lang};
        if topic == frame::topics::PROJECT_CHANGED {
            self.open_frame_project(&data).await;
            return;
        }
        if topic == frame::topics::PROJECT_DELETED {
            if let Some(id) = data.get("project_id").and_then(Value::as_str)
                && let Err(error) = self.forget_project(id).await
            {
                tracing::warn!(project = id, %error, "dropping a deleted project's memory failed");
            }
            return;
        }
        if topic != frame::topics::PLUGIN_STATE && topic != frame::topics::PLUGIN_SWAPPED {
            return;
        }
        let category = data.get("category").and_then(Value::as_str).unwrap_or("");
        let contract = data.get("contract").and_then(Value::as_str).unwrap_or("");
        if contract != lang::CONTRACT && contract_of(category) != Some(lang::CONTRACT) {
            return;
        }
        self.langs.invalidate();
        self.recover_all().await;
    }

    /// The frame's projects are the person's projects: each is opened (in the background) when the
    /// frame opens it and when this plugin starts, so its tasks, board and code graph answer before
    /// a conversation or an agent first asks for them.
    pub async fn open_frame_project(self: &Arc<Self>, project: &Value) {
        let (Some(id), Some(root)) = (project.get("id").and_then(Value::as_str), project.get("root").and_then(Value::as_str)) else {
            return;
        };
        if self.projects.read().await.contains_key(id) {
            return;
        }
        let root = PathBuf::from(root);
        match self.register(id, &root, Some(false)).await {
            Ok(_) => self.open_in_background(id, &root),
            Err(error) => tracing::warn!(project = %id, %error, "registering a frame project failed"),
        }
    }

    /// Open every project the frame knows (at start).
    pub async fn open_frame_projects(self: &Arc<Self>) {
        let Some(ctx) = self.ctx.get().cloned() else { return };
        match ctx.call(cc_protocol::contracts::frame::methods::PROJECT_LIST, json!({})).await {
            Ok(listed) => {
                for project in listed.get("projects").and_then(Value::as_array).cloned().unwrap_or_default() {
                    self.open_frame_project(&project).await;
                }
            }
            Err(error) => tracing::warn!(%error, "listing the frame's projects failed"),
        }
    }

    /// Re-match every open tree's files to the active language plugins.
    pub async fn recover_all(&self) {
        let projects: Vec<Arc<Project>> = self.projects.read().await.values().cloned().collect();
        for project in projects {
            let mut trees: Vec<(Option<String>, Arc<CodeTree>)> = vec![(None, project.tree.clone())];
            trees.extend(project.workspaces.lock().await.iter().map(|(path, tree)| (Some(path.to_string_lossy().into_owned()), tree.clone())));
            for (workspace, tree) in trees {
                if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                match tree.recover().await {
                    Ok(report) if report.files_changed + report.files_deleted > 0 => {
                        tracing::info!(project = %project.id, files = report.files_changed, ms = report.total_ms, "re-extracted after a language plugin change");
                        self.publish(topics::INDEXED, indexed_payload(&project.id, workspace.as_deref(), &report));
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "re-extraction after a language plugin change failed"),
                }
            }
        }
    }

    pub fn irongraph_dir(&self) -> PathBuf {
        self.home.join("irongraph")
    }

    /// The process's single embedded IronGraph instance (opened on first use, after the embedding
    /// model is installed and verified). A predecessor that still holds the directory is waited for
    /// (it releases ownership when it exits).
    pub async fn graph(&self) -> Result<Arc<Graph>, RpcError> {
        self.graph
            .get_or_try_init(|| async {
                self.engine.install().await?;
                let paths = self.engine.paths.clone();
                let started = Instant::now();
                loop {
                    match Graph::open(&paths, true).await {
                        Ok(graph) => {
                            let ms = started.elapsed().as_millis() as u64;
                            tracing::info!(dir = %paths.data_dir.display(), models = %paths.model_dir.display(), cache = %paths.cache_dir.display(), ms, "irongraph open");
                            self.engine.update(true, |s| {
                                s.state = "ready".into();
                                s.open_ms = Some(ms);
                            });
                            return Ok(graph);
                        }
                        Err(error) if started.elapsed().as_secs() < 90 => {
                            tracing::warn!(%error, "irongraph open failed; retrying");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(error) => {
                            self.engine.update(true, |s| {
                                s.state = "error".into();
                                s.error = Some(error.message.clone());
                            });
                            return Err(error);
                        }
                    }
                }
            })
            .await
            .cloned()
    }

    async fn global(&self) -> Result<Arc<WorkStore>, RpcError> {
        let graph = self.graph().await?;
        self.global
            .get_or_try_init(|| async {
                let id = graph.ensure_project(MAIN_PROJECT).await?;
                Ok::<_, RpcError>(Arc::new(WorkStore::new(graph.clone(), id, "global".into())))
            })
            .await
            .cloned()
    }

    async fn settings(&self) -> Result<Arc<Settings>, RpcError> {
        let global = self.global().await?;
        self.settings.get_or_try_init(|| async { Ok::<_, RpcError>(Arc::new(Settings::open(global.graph.clone(), global.ig_id.clone()).await?)) }).await.cloned()
    }

    /// The app's projects, sessions and journals (see [`crate::app`]).
    async fn app(&self) -> Result<Arc<crate::app::AppStore>, RpcError> {
        let global = self.global().await?;
        self.app.get_or_try_init(|| async { Ok::<_, RpcError>(Arc::new(crate::app::AppStore::open(global.graph.clone(), global.ig_id.clone()).await?)) }).await.cloned()
    }

    /// Open the database and create the main project (with its settings root) on first start.
    pub async fn ensure_main(&self) -> Result<(), RpcError> {
        self.settings().await.map(|_| ())
    }

    fn publish(&self, topic: &str, data: Value) {
        if let Some(ctx) = self.ctx.get() {
            ctx.publish(topic, data);
        }
    }

    /// Registry entry for a project id: its root.
    async fn registered_root(&self, project_id: &str) -> Result<Option<PathBuf>, RpcError> {
        let global = self.global().await?;
        let rows = global
            .graph
            .run(&global.ig_id, &format!("{} MATCH (p:ProjectEntry {{id: $id}}) RETURN p.root", layer::WORKSPACE_READ), params! { "id" => project_id })
            .await?;
        Ok(rows.rows.first().and_then(|r| r.first()).and_then(|v| v.as_str()).map(PathBuf::from))
    }

    async fn registered_projects(&self) -> Result<Vec<String>, RpcError> {
        let global = self.global().await?;
        let rows = global.graph.run(&global.ig_id, &format!("{} MATCH (p:ProjectEntry) RETURN p.id", layer::WORKSPACE_READ), Map::new()).await?;
        Ok(rows.column(0).filter_map(|v| v.as_str().map(str::to_owned)).collect())
    }

    /// Every registry entry (entries written before names were de-duplicated may share a name
    /// until one of them is opened again).
    async fn entries(&self) -> Result<Vec<Entry>, RpcError> {
        let global = self.global().await?;
        let rows = global
            .graph
            .run(&global.ig_id, &format!("{} MATCH (p:ProjectEntry) RETURN p.id, p.name, p.root, p.reference", layer::WORKSPACE_READ), Map::new())
            .await?;
        let text = |row: &[Value], index: usize| row.get(index).and_then(Value::as_str).unwrap_or("").to_owned();
        Ok(rows
            .rows
            .iter()
            .map(|row| Entry { id: text(row, 0), name: text(row, 1), root: text(row, 2), reference: row.get(3).and_then(Value::as_bool).unwrap_or(false) })
            .filter(|entry| !entry.id.is_empty())
            .collect())
    }

    /// Write a project's registry entry and return it. Its name is kept while no other project
    /// has it; `reference`: Some(flag) when the caller says what the project is (a project opened
    /// as a session project once stays one), None keeps the entry's flag (false when new).
    async fn register(&self, project_id: &str, root: &Path, reference: Option<bool>) -> Result<Entry, RpcError> {
        let _guard = self.registry.lock().await;
        let entries = self.entries().await?;
        let existing = entries.iter().find(|e| e.id == project_id);
        let taken: HashSet<String> = entries.iter().filter(|e| e.id != project_id).map(|e| e.name.clone()).collect();
        let name = match existing {
            Some(entry) if !entry.name.is_empty() && !taken.contains(&entry.name) && !RESERVED_NAMES.contains(&entry.name.as_str()) => entry.name.clone(),
            _ => unique_name(&base_name(root), &taken),
        };
        let reference = match (existing, reference) {
            (Some(entry), Some(flag)) => entry.reference && flag,
            (Some(entry), None) => entry.reference,
            (None, flag) => flag.unwrap_or(false),
        };
        let global = self.global().await?;
        global
            .graph
            .run(
                &global.ig_id,
                &format!(
                    "{} MERGE (p:ProjectEntry {{id: $id}}) SET p.name = $name, p.root = $root, p.ig_name = $ig, p.reference = $reference, p.updated_ms = $now",
                    layer::WORKSPACE_WRITE
                ),
                params! { "id" => project_id, "name" => name.clone(), "root" => root.to_string_lossy(), "ig" => project_ig_name(project_id), "reference" => reference, "now" => cc_protocol::contracts::now_ms() },
            )
            .await?;
        Ok(Entry { id: project_id.to_owned(), name, root: root.to_string_lossy().into_owned(), reference })
    }

    /// A read-only reference project refuses work state: tasks, board posts, agents, runs, rules
    /// and style belong to the session's own project.
    async fn refuse_reference(&self, project_id: &str, what: &str) -> Result<(), RpcError> {
        if project_id == "global" {
            return Ok(());
        }
        match self.entries().await?.into_iter().find(|e| e.id == project_id) {
            Some(entry) if entry.reference => Err(RpcError::invalid_params(format!(
                "project {} ({}) is a read-only reference; {what} belong to the session's own project",
                entry.name, entry.root
            ))),
            _ => Ok(()),
        }
    }

    /// Open a project in the background (once at a time per project): a read-only reference, or a
    /// project the frame opened; its queries wait for the index through the per-project opening lock.
    fn open_in_background(self: &Arc<Self>, project_id: &str, root: &Path) {
        let fresh = self.indexing.lock().map(|mut set| set.insert(project_id.to_owned())).unwrap_or(false);
        if !fresh {
            return;
        }
        let me = self.clone();
        let (id, root) = (project_id.to_owned(), root.to_path_buf());
        let handle = tokio::spawn(async move {
            let result = me.open_project(&id, &root, None).await;
            if let Ok(mut failed) = me.failed.lock() {
                match result {
                    Ok(_) => failed.remove(&id),
                    Err(error) => {
                        tracing::warn!(project = %id, %error, "background project open failed");
                        failed.insert(id.clone(), error.message)
                    }
                };
            }
            // Closed while it was indexed: forget it now that nothing writes it any more.
            let forget = match me.indexing.lock() {
                Ok(mut set) => {
                    set.remove(&id);
                    me.forgotten.lock().map(|mut forgotten| forgotten.remove(&id)).unwrap_or(false)
                }
                Err(_) => false,
            };
            if forget && let Err(error) = me.drop_reference(&id).await {
                tracing::warn!(project = %id, %error, "forgetting a closed reference failed");
            }
        });
        if let Ok(mut background) = self.background.lock() {
            background.retain(|_, handle| !handle.is_finished());
            background.insert(project_id.to_owned(), handle);
        }
    }

    /// Wait until the background open of a reference (if one was started) has ended, including
    /// forgetting it when it was closed meanwhile.
    pub async fn wait_background(&self, project_id: &str) {
        let handle = self.background.lock().ok().and_then(|mut background| background.remove(project_id));
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// The status of a reference whose background open runs now, answered without waiting for
    /// it (None when the project is loaded, or not opening in the background, or `scope` names
    /// another tree than its root).
    async fn background_status(&self, scope: &Scope) -> Result<Option<ProjectStatus>, RpcError> {
        let id = scope.project_id.as_str();
        let running = self.indexing.lock().map(|set| set.contains(id)).unwrap_or(false);
        if !running || self.projects.read().await.contains_key(id) {
            return Ok(None);
        }
        let Some(entry) = self.entries().await?.into_iter().find(|e| e.id == id) else { return Ok(None) };
        if let Some(workspace) = &scope.workspace
            && std::fs::canonicalize(workspace).map(|w| w != Path::new(&entry.root)).unwrap_or(true)
        {
            return Ok(None);
        }
        let mut status = indexing_status(&entry);
        if let Some(tree) = self.opening_trees.lock().ok().and_then(|trees| trees.get(id).cloned()) {
            status.indexed_files = tree.files_done.load(std::sync::atomic::Ordering::Relaxed);
            status.total_files = tree.files_total.load(std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Some(status))
    }

    /// `memory.project.close`: forget a read-only reference (by id or name): its registry entry
    /// now, its code graph and line index now or, while its background index runs, once it stops.
    async fn close_reference(&self, key: &str) -> Result<ProjectCloseResult, RpcError> {
        let entries = self.entries().await?;
        let Some(entry) = entries.iter().find(|e| e.id == key).or_else(|| entries.iter().find(|e| e.name == key)).cloned() else {
            let mut known: Vec<String> = entries.iter().filter(|e| e.reference).map(|e| format!("{} ({})", e.name, e.root)).collect();
            known.sort();
            let known = if known.is_empty() { "none".to_owned() } else { known.join(", ") };
            return Err(RpcError::not_found(format!("no project `{key}`; reference projects: {known}")));
        };
        if !entry.reference {
            return Err(RpcError::invalid_params(format!(
                "project {} ({}) is a session project, not a read-only reference; only references can be closed",
                entry.name, entry.root
            )));
        }
        // Under the lock the background open ends under, so the close is seen by it or done here.
        let running = match self.indexing.lock() {
            Ok(set) if set.contains(&entry.id) => {
                if let Ok(mut forgotten) = self.forgotten.lock() {
                    forgotten.insert(entry.id.clone());
                }
                true
            }
            _ => false,
        };
        self.unregister(&entry.id).await?;
        if !running {
            self.drop_reference(&entry.id).await?;
        }
        Ok(ProjectCloseResult { project_id: entry.id, name: entry.name, root: entry.root })
    }

    /// A project the person deleted: everything memory holds for it goes (its code graph and
    /// workspaces, line indexes, knowledge, registry entry), as for a closed reference.
    async fn forget_project(&self, project_id: &str) -> Result<(), RpcError> {
        // Under the lock the background open ends under, so the drop is seen by it or done here.
        let running = match self.indexing.lock() {
            Ok(set) if set.contains(project_id) => {
                if let Ok(mut forgotten) = self.forgotten.lock() {
                    forgotten.insert(project_id.to_owned());
                }
                true
            }
            _ => false,
        };
        self.unregister(project_id).await?;
        if !running {
            self.drop_reference(project_id).await?;
        }
        tracing::info!(project = project_id, "dropped a deleted project's memory");
        Ok(())
    }

    /// Remove a project's registry entry.
    async fn unregister(&self, project_id: &str) -> Result<(), RpcError> {
        let _guard = self.registry.lock().await;
        let global = self.global().await?;
        global
            .graph
            .run(&global.ig_id, &format!("{} MATCH (p:ProjectEntry {{id: $id}}) DETACH DELETE p", layer::WORKSPACE_WRITE), params! { "id" => project_id })
            .await?;
        Ok(())
    }

    /// Drop everything memory holds for a closed reference: the loaded project (watchers,
    /// attached workspaces), its IronGraph projects and line indexes, its registry entry.
    async fn drop_reference(&self, project_id: &str) -> Result<(), RpcError> {
        let project = self.projects.write().await.remove(project_id);
        let graph = self.graph().await?;
        if let Some(project) = project {
            project.watchers.lock().await.clear();
            let workspaces: Vec<Arc<CodeTree>> = project.workspaces.lock().await.drain().map(|(_, tree)| tree).collect();
            for tree in workspaces {
                graph.drop_project(&tree.ig_name).await?;
                let _ = std::fs::remove_dir_all(tree.lines.dir());
            }
        }
        let ig_name = project_ig_name(project_id);
        graph.drop_project(&ig_name).await?;
        let _ = std::fs::remove_dir_all(self.data.join("linestore").join(&ig_name));
        if let Ok(mut failed) = self.failed.lock() {
            failed.remove(project_id);
        }
        // An open that registered it again while it ran.
        self.unregister(project_id).await
    }

    /// Every known project, by name.
    async fn list_projects(&self) -> Result<ProjectListResult, RpcError> {
        let loaded: HashMap<String, Arc<Project>> = self.projects.read().await.clone();
        let indexing = self.indexing.lock().map(|set| set.clone()).unwrap_or_default();
        let failed = self.failed.lock().map(|map| map.clone()).unwrap_or_default();
        let mut projects = Vec::new();
        for entry in self.entries().await? {
            let (state, error) = match loaded.get(&entry.id) {
                Some(project) => {
                    let status = project.tree.status.lock().await;
                    (status.state.clone(), status.error.clone())
                }
                None if indexing.contains(&entry.id) => ("indexing".to_owned(), None),
                None => match failed.get(&entry.id) {
                    Some(error) => ("error".to_owned(), Some(error.clone())),
                    None => ("closed".to_owned(), None),
                },
            };
            projects.push(ProjectInfo { project_id: entry.id, name: entry.name, root: entry.root, reference: entry.reference, state, error });
        }
        projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.project_id.cmp(&b.project_id)));
        Ok(ProjectListResult { projects })
    }

    /// A project's status with its registry name and reference flag.
    async fn named_status(&self, project_id: &str, tree: &CodeTree) -> Result<ProjectStatus, RpcError> {
        let mut status = self.status_of(project_id, tree).await?;
        if let Some(entry) = self.entries().await?.into_iter().find(|e| e.id == project_id) {
            status.name = entry.name;
            status.reference = entry.reference;
        }
        Ok(status)
    }

    /// Work store of a project; needs only the registry, not the code index.
    async fn work(&self, project_id: &str) -> Result<Arc<WorkStore>, RpcError> {
        if project_id == "global" {
            return self.global().await;
        }
        if let Some(project) = self.projects.read().await.get(project_id) {
            return Ok(project.work.clone());
        }
        if self.registered_root(project_id).await?.is_none() {
            return Err(RpcError::not_found(format!("project {project_id} is not open; call memory.project.open first")));
        }
        let graph = self.graph().await?;
        let ig = graph.ensure_project(&project_ig_name(project_id)).await?;
        Ok(Arc::new(WorkStore::new(graph, ig, project_id.to_owned())))
    }

    /// A loaded project (opening it from the registry after a restart).
    async fn project(self: &Arc<Self>, project_id: &str) -> Result<Arc<Project>, RpcError> {
        if let Some(project) = self.projects.read().await.get(project_id) {
            return Ok(project.clone());
        }
        let Some(root) = self.registered_root(project_id).await? else {
            return Err(RpcError::not_found(format!("project {project_id} is not open; call memory.project.open first")));
        };
        self.open_project(project_id, &root, None).await
    }

    async fn open_project(self: &Arc<Self>, project_id: &str, root: &Path, reference: Option<bool>) -> Result<Arc<Project>, RpcError> {
        let lock = self.opening.lock().await.entry(project_id.to_owned()).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
        let _guard = lock.lock().await;
        if let Some(project) = self.projects.read().await.get(project_id)
            && project.root == root
        {
            return Ok(project.clone());
        }
        let graph = self.graph().await?;
        let ig_name = project_ig_name(project_id);
        let tree = CodeTree::open(root.to_path_buf(), ig_name.clone(), graph.clone(), self.data.join("linestore").join(&ig_name), self.langs.clone()).await?;
        if let Ok(mut trees) = self.opening_trees.lock() {
            trees.insert(project_id.to_owned(), tree.clone());
        }
        let _opening = OpeningTree { trees: &self.opening_trees, id: project_id.to_owned() };
        let work = Arc::new(WorkStore::new(graph.clone(), tree.ig_id.clone(), project_id.to_owned()));
        // Registry entry (WORKSPACE of the global project).
        let entry = self.register(project_id, root, reference).await?;
        let global = self.global().await?;
        let report = tree.full_index().await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                let mut status = tree.status.lock().await;
                status.state = "error".into();
                status.error = Some(error.message.clone());
                return Err(error);
            }
        };
        tracing::info!(project = project_id, files = report.files_changed, ms = report.total_ms, statements = report.statements, "indexed");
        // CORTEX.md rules: the project's file (not a reference's: its rules are not this work's),
        // and the global one in CORTEX_HOME.
        if !entry.reference {
            let text = std::fs::read_to_string(root.join("CORTEX.md")).unwrap_or_default();
            work.import_cortex_md(&text).await?;
        }
        if let Ok(text) = std::fs::read_to_string(self.home.join("CORTEX.md")) {
            global.import_cortex_md(&text).await?;
        }
        let project = Arc::new(Project {
            id: project_id.to_owned(),
            root: root.to_path_buf(),
            tree: tree.clone(),
            work,
            workspaces: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
        });
        self.start_watch(&project, root.to_path_buf(), tree.clone()).await;
        self.projects.write().await.insert(project_id.to_owned(), project.clone());
        self.publish(topics::INDEXED, json!({"project_id": project_id, "files": report.files_changed, "ms": report.total_ms, "full": true, "report": report}));
        // Only the project's own code is indexed: third-party dependencies are not mapped (owner).
        if let Err(error) = tree.drop_externals().await {
            tracing::warn!(%error, "removing third-party API nodes failed");
        }
        // A language plugin that became active while the index ran.
        match tree.recover().await {
            Ok(report) if report.files_changed + report.files_deleted > 0 => self.publish(topics::INDEXED, indexed_payload(project_id, None, &report)),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "re-extraction after open failed"),
        }
        Ok(project)
    }

    async fn start_watch(self: &Arc<Self>, project: &Arc<Project>, root: PathBuf, tree: Arc<CodeTree>) {
        let me = Arc::downgrade(self);
        let project_id = project.id.clone();
        let workspace = if root == project.root { None } else { Some(root.to_string_lossy().into_owned()) };
        let watch_root = root.clone();
        let work = project.work.clone();
        let result = watch::watch(&root, move |batch: watch::Batch| {
            let me = me.clone();
            let tree = tree.clone();
            let project_id = project_id.clone();
            let workspace = workspace.clone();
            let root = watch_root.clone();
            let work = work.clone();
            async move {
                let Some(me) = me.upgrade() else { return };
                if me.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let paths = batch.paths;
                let reconcile = batch.reconcile || paths.iter().any(|p| watch::changes_membership(&root, p));
                // HEAD or a ref moved: new commits for MODIFIED_IN.
                let git_moved = paths.iter().any(|p| {
                    let s = p.to_string_lossy();
                    s.contains("/.git/") && (s.ends_with("/HEAD") || s.contains("/.git/refs/") || s.ends_with("ORIG_HEAD"))
                });
                if !reconcile && git_moved && let Err(error) = tree.refresh_history().await {
                    tracing::warn!(%error, "history refresh failed");
                }
                let paths: Vec<PathBuf> = paths.into_iter().filter(|p| !p.to_string_lossy().contains("/.git/")).collect();
                let rels = if reconcile { Vec::new() } else { expand_paths(&root, &paths, &tree).await };
                if workspace.is_none() && (reconcile || paths.iter().any(|p| p.file_name().is_some_and(|n| n == "CORTEX.md"))) {
                    let text = std::fs::read_to_string(root.join("CORTEX.md")).unwrap_or_default();
                    if let Err(error) = work.import_cortex_md(&text).await {
                        tracing::warn!(%error, "CORTEX.md import failed");
                    }
                }
                if !reconcile && rels.is_empty() {
                    return;
                }
                let result = if reconcile { tree.full_index().await } else { tree.refresh_paths(&rels).await };
                match result {
                    Ok(report) if report.files_changed + report.files_deleted > 0 => {
                        me.publish(topics::INDEXED, indexed_payload(&project_id, workspace.as_deref(), &report));
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "incremental index failed"),
                }
            }
        });
        match result {
            Ok(watcher) => {
                project.watchers.lock().await.insert(root, watcher);
            }
            Err(error) => tracing::warn!(%error, root = %root.display(), "file watching unavailable"),
        }
    }

    /// The tree a scope reads: the project, or an attached workspace (attached on demand).
    async fn tree(self: &Arc<Self>, scope: &Scope) -> Result<Arc<CodeTree>, RpcError> {
        let project = self.project(&scope.project_id).await?;
        let Some(workspace) = &scope.workspace else { return Ok(project.tree.clone()) };
        let path = canonical(workspace)?;
        if path == project.root {
            return Ok(project.tree.clone());
        }
        if let Some(tree) = project.workspaces.lock().await.get(&path) {
            return Ok(tree.clone());
        }
        self.attach(&project, &path).await
    }

    /// Index a workspace (an agent's copy-on-write clone) as its own IronGraph project.
    async fn attach(self: &Arc<Self>, project: &Arc<Project>, path: &Path) -> Result<Arc<CodeTree>, RpcError> {
        let mut workspaces = project.workspaces.lock().await;
        if let Some(tree) = workspaces.get(path) {
            return Ok(tree.clone());
        }
        let graph = self.graph().await?;
        let ig_name = workspace_ig_name(&project.id, path);
        let tree = CodeTree::open(path.to_path_buf(), ig_name.clone(), graph, self.data.join("linestore").join(&ig_name), self.langs.clone()).await?;
        let report = tree.full_index().await?;
        workspaces.insert(path.to_path_buf(), tree.clone());
        drop(workspaces);
        self.start_watch(project, path.to_path_buf(), tree.clone()).await;
        self.publish(topics::INDEXED, json!({"project_id": project.id, "workspace": path.to_string_lossy(), "files": report.files_changed, "ms": report.total_ms, "full": true}));
        Ok(tree)
    }

    async fn status_of(&self, project_id: &str, tree: &CodeTree) -> Result<ProjectStatus, RpcError> {
        let status = tree.status.lock().await.clone();
        let described = tree.run(&format!("{} MATCH (s:Sym) WHERE s.description IS NOT NULL RETURN count(s)", layer::OBSERVED_READ), Map::new()).await?;
        let count = |rows: &crate::db::Rows| rows.rows.first().and_then(|r| r.first()).and_then(|v| v.as_u64()).unwrap_or(0);
        let embedding = if tree.embedding_ready().await? { "ready" } else { "pending" };
        Ok(ProjectStatus {
            project_id: project_id.to_owned(),
            root: tree.root.to_string_lossy().into_owned(),
            name: String::new(),
            reference: false,
            state: status.state,
            files: status.files,
            symbols: status.symbols,
            edges: status.edges + status.external_symbols,
            described: count(&described),
            last_index_ms: status.last_index_ms,
            languages: status.languages.into_iter().collect(),
            packages: status.packages.into_iter().collect(),
            error: status.error,
            embedding: Some(embedding.into()),
            externals: Some(status.externals_state),
            external_symbols: status.external_symbols,
            language_plugins: status
                .language_plugins
                .into_iter()
                .map(|(language, (plugin, version, files))| LanguagePlugin { language, plugin, version, files })
                .collect(),
            uncovered: {
                let mut uncovered: Vec<UncoveredFiles> =
                    status.uncovered.into_iter().map(|((extension, kind), (files, example))| UncoveredFiles { extension, kind, files, example }).collect();
                uncovered.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.extension.cmp(&b.extension)));
                uncovered
            },
            extract_problem_count: status.problems.len() as u64,
            extract_problems: status
                .problems
                .into_iter()
                .take(EXTRACT_PROBLEMS_SHOWN)
                .map(|(path, plugin, error, indexed_as)| ExtractProblem { path, plugin, error, indexed_as })
                .collect(),
            plugin_problems: status.plugin_problems,
            indexed_files: tree.files_done.load(std::sync::atomic::Ordering::Relaxed),
            total_files: tree.files_total.load(std::sync::atomic::Ordering::Relaxed),
        })
    }

    /// The store notes of `project_id` live in: the global scope (the main project) when it is
    /// absent or empty, else that project's. Writes to a read-only reference are refused.
    async fn note_store(&self, project_id: Option<&str>, write: bool) -> Result<Arc<WorkStore>, RpcError> {
        self.scoped_store(project_id, write, "notes").await
    }

    /// The store of a scope: the global one without a project id, else the project's (a read-only
    /// reference refuses writes of `what`).
    async fn scoped_store(&self, project_id: Option<&str>, write: bool, what: &str) -> Result<Arc<WorkStore>, RpcError> {
        match project_id.map(str::trim).filter(|id| !id.is_empty() && *id != "global") {
            None => self.global().await,
            Some(id) => {
                if write {
                    self.refuse_reference(id, what).await?;
                }
                self.work(id).await
            }
        }
    }

    fn knowledge_changed(&self, item: &KnowledgeItem, action: &str) {
        self.publish(topics::KNOWLEDGE_CHANGED, json!({"project_id": item.project_id, "id": item.id, "kind": item.kind, "state": item.state, "action": action}));
    }

    async fn find_task_store(&self, task_id: &str) -> Result<Arc<WorkStore>, RpcError> {
        if let Some(project) = self.task_owner.lock().await.get(task_id).cloned() {
            return self.work(&project).await;
        }
        for project in self.registered_projects().await? {
            let store = self.work(&project).await?;
            if store.task_get(task_id).await?.is_some() {
                self.task_owner.lock().await.insert(task_id.to_owned(), project);
                return Ok(store);
            }
        }
        Err(RpcError::not_found(format!("task {task_id} not found")))
    }

    pub async fn dispatch(self: Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::PROJECT_OPEN => {
                let p: ProjectOpenParams = parse_params(params)?;
                let what = if p.reference { "reference root" } else { "project root" };
                let root = std::fs::canonicalize(&p.root).map_err(|e| RpcError::invalid_params(format!("{what} {}: {e}", p.root)))?;
                if !root.is_dir() {
                    return Err(RpcError::invalid_params(format!("{what} {} is not a directory", root.display())));
                }
                let project_id = if !p.project_id.is_empty() {
                    p.project_id.clone()
                } else {
                    match self.entries().await?.into_iter().find(|e| Path::new(&e.root) == root) {
                        Some(entry) => entry.id,
                        None => root_project_id(&root),
                    }
                };
                // Opened again: a close still waiting for its background index is withdrawn.
                if let Ok(mut forgotten) = self.forgotten.lock() {
                    forgotten.remove(&project_id);
                }
                let existing = self.projects.read().await.get(&project_id).cloned();
                if p.reference {
                    let entry = self.register(&project_id, &root, Some(true)).await?;
                    if let Some(project) = &existing
                        && project.root == root
                    {
                        return to_value(&self.named_status(&project.id, &project.tree).await?);
                    }
                    if existing.is_some() {
                        self.projects.write().await.remove(&project_id);
                    }
                    self.open_in_background(&project_id, &root);
                    return to_value(&indexing_status(&entry));
                }
                let project = match existing {
                    Some(project) if project.root == root => {
                        self.register(&project_id, &root, Some(false)).await?;
                        project
                    }
                    Some(_) => {
                        self.projects.write().await.remove(&project_id);
                        self.open_project(&project_id, &root, Some(false)).await?
                    }
                    None => self.open_project(&project_id, &root, Some(false)).await?,
                };
                to_value(&self.named_status(&project.id, &project.tree).await?)
            }
            methods::PROJECT_LIST => to_value(&self.list_projects().await?),
            methods::PROJECT_CLOSE => {
                let p: ProjectCloseParams = parse_params(params)?;
                to_value(&self.close_reference(&p.project_id).await?)
            }
            methods::PROJECT_STATUS => {
                let scope: Scope = parse_params(params)?;
                if let Some(status) = self.background_status(&scope).await? {
                    return to_value(&status);
                }
                let tree = self.tree(&scope).await?;
                to_value(&self.named_status(&scope.project_id, &tree).await?)
            }
            methods::REFRESH => {
                let scope: Scope = parse_params(params.clone())?;
                let paths: Vec<String> = params.get("paths").and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                let tree = self.tree(&scope).await?;
                let report = if paths.is_empty() {
                    tree.full_index().await?
                } else {
                    let abs: Vec<PathBuf> = paths.iter().map(|p| if Path::new(p).is_absolute() { PathBuf::from(p) } else { tree.root.join(p) }).collect();
                    let rels = expand_paths(&tree.root, &abs, &tree).await;
                    tree.refresh_paths(&rels).await?
                };
                if report.files_changed + report.files_deleted > 0 {
                    self.publish(topics::INDEXED, indexed_payload(&scope.project_id, scope.workspace.as_deref(), &report));
                }
                to_value(&report)
            }
            methods::WORKSPACE_ATTACH => {
                let p: WorkspaceParams = parse_params(params)?;
                let project = self.project(&p.project_id).await?;
                let path = canonical(&p.workspace)?;
                let started = Instant::now();
                let tree = if path == project.root { project.tree.clone() } else { self.attach(&project, &path).await? };
                let mut status = self.status_of(&p.project_id, &tree).await?;
                status.last_index_ms = started.elapsed().as_millis() as u64;
                to_value(&status)
            }
            methods::WORKSPACE_DETACH => {
                let p: WorkspaceParams = parse_params(params)?;
                let project = self.project(&p.project_id).await?;
                let path = std::fs::canonicalize(&p.workspace).unwrap_or_else(|_| PathBuf::from(&p.workspace));
                project.watchers.lock().await.remove(&path);
                let removed = project.workspaces.lock().await.remove(&path);
                if let Some(tree) = removed {
                    let graph = self.graph().await?;
                    graph.drop_project(&tree.ig_name).await?;
                    let _ = std::fs::remove_dir_all(tree.lines.dir());
                }
                Ok(json!({"detached": true}))
            }
            methods::SEARCH => {
                let p: SearchParams = parse_params(params.clone())?;
                let tree = self.tree(&p.scope).await?;
                let external = p.kinds.iter().any(|k| k == "external_symbol");
                to_value(&tree.search(&p.text, &p.kinds, p.limit, external).await?)
            }
            methods::SYMBOL => {
                let p: SymbolParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                to_value(&tree.symbol(&p.name).await?)
            }
            methods::CALLERS | methods::CALLEES => {
                let p: EdgesParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                to_value(&tree.call_sites(&p.sid, p.depth, method == methods::CALLERS).await?)
            }
            methods::IMPACT => {
                let p: ImpactParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                to_value(&tree.impact(&p.sids, &p.paths, p.depth).await?)
            }
            methods::PATH => {
                let p: PathParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                to_value(&tree.path(&p.from, &p.to, p.max_hops).await?)
            }
            methods::OUTLINE => {
                let p: OutlineParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                let path = relative(&tree.root, &p.path);
                to_value(&tree.outline(&path).await?)
            }
            methods::LINES => {
                let p: LinesParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                let path = relative(&tree.root, &p.path);
                to_value(&tree.lines(&path, p.start, p.end).await?)
            }
            methods::REPO_MAP => {
                let p: RepoMapParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                let focus: Vec<String> = p.focus.iter().map(|f| relative(&tree.root, f)).collect();
                to_value(&tree.repo_map(&focus, p.budget_tokens).await?)
            }
            methods::FILES => {
                let p: FilesParams = parse_params(params)?;
                let tree = self.tree(&p.scope).await?;
                to_value(&tree.files().await?)
            }
            methods::SKILLS => to_value(&crate::skills::describe(self.plugin_env()?)?),
            methods::SKILL => to_value(&crate::skills::skill(self.plugin_env()?, parse_params(params)?)?),
            crate::queries::QUERY => {
                let p: crate::queries::QueryParams = parse_params(params.clone())?;
                if p.project_id == "global" || p.project_id == MAIN_PROJECT {
                    let global = self.global().await?;
                    return to_value(&crate::queries::read_only_query(&global.graph, &global.ig_id, &p.cypher, p.params).await?);
                }
                let scope = Scope { project_id: p.project_id.clone(), workspace: params.get("workspace").and_then(|w| w.as_str()).map(str::to_owned) };
                let tree = self.tree(&scope).await?;
                to_value(&tree.query(&p.cypher, p.params).await?)
            }
            methods::DESCRIBE => {
                let p: DescribeParams = parse_params(params)?;
                let project = self.project(&p.project_id).await?;
                to_value(&project.tree.describe(&p.sid, &p.description).await?)
            }
            methods::UNDESCRIBED => {
                let p: UndescribedParams = parse_params(params)?;
                let project = self.project(&p.project_id).await?;
                to_value(&project.tree.undescribed(p.limit).await?)
            }
            methods::TASK_CREATE => {
                let p: TaskCreateParams = parse_params(params)?;
                self.refuse_reference(&p.project_id, "tasks").await?;
                let store = self.work(&p.project_id).await?;
                let task = store.task_create(p).await?;
                self.task_owner.lock().await.insert(task.id.clone(), task.project_id.clone());
                self.publish(topics::TASK_CHANGED, json!({"task": task, "change": "created"}));
                to_value(&task)
            }
            methods::TASK_UPDATE => {
                let p: TaskUpdateParams = parse_params(params)?;
                let store = self.find_task_store(&p.id).await?;
                let id = p.id.clone();
                let task = store.task_update(p).await?.ok_or_else(|| RpcError::not_found(format!("task {id} not found")))?;
                self.publish(topics::TASK_CHANGED, json!({"task": task, "change": "updated"}));
                to_value(&task)
            }
            methods::TASK_GET => {
                let p: TaskGetParams = parse_params(params)?;
                let store = self.find_task_store(&p.id).await?;
                let task = store.task_get(&p.id).await?.ok_or_else(|| RpcError::not_found(format!("task {} not found", p.id)))?;
                to_value(&task)
            }
            methods::TASK_LIST => {
                let p: TaskListParams = parse_params(params)?;
                let store = self.work(&p.project_id).await?;
                to_value(&TaskListResult { tasks: store.task_list(p.status.as_deref(), p.parent_id.as_deref()).await? })
            }
            methods::BOARD_POST => {
                let p: BoardPostParams = parse_params(params)?;
                self.refuse_reference(&p.project_id, "board posts").await?;
                let store = self.work(&p.project_id).await?;
                let message = store.board_post(p).await?;
                self.publish(topics::BOARD_POSTED, json!({"message": message}));
                to_value(&message)
            }
            methods::BOARD_LIST => {
                let p: BoardListParams = parse_params(params)?;
                let store = self.work(&p.project_id).await?;
                to_value(&BoardListResult { messages: store.board_list(&p).await? })
            }
            methods::AGENT_UPSERT => {
                let record: AgentRecord = parse_params(params)?;
                self.refuse_reference(&record.project_id, "agents").await?;
                let store = self.work(&record.project_id).await?;
                to_value(&store.agent_upsert(record).await?)
            }
            methods::AGENT_LIST => {
                let p: AgentListParams = parse_params(params)?;
                let store = self.work(&p.project_id).await?;
                to_value(&AgentListResult { agents: store.agent_list().await? })
            }
            methods::RULE_UPSERT => {
                let rule: Rule = parse_params(params)?;
                self.refuse_reference(&rule.scope, "rules").await?;
                let store = self.work(&rule.scope).await?;
                to_value(&store.rule_upsert(rule).await?)
            }
            methods::RULE_LIST => {
                let p: RuleListParams = parse_params(params)?;
                let store = self.work(&p.scope).await?;
                to_value(&RuleListResult { rules: store.rule_list().await? })
            }
            methods::STYLE_UPSERT => {
                let entry: StyleEntry = parse_params(params)?;
                self.refuse_reference(&entry.scope, "style entries").await?;
                let store = self.work(&entry.scope).await?;
                to_value(&store.style_upsert(entry).await?)
            }
            methods::STYLE_LIST => {
                let p: StyleListParams = parse_params(params)?;
                let store = self.work(&p.scope).await?;
                to_value(&StyleListResult { entries: store.style_list().await? })
            }
            methods::RUN_RECORD => {
                let p: RunRecordParams = parse_params(params)?;
                self.refuse_reference(&p.project_id, "runs").await?;
                let store = self.work(&p.project_id).await?;
                to_value(&RunRecordResult { id: store.run_record(p).await? })
            }
            methods::SETTINGS_GET => {
                let p: SettingsGetParams = parse_params(params)?;
                to_value(&self.settings().await?.get(&p.scope, &p.path).await?)
            }
            methods::SETTINGS_SET => {
                let p: SettingsSetParams = parse_params(params)?;
                let stored = self.settings().await?.set(&p.scope, &p.path, p.value, &p.by, p.description).await?;
                self.publish(topics::SETTINGS_CHANGED, json!({"scope": p.scope, "path": stored.path, "value": stored.value, "by": stored.updated_by}));
                to_value(&stored)
            }
            methods::SETTINGS_LIST => {
                let p: SettingsListParams = parse_params(params)?;
                let settings = self.settings().await?.list(&p.scope, &p.prefix).await?;
                to_value(&SettingsListResult { prefix: p.prefix, settings })
            }
            methods::ENGINE_STATUS => to_value(&self.engine.status()),
            methods::APP_PROJECTS => to_value(&AppProjectsResult { projects: self.app().await?.projects().await? }),
            methods::APP_PROJECT_PUT => {
                let p: AppProjectPutParams = parse_params(params)?;
                self.app().await?.put_project(&p.project).await?;
                Ok(json!({}))
            }
            methods::APP_PROJECT_DELETE => {
                let p: AppProjectDeleteParams = parse_params(params)?;
                self.app().await?.delete_project(&p.id).await?;
                Ok(json!({}))
            }
            methods::APP_SESSIONS => to_value(&AppSessionsResult { sessions: self.app().await?.sessions().await? }),
            methods::APP_SESSION_PUT => {
                let p: AppSessionPutParams = parse_params(params)?;
                self.app().await?.put_session(&p.session).await?;
                Ok(json!({}))
            }
            methods::APP_SESSIONS_DELETE => {
                let p: AppSessionsDeleteParams = parse_params(params)?;
                self.app().await?.delete_sessions(&p.ids).await?;
                Ok(json!({}))
            }
            methods::JOURNAL_APPEND => {
                let p: JournalAppendParams = parse_params(params)?;
                to_value(&JournalAppendResult { next: self.app().await?.journal_append(&p.session_id, &p.patches).await? })
            }
            methods::JOURNAL_READ => {
                let p: JournalReadParams = parse_params(params)?;
                let (patches, next, end) = self.app().await?.journal_read(&p.session_id, p.from, p.max).await?;
                to_value(&JournalReadResult { patches, next, end })
            }
            methods::JOURNAL_DROP => {
                let p: JournalDropParams = parse_params(params)?;
                self.app().await?.journal_drop(&p.session_id).await?;
                Ok(json!({}))
            }
            methods::NOTE_ADD => {
                let p: NoteAddParams = parse_params(params)?;
                let store = self.note_store(p.project_id.as_deref(), true).await?;
                to_value(&store.note_add(p).await?)
            }
            methods::NOTE_SEARCH => {
                let p: NoteSearchParams = parse_params(params)?;
                let store = self.note_store(p.project_id.as_deref(), false).await?;
                to_value(&store.note_search(&p.text, p.topic.as_deref(), p.limit).await?)
            }
            methods::NOTE_LIST => {
                let p: NoteListParams = parse_params(params)?;
                let store = self.note_store(p.project_id.as_deref(), false).await?;
                to_value(&NoteListResult { notes: store.note_list(p.topic.as_deref()).await? })
            }
            methods::NOTE_REMOVE => {
                let p: NoteRemoveParams = parse_params(params)?;
                let store = self.note_store(p.project_id.as_deref(), true).await?;
                to_value(&NoteRemoveResult { removed: store.note_remove(&p.id).await? })
            }
            methods::KNOWLEDGE_UPSERT => {
                let p: KnowledgeUpsertParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), true, "knowledge").await?;
                let item = store.knowledge_upsert(p).await?;
                self.knowledge_changed(&item, "upsert");
                to_value(&item)
            }
            methods::KNOWLEDGE_GET => {
                let p: KnowledgeGetParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), false, "knowledge").await?;
                let item = store.knowledge_get(&p.id).await?.ok_or_else(|| RpcError::not_found(format!("knowledge item {} not found", p.id)))?;
                to_value(&item)
            }
            methods::KNOWLEDGE_LIST => {
                let p: KnowledgeListParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), false, "knowledge").await?;
                to_value(&KnowledgeListResult { items: store.knowledge_list(p.kind.as_deref(), p.state.as_deref(), p.include_history).await? })
            }
            methods::KNOWLEDGE_SEARCH => {
                let p: KnowledgeSearchParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), false, "knowledge").await?;
                to_value(&store.knowledge_search(&p.text, p.kind.as_deref(), p.limit).await?)
            }
            methods::KNOWLEDGE_LINK | methods::KNOWLEDGE_UNLINK => {
                let p: KnowledgeLinkParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), true, "knowledge").await?;
                let item = if method == methods::KNOWLEDGE_LINK { store.knowledge_link(&p.id, &p.rel, &p.target).await? } else { store.knowledge_unlink(&p.id, &p.rel, &p.target).await? };
                self.knowledge_changed(&item, if method == methods::KNOWLEDGE_LINK { "link" } else { "unlink" });
                to_value(&item)
            }
            methods::KNOWLEDGE_COUNT => {
                let p: KnowledgeCountParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), true, "knowledge").await?;
                let item = store.knowledge_count(&p.id, p.seen, p.helped, p.hurt).await?;
                self.knowledge_changed(&item, "count");
                to_value(&item)
            }
            methods::KNOWLEDGE_WITHDRAW => {
                let p: KnowledgeWithdrawParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), true, "knowledge").await?;
                let item = store.knowledge_withdraw(&p.id, &p.reason, &p.by).await?;
                self.knowledge_changed(&item, "withdraw");
                to_value(&item)
            }
            methods::KNOWLEDGE_SUPERSEDE => {
                let p: KnowledgeSupersedeParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), true, "knowledge").await?;
                let item = store.knowledge_supersede(&p.id, &p.by).await?;
                self.knowledge_changed(&item, "supersede");
                to_value(&item)
            }
            methods::KNOWLEDGE_LINKED => {
                let p: KnowledgeLinkedParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), false, "knowledge").await?;
                to_value(&store.knowledge_linked(&p.sids, &p.paths).await?)
            }
            methods::KNOWLEDGE_BRIEF => {
                let p: KnowledgeBriefParams = parse_params(params)?;
                let store = self.scoped_store(p.project_id.as_deref(), false, "knowledge").await?;
                to_value(&store.knowledge_brief(p.budget_tokens, self.langs.host()).await?)
            }
            methods::SETTINGS_DELETE => {
                let p: SettingsDeleteParams = parse_params(params.clone())?;
                let by = params.get("by").and_then(|b| b.as_str()).unwrap_or("").to_owned();
                let deleted = self.settings().await?.delete(&p.scope, &p.path).await?;
                for path in &deleted {
                    self.publish(topics::SETTINGS_CHANGED, json!({"scope": p.scope, "path": path, "value": null, "by": by}));
                }
                Ok(json!({"deleted": deleted}))
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    /// Real self-test: a temporary IronGraph project, a tiny two-language tree, queries over it.
    pub async fn self_test(self: Arc<Self>) -> Result<ProbeReport, RpcError> {
        let mut checks = Vec::new();
        let started = Instant::now();
        let graph = match self.graph().await {
            Ok(graph) => graph,
            Err(error) => {
                checks.push(ProbeCheck { name: "irongraph.open".into(), ok: false, detail: error.message, ms: started.elapsed().as_millis() as u64 });
                return Ok(ProbeReport::from_checks(checks));
            }
        };
        checks.push(ProbeCheck { name: "irongraph.open".into(), ok: true, detail: self.irongraph_dir().display().to_string(), ms: started.elapsed().as_millis() as u64 });
        let engine = self.engine.status();
        checks.push(ProbeCheck {
            name: "irongraph.embedding_model".into(),
            ok: engine.state == "ready",
            detail: format!("{}@{} in {} ({})", engine.model_id, engine.revision, engine.model_dir, engine.source.as_deref().unwrap_or("?")),
            ms: engine.install_ms.unwrap_or(0),
        });
        let skills = self.plugin_env().and_then(crate::skills::describe);
        checks.push(ProbeCheck {
            name: "skills".into(),
            ok: skills.as_ref().is_ok_and(|description| !description.skills.is_empty()),
            detail: match &skills {
                Ok(description) => description.skills.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>().join(", "),
                Err(error) => error.message.clone(),
            },
            ms: 0,
        });
        let dir = std::env::temp_dir().join(format!("cc-memory-probe-{}", uuid::Uuid::new_v4().simple()));
        let outcome = self.probe_tree(&graph, &dir, &mut checks).await;
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(error) = outcome {
            checks.push(ProbeCheck { name: "probe".into(), ok: false, detail: error.message, ms: started.elapsed().as_millis() as u64 });
        }
        Ok(ProbeReport::from_checks(checks))
    }

    async fn probe_tree(&self, graph: &Arc<Graph>, dir: &Path, checks: &mut Vec<ProbeCheck>) -> Result<(), RpcError> {
        let io = |e: std::io::Error| RpcError::internal(e.to_string());
        std::fs::create_dir_all(dir.join("src")).map_err(io)?;
        let rust = "/// Adds two numbers.\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn twice(x: u32) -> u32 {\n    add(x, x)\n}\n";
        std::fs::write(dir.join("src/lib.rs"), rust).map_err(io)?;
        std::fs::write(dir.join("tool.py"), "def greet(name, punctuation=\"!\"):\n    return name + punctuation\n").map_err(io)?;
        let ig_name = format!("cc_probe_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let t = Instant::now();
        let tree = CodeTree::open(dir.to_path_buf(), ig_name.clone(), graph.clone(), dir.join(".linestore"), self.langs.clone()).await?;
        let result = async {
            let report = tree.full_index().await?;
            checks.push(ProbeCheck { name: "index".into(), ok: report.files_changed == 2, detail: format!("{} files, {} statements", report.files_changed, report.statements), ms: t.elapsed().as_millis() as u64 });
            // The language plugins start beside this one: a check whose plugin is not active yet
            // says so (the files are indexed as text and extracted once it is).
            let snapshot = self.langs.snapshot().await;
            let not_active = |path: &str| -> Option<String> {
                snapshot.by_name(path).is_none().then(|| format!("skipped: no active language plugin covers {path} yet ({})", if snapshot.plugins.is_empty() { "none is active".to_owned() } else { snapshot.plugins.iter().map(|p| p.category.as_str()).collect::<Vec<_>>().join(", ") }))
            };
            let t = Instant::now();
            match not_active("src/lib.rs") {
                Some(detail) => checks.push(ProbeCheck { name: "callers".into(), ok: true, detail, ms: 0 }),
                None => {
                    let sites = tree.call_sites("add", 1, true).await?;
                    let ok = sites.sites.iter().any(|s| s.caller.name == "twice" && s.args == vec!["x".to_owned(), "x".to_owned()] && s.line == 7);
                    checks.push(ProbeCheck { name: "callers".into(), ok, detail: format!("{} call sites", sites.sites.len()), ms: t.elapsed().as_millis() as u64 });
                }
            }
            let t = Instant::now();
            match not_active("tool.py") {
                Some(detail) => checks.push(ProbeCheck { name: "symbol.params".into(), ok: true, detail, ms: 0 }),
                None => {
                    let symbol = tree.symbol("greet").await?;
                    let ok = symbol.matches.first().is_some_and(|m| m.params.get(1).is_some_and(|p| p.default_value.as_deref() == Some("\"!\"")));
                    checks.push(ProbeCheck { name: "symbol.params".into(), ok, detail: format!("{} matches", symbol.matches.len()), ms: t.elapsed().as_millis() as u64 });
                }
            }
            let t = Instant::now();
            let lines = tree.lines("src/lib.rs", None, None).await?;
            let expected: Vec<String> = cc_protocol::hashline::file_lines(rust).iter().map(|l| cc_protocol::hashline::tag(l)).collect();
            let ok = lines.lines.iter().map(|l| l.tag.clone()).collect::<Vec<_>>() == expected;
            checks.push(ProbeCheck { name: "line_store".into(), ok, detail: format!("{} lines", lines.total), ms: t.elapsed().as_millis() as u64 });
            Ok::<(), RpcError>(())
        }
        .await;
        let dropped = graph.drop_project(&ig_name).await;
        result?;
        dropped
    }

    pub async fn close(&self) {
        self.shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
        let projects: Vec<Arc<Project>> = self.projects.write().await.drain().map(|(_, p)| p).collect();
        for project in &projects {
            project.watchers.lock().await.clear();
        }
        drop(projects);
        if let Some(graph) = self.graph.get()
            && let Err(error) = graph.flush().await {
                tracing::warn!(%error, "irongraph flush failed");
            }
    }
}

/// The answer to a reference open whose index runs in the background.
fn indexing_status(entry: &Entry) -> ProjectStatus {
    ProjectStatus {
        project_id: entry.id.clone(),
        root: entry.root.clone(),
        name: entry.name.clone(),
        reference: entry.reference,
        state: "indexing".into(),
        files: 0,
        symbols: 0,
        edges: 0,
        described: 0,
        last_index_ms: 0,
        languages: Vec::new(),
        packages: Vec::new(),
        error: None,
        embedding: None,
        externals: None,
        external_symbols: 0,
        language_plugins: Vec::new(),
        uncovered: Vec::new(),
        extract_problems: Vec::new(),
        extract_problem_count: 0,
        plugin_problems: Vec::new(),
        indexed_files: 0,
        total_files: 0,
    }
}

fn relative(root: &Path, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if let Ok(rel) = canonical.strip_prefix(root).or_else(|_| p.strip_prefix(root)) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.trim_start_matches("./").to_owned()
}

fn indexed_payload(project_id: &str, workspace: Option<&str>, report: &SyncReport) -> Value {
    json!({
        "project_id": project_id,
        "workspace": workspace,
        "files": report.files_changed + report.files_deleted,
        "ms": report.total_ms,
        "paths": report.changed_paths,
        "renames": report.renames,
        "report": report,
    })
}

/// Turn changed absolute paths (files or directories) into repository-relative file paths: a
/// directory expands to its supported files and to indexed files under it that are gone.
async fn expand_paths(root: &Path, paths: &[PathBuf], tree: &CodeTree) -> Vec<String> {
    let model = tree.model.read().await;
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        let rel: PathBuf = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => match std::fs::canonicalize(path).ok().and_then(|c| c.strip_prefix(root).ok().map(Path::to_path_buf)) {
                Some(r) => r,
                None => continue,
            },
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        if path.is_dir() {
            // Ignored directories (build output, dependencies, .gitignore-d) are not walked.
            if !tree::included(root, &format!("{rel}/.")) {
                continue;
            }
            for file in tree::scan(path) {
                out.push(format!("{rel}/{file}"));
            }
            let prefix = format!("{rel}/");
            out.extend(model.files.keys().filter(|k| k.starts_with(&prefix) && !root.join(k).exists()).cloned());
        } else if path.exists() {
            out.push(rel);
        } else {
            let prefix = format!("{rel}/");
            let under: Vec<String> = model.files.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            if under.is_empty() {
                out.push(rel);
            } else {
                out.extend(under);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The plugin object handed to the runtime.
pub struct MemoryPlugin {
    pub inner: Arc<Memory>,
}

impl Plugin for MemoryPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.inner.attach_ctx(&ctx);
        let this = self.inner.clone();
        Box::pin(async move { this.dispatch(&call.method, call.params).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.inner.attach_ctx(&ctx);
        let this = self.inner.clone();
        Box::pin(async move { this.self_test().await })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.inner.attach_ctx(&ctx);
        let this = self.inner.clone();
        tokio::spawn(async move { this.open_frame_projects().await });
        crate::skills::watch(ctx);
        Box::pin(async { Ok(()) })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        self.inner.attach_ctx(&ctx);
        let this = self.inner.clone();
        // Re-extraction can take a while: never hold the connection's event delivery.
        tokio::spawn(async move { this.on_event(&topic, data).await });
        Box::pin(async {})
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        let this = self.inner.clone();
        Box::pin(async move { this.close().await })
    }
}
