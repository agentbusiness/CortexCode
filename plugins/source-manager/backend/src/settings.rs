//! Persistent state, kept in the app's main store (`source.settings`, `source.proposals`; see
//! `Service::load_stored`): the backup repository, the shared pull requests, the last update's
//! outcome and the proposals. The JSON files they were kept in before are read once and removed.

use cc_protocol::contracts::source::{Proposal, Shared};
use serde::{Deserialize, Serialize};

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

/// The files the settings and proposals were kept in before the store (read once, then removed).
pub const SETTINGS: &str = "settings.json";
pub const PROPOSALS: &str = "proposals.json";

/// The proposals, oldest first.
pub type ProposalList = Vec<Proposal>;
