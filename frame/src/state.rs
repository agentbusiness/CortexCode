//! Projects, sessions and the workbench's slot state. The memory plugin's main store keeps them
//! (see [`crate::store`]): they are loaded from it when it first serves, held here while the frame
//! runs, and every change is written through the store's outbox in the order it was made.

use crate::store::{Op, Outbox};
use cc_protocol::contracts::frame::{
    Project, Session, SessionCreateParams, SessionStatus, SessionUpdateParams, UiStateDocument,
};
use cc_protocol::contracts::now_ms;
use cc_protocol::RpcError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub struct StateStore {
    outbox: Outbox,
    inner: Mutex<Inner>,
    ui_state: Mutex<UiStateDocument>,
}

struct Inner {
    projects: BTreeMap<String, Project>,
    sessions: BTreeMap<String, Session>,
}

impl StateStore {
    /// Empty until [`StateStore::load`] fills it from the store.
    pub fn open(outbox: Outbox) -> StateStore {
        StateStore {
            outbox,
            ui_state: Mutex::new(UiStateDocument { document: serde_json::Value::Object(serde_json::Map::new()), rev: 0, updated_ms: 0 }),
            inner: Mutex::new(Inner { projects: BTreeMap::new(), sessions: BTreeMap::new() }),
        }
    }

    /// The records the store holds, when it first serves. A record made before (none is, since the
    /// UI starts after the store) is kept.
    pub fn load(&self, projects: Vec<Project>, sessions: Vec<Session>, ui_state: Option<UiStateDocument>) {
        let mut inner = self.lock();
        for project in projects {
            inner.projects.entry(project.id.clone()).or_insert(project);
        }
        for session in sessions {
            inner.sessions.entry(session.id.clone()).or_insert(session);
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

    /// Open (or re-open) a project by root. Returns the project and whether it is new.
    pub fn open_project(&self, root: &str) -> Result<(Project, bool), RpcError> {
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
        let project = Project { id: id.clone(), root, name, created_ms: now_ms() };
        inner.projects.insert(id, project.clone());
        self.outbox.send(Op::PutProject(project.clone()));
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
        if let Some(parent) = &params.parent
            && !inner.sessions.contains_key(parent)
        {
            return Err(RpcError::not_found(format!("no parent session `{parent}`")));
        }
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
        };
        inner.sessions.insert(session.id.clone(), session.clone());
        self.outbox.send(Op::PutSession(session.clone()));
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
        inner.sessions.insert(session.id.clone(), session.clone());
        self.outbox.send(Op::PutSession(session.clone()));
        Ok(session)
    }

    /// Remove a session and every session started from it (children first). Returns them.
    pub fn delete_session(&self, id: &str) -> Result<Vec<Session>, RpcError> {
        let mut inner = self.lock();
        if !inner.sessions.contains_key(id) {
            return Err(RpcError::not_found(format!("no session `{id}`")));
        }
        let doomed = descendants(&inner.sessions, &[id.to_owned()]);
        let removed: Vec<Session> = doomed.iter().filter_map(|id| inner.sessions.remove(id)).collect();
        self.outbox.send(Op::DeleteSessions(removed.iter().map(|session| session.id.clone()).collect()));
        Ok(removed)
    }

    /// Remove a project and all of its sessions (children first). Returns them.
    pub fn delete_project(&self, id: &str) -> Result<(Project, Vec<Session>), RpcError> {
        let mut inner = self.lock();
        let project = inner.projects.get(id).cloned().ok_or_else(|| RpcError::not_found(format!("no project `{id}`")))?;
        let own: Vec<String> = inner.sessions.values().filter(|session| session.project_id == id).map(|session| session.id.clone()).collect();
        let doomed = descendants(&inner.sessions, &own);
        let removed: Vec<Session> = doomed.iter().filter_map(|id| inner.sessions.remove(id)).collect();
        inner.projects.remove(id);
        // Sessions first: a project without its record but with sessions left would be worse than
        // the reverse, which the next delete finishes.
        self.outbox.send(Op::DeleteSessions(removed.iter().map(|session| session.id.clone()).collect()));
        self.outbox.send(Op::DeleteProject(project.id.clone()));
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
        self.outbox.send(Op::UiState(next.clone()));
        *state = next;
        Ok(state.rev)
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

