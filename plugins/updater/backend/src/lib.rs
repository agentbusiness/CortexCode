//! `cc-updater`: updates from the official releases (contract `update@1`, `docs/RELEASES.md`).
//!
//! It reads the release catalog of the official repository, installs the plugin releases the user
//! did not change when no run is going, asks about the plugins the user changed (the popup at the
//! bottom right), and stages a newer frame for the next start ("Restart to update").

pub mod fetch;
pub mod graph;
pub mod service;

use cc_protocol::contracts::update::methods;

/// Methods this plugin serves.
pub const PROVIDES: &[&str] = &[methods::STATUS, methods::CHECK, methods::DECIDE, methods::RESTART, methods::DISMISS, methods::RESET_PLAN, methods::RESET];
