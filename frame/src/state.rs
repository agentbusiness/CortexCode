//! Projects, sessions and the workbench's slot state, persisted as JSON under `<home>/state/`
//! with atomic writes.

use crate::home::write_atomic;
use cc_protocol::contracts::frame::{
    Project, Session, SessionCreateParams, SessionStatus, SessionUpdateParams, UiStateDocument,
};
use cc_protocol::contracts::now_ms;
use cc_protocol::RpcError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct StateStore {
    dir: PathBuf,
    inner: Mutex<Inner>,
    ui_state: Mutex<UiStateDocument>,
}

struct Inner {
    projects: BTreeMap<String, Project>,
    sessions: BTreeMap<String, Session>,
}

impl StateStore {
    pub fn open(dir: PathBuf) -> std::io::Result<StateStore> {
        std::fs::create_dir_all(&dir)?;
        let projects: Vec<Project> = read_json(&dir.join("projects.json"))?;
        let sessions: Vec<Session> = read_json(&dir.join("sessions.json"))?;
        let ui_state = match std::fs::read(dir.join("ui_state.json")) {
            Ok(bytes) => serde_json::from_slice::<UiStateDocument>(&bytes)
                .map_err(|error| std::io::Error::other(format!("{} is corrupt: {error}", dir.join("ui_state.json").display())))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                UiStateDocument { document: serde_json::Value::Object(serde_json::Map::new()), rev: 0, updated_ms: 0 }
            }
            Err(error) => return Err(error),
        };
        Ok(StateStore {
            ui_state: Mutex::new(ui_state),
            dir,
            inner: Mutex::new(Inner {
                projects: projects.into_iter().map(|p| (p.id.clone(), p)).collect(),
                sessions: sessions.into_iter().map(|s| (s.id.clone(), s)).collect(),
            }),
        })
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
        inner.projects.insert(id.clone(), project.clone());
        if let Err(error) = self.persist_projects(&inner) {
            inner.projects.remove(&id);
            return Err(error);
        }
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
        };
        inner.sessions.insert(session.id.clone(), session.clone());
        if let Err(error) = self.persist_sessions(&inner) {
            inner.sessions.remove(&session.id);
            return Err(error);
        }
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
        session.updated_ms = now_ms().max(previous.updated_ms);
        inner.sessions.insert(session.id.clone(), session.clone());
        if let Err(error) = self.persist_sessions(&inner) {
            inner.sessions.insert(previous.id.clone(), previous);
            return Err(error);
        }
        Ok(session)
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
        write_json(&self.dir.join("ui_state.json"), &next)?;
        *state = next;
        Ok(state.rev)
    }

    fn persist_projects(&self, inner: &Inner) -> Result<(), RpcError> {
        let list: Vec<&Project> = inner.projects.values().collect();
        write_json(&self.dir.join("projects.json"), &list)
    }

    fn persist_sessions(&self, inner: &Inner) -> Result<(), RpcError> {
        let list: Vec<&Session> = inner.sessions.values().collect();
        write_json(&self.dir.join("sessions.json"), &list)
    }
}

/// Stable project id: a hash of the canonical root path.
pub fn project_id(canonical_root: &str) -> String {
    let digest = Sha256::digest(canonical_root.as_bytes());
    format!("p_{}", &hex::encode(digest)[..16])
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<Vec<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| std::io::Error::other(format!("{} is corrupt: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RpcError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| RpcError::internal(error.to_string()))?;
    write_atomic(path, &bytes).map_err(|error| RpcError::internal(format!("cannot write {}: {error}", path.display())))
}
