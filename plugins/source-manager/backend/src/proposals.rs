//! Code changes waiting for the user: a proposal holds the change in a draft commit and shows a
//! card (in the conversation that made it, else under "Waiting for you"). Approving puts it on the
//! code folder's branch, builds and packs every plugin it affects and installs them into the running
//! frame (which probes the new version and rolls back a crash). A build or install that fails takes
//! the change back off the branch; the draft stays.

use crate::paths;
use crate::proc;
use crate::service::{Service, rpc};
use crate::{capability, code, summary};
use cc_protocol::contracts::frame::{JournalAppendResult, PackageSwapResult, Project, SessionKind, SessionListResult, methods as frame_methods};
use cc_protocol::contracts::source::{CHANGE_ENTRY, DecideParams, Progress, Proposal, ProposeParams, topics};
use cc_protocol::{Ctx, RpcError};
use git2::Oid;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Lines of build output kept for a failure's outcome.
const FAILURE_LINES: usize = 12;

fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

/// Code files changed in the work tree that no open proposal holds yet.
pub async fn unproposed(service: &Service) -> Result<Vec<String>, RpcError> {
    let open: Vec<Proposal> = service.proposals()?.into_iter().filter(|p| matches!(p.state.as_str(), "pending" | "building")).collect();
    service
        .git(move |repo| {
            let changed: Vec<String> = code::changed_paths(repo)?.into_iter().filter(|file| !paths::is_data(file)).collect();
            let mut out = Vec::new();
            for file in changed {
                let held = open.iter().any(|p| {
                    p.files.contains(&file) && Oid::from_str(&p.draft).ok().is_some_and(|draft| code::work_matches(repo, draft, std::slice::from_ref(&file)).unwrap_or(false))
                });
                if !held {
                    out.push(file);
                }
            }
            Ok(out)
        })
        .await
}

/// The session a change in the code folder was made in: the newest chat in the project whose root
/// is the code folder.
async fn session_for(ctx: &Ctx, code_dir: &Path) -> Option<String> {
    let projects: Vec<Project> = serde_json::from_value(ctx.call(frame_methods::PROJECT_LIST, json!({})).await.ok()?.get("projects")?.clone()).ok()?;
    let code = code_dir.canonicalize().ok()?;
    let project = projects.into_iter().find(|p| Path::new(&p.root).canonicalize().ok().as_deref() == Some(code.as_path()))?;
    let list: SessionListResult = serde_json::from_value(ctx.call(frame_methods::SESSION_LIST, json!({"project_id": project.id})).await.ok()?).ok()?;
    list.sessions.into_iter().filter(|s| s.kind == SessionKind::Chat).max_by_key(|s| s.updated_ms).map(|s| s.id)
}

/// Whether a chat in the code folder's project is running (its changes are not finished yet).
pub async fn code_session_running(ctx: &Ctx, code_dir: &Path) -> bool {
    let Some(session) = session_for(ctx, code_dir).await else { return false };
    let Ok(value) = ctx.call(frame_methods::SESSION_GET, json!({"id": session})).await else { return false };
    value.get("status").and_then(Value::as_str) == Some("running")
}

fn names(service: &Service, ids: &[String]) -> Vec<String> {
    let known = paths::plugin_names(&service.code_dir);
    ids.iter().map(|id| known.get(id).filter(|name| !name.is_empty()).cloned().unwrap_or_else(|| id.clone())).collect()
}

