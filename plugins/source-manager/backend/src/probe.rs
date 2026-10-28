//! `plugin.probe`: the contract self-test, run for real on a temporary folder: a live folder is
//! created from a small package, an edit is committed, a newer package merges into it keeping the
//! edit, and the edit is reverted.

use crate::live;
use crate::packages::Package;
use crate::service::Service;
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use std::path::Path;
use std::time::Instant;

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

fn round_trip(dir: &Path) -> Result<String, String> {
    let (package_dir, folder) = (dir.join("package"), dir.join("live"));
    let skill = "plugins/demo/data/skills/demo.md";
    let write = |root: &Path, file: &str, text: &str| -> Result<(), String> {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    };
    write(&package_dir, "data/skills/demo.md", "Demo skill.\n\nStep one.\n")?;
    write(&package_dir, "data/skills/other.md", "Other skill.\n\nStep one.\n")?;
    let package = Package { id: "demo".into(), dir: package_dir.clone(), dirs: vec!["data".into()] };
    live::populate(&folder, std::slice::from_ref(&package))?;
    let repo = live::open(&folder)?;
    write(&folder, skill, "Demo skill.\n\nStep one, better.\n")?;
    let edit = live::commit_files(&repo, &[skill.to_owned()], &live::message("Edited demo.md", &[("Origin", "edit".into())]))?;
    write(&package_dir, "data/skills/other.md", "Other skill.\n\nStep two.\n")?;
    let updated = live::release(&repo, &package, "2")?;
    if updated.state != "merged" {
        return Err(format!("the newer package did not merge ({})", updated.state));
    }
    let merged = std::fs::read_to_string(folder.join(skill)).map_err(|e| e.to_string())?;
    if !merged.contains("better") {
        return Err("the update lost the user's edit".into());
    }
    live::revert(&repo, &edit.to_string())?;
    let reverted = std::fs::read_to_string(folder.join(skill)).map_err(|e| e.to_string())?;
    if reverted.contains("better") {
        return Err("the revert did not undo the edit".into());
    }
    let history = live::history(&repo, 10)?;
    let origins: Vec<&str> = history.iter().map(|c| c.origin.as_str()).collect();
    // The shipped root commit is the starting point, not a change: History leaves it out.
    if origins != ["revert", "update", "edit"] {
        return Err(format!("unexpected history {origins:?}"));
    }
    Ok("created, edited, updated and reverted a live folder".into())
}

pub async fn run(service: &Service) -> ProbeReport {
    let mut checks = Vec::new();

    let started = Instant::now();
    let dir = service.data_dir.join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let probe_dir = dir.clone();
    let outcome = tokio::task::spawn_blocking(move || round_trip(&probe_dir)).await.unwrap_or_else(|e| Err(e.to_string()));
    let _ = std::fs::remove_dir_all(&dir);
    checks.push(check("live folder on a temporary copy", started, outcome));

    let started = Instant::now();
    let state = if !service.live_dir.exists() {
        "not created yet".to_owned()
    } else if service.managed() {
        format!("managed at {}", service.live_dir.display())
    } else {
        format!("a developer checkout at {} (read only)", service.live_dir.display())
    };
    checks.push(check("live folder", started, Ok(state)));

    ProbeReport::from_checks(checks)
}
