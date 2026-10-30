//! `cc-source`: the live folder manager (contract `source@1`, see `docs/LIVE-EDITING.md`).
//!
//! Every installed plugin's data (prompts, texts, stages, schemas, skills) is one git repository in
//! the live folder. This plugin creates it from the installed packages, keeps it in step with them
//! (a merge that keeps the user's edits), commits edits as they settle, keeps the history (revert),
//! backs the folder up to the user's GitHub repository and shares picked changes with the official
//! project. CortexCode's code is not here: it changes only with a new package.

pub mod backup;
pub mod graph;
pub mod live;
pub mod packages;
pub mod paths;
pub mod probe;
pub mod record;
pub mod release;
pub mod service;
pub mod settings;
pub mod share;
pub mod storage;
pub mod watch;

use cc_protocol::contracts::source::methods;

/// Methods this plugin serves.
pub const PROVIDES: &[&str] = &[
    methods::STATUS,
    methods::UPDATE,
    methods::HISTORY,
    methods::REVERT,
    methods::DIFF,
    methods::BACKUP_STATUS,
    methods::BACKUP_SET,
    methods::BACKUP_PUSH,
    methods::BACKUP_RESTORE,
    methods::SHARE_CANDIDATES,
    methods::SHARE_SEND,
    methods::SHARE_STATUS,
    methods::STORAGE,
    methods::RELEASE_STATUS,
    methods::RELEASE_MERGE,
    methods::RELEASE_RESET,
    methods::FILE_HISTORY,
    methods::FILE_RESTORE,
];
