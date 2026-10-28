//! Capabilities handed to sandboxed command descendants. The UI credential never enters a shell.
use cc_protocol::contracts::frame::{CommandAccessParams, CommandAccessResult};
use cc_protocol::RpcError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct CommandScope {
    pub session_id: String,
    pub workspace: PathBuf,
}

#[derive(Default)]
pub struct CommandAccess(Mutex<HashMap<String, Arc<CommandScope>>>);

impl CommandAccess {
    pub fn issue(
        &self,
        params: CommandAccessParams,
        port: u16,
        socket: &Path,
    ) -> Result<CommandAccessResult, RpcError> {
        if params.session_id.is_empty() || !Path::new(&params.workspace).is_absolute() {
            return Err(RpcError::invalid_params(
                "command access needs a session and absolute workspace",
            ));
        }
        let workspace = std::fs::canonicalize(&params.workspace).map_err(|error| {
            RpcError::invalid_params(format!("cannot resolve command workspace: {error}"))
        })?;
        let mut scopes = self
            .0
            .lock()
            .map_err(|_| RpcError::internal("command access state unavailable"))?;
        let token = if let Some((token, _)) = scopes.iter().find(|(_, scope)| {
            scope.session_id == params.session_id && scope.workspace == workspace
        }) {
            token.clone()
        } else {
            let token = crate::home::random_token(32)
                .map_err(|error| RpcError::internal(error.to_string()))?;
            scopes.insert(
                token.clone(),
                Arc::new(CommandScope {
                    session_id: params.session_id,
                    workspace,
                }),
            );
            token
        };
        Ok(CommandAccessResult {
            url: format!("http://127.0.0.1:{port}"),
            token,
            socket: socket.to_string_lossy().into_owned(),
        })
    }

    pub fn forget_session(&self, session: &str) {
        if let Ok(mut scopes) = self.0.lock() { scopes.retain(|_, scope| scope.session_id != session); }
    }

    pub fn find(&self, token: &str) -> Option<Arc<CommandScope>> {
        self.0.lock().ok()?.get(token).cloned()
    }
}

/// The frame's own methods a command in the model's shells may call. Plugins declare theirs in
/// their hello (`commands`).
pub const FRAME_COMMANDS: &[&str] = &[
    "frame.project.list",
    "frame.project.get",
    "frame.session.get",
    "frame.machine",
    "frame.plugins.list",
    "frame.plugins.providers",
];

impl CommandScope {
    /// Hold a permitted call to its command's workspace and session, and tell the serving plugin
    /// which session's command calls it (`_command_session`), so the plugin keeps that command to
    /// its own session's things.
    pub fn authorize(&self, method: &str, params: &mut Value) -> Result<(), RpcError> {
        if let Some(workspace) = params.get("workspace").and_then(Value::as_str)
            && std::fs::canonicalize(workspace).ok().as_deref() != Some(self.workspace.as_path())
        {
            return Err(RpcError::invalid_params("command access belongs to its original workspace"));
        }
        if let Some(session) = params.get("session_id").and_then(Value::as_str)
            && session != self.session_id
        {
            return Err(RpcError::invalid_params("command access belongs to its original session"));
        }
        if !method.starts_with("frame.") {
            if params.is_null() {
                *params = serde_json::json!({});
            }
            if let Some(object) = params.as_object_mut() {
                object.insert("_command_session".into(), Value::String(self.session_id.clone()));
            }
        }
        Ok(())
    }
}
