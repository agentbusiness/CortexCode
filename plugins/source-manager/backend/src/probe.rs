//! `plugin.probe`: the contract self-test, run for real on a temporary folder: a code folder is
//! created from a small shipped source, a data edit is committed and reverted, and an update of the
//! shipped source merges into it; then the tool detection runs.

use crate::service::Service;
use crate::{capability, code};
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
    let (source, folder) = (dir.join("source"), dir.join("code"));
    let skill = "plugins/demo/data/skills/demo.md";
    let write = |root: &Path, file: &str, text: &str| -> Result<(), String> {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    };
    write(&source, skill, "Demo skill.\n\nStep one.\n")?;
    write(&source, "plugins/demo/backend/src/lib.rs", "pub fn demo() {}\n")?;
    code::populate(&folder, &source)?;
    let repo = code::open(&folder)?;
    write(&folder, skill, "Demo skill.\n\nStep one, better.\n")?;
    let edit = code::commit_files(&repo, &[skill.to_owned()], &code::message("Edited demo.md", &[("Origin", "edit".into())]))?;
    write(&source, "plugins/demo/backend/src/lib.rs", "pub fn demo() -> u8 { 1 }\n")?;
    let updated = code::update(&repo, &source)?;
    if updated.state != "merged" {
        return Err(format!("the update of the shipped source did not merge ({})", updated.state));
    }
    let merged = std::fs::read_to_string(folder.join(skill)).map_err(|e| e.to_string())?;
    if !merged.contains("better") {
        return Err("the update lost the user's edit".into());
    }
    code::revert(&repo, &edit.to_string())?;
    let reverted = std::fs::read_to_string(folder.join(skill)).map_err(|e| e.to_string())?;
    if reverted.contains("better") {
        return Err("the revert did not undo the edit".into());
    }
    let history = code::history(&repo, 10)?;
    let origins: Vec<&str> = history.iter().map(|c| c.origin.as_str()).collect();
    if origins != ["revert", "update", "edit", "shipped"] {
        return Err(format!("unexpected history {origins:?}"));
    }
    Ok("created, edited, updated and reverted a code folder".into())
}

pub async fn run(service: &Service) -> ProbeReport {
    let mut checks = Vec::new();

    let started = Instant::now();
    let dir = service.data_dir.join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let probe_dir = dir.clone();
    let outcome = tokio::task::spawn_blocking(move || round_trip(&probe_dir)).await.unwrap_or_else(|e| Err(e.to_string()));
    let _ = std::fs::remove_dir_all(&dir);
    checks.push(check("code folder on a temporary copy", started, outcome));

    let started = Instant::now();
    let tools = capability::detect().await;
    let found: Vec<&str> = tools.iter().filter(|t| t.found).map(|t| t.name.as_str()).collect();
    checks.push(check("build tools", started, Ok(format!("found: {}", if found.is_empty() { "none".into() } else { found.join(", ") }))));

    let started = Instant::now();
    let state = if !service.code_dir.exists() {
        "not created yet".to_owned()
    } else if service.managed() {
        format!("managed at {}", service.code_dir.display())
    } else {
        format!("a developer checkout at {} (read only)", service.code_dir.display())
    };
    checks.push(check("code folder", started, Ok(state)));

    ProbeReport::from_checks(checks)
}
