//! `cc-notify`: the notify@1 plugin. Derives notifications from events (journal patches,
//! `agents.changed`, `plugin.swapped`, `tools.job`), applies the user's preferences (settings
//! section `notify`), the focus rule and dedup, and delivers them to macOS Notification Center,
//! through the Electron shell when it is connected and natively from the backend otherwise.

pub mod derive;
pub mod history;
pub mod native;
pub mod prefs;
pub mod service;
