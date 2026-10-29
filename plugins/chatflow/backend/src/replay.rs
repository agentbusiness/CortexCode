//! The replay gate: a lesson is tried on the run it was learned from, with it and without it, and
//! what the two runs end in decides (counterfactual evidence, not one trajectory read after the
//! fact).
//!
//! - **Snapshot at run start**: a run in a git workspace keeps its starting point in the plugin's
//!   data folder: the commit (`HEAD`), the uncommitted diff (`git diff HEAD --binary`) and copies of
//!   the untracked files git does not ignore. Only read-only git commands touch the repository; the
//!   newest `KEPT` snapshots stay.
//! - **At idle** (no run in any session, nothing left to learn) one candidate is replayed: a live
//!   lesson not replayed yet whose run has a snapshot, taking turns between INV (`tool:`,
//!   `approach`: how the agent works) and DOM (`lang:`: what a language demands). Its run's message
//!   is sent twice, each time in a new session on a fresh copy of the snapshot (`git archive` of the
//!   commit, the diff applied, the untracked files copied, made a repository of its own): once with
//!   the lesson shown with the knowledge note (`with`), once with it never shown (`without`). The
//!   arm is a `replay` note in the session; a replay session asks nobody for approval (a command
//!   that needs it is answered as not approved) and is never learned from.
//! - **The runs' ends decide**: `with` done and `without` not is better (`helped`), the reverse is
//!   worse (`hurt`, and the withdrawal rule applies), anything else is no observed effect. The
//!   lesson gets a `replay://` evidence either way, so it is not replayed again; a user's turn
//!   cancels a replay, which then decides nothing and is tried again later. Results go to
//!   `replays/results.jsonl` for the night report; the replay's projects and sessions are then
//!   deleted.

use crate::host::Host;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::{JournalEntry, ProjectListResult, SessionCreateParams, SessionKind, kinds, methods as frame_methods};
use cc_protocol::contracts::memory::{KnowledgeItem, KnowledgeListResult, knowledge, methods as memory_methods};
use cc_protocol::BoxFuture;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Snapshots kept (the newest runs').
const KEPT: usize = 40;
/// Older snapshots may have silently lost files or flattened links and cannot be trusted.
const SNAPSHOT_VERSION: u64 = 2;
/// A snapshot whose untracked files are larger than this is not taken (a build output git does
/// not ignore, a dataset).
const UNTRACKED_BYTES: u64 = 64 * 1024 * 1024;

/// The note purpose that marks a replay session and its arm.
pub const NOTE_PURPOSE: &str = "replay";

fn str_field<'a>(data: &'a Value, field: &str) -> &'a str {
    data.get(field).and_then(Value::as_str).unwrap_or("")
}

/// Where snapshots, replay workspaces and results live, under the plugin's data folder.
#[derive(Clone, Debug)]
pub struct Paths {
    pub snapshots: PathBuf,
    pub replays: PathBuf,
    pub reports: PathBuf,
}

impl Paths {
    pub fn new(data_dir: &Path) -> Self {
        Self { snapshots: data_dir.join("snapshots"), replays: data_dir.join("replays"), reports: data_dir.join("reports") }
    }

    fn snapshot(&self, session_id: &str, run_id: &str) -> PathBuf {
        self.snapshots.join(safe(session_id)).join(safe(run_id))
    }

    /// Whether a workspace is a replay's copy (its runs are not learned from).
    pub fn is_replay(&self, workspace: &str) -> bool {
        Path::new(workspace).starts_with(&self.replays)
    }

