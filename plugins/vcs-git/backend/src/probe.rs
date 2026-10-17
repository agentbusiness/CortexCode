//! `plugin.probe`: exercise the real implementation on a scratch repository.

use crate::checkpoint::shadow_dir;
use crate::service::Vcs;
use cc_protocol::RpcError;
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::vcs::{CheckpointsResult, Diff, RestoreResult, Status, methods};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

async fn call<T: serde::de::DeserializeOwned>(vcs: &Vcs, method: &str, params: Value) -> Result<T, String> {
    let value = vcs.dispatch(method, params).await.map_err(|e| format!("{method}: {e}"))?;
    serde_json::from_value(value).map_err(|e| format!("{method}: {e}"))
}

async fn git(vcs: &Vcs, dir: &Path, args: &[&str]) -> Result<String, String> {
    // The probe's own fixture commands must not depend on the user's global hooks, templates or
    // signing setup (a failing global pre-commit hook would otherwise fail the probe).
    vcs.git
        .cmd(dir)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "core.autocrlf=false"])
        .args(args.iter().copied())
        .env("GIT_TEMPLATE_DIR", "")
        .env("GIT_AUTHOR_NAME", "probe")
        .env("GIT_AUTHOR_EMAIL", "probe@cortexcode.invalid")
        .env("GIT_COMMITTER_NAME", "probe")
        .env("GIT_COMMITTER_EMAIL", "probe@cortexcode.invalid")
        .ok()
        .await
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .map_err(|e: RpcError| e.to_string())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

pub async fn run(vcs: &Vcs) -> ProbeReport {
    let mut checks = Vec::new();

    let started = Instant::now();
    let version = git(vcs, vcs.data_dir(), &["--version"]).await;
    let version_ok = version.as_ref().map_err(Clone::clone).and_then(|text| {
        let numbers: Vec<u32> = text
            .split_whitespace()
            .nth(2)
            .unwrap_or("")
            .split('.')
            .take(2)
            .filter_map(|n| n.parse().ok())
            .collect();
        // --path-format=absolute (rev-parse) needs git 2.31.
        if numbers.len() == 2 && (numbers[0], numbers[1]) >= (2, 31) {
            Ok(text.clone())
        } else {
            Err(format!("git 2.31 or newer is required, found `{text}`"))
        }
    });
    let git_ok = version_ok.is_ok();
    checks.push(check("git", started, version_ok));
    if !git_ok {
        return ProbeReport::from_checks(checks);
    }

    let dir = vcs.data_dir().join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let outcome = exercise(vcs, &dir, &mut checks).await;
    if let Err(detail) = outcome {
        checks.push(ProbeCheck { name: "setup".into(), ok: false, detail, ms: 0 });
    }
    if let Ok(canonical) = std::fs::canonicalize(&dir) {
        let _ = std::fs::remove_dir_all(shadow_dir(vcs.data_dir(), &canonical));
    }
    let _ = std::fs::remove_dir_all(&dir);
    ProbeReport::from_checks(checks)
}

async fn exercise(vcs: &Vcs, dir: &Path, checks: &mut Vec<ProbeCheck>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let root = std::fs::canonicalize(dir).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    let root_path = Path::new(&root);
    git(vcs, root_path, &["init", "--quiet", "--initial-branch=main"]).await?;
    write(&root_path.join("a.txt"), "one\ntwo\nthree\n")?;
    git(vcs, root_path, &["add", "a.txt"]).await?;
    git(vcs, root_path, &["commit", "--quiet", "-m", "probe base"]).await?;
    write(&root_path.join("a.txt"), "one\n2\nthree\n")?;
    write(&root_path.join("new.txt"), "x\ny\n")?;

    let started = Instant::now();
    let status: Result<Status, String> = call(vcs, methods::STATUS, json!({"root": root})).await;
    checks.push(check(
        "status",
        started,
        status.and_then(|s| {
            let a = s.files.iter().find(|f| f.path == "a.txt");
            let n = s.files.iter().find(|f| f.path == "new.txt");
            match (a, n) {
                (Some(a), Some(n))
                    if s.is_repo
                        && s.branch == "main"
                        && a.status == "modified"
                        && !a.staged
                        && (a.added, a.removed) == (1, 1)
                        && n.status == "untracked"
                        && n.added == 2 =>
                {
                    Ok(format!("{} files, +{} -{}", s.files.len(), s.added_lines, s.removed_lines))
                }
                _ => Err(format!("unexpected status: {:?}", s.files)),
            }
        }),
    ));

    let started = Instant::now();
    let diff: Result<Diff, String> = call(vcs, methods::DIFF, json!({"root": root, "scope": "unstaged"})).await;
    checks.push(check(
        "diff",
        started,
        diff.and_then(|d| {
            let lines: Vec<String> = d
                .files
                .iter()
                .flat_map(|f| f.hunks.iter())
                .flat_map(|h| h.lines.iter())
                .map(|l| format!("{}{}", l.kind, l.text))
                .collect();
            if d.files.len() == 1 && lines == [" one", "-two", "+2", " three"] {
                Ok("1 hunk".into())
            } else {
                Err(format!("unexpected diff lines: {lines:?}"))
            }
        }),
    ));

    let started = Instant::now();
    let staged: Result<Diff, String> = async {
        let _: Status = call(vcs, methods::STAGE, json!({"root": root, "paths": ["a.txt"], "hunk": 0})).await?;
        call(vcs, methods::DIFF, json!({"root": root, "scope": "staged"})).await
    }
    .await;
    checks.push(check(
        "stage_hunk",
        started,
        staged.and_then(|d| {
            let count = d.files.iter().map(|f| f.hunks.len()).sum::<usize>();
            if count == 1 && d.added == 1 && d.removed == 1 { Ok("hunk staged".into()) } else { Err(format!("staged diff: {d:?}")) }
        }),
    ));

    let started = Instant::now();
    let refs_before = git(vcs, root_path, &["rev-list", "--all"]).await?;
    let roundtrip: Result<String, String> = async {
        let cp: cc_protocol::contracts::vcs::Checkpoint =
            call(vcs, methods::CHECKPOINT, json!({"root": root, "label": "probe"})).await?;
        write(&root_path.join("a.txt"), "changed\n")?;
        write(&root_path.join("later.txt"), "later\n")?;
        let _: RestoreResult = call(vcs, methods::RESTORE, json!({"root": root, "id": cp.id})).await?;
        let a = std::fs::read_to_string(root_path.join("a.txt")).map_err(|e| e.to_string())?;
        let listed: CheckpointsResult = call(vcs, methods::CHECKPOINTS, json!({"root": root})).await?;
        let refs_after = git(vcs, root_path, &["rev-list", "--all"]).await?;
        if a != "one\n2\nthree\n" {
            Err(format!("a.txt after restore: {a:?}"))
        } else if root_path.join("later.txt").exists() {
            Err("later.txt survived the restore".into())
        } else if refs_after != refs_before {
            Err("the user's history changed".into())
        } else if listed.checkpoints.len() != 2 {
            Err(format!("expected checkpoint + backup, got {}", listed.checkpoints.len()))
        } else {
            Ok("restored; user history unchanged".into())
        }
    }
    .await;
    checks.push(check("checkpoint", started, roundtrip));
    Ok(())
}
