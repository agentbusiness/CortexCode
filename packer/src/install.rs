//! `--install`: hand a package to the frame through `<home>/plugins/.incoming/`.
//!
//! The package is written there under a hidden name and renamed into place, so the frame only ever
//! sees whole files. A running frame installs it at once (hot-swap, or a new plugin started) and
//! writes `<file>.result.json` beside it; a stopped frame installs it at its next start. The
//! package's data reaches the live folder through the source manager, which merges the data of
//! every installed package (`plugin.unit`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
