//! Code changes waiting for the user: a proposal holds the change in a draft commit and shows a
//! card (in the conversation that made it, else under "Waiting for you"). Approving puts it on the
//! code folder's branch, builds and packs every plugin it affects and installs them into the running
//! frame (which probes the new version and rolls back a crash). A build or install that fails takes
//! the change back off the branch; the draft stays.

use crate::paths;
use crate::proc;
use crate::service::{Service, rpc};
use crate::{capability, code, summary};
use cc_protocol::contracts::frame::{InstallManyResult, JournalAppendResult, Project, SessionKind, SessionListResult, methods as frame_methods};
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
    let open: Vec<Proposal> = service.proposals()?.into_iter().filter(|p| matches!(p.state.as_str(), "pending" | "building" | "installing")).collect();
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
    let draft_id = id.clone();
    let draft = service.git(move |repo| code::describe_draft(repo, &draft_id, draft, &text)).await?;
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
        p.commit = commit;
    })?;
    service.stored_proposals.flush().await?;
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
    let (proposal, started) = service.start_build(&proposal.id)?;
    if !started { return Ok(proposal); }
    if let Err(error) = service.stored_proposals.flush().await {
        service.update_proposal(&proposal.id, |p| p.state = "pending".into())?;
        return Err(error);
    }
    show(ctx, &proposal).await;
    service.changed(ctx);
    let (service, ctx, building) = (service.clone(), ctx.clone(), proposal.clone());
    tokio::spawn(async move {
        let (state, outcome, commit) = build(&service, &ctx, &building, draft).await;
        if let Err(error) = set_state(&service, &ctx, &building.id, state, outcome, commit).await {
            tracing::error!(%error.message, proposal = %building.id, "cannot record the approved build outcome");
        }
        crate::backup::after_change(&service, &ctx).await;
    });
    Ok(proposal)
}

