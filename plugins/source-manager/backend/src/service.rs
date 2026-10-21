//! The plugin's shared state: where the code folder and the shipped source are, the persistent
//! settings and proposals, and one queue for git work on the code folder.

use crate::code;
use crate::settings::{self, ProposalList, Settings};
use cc_protocol::contracts::source::{Proposal, topics};
use cc_protocol::{Ctx, RpcError};
use git2::Repository;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

pub struct Service {
    /// The backup repository, shared pull requests and the last update's outcome; and the
    /// proposals. Kept in the app's main store (see [`cc_protocol::stored`]).
    pub stored_settings: cc_protocol::stored::Stored<Settings>,
    pub stored_proposals: cc_protocol::stored::Stored<ProposalList>,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    /// The code folder (`CORTEX_CODE`).
    pub code_dir: PathBuf,
    config: RwLock<Value>,
    /// Serializes git work on the code folder (one writer at a time) and settings changes.
    git: tokio::sync::Mutex<()>,
    state: Mutex<()>,
}

pub fn rpc(message: impl Into<String>) -> RpcError {
    RpcError::internal(message.into())
}

impl Service {
    pub fn new(data_dir: PathBuf, home: PathBuf, code_dir: PathBuf) -> Self {
        Self {
            stored_settings: cc_protocol::stored::Stored::new("source.settings"),
            stored_proposals: cc_protocol::stored::Stored::new("source.proposals"),
            data_dir,
            home,
            code_dir,
            config: RwLock::new(Value::Null),
            git: tokio::sync::Mutex::new(()),
            state: Mutex::new(()),
        }
    }

    /// The plugin's `[plugin.config]`: `shipped_source` (the source the app ships, a folder),
    /// `packer` (path of `cortex-pack`), `official_repo` (`owner/name` improvements are shared with).
    pub fn configure(&self, config: Value) {
        if let Ok(mut current) = self.config.write() {
            *current = config;
        }
    }

    pub fn config_str(&self, key: &str) -> Option<String> {
        let config = self.config.read().ok()?;
        config.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    }

    /// A configured path, relative to the plugin's working directory (the frame config's folder).
    pub fn config_path(&self, key: &str) -> Option<PathBuf> {
        let value = self.config_str(key)?;
        let path = expand_tilde(&value);
        Some(if path.is_absolute() { path } else { std::env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or(path) })
    }

    pub fn shipped_source(&self) -> Option<PathBuf> {
        self.config_path("shipped_source").filter(|path| path.is_dir())
    }

    pub fn official_repo(&self) -> String {
        self.config_str("official_repo").unwrap_or_else(|| "agentbusiness/CortexCode".into())
    }

    /// Whether this plugin manages the code folder (creates, commits, merges). A developer checkout
    /// configured as the code folder is read, never written.
    pub fn managed(&self) -> bool {
        !self.code_dir.exists() || code::is_managed(&self.code_dir)
    }

    /// Run `work` on the code folder's repository, one at a time, off the async threads.
    pub async fn git<T: Send + 'static>(&self, work: impl FnOnce(&Repository) -> Result<T, String> + Send + 'static) -> Result<T, RpcError> {
        let _turn = self.git.lock().await;
        let dir = self.code_dir.clone();
        tokio::task::spawn_blocking(move || code::open(&dir).and_then(|repo| work(&repo)))
            .await
            .map_err(|error| rpc(format!("the git task failed: {error}")))?
            .map_err(rpc)
    }

    /// Run `work` while holding the git queue, without opening the repository (creating it).
    pub async fn git_raw<T: Send + 'static>(&self, work: impl FnOnce(&Path) -> Result<T, String> + Send + 'static) -> Result<T, RpcError> {
        let _turn = self.git.lock().await;
        let dir = self.code_dir.clone();
        tokio::task::spawn_blocking(move || work(&dir)).await.map_err(|error| rpc(format!("the git task failed: {error}")))?.map_err(rpc)
    }

    /// Read the settings and proposals from the store (or once from the JSON files they were kept
    /// in), when the plugin starts serving.
    pub async fn load_stored(&self, ctx: &Ctx) -> Result<(), RpcError> {
        self.stored_settings.load(ctx, Some(self.data_dir.join(settings::SETTINGS))).await?;
        self.stored_proposals.load(ctx, Some(self.data_dir.join(settings::PROPOSALS))).await
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

    pub fn proposals(&self) -> Result<ProposalList, RpcError> {
        Ok(self.stored_proposals.get())
    }

    /// Change one proposal and save the list.
    pub fn update_proposal(&self, id: &str, change: impl FnOnce(&mut Proposal)) -> Result<Proposal, RpcError> {
        let _guard = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.stored_proposals.get().iter().any(|p| p.id == id) {
            return Err(RpcError::invalid_params(format!("no proposal `{id}`")));
        }
        Ok(self.stored_proposals.update(|list| {
            let proposal = list.iter_mut().find(|p| p.id == id).expect("checked above");
            change(proposal);
            proposal.clone()
        }))
    }

    pub fn add_proposal(&self, proposal: Proposal) -> Result<(), RpcError> {
        let _guard = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stored_proposals.update(|list| list.push(proposal));
        Ok(())
    }

    /// Tell listeners (the settings panels, the card) to refetch.
    pub fn changed(&self, ctx: &Ctx) {
        ctx.publish(topics::CHANGED, serde_json::json!({}));
    }
}

pub fn expand_tilde(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)).unwrap_or_else(|| PathBuf::from(value)),
        None => PathBuf::from(value),
    }
}
