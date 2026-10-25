//! Capabilities handed to sandboxed command descendants. The UI credential never enters a shell.
use cc_protocol::contracts::frame::{CommandAccessParams, CommandAccessResult};
use cc_protocol::{RpcError, codes};
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

impl CommandScope {
    pub fn authorize(&self, method: &str, params: &mut Value) -> Result<(), RpcError> {
        let permitted = matches!(
            method,
            "frame.project.list"
                | "frame.project.get"
                | "frame.session.get"
                | "frame.machine"
                | "frame.plugins.list"
                | "frame.plugins.providers"
                | "memory.task.list"
                | "memory.task.get"
                | "memory.task.create"
                | "memory.task.update"
                | "memory.board.list"
                | "memory.board.post"
                | "memory.agent.list"
                | "memory.search"
                | "memory.symbol"
                | "memory.callers"
                | "memory.callees"
                | "memory.impact"
                | "memory.path"
                | "memory.outline"
                | "memory.project.list"
                | "research.search"
                | "research.fetch"
                | "research.note.add"
                | "research.notes"
                | "research.note.search"
                | "research.deep"
                | "research.job.status"
                | "research.jobs"
                | "research.job.cancel"
                | "research.sources"
                | "browser.open"
                | "browser.navigate"
                | "browser.tabs"
                | "browser.select"
                | "browser.close"
                | "browser.back"
                | "browser.forward"
                | "browser.reload"
                | "browser.snapshot"
                | "browser.screenshot"
                | "browser.console"
                | "browser.network"
                | "browser.eval"
                | "browser.click"
                | "browser.type"
                | "browser.press"
                | "browser.metrics"
                | "tools.job.list"
                | "tools.job.status"
                | "tools.job.kill"
        );
        if !permitted {
            return Err(RpcError::new(
                codes::CONTRACT_VIOLATION,
                format!(
                    "`{method}` requires the user's app connection; use the CortexCode window to approve or perform it"
                ),
            ));
        }
        if let Some(workspace) = params.get("workspace").and_then(Value::as_str) {
            if std::fs::canonicalize(workspace).ok().as_deref() != Some(self.workspace.as_path()) {
                return Err(RpcError::invalid_params(
                    "command access belongs to its original workspace",
                ));
            }
        }
        if let Some(session) = params.get("session_id").and_then(Value::as_str) {
            if session != self.session_id {
                return Err(RpcError::invalid_params(
                    "command access belongs to its original session",
                ));
            }
        }
        if method.starts_with("browser.") {
            if let Some(url) = params.get("url").and_then(Value::as_str) {
                let lower = url.trim().to_ascii_lowercase();
                if !(lower.starts_with("http://")
                    || lower.starts_with("https://")
                    || lower == "about:blank"
                    || lower.starts_with("data:")
                    || (!lower.starts_with('/') && !lower.contains(':'))
                    || (!lower.contains("://") && lower.rsplit_once(':').is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))))
                {
                    return Err(RpcError::invalid_params(
                        "command browser navigation supports web URLs; open local files in the app",
                    ));
                }
            }
            if !params.is_object() {
                *params = serde_json::json!({});
            }
            params["_command_session"] = Value::String(self.session_id.clone());
            if method == "browser.open" {
                params["session_id"] = Value::String(self.session_id.clone());
            }
        }
        if method.starts_with("tools.job.") {
            if !params.is_object() {
                *params = serde_json::json!({});
            }
            params["_command_session"] = Value::String(self.session_id.clone());
        }
        if method == "tools.job.list" {
            if !params.is_object() {
                *params = serde_json::json!({});
            }
            params["session_id"] = Value::String(self.session_id.clone());
        }
        Ok(())
    }
}
