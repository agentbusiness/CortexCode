//! Projects, sessions and the workbench's slot state. The memory plugin's main store keeps them
//! (see [`crate::store`]): they are loaded from it when it first serves, held here while the frame
//! runs, and every change is written through the store's outbox in the order it was made.
//!
//! A copy of the projects and sessions, and of the projects deleted, is kept in the frame's own
//! file (`<home>/state/records.json`), written at every change: a store that comes back without
//! records (a new database after an upgrade) gets them back from it at the next load, and a plugin
//! that was not running when a project was deleted hears of it when it starts (`project.deleted`
//! again, see `Frame::set_route`).

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
    /// The frame's copy of the records (`<home>/state/records.json`).
    copy: PathBuf,
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
    /// Empty until [`StateStore::load`] fills it from the store (and the frame's copy).
    pub fn open(outbox: Outbox, copy: PathBuf) -> StateStore {
        StateStore {
            outbox,
            ui_state: Mutex::new(UiStateDocument { document: serde_json::Value::Object(serde_json::Map::new()), rev: 0, updated_ms: 0 }),
            inner: Mutex::new(Inner::default()),
            copy,
        }
    }

    /// Write the frame's copy of the records (atomically: a stopped write leaves the last one).
    fn save(&self, inner: &Inner) {
        let written = serde_json::to_vec(inner).map_err(|error| error.to_string()).and_then(|bytes| {
            if let Some(dir) = self.copy.parent() {
                std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
            }
            let tmp = self.copy.with_extension("json.tmp");
            std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &self.copy)).map_err(|error| error.to_string())
        });
        if let Err(error) = written {
            tracing::warn!(path = %self.copy.display(), %error, "writing the frame's copy of the records failed");
        }
    }

    /// The projects deleted, as their `project.deleted` events carried them.
    pub fn deleted_projects(&self) -> Vec<ProjectDeleted> {
        self.lock().deleted.values().cloned().collect()
    }

    /// The records the store holds, when it first serves. A record made before is kept (none is:
    /// calls on the records wait until this load is done, see `Frame::records_ready`).
    pub fn load(&self, projects: Vec<Project>, sessions: Vec<Session>, ui_state: Option<UiStateDocument>) {
        let mut inner = self.lock();
        for project in projects {
            inner.projects.entry(project.id.clone()).or_insert(project);
        }
        for session in sessions {
            inner.sessions.entry(session.id.clone()).or_insert(session);
        }
        // What the frame's copy has and the store lost goes back into the store; a session the
        // copy has newer (its write was still on the way) wins.
        let copy: Inner = std::fs::read(&self.copy).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
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
            if !inner.projects.contains_key(&id) {
                inner.deleted.insert(id, deleted);
            }
        }
        if restored > 0 {
            tracing::info!(restored, "records the store had lost were written back from the frame's copy");
        }
        self.save(&inner);
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
        self.save(&inner);
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
        self.save(&inner);
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
        self.save(&inner);
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
        self.save(&inner);
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
        self.save(&inner);
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

