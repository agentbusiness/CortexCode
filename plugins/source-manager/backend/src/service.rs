//! The plugin's shared state: where the live folder is, the settings (kept in the graph, see
//! [`crate::settings`]), the plugins' names, and one queue for git work on the live folder.

use crate::graph::{self, Graph};
use crate::live;
use crate::record::Recorder;
use crate::settings::{self, Settings};
use cc_protocol::contracts::source::topics;
use cc_protocol::{Ctx, RpcError};
use git2::Repository;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

pub struct Service {
    /// The backup repository, shared pull requests and the last update's outcome, as the graph
    /// keeps them.
    settings: Mutex<Settings>,
    /// Where the settings and the push outcomes are written.
    pub graph: Graph,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    /// The live folder (`CORTEX_LIVE`).
    pub live_dir: PathBuf,
    config: RwLock<Value>,
    /// Plugin names by id, from the last listing of the installed plugins (commit summaries).
    names: RwLock<BTreeMap<String, String>>,
    /// Serializes git work on the live folder (one writer at a time).
    git: tokio::sync::Mutex<()>,
    /// How far the live folder's changes are recorded in the graph ([`crate::record`]).
    pub recorder: Recorder,
}

pub fn rpc(message: impl Into<String>) -> RpcError {
    RpcError::internal(message.into())
}

impl Service {
    pub fn new(data_dir: PathBuf, home: PathBuf, live_dir: PathBuf) -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            graph: Graph::new(),
            data_dir,
            home,
            live_dir,
            config: RwLock::new(Value::Null),
            names: RwLock::new(BTreeMap::new()),
            git: tokio::sync::Mutex::new(()),
            recorder: Recorder::default(),
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

    /// When the plugin starts serving: write through the graph, move the old setting into it
    /// once, and read the settings back. Changes made meanwhile are written already (the read
    /// follows them).
    pub async fn load_settings(&self, ctx: &Ctx) -> Result<(), RpcError> {
        self.graph.start(graph::ctx_caller(ctx));
        settings::migrate(&self.graph, &self.data_dir).await;
        self.graph.flush().await;
        let stored = settings::read(&self.graph).await?;
        *self.settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = stored;
        Ok(())
    }

    pub fn settings(&self) -> Result<Settings, RpcError> {
        Ok(self.settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone())
    }

    /// Change the settings; what changed is written to the graph, in order.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, RpcError> {
        let mut settings = self.settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = settings.clone();
        change(&mut settings);
        settings::write_changes(&self.graph, &old, &settings);
        Ok(settings.clone())
    }

    /// Tell listeners (the settings panels, plugins reading live data) to refetch.
    pub fn changed(&self, ctx: &Ctx) {
        ctx.publish(topics::CHANGED, serde_json::json!({}));
    }
}
