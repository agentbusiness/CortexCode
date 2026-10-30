//! Projects, sessions and the workbench's slot state. The memory plugin's main store keeps them
//! (see [`crate::store`]): they are loaded from it when it first serves, held here while the frame
//! runs, and every change is written through the store's outbox in the order it was made.
//!
//! The store is the only record: a deleted project stays there retired (`memory.app.deleted`), so a
//! plugin that was not running when it was deleted hears of it when it starts (`project.deleted`
//! again, see `Frame::set_route`). The file earlier frames kept a copy in
//! (`<home>/state/records.json`) is read once, what the store lacks is written to it, and the file
//! is removed.

use crate::store::{Op, Outbox};
use cc_protocol::contracts::frame::{
    ProjectDeleted,
    flows, Project, Session, SessionCreateParams, SessionStatus, SessionUpdateParams, UiStateDocument,
};
use cc_protocol::contracts::now_ms;
use cc_protocol::RpcError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct StateStore {
    outbox: Outbox,
    inner: Mutex<Inner>,
    ui_state: Mutex<UiStateDocument>,
    /// The copy earlier frames kept (`<home>/state/records.json`), read once and removed.
    legacy: PathBuf,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Inner {
    projects: BTreeMap<String, Project>,
    sessions: BTreeMap<String, Session>,
    /// Projects deleted, by id: told again to plugins that start later (dropped when the project is
    /// opened again).
    #[serde(default)]
    deleted: BTreeMap<String, ProjectDeleted>,
}

impl StateStore {
    /// Empty until [`StateStore::load`] fills it from the store.
    pub fn open(outbox: Outbox, legacy: PathBuf) -> StateStore {
        StateStore {
            outbox,
            ui_state: Mutex::new(UiStateDocument { document: serde_json::Value::Object(serde_json::Map::new()), rev: 0, updated_ms: 0 }),
            inner: Mutex::new(Inner::default()),
            legacy,
        }
    }

    /// The projects deleted, as their `project.deleted` events carried them.
    pub fn deleted_projects(&self) -> Vec<ProjectDeleted> {
        self.lock().deleted.values().cloned().collect()
    }

    /// The records the store holds, when it first serves. A record made before is kept (none is:
    /// calls on the records wait until this load is done, see `Frame::records_ready`).
    pub fn load(&self, projects: Vec<Project>, sessions: Vec<Session>, ui_state: Option<UiStateDocument>, deleted: Vec<ProjectDeleted>) {
        let mut inner = self.lock();
        for project in projects {
            inner.projects.entry(project.id.clone()).or_insert(project);
        }
        for session in sessions {
            inner.sessions.entry(session.id.clone()).or_insert(session);
        }
        for project in deleted {
            if !inner.projects.contains_key(&project.project_id) {
                inner.deleted.insert(project.project_id.clone(), project);
            }
        }
        // The copy an earlier frame kept: what it has and the store lacks goes into the store (a
        // session it has newer wins), then the file goes.
        let copy: Inner = std::fs::read(&self.legacy).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let mut restored = 0usize;
        for (id, project) in copy.projects {
            if !inner.projects.contains_key(&id) && !inner.deleted.contains_key(&id) {
                let _ = self.outbox.send(Op::PutProject(project.clone()));
                inner.projects.insert(id, project);
                restored += 1;
            }
        }
        for (id, session) in copy.sessions {
            let newer = inner.sessions.get(&id).is_none_or(|stored| stored.updated_ms < session.updated_ms);
            if newer && inner.projects.contains_key(&session.project_id) {
                let _ = self.outbox.send(Op::PutSession(session.clone()));
                inner.sessions.insert(id, session);
                restored += 1;
            }
        }
        for (id, deleted) in copy.deleted {
            if !inner.projects.contains_key(&id) && !inner.deleted.contains_key(&id) {
                // The store keeps a deleted project retired: put, then delete.
                let name = std::path::Path::new(&deleted.root).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let _ = self.outbox.send(Op::PutProject(Project { id: id.clone(), root: deleted.root.clone(), name, created_ms: 0, flows: Vec::new() }));
                let _ = self.outbox.send(Op::DeleteProject(id.clone()));
                inner.deleted.insert(id, deleted);
                restored += 1;
            }
        }
        if restored > 0 {
            tracing::info!(restored, "records only the frame's old copy had were written to the store");
        }
        if self.legacy.exists() {
            match std::fs::remove_file(&self.legacy) {
                Ok(()) => tracing::info!(path = %self.legacy.display(), "the frame's old copy of the records is in the store now; file removed"),
                Err(error) => tracing::warn!(path = %self.legacy.display(), %error, "cannot remove the frame's old copy of the records"),
            }
        }
        drop(inner);
        if let Some(document) = ui_state {
            let mut state = self.ui_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.rev == 0 {
                *state = document;
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Open (or re-open) a project by root; a new one takes `flows` (the flows installed now).
    /// Returns the project and whether it is new.
    pub fn open_project(&self, root: &str, flows: Vec<String>) -> Result<(Project, bool), RpcError> {
        let canonical = std::fs::canonicalize(root)
            .map_err(|error| RpcError::invalid_params(format!("project root `{root}`: {error}")))?;
        if !canonical.is_dir() {
            return Err(RpcError::invalid_params(format!("project root `{root}` is not a directory")));
        }
        let root = canonical.to_string_lossy().into_owned();
        let id = project_id(&root);
        let mut inner = self.lock();
        if let Some(existing) = inner.projects.get(&id) {
            return Ok((existing.clone(), false));
        }
        let name = canonical.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.clone());
        let project = Project { id: id.clone(), root, name, created_ms: now_ms(), flows };
        self.outbox.send(Op::PutProject(project.clone()))?;
        inner.deleted.remove(&id);
        inner.projects.insert(id, project.clone());
        Ok((project, true))
    }

    pub fn projects(&self) -> Vec<Project> {
        let mut projects: Vec<Project> = self.lock().projects.values().cloned().collect();
        projects.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then_with(|| a.id.cmp(&b.id)));
        projects
    }

    pub fn project(&self, id: &str) -> Result<Project, RpcError> {
        self.lock().projects.get(id).cloned().ok_or_else(|| RpcError::not_found(format!("no project `{id}`")))
    }

    pub fn create_session(&self, params: SessionCreateParams) -> Result<Session, RpcError> {
        let mut inner = self.lock();
        let project = inner
            .projects
            .get(&params.project_id)
            .cloned()
            .ok_or_else(|| RpcError::not_found(format!("no project `{}`", params.project_id)))?;
        let parent_flow = match &params.parent {
            Some(parent) => Some(inner.sessions.get(parent).ok_or_else(|| RpcError::not_found(format!("no parent session `{parent}`")))?.flow.clone()),
            None => None,
        };
        // Its flow: the one asked for, else the parent's when the project takes it, else the
        // project's first.
        let flow = match params.flow {
            Some(flow) if !project.flows.contains(&flow) => {
                return Err(RpcError::invalid_params(format!("the project {} does not take the {flow} flow (it takes: {})", project.name, project.flows.join(", "))));
            }
            Some(flow) => flow,
            None => parent_flow.filter(|flow| project.flows.contains(flow)).or_else(|| project.flows.first().cloned()).unwrap_or_else(flows::default_flow),
        };
        let now = now_ms();
        let session = Session {
            id: format!("s_{}", uuid::Uuid::new_v4().simple()),
            project_id: project.id,
            kind: params.kind,
            title: params.title,
            agent: params.agent,
            parent: params.parent,
            workspace: params.workspace.unwrap_or(project.root),
            task_id: params.task_id,
            status: SessionStatus::Idle,
            created_ms: now,
            updated_ms: now,
            model: None,
            flow,
        };
        self.outbox.send(Op::PutSession(session.clone()))?;
        inner.sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub fn sessions(&self, project_id: Option<&str>) -> Vec<Session> {
        let mut sessions: Vec<Session> = self
            .lock()
            .sessions
            .values()
            .filter(|s| project_id.is_none_or(|id| s.project_id == id))
            .cloned()
            .collect();
        sessions.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then_with(|| a.id.cmp(&b.id)));
        sessions
    }

    pub fn session(&self, id: &str) -> Result<Session, RpcError> {
        self.lock().sessions.get(id).cloned().ok_or_else(|| RpcError::not_found(format!("no session `{id}`")))
    }

    pub fn update_session(&self, params: SessionUpdateParams) -> Result<Session, RpcError> {
        let mut inner = self.lock();
        let previous =
            inner.sessions.get(&params.id).cloned().ok_or_else(|| RpcError::not_found(format!("no session `{}`", params.id)))?;
        let mut session = previous.clone();
        if let Some(title) = params.title {
            session.title = title;
        }
        if let Some(status) = params.status {
            session.status = status;
        }
        if let Some(task_id) = params.task_id {
            session.task_id = Some(task_id);
        }
        if let Some(model) = params.model {
            session.model = Some(model);
        }
        session.updated_ms = now_ms().max(previous.updated_ms);
        self.outbox.send(Op::PutSession(session.clone()))?;
        inner.sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    /// Remove a session and every session started from it (children first). Returns them.
    pub fn delete_session(&self, id: &str) -> Result<Vec<Session>, RpcError> {
        let mut inner = self.lock();
        if !inner.sessions.contains_key(id) {
            return Err(RpcError::not_found(format!("no session `{id}`")));
        }
        let doomed = descendants(&inner.sessions, &[id.to_owned()]);
        self.outbox.send(Op::DeleteSessions(doomed.clone()))?;
        let removed: Vec<Session> = doomed.iter().filter_map(|id| inner.sessions.remove(id)).collect();
        Ok(removed)
    }

    /// Remove a project and all of its sessions (children first). Returns them.
    pub fn delete_project(&self, id: &str) -> Result<(Project, Vec<Session>), RpcError> {
        let mut inner = self.lock();
        let project = inner.projects.get(id).cloned().ok_or_else(|| RpcError::not_found(format!("no project `{id}`")))?;
        let own: Vec<String> = inner.sessions.values().filter(|session| session.project_id == id).map(|session| session.id.clone()).collect();
        let doomed = descendants(&inner.sessions, &own);
        // Sessions first: a project without its record but with sessions left would be worse than
        // the reverse, which the next delete finishes.
        self.outbox.send(Op::DeleteSessions(doomed.clone()))?;
        self.outbox.send(Op::DeleteProject(project.id.clone()))?;
        let removed: Vec<Session> = doomed.iter().filter_map(|id| inner.sessions.remove(id)).collect();
        inner.projects.remove(id);
        inner.deleted.insert(project.id.clone(), ProjectDeleted { project_id: project.id.clone(), root: project.root.clone() });
        Ok((project, removed))
    }

    /// `frame.ui_state.get`.
    pub fn ui_state(&self) -> UiStateDocument {
        self.ui_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// `frame.ui_state.put`: replace the document (a JSON object); returns the new revision.
    pub fn put_ui_state(&self, document: serde_json::Value) -> Result<u64, RpcError> {
        if !document.is_object() {
            return Err(RpcError::invalid_params("the slot state document must be a JSON object"));
        }
        let mut state = self.ui_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = UiStateDocument { document, rev: state.rev + 1, updated_ms: now_ms() };
        self.outbox.send(Op::UiState(next.clone()))?;
        *state = next;
        Ok(state.rev)
    }

    /// Mutating frame calls acknowledge only after the ordered store outbox is durable.
    pub async fn flush(&self) -> Result<(), RpcError> {
        self.outbox.flush().await
    }
}

/// `roots` and every session started from one of them, transitively, children before parents.
fn descendants(sessions: &BTreeMap<String, Session>, roots: &[String]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<(String, bool)> = roots.iter().rev().map(|id| (id.clone(), false)).collect();
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            order.push(id);
            continue;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        stack.push((id.clone(), true));
        for child in sessions.values().filter(|session| session.parent.as_deref() == Some(id.as_str())) {
            stack.push((child.id.clone(), false));
        }
    }
    order
}

/// Stable project id: a hash of the canonical root path.
pub fn project_id(canonical_root: &str) -> String {
    let digest = Sha256::digest(canonical_root.as_bytes());
    format!("p_{}", &hex::encode(digest)[..16])
}

