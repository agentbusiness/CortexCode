//! Persistent state in the plugin's data directory (stable across plugin versions): the backup
//! repository, the shared pull requests, the last update's outcome and the proposals. Each file is
//! written atomically.

use cc_protocol::contracts::source::{Proposal, Shared};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// `owner/name` of the backup repository.
    #[serde(default)]
    pub backup_repo: Option<String>,
    /// Push after every change.
    #[serde(default)]
    pub backup_auto: bool,
    #[serde(default)]
    pub last_push_ms: Option<u64>,
    #[serde(default)]
    pub last_push_error: Option<String>,
    /// Pull requests sent to the official repository.
    #[serde(default)]
    pub shared: Vec<Shared>,
    /// up_to_date | conflicts | no_source (the last update).
    #[serde(default)]
    pub update: String,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

pub const SETTINGS: &str = "settings.json";
pub const PROPOSALS: &str = "proposals.json";

/// Read `name` from `dir` (the default when it does not exist yet).
pub fn load<T: DeserializeOwned + Default>(dir: &Path, name: &str) -> Result<T, String> {
    let file = dir.join(name);
    match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| format!("{} is not valid: {error}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(format!("cannot read {}: {error}", file.display())),
    }
}

/// Write `value` to `dir/name` atomically (temp file, fsync, rename).
pub fn save<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let file = dir.join(name);
    let tmp = dir.join(format!(".{name}-{}.tmp", uuid::Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let write = || -> std::io::Result<()> {
        let mut handle = std::fs::File::create(&tmp)?;
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        std::fs::rename(&tmp, &file)
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {error}", file.display())
    })
}

/// The proposals, oldest first.
pub type ProposalList = Vec<Proposal>;
