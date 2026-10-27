//! `cc-source`: the code folder manager (contract `source@1`, see `docs/LIVE-EDITING.md`).
//!
//! CortexCode's full source is one git repository in the code folder. This plugin creates it from
//! the shipped source, updates it (a merge that keeps the user's changes), commits data edits as
//! they settle, turns code changes into proposals the user approves, builds and installs approved
//! changes live, keeps the history (revert), backs the folder up to the user's GitHub repository
//! and shares picked changes with the official project.

pub mod backup;
pub mod capability;
pub mod code;
pub mod paths;
pub mod probe;
pub mod proc;
pub mod proposals;
pub mod release;
pub mod service;
pub mod settings;
pub mod share;
pub mod storage;
pub mod summary;
pub mod watch;

use cc_protocol::contracts::source::methods;

/// Methods this plugin serves.
pub const PROVIDES: &[&str] = &[
    methods::STATUS,
    methods::UPDATE,
    methods::HISTORY,
    methods::REVERT,
    methods::DIFF,
    methods::PROPOSALS,
    methods::PROPOSE,
    methods::DECIDE,
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
];
