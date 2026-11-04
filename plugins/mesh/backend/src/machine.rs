//! The cortex-network on one computer: every app of this person on it is in the same network,
//! whether the others run or not.
//!
//! - **The machine key** (`cortex-network machine` in the Keychain, or the Secret Service, with the
//!   home folder as the account; [`crate::identity::Identity::machine`]): one Ed25519 key every app
//!   on this computer reads. It never connects. A `machine` entry in the membership log makes it a
//!   signer there, and it signs in only the apps of its own computer (an `add` whose `by` is the
//!   machine key, `Log::absorb`). So an app that starts while the others are closed takes its place
//!   in the network by itself: no invitation, no other app running.
//! - **The shared log** (`~/.cortex-network/network.json`, the folder 0700 and the file 0600;
//!   `CORTEX_NETWORK_DIR` moves the folder): the membership log as the app that changed it last
//!   left it. Every app reads it at start and every 15 s, and writes it whenever its own log
//!   changes. It holds public keys and signatures only: a copy of it lets nobody in.
//! - **The rollback guard** (`cortex-network log` in the Keychain): the id, length and last hash of
//!   the log written last. A file that does not reach it (an older copy put back, say to bring in
//!   a computer a member removed) is not read, and a write never shortens the file.
//!
//! Each app stays its own member with its own key, connection, card and resources: its model is
//! its own and is lent as its own. The machine key only says which computer the apps are on.
//! Removing a member removes its computer (`Log::computer_of`); leaving takes the computer out.
//! Two networks meet by a link (`Log::link`): the network in the file is the computer's, and an
//! app on another one links its own into it.

use crate::identity::{Identity, kept_get, kept_set};
use crate::members::{Entry, Log};
use crate::service::{Mesh, lock};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The shared log's file in the folder.
const LOG_FILE: &str = "network.json";
/// The Keychain (Secret Service) service of the rollback guard.
const GUARD: &str = "cortex-network log";

/// The cortex-network folder of this person.
pub fn dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CORTEX_NETWORK_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".cortex-network"))
}

/// The folder, made (0700) when missing; `None` when it is not this person's alone, and then
/// nothing in it is read or written.
pub fn private_dir() -> Option<PathBuf> {
    let dir = dir()?;
    if !dir.exists() && std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let meta = std::fs::metadata(&dir).ok()?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    (meta.uid() == uid && meta.mode() & 0o077 == 0).then_some(dir)
}

/// This computer's machine key, read once; `None` (with a warning) when no store holds or takes it.
pub fn identity() -> Option<&'static Identity> {
    static MACHINE: OnceLock<Option<Identity>> = OnceLock::new();
    MACHINE
        .get_or_init(|| match Identity::machine() {
            Ok(machine) => Some(machine),
            Err(error) => {
                tracing::warn!(%error, "no machine key: this app joins its computer's network only by an invitation");
                None
            }
        })
        .as_ref()
}

/// The guard's account: the person's home folder, as the machine key's.
fn guard_account() -> Option<String> {
    Some(PathBuf::from(std::env::var_os("HOME")?).display().to_string())
}

/// The shared log, when it is this person's, valid, and not older than the guard.
fn read_log() -> Option<Log> {
    let path = private_dir()?.join(LOG_FILE);
    let meta = std::fs::metadata(&path).ok()?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return None;
    }
    let entries: Vec<Entry> = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let mut log = Log { entries };
    log.validate().ok()?;
    if !reaches_guard(&log) {
        tracing::warn!("the shared membership log is older than the one written last; it is not read");
        return None;
    }
    Some(log)
}

/// Whether `log` is at least the log written last: the same chain at least as long and with the
/// same entry at the guard's length, or a chain that linked that one in. No guard: true.
fn reaches_guard(log: &Log) -> bool {
    let Some(text) = guard_account().and_then(|account| kept_get(GUARD, &account)) else { return true };
    let mut parts = text.splitn(3, ':');
    let (Some(id), Some(len), Some(head)) = (parts.next(), parts.next().and_then(|len| len.parse::<usize>().ok()), parts.next()) else { return true };
    if log.linked(id) {
        return true;
    }
    log.id().as_deref() == Some(id) && len >= 1 && log.entries.get(len - 1).is_some_and(|entry| entry.hash() == head)
}

/// Leave `log` in the shared folder, and raise the guard to it. A file of the same chain that is
/// longer, or of a chain that linked this one in, is newer and stays.
pub fn write_log(log: &Log) {
    let (Some(dir), Some(id), Some(last)) = (private_dir(), log.id(), log.entries.last()) else { return };
    let path = dir.join(LOG_FILE);
    if let Some(current) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Vec<Entry>>(&bytes).ok()) {
        if current == log.entries {
            return;
        }
        let current = Log { entries: current };
        let same = current.id().as_deref() == Some(id.as_str());
        if (same && current.entries.len() > log.entries.len()) || current.linked(&id) {
            return;
        }
    }
    let Ok(text) = serde_json::to_vec(&log.entries) else { return };
    let tmp = path.with_extension("json.tmp");
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp).and_then(|mut out| out.write_all(&text));
    if written.is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("the shared membership log could not be written");
        return;
    }
    let guard = format!("{id}:{}:{}", log.entries.len(), last.hash());
    if !guard_account().is_some_and(|account| kept_set(GUARD, &account, &guard)) {
        tracing::warn!("the shared membership log's guard could not be kept");
    }
}

impl Mesh {
    /// Keep this app in its computer's network, at start and every 15 s: fold the shared log in
    /// (`Log::absorb`: this app signed in by the machine key, its own network linked in), bring
    /// this computer's machine key into a network this app is in (`Log::bring_machine`), and leave
    /// the result in the shared folder for the apps that start later.
    pub async fn sync_machine(self: &Arc<Self>) {
        if !self.loaded() {
            return;
        }
        let Some(machine) = identity() else { return };
        let name = self.name();
        let addrs: Vec<String> = self.my_addr().and_then(|addr| serde_json::to_string(&addr).ok()).into_iter().collect();
        let file = read_log();
        let changed = {
            let mut log = lock(&self.log);
            let mut changed = file.is_some_and(|file| log.absorb(&self.me, machine, file, &name, &addrs));
            changed |= log.bring_machine(&self.me, machine, &name);
            changed
        };
        if !changed {
            if self.network().is_some() {
                let entries = self.log_entries();
                write_log(&Log { entries });
            }
            return;
        }
        if !self.in_log(&self.me.hex()) {
            // The computer left, or a member removed it: this app is alone again.
            tracing::info!("this computer is no longer in its network");
            self.forget_network();
            return;
        }
        tracing::info!("this app took its place in its computer's network");
        self.save_log();
        self.notify("members");
        self.push_log().await;
        self.dial_all().await;
    }
}
