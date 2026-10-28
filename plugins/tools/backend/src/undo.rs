//! The step log: what every step changed in a workspace, kept since the last commit, for undo.
//!
//! One log per workspace under `<plugin data>/undo/<hash of the workspace path>/`: `log.jsonl`
//! (one record per line, append-only between commit boundaries), `blobs/<sha256>.zst` (the
//! contents before and after each change, compressed, shared between records) and `meta.json`
//! (the workspace and the commit the log stands on).
//!
//! Records come from three places. An edit made through this plugin (the edit tool, the editor's
//! save) is recorded with the text it had in hand. A command's side effects and edits made by hand
//! are found by comparing the tree with what the log knows ([`ProjectLog::reconcile`]): after every
//! command, after a quiet moment following a file-system event, before every timeline read and
//! when the log opens. git does the walking (`git status`), so a reconcile costs one status call
//! plus a read of each candidate file; a workspace that is not a repository is walked with the
//! ignore rules instead, and its files are recorded once when the log opens so there is something
//! to go back to.
//!
//! The commit boundary: on a new HEAD, every path whose content now equals HEAD leaves the log
//! (its entries are dropped, its blobs when nothing else needs them), so a full commit empties the
//! log and a partial commit keeps what stays uncommitted.
//!
//! Undo and redo are records too (`undoes` names the steps they reverted), so the log is linear
//! and append-only; which steps stand is derived ([`active_set`]). Nothing here holds a lock while
//! waiting: the one mutex guards the in-memory state and file writes, which are short.

use crate::util;
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{
    TimelineDiffParams, TimelineDiffResult, TimelineFile, TimelineParams, TimelineRecord, TimelineResult, UndoParams, UndoResult, topics,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Files larger than this are stored but not diffed line by line.
const TEXT_DIFF_LIMIT: usize = 2 * 1024 * 1024;
/// A file-system event followed by this much quiet is one edit by hand.
const HAND_SETTLE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Meta {
    workspace: String,
    /// The commit the log stands on (the last HEAD seen), when a repository.
    #[serde(default)]
    head: Option<String>,
    /// A workspace that is not a repository had its files recorded once.
    #[serde(default)]
    baselined: bool,
}

/// How a step is attributed: who, which session, what for.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub actor: String,
    pub session_id: Option<String>,
    pub label: String,
    pub detail: String,
}

impl Attribution {
    pub fn session(session_id: &str, label: &str, detail: &str) -> Self {
        Self { actor: format!("session:{session_id}"), session_id: Some(session_id.to_owned()), label: label.to_owned(), detail: detail.to_owned() }
    }

    pub fn user(label: &str) -> Self {
        Self { actor: "user".to_owned(), session_id: None, label: label.to_owned(), detail: String::new() }
    }

    pub fn host(label: &str, detail: &str) -> Self {
        Self { actor: "host".to_owned(), session_id: None, label: label.to_owned(), detail: detail.to_owned() }
    }
}

pub trait Publisher: Send + Sync {
    fn publish(&self, topic: &str, data: Value);
}

struct State {
    records: Vec<TimelineRecord>,
    next_seq: u64,
    meta: Meta,
    /// Non-repository workspaces: the stat of every file the log knows (size, mtime ns).
    stats: HashMap<String, (u64, u128)>,
}

/// The log of one workspace.
pub struct ProjectLog {
    workspace: PathBuf,
    dir: PathBuf,
    git: Option<PathBuf>,
    state: Mutex<State>,
    /// Commands and edits of this plugin in flight (their effects are recorded when they end, not
    /// by the watcher).
    busy: Mutex<u32>,
    /// The newest file-system event not yet reconciled.
    pending: Mutex<Option<Instant>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    publisher: Mutex<Option<Arc<dyn Publisher>>>,
}

