//! One tree, many hands: every session edits the project's own folder. Two host rules keep them
//! from tripping over each other (`docs/MESH-IMPLEMENTATION-PLAN.md` §4):
//!
//! - **A file under edit is reserved.** When a session's edit writes a path, the path is reserved
//!   for that session until its turn ends (the turn's cleanup releases it). Another session's edit
//!   of a reserved path is refused with who holds it and when it frees. A command that changes a
//!   reserved file is not blocked (what `sed` will touch is not known beforehand); the step log
//!   shows it afterwards.
//! - **A check runs on a throw-away copy.** When a command is one of the project's checks (a
//!   verifier the language plugins declare: build, test, lint) *and* another session is active in
//!   the tree (holds a reservation or runs a command), the command runs on a `clonefile` copy of
//!   the tree made for it alone, so a half-finished edit of someone else cannot make a test lie.
//!   The copy drops what git ignores and links the caches the language plugins declare (`target`,
//!   `node_modules`) to the project's own, then goes when the command ends. A session alone in the
//!   tree pays nothing: its checks run in the tree itself.

use crate::host::Host;
use crate::util;
use cc_protocol::contracts::lang::{CONTRACT, Description};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the language plugins' verifier commands and cache directories are kept before asking again.
const VERIFIERS_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Reservation {
    pub session_id: String,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct Verifiers {
    /// The first words of every verifier command (`cargo test`, `npm run build`, `pytest`).
    prefixes: Vec<Vec<String>>,
    caches: Vec<String>,
}

pub struct Tree {
    reserved: Mutex<HashMap<PathBuf, Reservation>>,
    /// Per workspace: the sessions with a command running, and how many.
    running: Mutex<HashMap<PathBuf, HashMap<String, u32>>>,
    verifiers: Mutex<Option<(Instant, Verifiers)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Tree {
        Tree { reserved: Mutex::new(HashMap::new()), running: Mutex::new(HashMap::new()), verifiers: Mutex::new(None) }
    }

    // --- reservations ------------------------------------------------------------------------

    /// `session` wrote `path`: it is the session's until its turn ends.
    pub fn reserve(&self, path: &Path, workspace: &Path, session: &str) {
        lock(&self.reserved).insert(util::canonical_lenient(path), Reservation { session_id: session.to_owned(), workspace: util::canonical_lenient(workspace) });
    }

    /// Who else holds `path`, if anyone.
    pub fn reserved_by_other(&self, path: &Path, session: &str) -> Option<String> {
        lock(&self.reserved).get(&util::canonical_lenient(path)).filter(|reservation| reservation.session_id != session).map(|reservation| reservation.session_id.clone())
    }

    /// The session's turn ended (or the session is gone): its reservations free.
    pub fn release_session(&self, session: &str) {
        lock(&self.reserved).retain(|_, reservation| reservation.session_id != session);
    }

    // --- running commands --------------------------------------------------------------------

    pub fn begin_run(&self, workspace: &Path, session: &str) {
        *lock(&self.running).entry(util::canonical_lenient(workspace)).or_default().entry(session.to_owned()).or_insert(0) += 1;
    }

    pub fn end_run(&self, workspace: &Path, session: &str) {
        let mut running = lock(&self.running);
        if let Some(sessions) = running.get_mut(&util::canonical_lenient(workspace)) {
            if let Some(count) = sessions.get_mut(session) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    sessions.remove(session);
                }
            }
            if sessions.is_empty() {
                running.remove(&util::canonical_lenient(workspace));
            }
        }
    }

    /// Whether another session is at work in `workspace`: a reservation of its, or a running
    /// command. The moment a check decides whether it needs a copy of its own.
    pub fn others_active(&self, workspace: &Path, session: &str) -> bool {
        let workspace = util::canonical_lenient(workspace);
        if lock(&self.reserved).values().any(|reservation| reservation.workspace == workspace && reservation.session_id != session) {
            return true;
        }
        lock(&self.running).get(&workspace).is_some_and(|sessions| sessions.keys().any(|other| other != session))
    }

    // --- the project's checks ----------------------------------------------------------------

    async fn verifiers(&self, host: &dyn Host) -> Verifiers {
        if let Some((at, known)) = lock(&self.verifiers).clone()
            && at.elapsed() < VERIFIERS_TTL
        {
            return known;
        }
        let mut found = Verifiers::default();
        if let Ok(listed) = host.call("frame.plugins.list", json!({})).await {
            for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
                let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
                if field("contract") != CONTRACT || field("state") != "active" {
                    continue;
                }
                let Ok(value) = host.call(&format!("{}.describe", field("category")), json!({})).await else { continue };
                let Ok(description) = serde_json::from_value::<Description>(value) else { continue };
                for verifier in &description.verifiers {
                    let words: Vec<String> = verifier.command.iter().take(2).filter(|word| !word.starts_with('{')).cloned().collect();
                    if !words.is_empty() && !found.prefixes.contains(&words) {
                        found.prefixes.push(words);
                    }
                }
                for cache in &description.caches {
                    if !found.caches.contains(cache) {
                        found.caches.push(cache.clone());
                    }
                }
            }
        }
        *lock(&self.verifiers) = Some((Instant::now(), found.clone()));
        found
    }

    /// Whether `command` is one of the project's checks: its first words are a verifier's.
    pub async fn is_check(&self, host: &dyn Host, command: &str) -> bool {
        let Some(argv) = crate::approval::simple_argv(command) else { return false };
        let verifiers = self.verifiers(host).await;
        verifiers.prefixes.iter().any(|prefix| prefix.len() <= argv.len() && prefix.iter().zip(argv.iter()).all(|(a, b)| a == b))
    }

    /// The cache directories the language plugins declare.
    pub async fn caches(&self, host: &dyn Host) -> Vec<String> {
        self.verifiers(host).await.caches
    }
}