/// Turn the work tree's unproposed code changes into a proposal and show its card.
pub async fn propose(service: &Arc<Service>, ctx: &Ctx, params: ProposeParams) -> Result<Proposal, RpcError> {
    if !service.managed() {
        return Err(RpcError::invalid_params("the code folder is a developer checkout: build and install from it directly"));
    }
    let files = unproposed(service).await?;
    if files.is_empty() {
        return Err(RpcError::invalid_params("there are no code changes to propose"));
    }
    let (mut plugins, parts) = paths::affected(&service.code_dir, &files);
    plugins.extend(parts);
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
    // The draft first, so the card describes exactly what it holds.
    let (draft_files, draft_id) = (files.clone(), id.clone());
    let probe_message = code::message("Proposed change", &[]);
    let draft = service.git(move |repo| code::draft(repo, &draft_id, &draft_files, &probe_message)).await?;
    let diffs = service.git(move |repo| code::diff_commit(repo, draft)).await?;
    let card = match params.summary.filter(|s| !s.trim().is_empty()) {
        Some(summary) => summary::Card { summary, why: params.why.unwrap_or_default(), risk: params.risk.unwrap_or_default() },
        None => match summary::write(ctx, &names(service, &plugins), &diffs).await {
            Some(card) => card,
            None => summary::fallback(&names(service, &plugins), &files),
        },
    };
    // The draft again, now with the message the history will show once it is approved.
    let text = code::message(&card.summary, &[("Origin", "approved".into()), ("Plugins", plugins.join(", "))]);
    let (draft_files, draft_id) = (files.clone(), id.clone());
    let draft = service.git(move |repo| code::draft(repo, &draft_id, &draft_files, &text)).await?;
    let session_id = match params.session_id {
        Some(session) => Some(session),
        None => session_for(ctx, &service.code_dir).await,
    };
    let mut proposal = Proposal {
        id: id.clone(),
        created_ms: now_ms(),
        plugins,
        summary: card.summary,
        why: card.why,
        risk: card.risk,
        files,
        session_id: session_id.clone(),
        entry_seq: None,
        state: "pending".into(),
        outcome: String::new(),
        draft: draft.to_string(),
        commit: None,
    };
    if let Some(session) = &session_id {
        let appended = ctx.call(frame_methods::JOURNAL_APPEND, json!({"session_id": session, "kind": CHANGE_ENTRY, "data": proposal})).await;
        if let Ok(value) = appended
            && let Ok(result) = serde_json::from_value::<JournalAppendResult>(value)
        {
            proposal.entry_seq = Some(result.seq);
        }
    }
    service.add_proposal(proposal.clone())?;
    service.changed(ctx);
    Ok(proposal)
}

/// Mirror a proposal's state onto its card in the conversation.
async fn show(ctx: &Ctx, proposal: &Proposal) {
    let (Some(session), Some(seq)) = (&proposal.session_id, proposal.entry_seq) else { return };
    for (field, value) in [("state", json!(proposal.state)), ("outcome", json!(proposal.outcome)), ("commit", json!(proposal.commit))] {
        let _ = ctx.call(frame_methods::JOURNAL_PATCH, json!({"session_id": session, "seq": seq, "field": field, "value": value})).await;
    }
}

async fn set_state(service: &Service, ctx: &Ctx, id: &str, state: &str, outcome: String, commit: Option<String>) -> Result<Proposal, RpcError> {
    let proposal = service.update_proposal(id, |p| {
        p.state = state.to_owned();
        p.outcome = outcome;
        if commit.is_some() {
            p.commit = commit;
        }
    })?;
    show(ctx, &proposal).await;
    service.changed(ctx);
    Ok(proposal)
}

/// Approve or decline a proposal. Approving returns at once with state `building`; the build runs
/// in the background and the card follows it.
pub async fn decide(service: &Arc<Service>, ctx: &Ctx, params: DecideParams) -> Result<Proposal, RpcError> {
    let proposal = service.proposals()?.into_iter().find(|p| p.id == params.id).ok_or_else(|| RpcError::invalid_params(format!("no proposal `{}`", params.id)))?;
    if proposal.state != "pending" {
        return Ok(proposal);
    }
    let draft = Oid::from_str(&proposal.draft).map_err(|e| rpc(e.message()))?;
    if !params.approve {
        let files = proposal.files.clone();
        service
            .git(move |repo| if code::work_matches(repo, draft, &files)? { code::discard(repo, &files) } else { Ok(()) })
            .await?;
        return set_state(service, ctx, &proposal.id, "declined", "Kept as a draft; nothing was built.".into(), None).await;
    }
    let proposal = set_state(service, ctx, &proposal.id, "building", String::new(), None).await?;
    let (service, ctx, building) = (service.clone(), ctx.clone(), proposal.clone());
    tokio::spawn(async move {
        let (state, outcome, commit) = build(&service, &ctx, &building, draft).await;
        let _ = set_state(&service, &ctx, &building.id, state, outcome, commit).await;
        crate::backup::after_change(&service, &ctx).await;
    });
    Ok(proposal)
}