/// Build only the approved commit's complete source tree. Finish every artifact before the frame
/// owns activation and recovery; a source-manager hot swap must not kill its own coordinator.
async fn build(service: &Arc<Service>, ctx: &Ctx, proposal: &Proposal, draft: Oid) -> (&'static str, String, Option<String>) {
    let commit = match service.git(move |repo| code::apply_draft(repo, draft)).await {
        Ok(oid) => oid,
        Err(error) => return ("failed", error.message, None),
    };
    if let Err(error) = set_state(service, ctx, &proposal.id, "building", String::new(), Some(commit.to_string())).await {
        return failed_build(service, commit, format!("The approved commit could not be recorded: {}", error.message)).await;
    }
    let snapshot = service.data_dir.join(format!("build-{}", uuid::Uuid::new_v4().simple()));
    let destination = snapshot.clone();
    if let Err(error) = service.git(move |repo| code::export_commit(repo, commit, &destination)).await {
        let _ = std::fs::remove_dir_all(&snapshot);
        return failed_build(service, commit, error.message).await;
    }
    let tools = capability::detect().await;
    let manifests = paths::plugin_manifests(&snapshot);
    let (plugins, parts) = paths::affected(&snapshot, &proposal.files);
    let mut packages = Vec::new();
    for plugin in &plugins {
        let failure = match manifests.get(plugin) {
            Some(manifest) => {
                let missing = capability::missing(manifest, &tools);
                if missing.is_empty() { None } else { Some(format!("This Mac cannot build {plugin}: it needs {}.", missing.join(", "))) }
            }
            None => Some(format!("The approved source has no manifest for {plugin}.")),
        };
        if let Some(failure) = failure {
            remove_packages(&packages);
            let _ = std::fs::remove_dir_all(&snapshot);
            return failed_build(service, commit, failure).await;
        }
        progress(ctx, &proposal.id, plugin, "started", format!("building {plugin}"));
        match build_package(service, ctx, &proposal.id, plugin, &snapshot).await {
            Ok(package) => packages.push(package),
            Err((_, failure)) => {
                progress(ctx, &proposal.id, plugin, "failed", failure.clone());
                remove_packages(&packages);
                let _ = std::fs::remove_dir_all(&snapshot);
                return failed_build(service, commit, failure).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&snapshot);
    let mut outcome = if plugins.is_empty() { String::new() } else { format!("Built and swapped: {}.", names(service, &plugins).join(", ")) };
    if !parts.is_empty() {
        outcome.push_str(&format!(" {} {} part of the app itself: the change is kept and reaches the app with its next update.", parts.join(", "), if parts.len() == 1 { "is" } else { "are" }));
    }
    if packages.is_empty() { return ("done", outcome.trim().to_owned(), Some(commit.to_string())); }
    // The completion message and commit are durable before dispatch. The successor waits on the
    // frame's own operation and finishes this card using its durable batch result.
    if let Err(error) = set_state(service, ctx, &proposal.id, "installing", outcome.trim().to_owned(), Some(commit.to_string())).await {
        remove_packages(&packages);
        return failed_build(service, commit, format!("Installation was not started: {}", error.message)).await;
    }
    let answer = ctx.call(frame_methods::PLUGINS_INSTALL_MANY, json!({"id": proposal.id, "paths": packages.iter().map(|p| p.display().to_string()).collect::<Vec<_>>() })).await;
    let result = match answer.and_then(|value| serde_json::from_value::<InstallManyResult>(value).map_err(|e| rpc(format!("unexpected installation answer: {e}")))) {
        Ok(result) => Ok(result),
        Err(error) => {
            // A disconnect or malformed response is not evidence that nothing was installed.
            match wait_install(ctx, &proposal.id).await {
                Ok(result) => Ok(result),
                Err(wait) if wait.code == cc_protocol::codes::NOT_FOUND => {
                    remove_packages(&packages);
                    return failed_build(service, commit, format!("Installation did not start: {}", error.message)).await;
                }
                Err(wait) => Err(format!("Installation outcome is unknown: {}. Recovery could not read its result: {}. The approved source and packages are retained.", error.message, wait.message)),
            }
        }
    };
    match result {
        Ok(result) => {
            if result.recovery_errors.is_empty() { remove_packages(&packages); }
            finish_install(service, commit, &outcome, result).await
        }
        Err(error) => ("failed", error, Some(commit.to_string())),
    }
}

fn remove_packages(packages: &[PathBuf]) {
    for package in packages { let _ = std::fs::remove_file(package); }
}

async fn failed_build(service: &Service, commit: Oid, failure: String) -> (&'static str, String, Option<String>) {
    match service.git(move |repo| code::revert(repo, &commit.to_string()).map(|_| ())).await {
        Ok(()) => ("failed", format!("{failure} The change was taken back off the code folder; it stays as a draft."), None),
        Err(error) => ("failed", format!("{failure} The source could not be restored: {}. The approved commit remains in History; your edits were kept.", error.message), Some(commit.to_string())),
    }
}

async fn wait_install(ctx: &Ctx, id: &str) -> Result<InstallManyResult, RpcError> {
    let value = ctx.call(frame_methods::PLUGINS_INSTALL_WAIT, json!({"id": id})).await?;
    serde_json::from_value(value).map_err(|e| rpc(format!("unexpected installation result: {e}")))
}

async fn finish_install(service: &Service, commit: Oid, outcome: &str, result: InstallManyResult) -> (&'static str, String, Option<String>) {
    if result.ok { return ("done", outcome.trim().to_owned(), Some(commit.to_string())); }
    let failure = result.error.unwrap_or_else(|| "Installation failed.".into());
    if !result.recovery_errors.is_empty() {
        return ("failed", format!("{failure} Previous plugin versions could not all be restored: {}. The approved source is retained for recovery.", result.recovery_errors.join("; ")), Some(commit.to_string()));
    }
    let (_, outcome, retained) = failed_build(service, commit, format!("{failure} Previous plugin versions were restored.")).await;
    (if retained.is_none() { "rolled_back" } else { "failed" }, outcome, retained)
}

fn progress(ctx: &Ctx, proposal: &str, plugin: &str, state: &str, detail: String) {
    let item = Progress { proposal: proposal.to_owned(), plugin: plugin.to_owned(), state: state.to_owned(), detail, ts_ms: now_ms() };
    ctx.publish(topics::PROGRESS, serde_json::to_value(item).unwrap_or(Value::Null));
}

/// `cortex-pack`: the configured one, else the one on the search path.
pub fn packer(service: &Service) -> Option<PathBuf> {
    service.config_path("packer").filter(|path| path.is_file()).or_else(|| proc::which("cortex-pack"))
}

/// Build and pack one plugin, without changing the running app.
async fn build_package(service: &Service, ctx: &Ctx, proposal: &str, plugin: &str, source: &Path) -> Result<PathBuf, (&'static str, String)> {
    let packer = packer(service).ok_or(("failed", "cortex-pack was not found, so nothing could be built.".to_owned()))?;
    let packages = service.data_dir.join("packages");
    std::fs::create_dir_all(&packages).map_err(|e| ("failed", format!("cannot create {}: {e}", packages.display())))?;
    let package = packages.join(format!("{proposal}-{plugin}-{}.ccplugin", uuid::Uuid::new_v4().simple()));
    let folder = source.join("plugins").join(plugin);
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
        let _ = std::fs::remove_file(&package);
        return Err(("failed", format!("Building {plugin} failed:\n{}", tail.join("\n"))));
    }
    Ok(package)
}

/// A successor of the source plugin waits for the frame-owned batch; a frame restart has already
/// restored interrupted batches before plugins start. A build interrupted before activation is safe
/// to take back because no package was activated.
pub async fn recover(service: &Arc<Service>, ctx: &Ctx) {
    let Ok(list) = service.proposals() else { return };
    for proposal in list.into_iter().filter(|p| matches!(p.state.as_str(), "building" | "installing")) {
        let commit = proposal.commit.as_deref().and_then(|id| Oid::from_str(id).ok());
        let (state, outcome, retained) = if proposal.state == "installing" {
            match wait_install(ctx, &proposal.id).await {
                Ok(result) => match commit {
                    Some(commit) => finish_install(service, commit, &proposal.outcome, result).await,
                    None => ("failed", "The installation result was found, but its source commit is missing; inspect History before changing the source.".into(), None),
                },
                Err(error) if error.code == cc_protocol::codes::NOT_FOUND => match commit {
                    Some(commit) => failed_build(service, commit, "The app stopped before installation started.".into()).await,
                    None => ("failed", "The app stopped before installation started.".into(), None),
                },
                Err(error) => ("failed", format!("The installation outcome could not be recovered: {}. The approved source is retained; inspect installed versions before reverting it.", error.message), proposal.commit.clone()),
            }
        } else {
            match commit {
                Some(commit) => failed_build(service, commit, "The app stopped while this change was being built; no plugins were activated.".into()).await,
                None => ("failed", "The app stopped before the approved build started.".into(), None),
            }
        };
        if matches!(state, "done" | "rolled_back") || (state == "failed" && retained.is_none()) {
            if let Ok(packages) = std::fs::read_dir(service.data_dir.join("packages")) {
                let prefix = format!("{}-", proposal.id);
                for package in packages.flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)) {
                    let _ = std::fs::remove_file(package.path());
                }
            }
        }
        if let Err(error) = set_state(service, ctx, &proposal.id, state, outcome, retained).await {
            tracing::error!(%error.message, proposal = %proposal.id, "cannot record installation recovery");
        }
    }
    service.changed(ctx);
}

