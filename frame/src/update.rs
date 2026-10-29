//! Frame updates (`docs/RELEASES.md`): released frames staged beside the app, the hand-over to the
//! newest one at start, and the restart onto it.
//!
//! - A frame release unpacks into `<home>/frame/<version>/` (the frame `CortexCode`, `cortex.toml`,
//!   `bin/`, `ui/`), Electron into `<home>/electron/<version>/`, linked into the frame's
//!   `ui/node_modules/electron`. `.ready` marks a completely unpacked version.
//! - The frame inside the app is never changed: it is the safe copy. At every start it hands over
//!   (`exec`, same process and arguments) to the newest staged frame newer than itself. A staged
//!   frame that did not get its plugins started twice in a row is marked bad and skipped, and the
//!   frame inside the app runs on.
//! - `frame.update.restart` shuts down as on quit and then executes the frame the app started with,
//!   which hands over again.

use crate::hub::FRAME_VERSION;
use cc_protocol::contracts::frame::{UpdateStageParams, UpdateStatus};
use cc_protocol::version::Version;
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The executable the app started with, passed on through hand-overs (restart runs it again).
const ORIGIN_ENV: &str = "CORTEX_FRAME_ORIGIN";
/// `1`: run this frame, never a staged one (the development start sets it).
const NO_HANDOVER_ENV: &str = "CORTEX_NO_HANDOVER";
/// The frame executable inside a staged frame folder.
const FRAME_EXE: &str = "CortexCode";
/// Marks a completely unpacked frame or Electron folder.
const READY: &str = ".ready";
/// What the hand-over remembers: `<home>/frame/handover.json`.
const STATE: &str = "handover.json";
/// Starts a staged frame gets before it is marked bad.
const MAX_ATTEMPTS: u32 = 2;
/// Staged frames kept besides the running one.
const KEEP: usize = 2;

static RESTART: AtomicBool = AtomicBool::new(false);
/// This frame never hands over (`--config`, `CORTEX_NO_HANDOVER=1`): its staged frames do not run.
static NO_HANDOVER: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, Serialize, Deserialize)]
struct HandoverState {
    /// The staged version being tried, until it has started its plugins once.
    #[serde(default)]
    trying: String,
    #[serde(default)]
    attempts: u32,
    /// Versions that started their plugins.
    #[serde(default)]
    good: Vec<String>,
    /// Versions that failed to, never handed over to again.
    #[serde(default)]
    bad: Vec<String>,
}

fn frames_dir(root: &Path) -> PathBuf {
    root.join("frame")
}

fn electron_root(root: &Path) -> PathBuf {
    root.join("electron")
}