/// Put the change on the branch, then build and install what it affects. Returns the final state,
/// the outcome in plain words and the commit (when it stays on the branch).
async fn build(service: &Arc<Service>, ctx: &Ctx, proposal: &Proposal, draft: Oid) -> (&'static str, String, Option<String>) {
    let tools = capability::detect().await;
    let manifests = paths::plugin_manifests(&service.code_dir);
    let (plugins, parts) = paths::affected(&service.code_dir, &proposal.files);
    for plugin in &plugins {
        if let Some(manifest) = manifests.get(plugin) {
            let missing = capability::missing(manifest, &tools);
            if !missing.is_empty() {
                return ("failed", format!("This Mac cannot build {plugin}: it needs {}.", missing.join(", ")), None);
            }
        }
    }
    let commit = match service.git(move |repo| code::apply_draft(repo, draft)).await {
        Ok(oid) => oid,
        Err(error) => return ("failed", error.message, None),
    };
    // Recorded at once: a restart during the build finds the change on the branch ([`recover`]).
    let _ = service.update_proposal(&proposal.id, |p| p.commit = Some(commit.to_string()));
    let mut done: Vec<String> = Vec::new();
    for plugin in &plugins {
        progress(ctx, &proposal.id, plugin, "started", format!("building {plugin}"));
        match build_and_install(service, ctx, &proposal.id, plugin).await {
            Ok(note) => {
                progress(ctx, &proposal.id, plugin, "done", note);
                done.push(plugin.clone());
            }
            Err((state, failure)) => {
                progress(ctx, &proposal.id, plugin, "failed", failure.clone());
                // The branch goes back to what runs; the draft keeps the change.
                let reverted = service.git(move |repo| code::revert(repo, &commit.to_string()).map(|_| ())).await;
                let _ = service.update_proposal(&proposal.id, |p| p.commit = None);
                let back = match reverted {
                    Ok(()) => "The change was taken back off the code folder; it stays as a draft.",
                    Err(_) => "The change is still in the code folder; revert it in History.",
                };
                let installed = if done.is_empty() { String::new() } else { format!(" Already installed: {}.", done.join(", ")) };
                return (state, format!("{failure}{installed} {back}"), None);
            }
        }
    }
    let mut outcome = if plugins.is_empty() { String::new() } else { format!("Built and swapped: {}.", names(service, &plugins).join(", ")) };
    if !parts.is_empty() {
        outcome.push_str(&format!(
            " {} {} part of the app itself: the change is kept and reaches the app with its next update.",
            parts.join(", "),
            if parts.len() == 1 { "is" } else { "are" }
        ));
    }
    ("done", outcome.trim().to_owned(), Some(commit.to_string()))
}

fn progress(ctx: &Ctx, proposal: &str, plugin: &str, state: &str, detail: String) {
    let item = Progress { proposal: proposal.to_owned(), plugin: plugin.to_owned(), state: state.to_owned(), detail, ts_ms: now_ms() };
    ctx.publish(topics::PROGRESS, serde_json::to_value(item).unwrap_or(Value::Null));
}

/// `cortex-pack`: the configured one, else the one on the search path.
pub fn packer(service: &Service) -> Option<PathBuf> {
    service.config_path("packer").filter(|path| path.is_file()).or_else(|| proc::which("cortex-pack"))
}