/// Build and install `plugins` from the code folder without a card (after an update or a restore
/// carried the user's own code onto a new base). Returns the plugins installed and the failures.
pub async fn rebuild(service: &Service, ctx: &Ctx, plugins: &[String]) -> (Vec<String>, Vec<String>) {
    if plugins.is_empty() { return (Vec::new(), Vec::new()); }
    let tools = capability::detect().await;
    let manifests = paths::plugin_manifests(&service.code_dir);
    if plugins.iter().any(|plugin| !manifests.get(plugin).is_some_and(|manifest| capability::missing(manifest, &tools).is_empty())) {
        return (Vec::new(), plugins.to_vec());
    }
    let mut packages = Vec::new();
    let id = uuid::Uuid::new_v4().simple().to_string();
    for plugin in plugins {
        match build_package(service, ctx, &id, plugin, &service.code_dir).await {
            Ok(package) => packages.push(package),
            Err((_, error)) => {
                tracing::error!(%error, "rebuilding the restored source failed; no plugins were activated");
                remove_packages(&packages);
                return (Vec::new(), plugins.to_vec());
            }
        }
    }
    let result = ctx.call(frame_methods::PLUGINS_INSTALL_MANY, json!({"id": id, "paths": packages.iter().map(|p| p.display().to_string()).collect::<Vec<_>>() })).await;
    remove_packages(&packages);
    match result.and_then(|value| serde_json::from_value::<InstallManyResult>(value).map_err(|e| rpc(e.to_string()))) {
        Ok(result) if result.ok => (plugins.to_vec(), Vec::new()),
        Ok(result) => {
            tracing::error!(error = ?result.error, recovery_errors = ?result.recovery_errors, "the rebuilt plugins could not all be activated");
            (Vec::new(), plugins.to_vec())
        }
        Err(error) => {
            tracing::error!(%error.message, "the rebuild installation outcome could not be read");
            (Vec::new(), plugins.to_vec())
        }
    }
}
