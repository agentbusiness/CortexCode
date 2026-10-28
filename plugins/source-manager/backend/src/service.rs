//! The plugin's shared state: where the live folder is, the persistent settings, the plugins'
//! names, and one queue for git work on the live folder.

use crate::live;
use crate::settings::{self, Settings};
use cc_protocol::contracts::source::topics;
use cc_protocol::{Ctx, RpcError};
use git2::Repository;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

pub struct Service {
    /// The backup repository, shared pull requests and the last update's outcome. Kept in the
    /// app's main store (see [`cc_protocol::stored`]).
    pub stored_settings: cc_protocol::stored::Stored<Settings>,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    /// The live folder (`CORTEX_LIVE`).
    pub live_dir: PathBuf,
    config: RwLock<Value>,
    /// Plugin names by id, from the last listing of the installed plugins (commit summaries).
    names: RwLock<BTreeMap<String, String>>,
    /// Serializes git work on the live folder (one writer at a time) and settings changes.
    git: tokio::sync::Mutex<()>,
    state: Mutex<()>,
}

pub fn rpc(message: impl Into<String>) -> RpcError {
    RpcError::internal(message.into())
}

impl Service {
    pub fn new(data_dir: PathBuf, home: PathBuf, live_dir: PathBuf) -> Self {
        Self {
            stored_settings: cc_protocol::stored::Stored::new("source.settings"),
            data_dir,
            home,
            live_dir,
            config: RwLock::new(Value::Null),
            names: RwLock::new(BTreeMap::new()),
            git: tokio::sync::Mutex::new(()),
            state: Mutex::new(()),
        }
    }

    /// The plugin's `[plugin.config]`: `official_repo` (`owner/name` improvements are shared with).
    pub fn configure(&self, config: Value) {
        if let Ok(mut current) = self.config.write() {
            *current = config;
        }
    }

    pub fn config_str(&self, key: &str) -> Option<String> {
        let config = self.config.read().ok()?;
        config.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    }

    pub fn official_repo(&self) -> String {
        self.config_str("official_repo").unwrap_or_else(|| "agentbusiness/CortexCode".into())
    }

    /// Whether this plugin manages the live folder (creates, commits, merges). A developer checkout
    /// configured as the live folder is read, never written.
    pub fn managed(&self) -> bool {
        !self.live_dir.exists() || live::is_managed(&self.live_dir)
    }

    /// The code folder of earlier versions (CortexCode's full source), which the live folder
    /// replaces.
    pub fn old_code_dir(&self) -> PathBuf {
        self.home.join("code")
    }

    pub fn remember_names(&self, names: impl IntoIterator<Item = (String, String)>) {
        if let Ok(mut known) = self.names.write() {
            known.extend(names);
        }
    }

    /// The plugin's name, or its id when it is not known.
    pub fn name_of(&self, id: &str) -> String {
        self.names.read().ok().and_then(|names| names.get(id).cloned()).filter(|name| !name.is_empty()).unwrap_or_else(|| id.to_owned())
    }

    /// Run `work` on the live folder's repository, one at a time, off the async threads.
    pub async fn git<T: Send + 'static>(&self, work: impl FnOnce(&Repository) -> Result<T, String> + Send + 'static) -> Result<T, RpcError> {
        let _turn = self.git.lock().await;
        let dir = self.live_dir.clone();
        tokio::task::spawn_blocking(move || live::open(&dir).and_then(|repo| work(&repo)))
            .await
            .map_err(|error| rpc(format!("the git task failed: {error}")))?
            .map_err(rpc)
    }

    /// Run `work` while holding the git queue, without opening the repository (creating it).
    pub async fn git_raw<T: Send + 'static>(&self, work: impl FnOnce(&Path) -> Result<T, String> + Send + 'static) -> Result<T, RpcError> {
        let _turn = self.git.lock().await;
        let dir = self.live_dir.clone();
        tokio::task::spawn_blocking(move || work(&dir)).await.map_err(|error| rpc(format!("the git task failed: {error}")))?.map_err(rpc)
    }

    /// Read the settings from the store (or once from the JSON file they were kept in), when the
    /// plugin starts serving.
    pub async fn load_stored(&self, ctx: &Ctx) -> Result<(), RpcError> {
        self.stored_settings.load(ctx, Some(self.data_dir.join(settings::SETTINGS))).await
    }

    pub fn settings(&self) -> Result<Settings, RpcError> {
        Ok(self.stored_settings.get())
    }

    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, RpcError> {
        let _guard = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(self.stored_settings.update(|settings| {
            change(settings);
            settings.clone()
        }))
    }

    /// Tell listeners (the settings panels, plugins reading live data) to refetch.
    pub fn changed(&self, ctx: &Ctx) {
        ctx.publish(topics::CHANGED, serde_json::json!({}));
    }
}