/// Build and pack one plugin from the code folder and install it into the running frame. A failure
/// is `(state, plain outcome)`: `failed` for a build that did not finish, `rolled_back` when the
/// frame put the previous version back.
pub async fn build_and_install(service: &Service, ctx: &Ctx, proposal: &str, plugin: &str) -> Result<String, (&'static str, String)> {
    let packer = packer(service).ok_or(("failed", "cortex-pack was not found, so nothing could be built.".to_owned()))?;
    let packages = service.data_dir.join("packages");
    std::fs::create_dir_all(&packages).map_err(|e| ("failed", format!("cannot create {}: {e}", packages.display())))?;
    let package = packages.join(format!("{plugin}-{}.ccplugin", now_ms()));
    let folder = service.code_dir.join("plugins").join(plugin);
    let mut cmd = proc::command(&packer, &folder);
    cmd.arg("build").arg(&folder).arg("-o").arg(&package);
    let mut tail: Vec<String> = Vec::new();
    let status = proc::stream(cmd, |line| {
        if !line.overwrite && !line.text.trim().is_empty() {
            progress(ctx, proposal, plugin, "output", line.text.clone());
            tail.push(line.text);
            if tail.len() > FAILURE_LINES {
                tail.remove(0);
            }
        }
    })
    .await
    .map_err(|e| ("failed", format!("cortex-pack could not start: {e}")))?;
    if !status.success() {
        return Err(("failed", format!("Building {plugin} failed:\n{}", tail.join("\n"))));
    }
    let value = ctx
        .call(frame_methods::PLUGINS_INSTALL, json!({"path": package.display().to_string()}))
        .await
        .map_err(|e| ("failed", format!("Installing {plugin} failed: {}", e.message)))?;
    let _ = std::fs::remove_file(&package);
    let result: PackageSwapResult = serde_json::from_value(value).map_err(|e| ("failed", format!("Installing {plugin}: unexpected answer ({e})")))?;
    if result.ok {
        Ok(format!("{plugin} {} is running", result.to_version))
    } else if result.rolled_back {
        Err(("rolled_back", format!("{plugin} did not start properly, so the previous version was put back: {}", result.error.unwrap_or_default())))
    } else {
        Err(("failed", format!("Installing {plugin} failed: {}", result.error.unwrap_or_default())))
    }
}

/// After a start: a proposal still `building` was cut off by a restart. Its change is taken back off
/// the branch (the draft stays) and the card says so.
pub async fn recover(service: &Arc<Service>, ctx: &Ctx) {
    let Ok(list) = service.proposals() else { return };
    for proposal in list.into_iter().filter(|p| p.state == "building") {
        let mut outcome = "The app stopped while this change was being built.".to_owned();
        if let Some(commit) = proposal.commit.clone() {
            match service.git(move |repo| code::revert(repo, &commit).map(|_| ())).await {
                Ok(()) => outcome.push_str(" It was taken back off the code folder; it stays as a draft."),
                Err(error) => outcome.push_str(&format!(" It is still in the code folder ({}); revert it in History.", error.message)),
            }
        }
        if let Ok(updated) = service.update_proposal(&proposal.id, |p| {
            p.state = "failed".into();
            p.outcome = outcome;
            p.commit = None;
        }) {
            show(ctx, &updated).await;
        }
    }
    service.changed(ctx);
}

/// Build and install `plugins` from the code folder without a card (after an update or a restore
/// carried the user's own code onto a new base). Returns the plugins installed and the failures.
pub async fn rebuild(service: &Service, ctx: &Ctx, plugins: &[String]) -> (Vec<String>, Vec<String>) {
    let tools = capability::detect().await;
    let manifests = paths::plugin_manifests(&service.code_dir);
    let (mut built, mut failed) = (Vec::new(), Vec::new());
    for plugin in plugins {
        let buildable = manifests.get(plugin).is_some_and(|manifest| capability::missing(manifest, &tools).is_empty());
        if !buildable {
            failed.push(plugin.clone());
            continue;
        }
        match build_and_install(service, ctx, "rebuild", plugin).await {
            Ok(_) => built.push(plugin.clone()),
            Err(_) => failed.push(plugin.clone()),
        }
    }
    (built, failed)
}