/// A throw-away copy of a workspace for one check; the folder goes when the guard drops.
pub struct CheckCopy {
    pub path: PathBuf,
}

impl Drop for CheckCopy {
    fn drop(&mut self) {
        let path = self.path.clone();
        std::thread::spawn(move || {
            let _ = std::fs::remove_dir_all(&path);
        });
    }
}

/// Copy `workspace` for one check: `clonefile` of the tree, the ignored paths dropped, the declared
/// caches linked to the workspace's own.
pub fn copy_for_check(home: &Path, git: Option<&Path>, workspace: &Path, caches: &[String]) -> std::io::Result<CheckCopy> {
    let root = home.join("checks");
    std::fs::create_dir_all(&root)?;
    let name = format!("{}-{}", workspace.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "tree".to_owned()), &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let target = root.join(name);
    match crate::workspace::clone_tree(workspace, &target) {
        Ok(true) => {}
        Ok(false) => crate::workspace::copy_tree(workspace, &target)?,
        Err(error) => return Err(error),
    }
    let copy = CheckCopy { path: target.clone() };
    // What git ignores is build output and dependencies: not part of the check's input, and the
    // caches among them are linked back below.
    if let Some(git) = git {
        let output = std::process::Command::new(git).arg("-C").arg(workspace).args(["ls-files", "-o", "-i", "--directory", "--exclude-standard", "-z"]).output();
        if let Ok(output) = output
            && output.status.success()
        {
            for entry in output.stdout.split(|b| *b == 0) {
                if entry.is_empty() {
                    continue;
                }
                let relative = String::from_utf8_lossy(entry).into_owned();
                let path = target.join(relative.trim_end_matches('/'));
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    for cache in caches {
        let own = workspace.join(cache);
        if !own.is_dir() {
            continue;
        }
        let link = target.join(cache);
        let _ = std::fs::remove_dir_all(&link);
        if let Some(parent) = link.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::os::unix::fs::symlink(&own, &link);
    }
    Ok(copy)
}