fn read_state(root: &Path) -> HandoverState {
    std::fs::read(frames_dir(root).join(STATE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_state(root: &Path, state: &HandoverState) {
    let path = frames_dir(root).join(STATE);
    let written = std::fs::create_dir_all(frames_dir(root))
        .and_then(|()| serde_json::to_vec_pretty(state).map_err(std::io::Error::other))
        .and_then(|bytes| crate::home::write_atomic(&path, &bytes));
    if let Err(error) = written {
        eprintln!("cortex-frame: cannot write {}: {error}", path.display());
    }
}

/// Every completely staged frame: (version, folder).
fn staged(root: &Path) -> Vec<(Version, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(frames_dir(root)) else { return Vec::new() };
    let mut out: Vec<(Version, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = Version::parse(&name).filter(|version| version.to_string() == name)?;
            let dir = entry.path();
            (dir.join(READY).is_file() && dir.join(FRAME_EXE).is_file() && dir.join("cortex.toml").is_file()).then_some((version, dir))
        })
        .collect();
    out.sort();
    out
}

/// The newest staged frame newer than this one and not marked bad.
fn newest(root: &Path, state: &HandoverState) -> Option<(Version, PathBuf)> {
    let current = Version::parse(FRAME_VERSION)?;
    staged(root).into_iter().rev().find(|(version, _)| *version > current && !state.bad.contains(&version.to_string()))
}

/// At start, before anything else: hand over to the newest staged frame (does not return then).
/// Skipped with an explicit `--config` or `CORTEX_NO_HANDOVER=1`.
pub fn handover(explicit_config: bool, root: &Path) {
    if explicit_config || std::env::var(NO_HANDOVER_ENV).is_ok_and(|value| value == "1") {
        NO_HANDOVER.store(true, Ordering::Relaxed);
        return;
    }
    // A start while the app runs is refused by the home lock: it is no try of the staged frame
    // (counted, two such starts would mark a good frame bad).
    if crate::home::lock_held(&crate::home::Home::new(root.to_path_buf())) {
        return;
    }
    let mut state = read_state(root);
    let Some((version, dir)) = newest(root, &state) else { return };
    let target = dir.join(FRAME_EXE);
    let exe = std::env::current_exe().and_then(|exe| exe.canonicalize()).ok();
    if exe.is_some() && exe == target.canonicalize().ok() {
        return;
    }
    let name = version.to_string();
    if !state.good.contains(&name) {
        if state.trying == name {
            state.attempts += 1;
        } else {
            state.trying = name.clone();
            state.attempts = 1;
        }
        if state.attempts > MAX_ATTEMPTS {
            eprintln!("cortex-frame: the frame {name} did not start its plugins in {MAX_ATTEMPTS} starts; running {FRAME_VERSION}");
            state.bad.push(name);
            state.trying.clear();
            state.attempts = 0;
            write_state(root, &state);
            return;
        }
        write_state(root, &state);
    }
    let origin = std::env::var_os(ORIGIN_ENV).or_else(|| exe.map(PathBuf::into_os_string)).unwrap_or_default();
    let error = std::process::Command::new(&target).args(std::env::args_os().skip(1)).env(ORIGIN_ENV, origin).exec();
    eprintln!("cortex-frame: cannot start the frame {}: {error}; running {FRAME_VERSION}", target.display());
}

/// This start was refused (another frame runs): take back the try the hand-over counted for it.
pub fn not_a_try(root: &Path) {
    let mut state = read_state(root);
    if state.trying == FRAME_VERSION && state.attempts > 0 {
        state.attempts -= 1;
        write_state(root, &state);
    }
}

/// The plugins started: this frame version is good (a staged one is not tried as new again).
pub fn mark_good(root: &Path) {
    let mut state = read_state(root);
    let name = FRAME_VERSION.to_owned();
    if state.trying == name {
        state.trying.clear();
        state.attempts = 0;
    }
    if staged(root).iter().any(|(version, _)| version.to_string() == name) && !state.good.contains(&name) {
        state.good.push(name);
    }
    write_state(root, &state);
}

/// The staged frame the next start runs, if any. Plugin packages wait for that start
/// (`units::install`): swapped into this frame, they would race the restart.
pub fn restart_pending(root: &Path) -> Option<String> {
    if NO_HANDOVER.load(Ordering::Relaxed) {
        return None;
    }
    newest(root, &read_state(root)).map(|(version, _)| version.to_string())
}

/// The running version and the newest staged one.
pub fn status(root: &Path) -> UpdateStatus {
    let state = read_state(root);
    let electron = std::fs::read_dir(electron_root(root))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().join(READY).is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    UpdateStatus { running: FRAME_VERSION.into(), staged: newest(root, &state).map(|(version, _)| version.to_string()), electron }
}

/// An Electron version usable as a folder name.
fn valid_electron(version: &str) -> bool {
    !version.is_empty() && version.len() <= 64 && !version.starts_with('.') && version.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// Unpack a `.tar.gz` into a new folder `dir` with the system's tar (it keeps the symbolic links
/// an Electron framework holds, and refuses entries outside the folder).
fn untar(archive: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let output = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("cannot unpack {}: {}", archive.display(), String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn staging(parent: &Path) -> PathBuf {
    parent.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()))
}

/// `frame.update.stage`: unpack a downloaded frame (and its Electron when new) beside the others.
pub fn stage(root: &Path, params: &UpdateStageParams) -> Result<UpdateStatus, String> {
    let version = Version::parse(&params.version).ok_or_else(|| format!("`{}` is not a frame version", params.version))?.to_string();
    if !valid_electron(&params.electron_version) {
        return Err(format!("`{}` is not an Electron version", params.electron_version));
    }
    let electron = electron_root(root).join(&params.electron_version);
    if !electron.join(READY).is_file() {
        if params.electron_archive.is_empty() {
            return Err(format!("Electron {} is not staged and no archive was given", params.electron_version));
        }
        let unpacked = staging(&electron_root(root));
        let outcome = untar(Path::new(&params.electron_archive), &unpacked).and_then(|()| {
            if !unpacked.join("path.txt").is_file() {
                return Err("the Electron archive has no path.txt".into());
            }
            if electron.exists() {
                std::fs::remove_dir_all(&electron).map_err(|e| format!("cannot replace {}: {e}", electron.display()))?;
            }
            std::fs::rename(&unpacked, &electron).map_err(|e| format!("cannot store Electron: {e}"))?;
            std::fs::write(electron.join(READY), b"").map_err(|e| e.to_string())
        });
        if outcome.is_err() {
            let _ = std::fs::remove_dir_all(&unpacked);
        }
        outcome?;
    }
    let target = frames_dir(root).join(&version);
    let running = std::env::current_exe().and_then(|exe| exe.canonicalize()).ok();
    if running.as_deref().is_some_and(|exe| exe.starts_with(&target)) {
        return Err(format!("the frame {version} is the one running"));
    }
    let unpacked = staging(&frames_dir(root));
    let outcome = untar(Path::new(&params.frame_archive), &unpacked).and_then(|()| {
        if !unpacked.join(FRAME_EXE).is_file() || !unpacked.join("cortex.toml").is_file() {
            return Err(format!("the frame archive has no {FRAME_EXE} or cortex.toml"));
        }
        let modules = unpacked.join("ui/node_modules");
        std::fs::create_dir_all(&modules).map_err(|e| format!("cannot create {}: {e}", modules.display()))?;
        let link = modules.join("electron");
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).or_else(|_| std::fs::remove_dir_all(&link)).map_err(|e| format!("cannot replace {}: {e}", link.display()))?;
        }
        std::os::unix::fs::symlink(&electron, &link).map_err(|e| format!("cannot link Electron: {e}"))?;
        if target.exists() {
            std::fs::remove_dir_all(&target).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
        }
        std::fs::rename(&unpacked, &target).map_err(|e| format!("cannot store the frame: {e}"))?;
        std::fs::write(target.join(READY), b"").map_err(|e| e.to_string())
    });
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&unpacked);
    }
    outcome?;
    // A version staged again gets its tries again.
    let mut state = read_state(root);
    state.bad.retain(|bad| *bad != version);
    write_state(root, &state);
    prune(root, running.as_deref());
    tracing::info!(%version, "a frame update is staged; it runs from the next start");
    Ok(status(root))
}