/// Every workspace's log.
pub struct StepLog {
    root: PathBuf,
    git: Option<PathBuf>,
    logs: Mutex<HashMap<PathBuf, Arc<ProjectLog>>>,
    publisher: Mutex<Option<Arc<dyn Publisher>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn find_git() -> Option<PathBuf> {
    for candidate in ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git"] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("git")).find(|git| git.is_file()))
}

fn hash_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Lines added and removed between two texts (0, 0 and `binary` for what is not text).
fn line_counts(before: Option<&[u8]>, after: Option<&[u8]>) -> (u32, u32, bool) {
    let binary = before.is_some_and(is_binary) || after.is_some_and(is_binary);
    let too_big = before.is_some_and(|b| b.len() > TEXT_DIFF_LIMIT) || after.is_some_and(|b| b.len() > TEXT_DIFF_LIMIT);
    if binary || too_big {
        return (0, 0, binary);
    }
    let old = before.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let new = after.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let diff = similar::TextDiff::from_lines(&old, &new);
    let (mut added, mut removed) = (0u32, 0u32);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed, false)
}

/// Which steps stand: a step is active unless a later active undo lists it; undoing an undo puts
/// what it had undone back (a redo), recursively.
pub fn active_set(records: &[TimelineRecord]) -> BTreeSet<u64> {
    let by_seq: BTreeMap<u64, &TimelineRecord> = records.iter().map(|record| (record.seq, record)).collect();
    fn deactivate(seq: u64, by_seq: &BTreeMap<u64, &TimelineRecord>, active: &mut BTreeSet<u64>) {
        active.remove(&seq);
        if let Some(record) = by_seq.get(&seq) {
            for &inner in &record.undoes {
                reactivate(inner, by_seq, active);
            }
        }
    }
    fn reactivate(seq: u64, by_seq: &BTreeMap<u64, &TimelineRecord>, active: &mut BTreeSet<u64>) {
        active.insert(seq);
        if let Some(record) = by_seq.get(&seq) {
            for &inner in &record.undoes {
                deactivate(inner, by_seq, active);
            }
        }
    }
    let mut active = BTreeSet::new();
    for record in records {
        active.insert(record.seq);
        for &undone in &record.undoes {
            deactivate(undone, &by_seq, &mut active);
        }
    }
    active
}

/// A change of one path found by comparing the tree with the log.
struct Found {
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    mode: u32,
}

impl StepLog {
    pub fn new(data_dir: &Path) -> StepLog {
        StepLog { root: data_dir.join("undo"), git: find_git(), logs: Mutex::new(HashMap::new()), publisher: Mutex::new(None) }
    }

    /// The git program the logs use, when one was found.
    pub fn git(&self) -> Option<&Path> {
        self.git.as_deref()
    }

    pub fn set_publisher(&self, publisher: Arc<dyn Publisher>) {
        *lock(&self.publisher) = Some(publisher.clone());
        for log in lock(&self.logs).values() {
            *lock(&log.publisher) = Some(publisher.clone());
        }
    }

    /// The log of `workspace`, opened on first use: its records are read, the commit boundary is
    /// applied when HEAD moved since the log last looked, and changes made while nobody looked
    /// are recorded as made by hand.
    pub fn open(&self, workspace: &Path) -> Arc<ProjectLog> {
        let workspace = util::canonical_lenient(workspace);
        if let Some(log) = lock(&self.logs).get(&workspace) {
            return log.clone();
        }
        let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
        let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
        let dir = self.root.join(hex);
        let git = self.git.clone().filter(|_| workspace.join(".git").exists());
        let log = Arc::new(ProjectLog::load(workspace.clone(), dir, git));
        *lock(&log.publisher) = lock(&self.publisher).clone();
        let existing = lock(&self.logs).entry(workspace).or_insert_with(|| log.clone()).clone();
        if Arc::ptr_eq(&existing, &log) {
            log.start_watching();
            log.catch_up();
        }
        existing
    }

    /// Forget a workspace that no longer exists (its log folder goes with it).
    pub fn forget(&self, workspace: &Path) {
        let workspace = util::canonical_lenient(workspace);
        if let Some(log) = lock(&self.logs).remove(&workspace) {
            let _ = std::fs::remove_dir_all(&log.dir);
        }
    }
}