    pub fn has_snapshot(&self, session_id: &str, run_id: &str) -> bool {
        std::fs::read(self.snapshot(session_id, run_id).join("meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|meta| meta.get("version").and_then(Value::as_u64) == Some(SNAPSHOT_VERSION))
    }

    pub fn results(&self) -> PathBuf {
        self.replays.join("results.jsonl")
    }
}

fn safe(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// A git command's stdout, or what went wrong. `--no-optional-locks`: a read never writes the
/// index.
async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git").arg("--no-optional-locks").arg("-C").arg(dir).args(args).output().await.map_err(|error| format!("git: {error}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// Copy an untracked entry without following symlinks, including dangling links.
fn copy_entry(from: &Path, to: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(from).map_err(|error| format!("{}: {error}", from.display()))?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from).map_err(|error| format!("{}: {error}", from.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to).map_err(|error| format!("{}: {error}", to.display()))?;
        #[cfg(not(unix))]
        return Err(format!("cannot preserve symlink {} on this platform", from.display()));
    } else if meta.is_file() {
        std::fs::copy(from, to).map_err(|error| format!("{}: {error}", from.display()))?;
    } else {
        return Err(format!("unsupported snapshot entry: {}", from.display()));
    }
    Ok(())
}

fn remove_tree(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Keep the run's starting point (see the module's doc). A workspace that is not in a git
/// repository with a commit is not snapshotted.
pub async fn snapshot(paths: &Paths, session_id: &str, run_id: &str, workspace: &str) -> Result<(), String> {
    let workspace = Path::new(workspace);
    let root = String::from_utf8_lossy(&git(workspace, &["rev-parse", "--show-toplevel"]).await?).trim().to_owned();
    let root = PathBuf::from(root);
    let head = String::from_utf8_lossy(&git(&root, &["rev-parse", "HEAD"]).await?).trim().to_owned();
    let diff = git(&root, &["diff", "HEAD", "--binary"]).await?;
    let listed = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let untracked: Vec<String> = listed.split(|byte| *byte == 0).filter(|name| !name.is_empty())
        .map(|name| String::from_utf8(name.to_vec()).map_err(|_| "snapshot contains a non-UTF-8 path".to_owned()))
        .collect::<Result<_, _>>()?;
    let mut bytes = 0u64;
    for name in &untracked {
        let meta = std::fs::symlink_metadata(root.join(name)).map_err(|error| format!("{name}: {error}"))?;
        bytes = bytes.checked_add(meta.len()).ok_or_else(|| "snapshot size overflow".to_owned())?;
        if bytes > UNTRACKED_BYTES {
            return Err(format!("untracked files over {} MB", UNTRACKED_BYTES / (1024 * 1024)));
        }
    }
    let dir = paths.snapshot(session_id, run_id);
    remove_tree(&dir)?;
    std::fs::create_dir_all(dir.join("untracked")).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("diff.patch"), &diff).map_err(|error| error.to_string())?;
    for name in &untracked {
        copy_entry(&root.join(name), &dir.join("untracked").join(name))?;
    }
    let subdir = workspace.strip_prefix(&root).map(|rest| rest.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = json!({"version": SNAPSHOT_VERSION, "root": root, "head": head, "subdir": subdir, "untracked": untracked, "session_id": session_id, "run_id": run_id});
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    prune(paths);
    Ok(())
}

/// Keep the newest `KEPT` snapshots.
fn prune(paths: &Paths) {
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for session in std::fs::read_dir(&paths.snapshots).into_iter().flatten().flatten() {
        for run in std::fs::read_dir(session.path()).into_iter().flatten().flatten() {
            let modified = run.metadata().and_then(|meta| meta.modified()).unwrap_or(std::time::UNIX_EPOCH);
            all.push((modified, run.path()));
        }
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, dir) in all.into_iter().skip(KEPT) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A fresh copy of a snapshot at `dest`; returns the workspace in it (the snapshot's subfolder).
async fn materialize(paths: &Paths, session_id: &str, run_id: &str, dest: &Path) -> Result<PathBuf, String> {
    let dir = paths.snapshot(session_id, run_id);
    let meta: Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    if meta.get("version").and_then(Value::as_u64) != Some(SNAPSHOT_VERSION) {
        return Err("snapshot predates complete file and symlink capture".to_owned());
    }
    let root = PathBuf::from(str_field(&meta, "root"));
    let head = str_field(&meta, "head");
    remove_tree(dest)?;
    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let tar = dest.join(".snapshot.tar");
    git(&root, &["archive", "--format=tar", "-o", &tar.to_string_lossy(), head]).await?;
    let out = Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(dest).output().await.map_err(|error| format!("tar: {error}"))?;
    let _ = std::fs::remove_file(&tar);
    if !out.status.success() {
        return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let patch = dir.join("diff.patch");
    if std::fs::metadata(&patch).map_err(|error| format!("{}: {error}", patch.display()))?.len() > 0 {
        let out = Command::new("git").arg("apply").arg("--binary").arg(&patch).current_dir(dest).output().await.map_err(|error| format!("git apply: {error}"))?;
        if !out.status.success() {
            return Err(format!("git apply: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    let untracked = meta.get("untracked").and_then(Value::as_array).ok_or_else(|| "snapshot has no untracked-file manifest".to_owned())?;
    for entry in untracked {
        let name = entry.as_str().ok_or_else(|| "snapshot has an invalid untracked path".to_owned())?;
        if name.is_empty() || Path::new(name).components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
            return Err(format!("invalid untracked snapshot path: {name}"));
        }
        copy_entry(&dir.join("untracked").join(name), &dest.join(name))?;
    }
    // A repository of its own, so the run sees what it changed as it would at home.
    for args in [&["init", "-q"][..], &["add", "-A"][..], &["-c", "user.name=replay", "-c", "user.email=replay@localhost", "commit", "-q", "--no-verify", "-m", "snapshot"][..]] {
        git(dest, args).await?;
    }
    Ok(dest.join(str_field(&meta, "subdir")))
}

/// The arm a replay session runs (`with` or `without`), with the lesson's id and text.
pub fn arm(entries: &[JournalEntry]) -> Option<(String, String, String)> {
    entries
        .iter()
        .find(|entry| entry.kind == kinds::NOTE && str_field(&entry.data, "purpose") == NOTE_PURPOSE)
        .map(|entry| (str_field(&entry.data, "arm").to_owned(), str_field(&entry.data, "lesson").to_owned(), str_field(&entry.data, "lesson_text").to_owned()))
}

/// Owns a complete replay, including preparation and both arms.
pub struct ReplayGuard {
    pub cancel: CancellationToken,
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl ReplayGuard {
    pub fn new(cancel: CancellationToken, release: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self { cancel, release: Some(Box::new(release)) }
    }
}

impl Drop for ReplayGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// What runs a replay's turns: exclusive idle ownership and a turn awaited to its final state.
pub trait Replayer: Send + Sync {
    fn idle(&self) -> bool;
    fn begin(&self) -> Option<ReplayGuard>;
    fn turn(&self, session_id: String, text: String, cancel: CancellationToken) -> BoxFuture<'static, Result<String, RpcError>>;
}

/// A lesson to replay and the run it was learned from.
struct Candidate {
    item: KnowledgeItem,
    session_id: String,
    run_id: String,
    message: String,
    dom: bool,
}

fn is_dom(name: &str) -> bool {
    name.starts_with("lang:")
}

/// The run of journal entry `seq` and its first user message.
fn run_message(entries: &[JournalEntry], seq: u64) -> Option<(String, String)> {
    let run_id = entries.iter().find(|entry| entry.seq == seq).map(|entry| str_field(&entry.data, "run_id").to_owned()).filter(|run| !run.is_empty())?;
    let message = entries.iter().find(|entry| entry.kind == kinds::USER_MESSAGE && str_field(&entry.data, "run_id") == run_id).map(|entry| str_field(&entry.data, "text").trim().to_owned()).filter(|text| !text.is_empty())?;
    Some((run_id, message))
}

/// The oldest live lesson not replayed yet whose run has a snapshot, of the kind (INV or DOM) not
/// replayed last when there is one.
async fn candidate(host: &Host, paths: &Paths, last_dom: Option<bool>) -> Option<Candidate> {
    let listed = host.get::<KnowledgeListResult>(memory_methods::KNOWLEDGE_LIST, json!({"kind": knowledge::LESSON})).await.ok()?;
    let mut items: Vec<KnowledgeItem> = listed.items.into_iter().filter(|item| !item.evidence.iter().any(|uri| uri.starts_with("replay://"))).collect();
    items.sort_by_key(|item| item.created_ms);
    let mut found: Vec<Candidate> = Vec::new();
    for item in items {
        let Some((session_id, seq)) = item.evidence.iter().find_map(|uri| uri.strip_prefix("journal://")?.split_once('/').and_then(|(session, seq)| Some((session.to_owned(), seq.parse::<u64>().ok()?)))) else { continue };
        let Ok(entries) = host.journal_read(&session_id, 0).await else { continue };
        let Some((run_id, message)) = run_message(&entries, seq) else { continue };
        if !paths.has_snapshot(&session_id, &run_id) {
            continue;
        }
        let dom = is_dom(&item.name);
        found.push(Candidate { item, session_id, run_id, message, dom });
        if last_dom.is_none_or(|last| last != dom) {
            break;
        }
    }
    let wanted = last_dom.map(|last| !last);
    let index = found.iter().position(|c| wanted.is_none_or(|dom| c.dom == dom)).unwrap_or(0);
    (index < found.len()).then(|| found.swap_remove(index))
}

/// One arm: a fresh copy, a session on it marked with the arm, the run's message sent; its end.
async fn run_arm(host: &Host, paths: &Paths, replayer: &dyn Replayer, c: &Candidate, arm: &str, stamp: &str, cancel: &CancellationToken) -> Result<(String, String), String> {
    let check = || if cancel.is_cancelled() { Err("cancelled".to_owned()) } else { Ok(()) };
    check()?;
    let dest = paths.replays.join(format!("{stamp}-{arm}"));
    let workspace = materialize(paths, &c.session_id, &c.run_id, &dest).await?;
    check()?;
    let project = host.call(frame_methods::PROJECT_OPEN, json!({"root": workspace})).await.map_err(|error| error.message)?;
    check()?;
    let project_id = str_field(&project, "id").to_owned();
    let head: String = c.item.name.chars().take(60).collect();
    let session = host
        .session_create(&SessionCreateParams { project_id, kind: SessionKind::Chat, title: format!("Replay {arm}: {head}"), agent: None, parent: None, workspace: None, task_id: None, flow: Some(crate::chatflow::FLOW.to_owned()) })
        .await
        .map_err(|error| error.message)?;
    check()?;
    host.journal_append(&session.id, kinds::NOTE, json!({"text": "", "purpose": NOTE_PURPOSE, "host_message": false, "arm": arm, "lesson": c.item.id, "lesson_text": c.item.text, "of": format!("journal://{}/{}", c.session_id, c.run_id)}))
        .await
        .map_err(|error| error.message)?;
    check()?;
    let state = replayer.turn(session.id.clone(), c.message.clone(), cancel.clone()).await.map_err(|error| error.message)?;
    check()?;
    Ok((session.id, state))
}

/// Forget the replays' projects, with their sessions and journals: a finished replay leaves only
/// its result (and older replays left by a stopped app go too).
async fn forget(host: &Host, paths: &Paths) {
    let Ok(listed) = host.get::<ProjectListResult>(frame_methods::PROJECT_LIST, json!({})).await else { return };
    for project in listed.projects.into_iter().filter(|project| paths.is_replay(&project.root)) {
        let _ = host.call(frame_methods::PROJECT_DELETE, json!({"id": project.id})).await;
    }
}

/// Replay one candidate (see the module's doc). Returns whether one was replayed to a decision,
/// and the kind it was (for taking turns).
pub async fn replay_one(host: &Host, paths: &Paths, replayer: &dyn Replayer, last_dom: Option<bool>) -> Option<bool> {
    let guard = replayer.begin()?;
    let c = tokio::select! {
        biased;
        _ = guard.cancel.cancelled() => return None,
        candidate = candidate(host, paths, last_dom) => candidate?,
    };
    let stamp = format!("{}-{}", now_ms(), safe(&c.item.id));
    tracing::info!(lesson = %c.item.name, "replaying a lesson with and without it");
    let with = run_arm(host, paths, replayer, &c, "with", &stamp, &guard.cancel).await;
    let without = match &with {
        Ok((_, state)) if state != "cancelled" && !guard.cancel.is_cancelled() => run_arm(host, paths, replayer, &c, "without", &stamp, &guard.cancel).await,
        Ok(_) => Err("cancelled".to_owned()),
        Err(error) => Err(error.clone()),
    };
    for arm in ["with", "without"] {
        let _ = std::fs::remove_dir_all(paths.replays.join(format!("{stamp}-{arm}")));
    }
    forget(host, paths).await;
    let ((with_session, with_state), (without_session, without_state)) = match (with, without) {
        (Ok(with), Ok(without)) if with.1 != "cancelled" && without.1 != "cancelled" => (with, without),
        (with, without) => {
            tracing::info!(lesson = %c.item.name, with = ?with.err(), without = ?without.err(), "the replay decided nothing");
            return None;
        }
    };
    if guard.cancel.is_cancelled() {
        return None;
    }
    let verdict = match (with_state == "done", without_state == "done") {
        (true, false) => "helped",
        (false, true) => "hurt",
        _ => "no_observed_effect",
    };
    let evidence = format!("replay://{with_session}+{without_session}");
    let _ = host.call(memory_methods::KNOWLEDGE_UPSERT, json!({"kind": knowledge::LESSON, "name": c.item.name, "text": "", "by": "replay", "evidence": [evidence]})).await;
    let (helped, hurt) = match verdict {
        "helped" => (1, 0),
        "hurt" => (0, 1),
        _ => (0, 0),
    };
    if helped + hurt > 0
        && let Ok(counted) = host.get::<KnowledgeItem>(memory_methods::KNOWLEDGE_COUNT, json!({"id": c.item.id, "helped": helped, "hurt": hurt})).await
        && counted.hurt > counted.helped
        && counted.hurt + counted.helped >= 3
    {
        let _ = host.call(memory_methods::KNOWLEDGE_WITHDRAW, json!({"id": c.item.id, "reason": "replayed: the run without it went better", "by": "replay"})).await;
    }
    let row = json!({"ts_ms": now_ms(), "lesson": c.item.id, "name": c.item.name, "text": c.item.text, "dom": c.dom, "with": with_state, "without": without_state, "verdict": verdict, "evidence": evidence, "of": format!("journal://{}/{}", c.session_id, c.run_id)});
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(paths.results()) {
        use std::io::Write;
        let _ = writeln!(file, "{row}");
    }
    tracing::info!(lesson = %c.item.name, %with_state, %without_state, %verdict, "replayed a lesson");
    Some(c.dom)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as u64).unwrap_or(0)
}