/// Keep the running frame and the newest [`KEEP`] staged ones, and the Electron versions they use.
fn prune(root: &Path, running: Option<&Path>) {
    let all = staged(root);
    let keep: Vec<&PathBuf> = all.iter().rev().take(KEEP).map(|(_, dir)| dir).collect();
    for (_, dir) in &all {
        if keep.contains(&dir) || running.is_some_and(|exe| exe.starts_with(dir)) {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), %error, "cannot remove an old frame version");
        }
    }
    let used: Vec<PathBuf> = staged(root).iter().filter_map(|(_, dir)| std::fs::read_link(dir.join("ui/node_modules/electron")).ok()).collect();
    let Ok(entries) = std::fs::read_dir(electron_root(root)) else { return };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".staging-") || (dir.join(READY).is_file() && !used.contains(&dir)) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// `frame.update.restart`: shut down, then [`exec_restart`].
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
}

pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// After the shutdown: run the frame the app started with again (it hands over to the newest).
pub fn exec_restart() {
    let origin = std::env::var_os(ORIGIN_ENV).map(PathBuf::from).or_else(|| std::env::current_exe().ok());
    let Some(origin) = origin else {
        eprintln!("cortex-frame: cannot tell which frame to restart");
        return;
    };
    let error = std::process::Command::new(&origin).args(std::env::args_os().skip(1)).exec();
    eprintln!("cortex-frame: cannot restart {}: {error}", origin.display());
}