impl ProjectLog {
    fn load(workspace: PathBuf, dir: PathBuf, git: Option<PathBuf>) -> ProjectLog {
        let _ = std::fs::create_dir_all(dir.join("blobs"));
        let meta: Meta = std::fs::read(dir.join("meta.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let mut records: Vec<TimelineRecord> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(dir.join("log.jsonl")) {
            for line in text.lines() {
                if let Ok(record) = serde_json::from_str::<TimelineRecord>(line) {
                    records.push(record);
                }
            }
        }
        let next_seq = records.iter().map(|record| record.seq).max().unwrap_or(0) + 1;
        let meta = Meta { workspace: workspace.to_string_lossy().into_owned(), ..meta };
        ProjectLog {
            workspace,
            dir,
            git,
            state: Mutex::new(State { records, next_seq, meta, stats: HashMap::new() }),
            busy: Mutex::new(0),
            pending: Mutex::new(None),
            watcher: Mutex::new(None),
            publisher: Mutex::new(None),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn publish(&self, seq: u64) {
        let publisher = lock(&self.publisher).clone();
        if let Some(publisher) = publisher {
            publisher.publish(topics::TIMELINE, json!({ "workspace": self.workspace.to_string_lossy(), "seq": seq }));
        }
    }

    // --- storage ---------------------------------------------------------------------------

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(format!("{hash}.zst"))
    }

    /// Keep `bytes`; returns its hash. A content already kept is not written again.
    fn keep(&self, bytes: &[u8]) -> std::io::Result<String> {
        let hash = hash_hex(bytes);
        let path = self.blob_path(&hash);
        if !path.exists() {
            let compressed = zstd::encode_all(bytes, 3)?;
            let tmp = path.with_extension(format!("zst.{}", std::process::id()));
            std::fs::write(&tmp, compressed)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    /// The kept content of `hash`.
    pub fn content(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        let compressed = std::fs::read(self.blob_path(hash))?;
        zstd::decode_all(&compressed[..])
    }

    fn write_meta(&self, meta: &Meta) {
        if let Ok(bytes) = serde_json::to_vec_pretty(meta) {
            let _ = util::atomic_write(&self.dir.join("meta.json"), &bytes);
        }
    }

    fn append_line(&self, record: &TimelineRecord) {
        if let Ok(line) = serde_json::to_string(record)
            && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("log.jsonl"))
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn rewrite_log(&self, records: &[TimelineRecord]) {
        let mut text = String::new();
        for record in records {
            if let Ok(line) = serde_json::to_string(record) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        let _ = util::atomic_write(&self.dir.join("log.jsonl"), text.as_bytes());
    }

    /// Drop blobs no record names.
    fn collect_blobs(&self, records: &[TimelineRecord]) {
        let used: BTreeSet<&str> = records.iter().flat_map(|record| record.files.iter().flat_map(|file| [file.before.as_deref(), file.after.as_deref()])).flatten().collect();
        let Ok(entries) = std::fs::read_dir(self.dir.join("blobs")) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(hash) = name.strip_suffix(".zst")
                && !used.contains(hash)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    // --- git -------------------------------------------------------------------------------

    fn git(&self, args: &[&str]) -> Option<Vec<u8>> {
        let git = self.git.as_ref()?;
        let output = std::process::Command::new(git).arg("-C").arg(&self.workspace).args(args).env("GIT_OPTIONAL_LOCKS", "0").output().ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "HEAD"]).map(|out| String::from_utf8_lossy(&out).trim().to_owned()).filter(|head| !head.is_empty())
    }

    /// Paths that differ from HEAD or are untracked (not ignored), by git.
    fn dirty(&self) -> Option<BTreeSet<String>> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])?;
        let mut paths = BTreeSet::new();
        for entry in out.split(|b| *b == 0) {
            if entry.len() > 3 {
                paths.insert(String::from_utf8_lossy(&entry[3..]).into_owned());
            }
        }
        Some(paths)
    }

    /// The content of `path` in HEAD (None: not in HEAD).
    fn head_content(&self, path: &str) -> Option<Vec<u8>> {
        self.git(&["show", &format!("HEAD:{path}")])
    }

    // --- reading the tree -----------------------------------------------------------------

    /// The file's bytes and mode, None when it does not exist (a directory or a link counts as none).
    fn read(&self, path: &str) -> Option<(Vec<u8>, u32)> {
        let absolute = self.workspace.join(path);
        let meta = std::fs::symlink_metadata(&absolute).ok()?;
        if !meta.is_file() {
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        std::fs::read(&absolute).ok().map(|bytes| (bytes, mode))
    }

    /// The newest active record's view of `path`: its hash after that step (None: deleted), or
    /// `Err` when no active step touched it.
    fn last_known(records: &[TimelineRecord], active: &BTreeSet<u64>, path: &str) -> Result<Option<String>, ()> {
        for record in records.iter().rev() {
            if !active.contains(&record.seq) {
                continue;
            }
            if let Some(file) = record.files.iter().find(|file| file.path == path) {
                return Ok(file.after.clone());
            }
        }
        Err(())
    }

    // --- recording -------------------------------------------------------------------------

    fn file_change(&self, found: &Found) -> Option<TimelineFile> {
        let before = match &found.before {
            Some(bytes) => Some(self.keep(bytes).ok()?),
            None => None,
        };
        let after = match &found.after {
            Some(bytes) => Some(self.keep(bytes).ok()?),
            None => None,
        };
        if before == after {
            return None;
        }
        let (added, removed, binary) = line_counts(found.before.as_deref(), found.after.as_deref());
        Some(TimelineFile {
            path: found.path.clone(),
            before,
            after,
            mode: if found.after.is_some() { found.mode } else { 0 },
            added,
            removed,
            binary,
            bytes: found.after.as_ref().map(|b| b.len() as u64).unwrap_or(0),
        })
    }

    fn append(&self, state: &mut State, who: &Attribution, files: Vec<TimelineFile>, undoes: Vec<u64>) -> TimelineRecord {
        let record = TimelineRecord {
            seq: state.next_seq,
            ts_ms: now_ms(),
            actor: who.actor.clone(),
            session_id: who.session_id.clone(),
            label: who.label.clone(),
            detail: who.detail.clone(),
            files,
            undoes,
            undone: false,
        };
        state.next_seq += 1;
        self.append_line(&record);
        state.records.push(record.clone());
        record
    }

    /// Record an edit this plugin made, with the contents it had in hand. Nothing is recorded when
    /// the log already knows `after` (the watcher saw it first).
    pub fn record_edit(&self, who: &Attribution, path: &str, before: Option<&[u8]>, after: Option<&[u8]>, mode: u32) -> Option<TimelineRecord> {
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        if let Ok(known) = Self::last_known(&state.records, &active, path)
            && known == after.map(hash_hex)
        {
            return None;
        }
        let found = Found { path: path.to_owned(), before: before.map(<[u8]>::to_vec), after: after.map(<[u8]>::to_vec), mode };
        let file = self.file_change(&found)?;
        if let Some(after) = &found.after {
            state.stats.insert(path.to_owned(), (after.len() as u64, 0));
        }
        let record = self.append(&mut state, who, vec![file], Vec::new());
        drop(state);
        self.publish(record.seq);
        Some(record)
    }

    /// Record what changed on disk since the log last looked, attributed to `who`.
    pub fn reconcile(&self, who: &Attribution) -> Option<TimelineRecord> {
        let mut state = lock(&self.state);
        let found = if self.git.is_some() { self.find_git_changes(&mut state) } else { self.find_walk_changes(&mut state) };
        if found.is_empty() {
            return None;
        }
        let files: Vec<TimelineFile> = found.iter().filter_map(|change| self.file_change(change)).collect();
        if files.is_empty() {
            return None;
        }
        let record = self.append(&mut state, who, files, Vec::new());
        drop(state);
        self.publish(record.seq);
        Some(record)
    }

    fn find_git_changes(&self, state: &mut State) -> Vec<Found> {
        let Some(dirty) = self.dirty() else { return Vec::new() };
        self.apply_boundary(state, &dirty);
        let active = active_set(&state.records);
        let mut candidates: BTreeSet<String> = dirty.clone();
        for record in &state.records {
            if active.contains(&record.seq) {
                candidates.extend(record.files.iter().map(|file| file.path.clone()));
            }
        }
        let mut found = Vec::new();
        for path in candidates {
            if path.starts_with(".git/") || path == ".git" {
                continue;
            }
            let current = self.read(&path);
            let current_hash = current.as_ref().map(|(bytes, _)| hash_hex(bytes));
            let known = match Self::last_known(&state.records, &active, &path) {
                Ok(hash) => hash,
                // Never seen: unchanged unless git says it differs from HEAD.
                Err(()) => {
                    if !dirty.contains(&path) {
                        continue;
                    }
                    self.head_content(&path).map(|bytes| hash_hex(&bytes))
                }
            };
            if known == current_hash {
                continue;
            }
            let before = match &known {
                Some(hash) => self.content(hash).ok().or_else(|| self.head_content(&path)),
                None => None,
            };
            let (after, mode) = match current {
                Some((bytes, mode)) => (Some(bytes), mode),
                None => (None, 0),
            };
            found.push(Found { path, before, after, mode });
        }
        found
    }

    /// Without git: every file the ignore rules admit, compared by size and mtime with what the log
    /// saw, then by content.
    fn find_walk_changes(&self, state: &mut State) -> Vec<Found> {
        let mut seen: HashMap<String, (u64, u128)> = HashMap::new();
        for entry in ignore::WalkBuilder::new(&self.workspace).hidden(false).parents(true).build().flatten() {
            let path = entry.path();
            if path == self.workspace || path.components().any(|part| part.as_os_str() == ".git") {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(path) else { continue };
            if !meta.is_file() {
                continue;
            }
            let relative = util::display_rel(path, &self.workspace);
            let mtime = meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            seen.insert(relative, (meta.len(), mtime));
        }
        let active = active_set(&state.records);
        let first = !state.meta.baselined;
        let mut found = Vec::new();
        let mut candidates: BTreeSet<String> = seen.keys().cloned().collect();
        candidates.extend(state.stats.keys().cloned());
        for path in candidates {
            let unchanged_stat = seen.get(&path).is_some_and(|stat| state.stats.get(&path) == Some(stat));
            if unchanged_stat && !first {
                continue;
            }
            let current = self.read(&path);
            let current_hash = current.as_ref().map(|(bytes, _)| hash_hex(bytes));
            let known = Self::last_known(&state.records, &active, &path).unwrap_or(None);
            if known == current_hash && !(first && known.is_none() && current.is_some()) {
                continue;
            }
            let before = known.as_ref().and_then(|hash| self.content(hash).ok());
            let (after, mode) = match current {
                Some((bytes, mode)) => (Some(bytes), mode),
                None => (None, 0),
            };
            found.push(Found { path, before, after, mode });
        }
        state.stats = seen;
        if first {
            state.meta.baselined = true;
            self.write_meta(&state.meta);
        }
        found
    }

    // --- the commit boundary ----------------------------------------------------------------

    /// The commit boundary, applied with every look at the tree: a path git no longer reports as
    /// different from HEAD is committed (or gone, or ignored now), so nothing is left to go back to
    /// and its entries leave the log; blobs nobody names go with them. HEAD is noted for the
    /// timeline. Idempotent: running it between a commit's steps or after it ends the same.
    fn apply_boundary(&self, state: &mut State, dirty: &BTreeSet<String>) {
        let head = self.head();
        let head_moved = head != state.meta.head;
        if head_moved {
            state.meta.head = head;
            self.write_meta(&state.meta);
        }
        let before = state.records.len();
        let mut touched = false;
        for record in &mut state.records {
            let kept = record.files.len();
            record.files.retain(|file| dirty.contains(&file.path));
            touched |= kept != record.files.len();
        }
        if !touched {
            return;
        }
        let mut kept: Vec<TimelineRecord> = std::mem::take(&mut state.records).into_iter().filter(|record| !record.files.is_empty() || !record.undoes.is_empty()).collect();
        // An undo whose every target left the log is bookkeeping with nothing to undo; it goes
        // too, and so does an undo of it, until nothing more goes.
        loop {
            let seqs: BTreeSet<u64> = kept.iter().map(|record| record.seq).collect();
            let before = kept.len();
            kept.retain(|record| !record.files.is_empty() || record.undoes.iter().any(|seq| seqs.contains(seq)));
            if kept.len() == before {
                break;
            }
        }
        self.rewrite_log(&kept);
        self.collect_blobs(&kept);
        let changed = kept.len() != before;
        state.records = kept;
        if changed || touched {
            let seq = state.next_seq - 1;
            self.publish(seq);
        }
    }

    /// A commit, a checkout, a reset: look at the tree (the boundary is part of it).
    pub fn on_head_change(&self) {
        self.reconcile(&Attribution::user("hand"));
    }

    /// At open: the boundary when HEAD moved while nobody looked, then changes made meanwhile.
    fn catch_up(&self) {
        self.reconcile(&Attribution::user("hand"));
    }

    // --- in flight and the watcher ---------------------------------------------------------

    /// A command or edit of this plugin starts: the watcher leaves its effects to [`Self::end`].
    pub fn begin(&self) {
        *lock(&self.busy) += 1;
    }

    /// It ended: its effects are recorded as `who`'s.
    pub fn end(&self, who: &Attribution) -> Option<TimelineRecord> {
        let record = self.reconcile(who);
        let mut busy = lock(&self.busy);
        *busy = busy.saturating_sub(1);
        record
    }

    fn start_watching(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let weak: Weak<ProjectLog> = Arc::downgrade(self);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            // Files of the tree, and the repository's own HEAD, index and refs (a commit, a
            // checkout, a reset: the boundary); git's lock files and objects say nothing.
            if let Ok(event) = event
                && event.paths.iter().any(|path| {
                    let inside_git = path.components().any(|part| part.as_os_str() == ".git");
                    if !inside_git {
                        return true;
                    }
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    let under_refs = path.components().any(|part| part.as_os_str() == "refs");
                    (name == "HEAD" || name == "index" || name == "packed-refs" || name == "ORIG_HEAD" || under_refs) && !name.ends_with(".lock")
                })
            {
                let _ = tx.send(());
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(&self.workspace, RecursiveMode::Recursive).is_err() {
            return;
        }
        *lock(&self.watcher) = Some(watcher);
        runtime.spawn(async move {
            while rx.recv().await.is_some() {
                let Some(log) = weak.upgrade() else { return };
                *lock(&log.pending) = Some(Instant::now());
                drop(log);
                // One edit by hand is a burst of events followed by quiet; the first event of the
                // burst waits it out, the rest find the moment already taken.
                loop {
                    tokio::time::sleep(HAND_SETTLE).await;
                    let Some(log) = weak.upgrade() else { return };
                    let settled = lock(&log.pending).is_some_and(|last| last.elapsed() >= HAND_SETTLE);
                    if !settled {
                        continue;
                    }
                    *lock(&log.pending) = None;
                    if *lock(&log.busy) == 0 {
                        let log = log.clone();
                        let _ = tokio::task::spawn_blocking(move || log.reconcile(&Attribution::user("hand"))).await;
                    }
                    break;
                }
                // Events that arrived while reconciling are drained: the reconcile saw the tree.
                while rx.try_recv().is_ok() {}
            }
        });
    }

    // --- reading ---------------------------------------------------------------------------

    pub fn timeline(&self, params: &TimelineParams) -> TimelineResult {
        let state = lock(&self.state);
        let active = active_set(&state.records);
        let mut records: Vec<TimelineRecord> = state
            .records
            .iter()
            .filter(|record| params.since_seq.is_none_or(|since| record.seq > since))
            .filter(|record| params.session_id.as_deref().is_none_or(|session| record.session_id.as_deref() == Some(session)))
            .filter(|record| params.since_ms.is_none_or(|since| record.ts_ms >= since))
            .cloned()
            .map(|mut record| {
                record.undone = !active.contains(&record.seq);
                record
            })
            .collect();
        records.reverse();
        if let Some(limit) = params.limit {
            records.truncate(limit);
        }
        let mut paths = BTreeSet::new();
        let (mut added, mut removed) = (0u32, 0u32);
        for record in state.records.iter().filter(|record| active.contains(&record.seq) && record.undoes.is_empty()) {
            for file in &record.files {
                paths.insert(file.path.clone());
                added += file.added;
                removed += file.removed;
            }
        }
        TimelineResult {
            records,
            head_seq: state.next_seq - 1,
            head: state.meta.head.clone(),
            files: paths.len() as u32,
            added,
            removed,
        }
    }

    pub fn diff(&self, params: &TimelineDiffParams) -> TimelineDiffResult {
        let state = lock(&self.state);
        let to = params.to_seq.unwrap_or(params.from_seq);
        let mut before: Option<Option<String>> = None;
        let mut after: Option<Option<String>> = None;
        for record in &state.records {
            if record.seq < params.from_seq || record.seq > to {
                continue;
            }
            if let Some(file) = record.files.iter().find(|file| file.path == params.path) {
                if before.is_none() {
                    before = Some(file.before.clone());
                }
                after = Some(file.after.clone());
            }
        }
        drop(state);
        let (Some(before), Some(after)) = (before, after) else {
            return TimelineDiffResult { path: params.path.clone(), diff: String::new(), binary: false, status: "unchanged".to_owned() };
        };
        let old = before.as_deref().and_then(|hash| self.content(hash).ok());
        let new = after.as_deref().and_then(|hash| self.content(hash).ok());
        let status = match (&old, &new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            (Some(a), Some(b)) if a == b => "unchanged",
            _ => "modified",
        };
        let binary = old.as_deref().is_some_and(is_binary) || new.as_deref().is_some_and(is_binary);
        if binary || status == "unchanged" {
            return TimelineDiffResult { path: params.path.clone(), diff: String::new(), binary, status: status.to_owned() };
        }
        let old_text = old.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let new_text = new.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let diff = similar::TextDiff::from_lines(&old_text, &new_text)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", params.path), &format!("b/{}", params.path))
            .to_string();
        TimelineDiffResult { path: params.path.clone(), diff, binary: false, status: status.to_owned() }
    }

    // --- undo and redo ---------------------------------------------------------------------

    /// Undo steps. `allowed` says which paths may be written (the sandbox's rule for the workspace).
    pub fn undo(&self, params: &UndoParams, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        let who = Attribution {
            actor: params.actor.clone().unwrap_or_else(|| params.session_id.as_deref().map(|s| format!("session:{s}")).unwrap_or_else(|| "user".to_owned())),
            session_id: params.session_id.clone(),
            label: "undo".to_owned(),
            detail: String::new(),
        };
        // What stands on disk now is the starting point: a change nobody recorded yet becomes a
        // step first, so the undo never overwrites it unseen.
        self.reconcile(&Attribution::user("hand"));
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        let mut wanted: BTreeSet<u64> = BTreeSet::new();
        if let Some(to) = params.to_seq {
            wanted.extend(state.records.iter().filter(|record| record.seq > to && active.contains(&record.seq)).map(|record| record.seq));
        }
        for seq in &params.seqs {
            if active.contains(seq) {
                wanted.insert(*seq);
            }
        }
        if wanted.is_empty() {
            return Ok(UndoResult::default());
        }
        self.revert(&mut state, who, wanted, params.cascade || params.to_seq.is_some(), allowed)
    }

    /// Undo the newest active undo (which puts back what it had undone).
    pub fn redo(&self, session_id: Option<&str>, actor: Option<&str>, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        self.reconcile(&Attribution::user("hand"));
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        let Some(latest) = state.records.iter().rev().find(|record| active.contains(&record.seq) && !record.undoes.is_empty()).map(|record| record.seq) else {
            return Ok(UndoResult::default());
        };
        let who = Attribution {
            actor: actor.map(str::to_owned).unwrap_or_else(|| session_id.map(|s| format!("session:{s}")).unwrap_or_else(|| "user".to_owned())),
            session_id: session_id.map(str::to_owned),
            label: "redo".to_owned(),
            detail: String::new(),
        };
        self.revert(&mut state, who, BTreeSet::from([latest]), true, allowed)
    }

    /// Revert the active steps `wanted` (and, with `cascade`, every later active step that shares
    /// a file with them, transitively): each path goes back to what it was before the earliest of
    /// those steps touched it.
    fn revert(&self, state: &mut State, who: Attribution, wanted: BTreeSet<u64>, cascade: bool, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        let active = active_set(&state.records);
        let by_seq: BTreeMap<u64, TimelineRecord> = state.records.iter().filter(|record| active.contains(&record.seq)).cloned().map(|record| (record.seq, record)).collect();
        let paths_of = |seq: u64| -> BTreeSet<String> { by_seq.get(&seq).map(|record| record.files.iter().map(|file| file.path.clone()).collect()).unwrap_or_default() };
        let mut closure = wanted.clone();
        let mut paths: BTreeSet<String> = closure.iter().flat_map(|seq| paths_of(*seq)).collect();
        loop {
            let earliest = *closure.iter().next().unwrap_or(&0);
            let more: BTreeSet<u64> = by_seq.keys().copied().filter(|seq| *seq > earliest && !closure.contains(seq) && !paths_of(*seq).is_disjoint(&paths)).collect();
            if more.is_empty() {
                break;
            }
            if !cascade {
                return Ok(UndoResult { record: None, blocked_by: more.into_iter().collect(), restored: Vec::new(), skipped: Vec::new() });
            }
            for seq in more {
                paths.extend(paths_of(seq));
                closure.insert(seq);
            }
        }
        // Per path: the content before the earliest step of the closure that touched it, and what
        // stands now (the newest active step's after), which must be what is on disk.
        let mut targets: BTreeMap<String, (Option<String>, u32)> = BTreeMap::new();
        let mut current: BTreeMap<String, Option<String>> = BTreeMap::new();
        for seq in &closure {
            let Some(record) = by_seq.get(seq) else { continue };
            for file in &record.files {
                targets.entry(file.path.clone()).or_insert((file.before.clone(), file.mode));
            }
        }
        for path in targets.keys() {
            current.insert(path.clone(), Self::last_known(&state.records, &active, path).unwrap_or(None));
        }
        let mut restored = Vec::new();
        let mut skipped = Vec::new();
        let mut files = Vec::new();
        for (path, (target, _)) in &targets {
            let absolute = self.workspace.join(path);
            if !util::is_inside(&absolute, &self.workspace) || !allowed(&absolute) {
                skipped.push(format!("{path}: not writable here"));
                continue;
            }
            let on_disk = self.read(path);
            let on_disk_hash = on_disk.as_ref().map(|(bytes, _)| hash_hex(bytes));
            if &on_disk_hash != current.get(path).unwrap_or(&None) {
                skipped.push(format!("{path}: changed on disk since the log last looked"));
                continue;
            }
            let before_bytes = on_disk.as_ref().map(|(bytes, _)| bytes.clone());
            let mode = on_disk.as_ref().map(|(_, mode)| *mode).unwrap_or(0o644);
            let after_bytes = match target {
                Some(hash) => match self.content(hash) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        skipped.push(format!("{path}: its earlier content is no longer kept ({error})"));
                        continue;
                    }
                },
                None => None,
            };
            let outcome = match &after_bytes {
                Some(bytes) => {
                    if let Some(parent) = absolute.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    util::atomic_write_checked(&absolute, bytes, allowed).and_then(|()| {
                        use std::os::unix::fs::PermissionsExt;
                        let keep_mode = targets.get(path).map(|(_, mode)| *mode).filter(|mode| *mode != 0).unwrap_or(mode);
                        std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(keep_mode))
                    })
                }
                None => match std::fs::remove_file(&absolute) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                },
            };
            match outcome {
                Ok(()) => {
                    restored.push(path.clone());
                    let found = Found { path: path.clone(), before: before_bytes, after: after_bytes, mode };
                    if let Some(file) = self.file_change(&found) {
                        files.push(file);
                    }
                }
                Err(error) => skipped.push(format!("{path}: {error}")),
            }
        }
        if restored.is_empty() {
            return Err(if skipped.is_empty() { "nothing to undo".to_owned() } else { skipped.join("; ") });
        }
        let record = self.append(state, &who, files, closure.into_iter().collect());
        self.publish(record.seq);
        Ok(UndoResult { record: Some(record), blocked_by: Vec::new(), restored, skipped })
    }
}

impl ProjectLog {
    /// An edit of this plugin ended; it recorded itself, so nothing is looked for.
    pub fn end_quiet(&self) {
        let mut busy = lock(&self.busy);
        *busy = busy.saturating_sub(1);
    }
}
