//! `--install`: hand a package to the frame through `<home>/plugins/.incoming/`.
//!
//! The package is written there under a hidden name and renamed into place, so the frame only ever
//! sees whole files. A running frame installs it at once (hot-swap, or a new plugin started) and
//! writes `<file>.result.json` beside it; a stopped frame installs it at its next start.
//!
//! Plugins read their live data (prompts, texts, stages, schemas, skills, scripts) from the code
//! folder, `<home>/code/<source subdir>/`, not from the installed package, and the frame refreshes
//! that folder only when the app brings a new source version. So an install from a repository also
//! copies the plugin's data folders into the code folder ([`refresh_live_data`]): what was packed
//! is what the new backend reads (seen: a day of prompt and script fixes installed as packages
//! while the app kept running the old copies).

use cc_protocol::contracts::frame::package::PluginToml;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The folders a plugin reads as live data, relative to its folder: the backend's declared assets
/// and `backend/data` (the chat flow's prompts and texts), whichever exist.
fn live_data_dirs(dir: &Path, toml: &PluginToml) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    if let Some(backend) = &toml.backend {
        for asset in &backend.assets {
            if dir.join(asset).is_dir() && !dirs.contains(asset) {
                dirs.push(asset.clone());
            }
        }
        let data = format!("{}/data", backend.dir.trim_end_matches('/'));
        if dir.join(&data).is_dir() && !dirs.contains(&data) {
            dirs.push(data);
        }
    }
    dirs
}

/// Mirror the plugin's live data folders into the code folder: every source file of those folders
/// (what git tracks or would track, so build caches stay out) is copied over, and files the code
/// folder has there that the plugin no longer has are removed. Returns a one-line summary, or None
/// when the plugin has no `[source]`, no data folders, or the code folder does not hold it yet (the
/// app brings the source at its first start; nothing to refresh before that).
pub fn refresh_live_data(dir: &Path, toml: &PluginToml, home: &Path) -> Result<Option<String>, String> {
    let Some(source) = &toml.source else { return Ok(None) };
    let dest_root = home.join("code").join(&source.subdir);
    if !dest_root.is_dir() {
        return Ok(None);
    }
    let dirs = live_data_dirs(dir, toml);
    if dirs.is_empty() {
        return Ok(None);
    }
    let files = crate::pack::source_files(dir)?;
    let mut copied = 0usize;
    let mut removed = 0usize;
    for data_dir in &dirs {
        let prefix = format!("{data_dir}/");
        let wanted: BTreeSet<&String> = files.iter().filter(|file| file.starts_with(&prefix)).collect();
        for file in &wanted {
            let from = dir.join(file);
            let to = dest_root.join(file);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            if !same_contents(&from, &to) {
                std::fs::copy(&from, &to).map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
                if let Ok(metadata) = std::fs::metadata(&from) {
                    let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(metadata.permissions().mode()));
                }
                copied += 1;
            }
        }
        let dest_dir = dest_root.join(data_dir);
        for existing in files_under(&dest_dir) {
            let relative = existing.strip_prefix(&dest_root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if !wanted.contains(&&relative) {
                std::fs::remove_file(&existing).map_err(|e| format!("cannot remove {}: {e}", existing.display()))?;
                removed += 1;
            }
        }
    }
    Ok(Some(format!("live data in {}: {copied} file(s) copied, {removed} removed ({})", dest_root.display(), dirs.join(", "))))
}

/// Every regular file below `dir` (empty when it does not exist).
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The frame's home: `CORTEX_HOME`, else `~/.cortexcode`.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("CORTEX_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cortexcode")
}

/// Whether a frame runs on `home` (its frame.json names a live process).
fn frame_running(home: &Path) -> bool {
    let Ok(bytes) = std::fs::read(home.join("run/frame.json")) else { return false };
    let pid = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|info| info["pid"].as_u64()).unwrap_or(0);
    pid > 0 && std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|status| status.success())
}

/// Drop `package` into the incoming folder and, when a frame runs, wait for its outcome.
pub fn install(package: &Path, home: &Path) -> Result<(), String> {
    let incoming = home.join("plugins/.incoming");
    std::fs::create_dir_all(&incoming).map_err(|e| format!("{}: {e}", incoming.display()))?;
    let name = package.file_name().ok_or("the package has no file name")?.to_string_lossy().into_owned();
    let target = incoming.join(&name);
    let result_path = incoming.join(format!("{name}.result.json"));
    let _ = std::fs::remove_file(&result_path);
    let staging = incoming.join(format!(".{name}.{}", std::process::id()));
    std::fs::copy(package, &staging).map_err(|e| format!("cannot copy into {}: {e}", incoming.display()))?;
    std::fs::rename(&staging, &target).map_err(|e| format!("cannot place {}: {e}", target.display()))?;
    if !frame_running(home) {
        println!("the frame is not running; it installs {name} at its next start");
        return Ok(());
    }
    println!("installing into the running frame ...");
    let started = Instant::now();
    let mut reported = Instant::now();
    loop {
        if let Ok(bytes) = std::fs::read(&result_path)
            && let Ok(result) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            let _ = std::fs::remove_file(&result_path);
            return report(&result);
        }
        if !frame_running(home) {
            return Err(format!("the frame stopped before installing {name}; it installs it at its next start"));
        }
        if reported.elapsed() >= Duration::from_secs(5) {
            println!("  still installing ({} s): the new version is starting and being probed", started.elapsed().as_secs());
            reported = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn report(result: &serde_json::Value) -> Result<(), String> {
    for step in result["steps"].as_array().into_iter().flatten() {
        let mark = if step["ok"].as_bool().unwrap_or(false) { "ok " } else { "FAIL" };
        println!("  {mark} {:<8} {}", step["step"].as_str().unwrap_or_default(), step["detail"].as_str().unwrap_or_default());
    }
    let plugin = result["plugin"].as_str().unwrap_or_default();
    // A frame update is staged: the package stays in the incoming folder for the restart.
    if result["waiting"].as_bool().unwrap_or(false) {
        println!("{}", result["error"].as_str().unwrap_or("the package installs when the app restarts"));
        return Ok(());
    }
    if result["ok"].as_bool().unwrap_or(false) {
        let from = result["from_version"].as_str().unwrap_or_default();
        let to = result["to_version"].as_str().unwrap_or_default();
        if from.is_empty() {
            println!("installed {plugin} {to}");
        } else {
            println!("installed {plugin} {to} (was {from}; `cx plugin rollback {plugin}` goes back)");
        }
        Ok(())
    } else {
        let error = result["error"].as_str().unwrap_or("the install failed");
        let kept = if result["rolled_back"].as_bool().unwrap_or(false) { "; the previous version keeps running" } else { "" };
        Err(format!("{error}{kept}"))
    }
}
