//! The `tools@1` plugin: method dispatch, probe and state handoff.

use crate::core::{Config, Core};
use crate::exec::{self, Session};
use crate::host::{CtxHost, Host};
use crate::schemas::{self, EditFormat};
use crate::shell::RunRequest;
use crate::util;
use cc_protocol::contracts::frame::{ProjectDeleted, SessionDeleted, topics as frame_topics};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::model::ToolCall;
use cc_protocol::contracts::tools::{
    ApproveParams, ApproveResult, ArtifactParams, ArtifactResult, ExecContext, ExecuteParams, FileEntry, FilesParams, FilesResult,
    JobListParams, JobParams, JobStatus, NormalizeParams, NormalizeResult, ReadParams, ReadResult, RedoParams, RunParams, RunResult,
    SaveParams, SaveResult, SchemasParams, SkillsShownParams, TimelineDiffParams, TimelineParams, UndoParams, WorkspaceCreateParams, WorkspaceListParams,
    WorkspaceListResult, WorkspaceRemoveParams, methods, topics,
};
use crate::undo::Attribution;
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The methods a command in the model's shells may call (`cx`): its session's background jobs.
pub const COMMANDS: &[&str] = &[methods::JOB_LIST, methods::JOB_STATUS, methods::JOB_KILL];

pub const PROVIDES: &[&str] = &[
    methods::SCHEMAS,
    methods::EXECUTE,
    methods::READ,
    methods::RUN,
    methods::JOB_STATUS,
    methods::JOB_KILL,
    methods::JOB_LIST,
    methods::ARTIFACT,
    methods::WORKSPACE_CREATE,
    methods::WORKSPACE_REMOVE,
    methods::WORKSPACE_LIST,
    methods::FILES,
    methods::SAVE,
    methods::NORMALIZE,
    methods::APPROVE,
    methods::SKILLS_SHOWN,
    methods::CLEANUP,
    methods::TESTS_REPORT,
    methods::TESTS_BASELINE,
    methods::TIMELINE,
    methods::UNDO,
    methods::REDO,
    methods::TIMELINE_DIFF,
];

pub struct ToolsPlugin {
    core: Arc<tokio::sync::OnceCell<Arc<Core>>>,
    /// Set when this instance exported its state to a successor (a hot-swap, not a full stop).
    handed_off: Arc<std::sync::atomic::AtomicBool>,
    /// For tests and the probe: a core created before the frame connection exists.
    preset: Option<Arc<Core>>,
}

impl Default for ToolsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolsPlugin {
    pub fn new() -> Self {
        Self { core: Arc::new(tokio::sync::OnceCell::new()), handed_off: Arc::new(std::sync::atomic::AtomicBool::new(false)), preset: None }
    }

    pub fn with_core(core: Arc<Core>) -> Self {
        Self {
            core: Arc::new(tokio::sync::OnceCell::new()),
            handed_off: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            preset: Some(core),
        }
    }

    /// The shared state, created on first use. The frame's hello acknowledgement (which carries
    /// `[plugin.config]`) can be stored a moment after the first request arrives, so creation waits
    /// briefly for it before falling back to defaults.
    fn core(&self, ctx: &Ctx) -> impl std::future::Future<Output = Arc<Core>> + Send + 'static {
        let cell = self.core.clone();
        let preset = self.preset.clone();
        let ctx = ctx.clone();
        async move {
            if let Some(core) = preset {
                return core;
            }
            cell.get_or_init(|| async move {
                let mut config = ctx.config();
                for _ in 0..40 {
                    if !config.is_null() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    config = ctx.config();
                }
                let core = Core::new(&ctx.env.home, &ctx.env.data_dir, Config::from_value(&config));
                core.graph.attach(Arc::new(CtxHost(ctx.clone())));
                core.jobs.set_publisher(Arc::new(JobEnds { host: CtxHost(ctx.clone()), core: Arc::downgrade(&core) }));
                core.steps.set_publisher(Arc::new(CtxHost(ctx.clone())));
                core.jobs.adopt();
                crate::guard::Guard::start(core.jobs.clone(), core.shells.groups.clone());
                core
            })
            .await
            .clone()
        }
    }
}

/// Publishes job events as the frame connection does and, when a job ended, records what its
/// command changed in its workspace as that session's step.
struct JobEnds {
    host: CtxHost,
    core: std::sync::Weak<Core>,
}

impl crate::jobs::Publisher for JobEnds {
    fn publish(&self, topic: &str, data: Value) {
        let ended = data.get("state").and_then(Value::as_str).is_some_and(|state| state != "running");
        if ended
            && let Some(core) = self.core.upgrade()
            && let Some(job_id) = data.get("job_id").and_then(Value::as_str)
            && let Some((meta, _)) = core.jobs.status(job_id)
            && !meta.cwd.is_empty()
        {
            let who = Attribution::session(&meta.session_id, "run", &meta.command);
            let cwd = std::path::PathBuf::from(&meta.cwd);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let log = core.steps.open(&cwd).await;
                    let _ = tokio::task::spawn_blocking(move || log.reconcile(&who)).await;
                });
            } else if let Some(log) = core.steps.opened(&cwd) {
                log.reconcile(&who);
            }
        }
        crate::jobs::Publisher::publish(&self.host, topic, data);
    }
}

fn job_status(core: &Core, job_id: &str) -> Result<JobStatus, RpcError> {
    let (meta, tail) = core.jobs.status(job_id).ok_or_else(|| RpcError::not_found(format!("no job `{job_id}`")))?;
    Ok(JobStatus { job_id: meta.job_id, state: meta.state, exit_code: meta.exit_code, output_tail: tail, started_ms: meta.started_ms })
}

pub(crate) fn host_context(workspace: &str, project_id: Option<&str>, session_id: Option<&str>) -> ExecContext {
    ExecContext {
        project_id: project_id.unwrap_or("").to_owned(),
        session_id: session_id.map(str::to_owned).unwrap_or_else(|| format!("host-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])),
        workspace: workspace.to_owned(),
        mode: "auto".to_owned(),
        edit_format: None,
    }
}

pub async fn dispatch(core: Arc<Core>, host: Arc<dyn Host>, call: Call) -> Result<Value, RpcError> {
    let Call { method, params, items, cancel } = call;
    match method.as_str() {
        methods::SCHEMAS => {
            let params: SchemasParams = if params.is_null() { SchemasParams { edit_format: None, workspace: None } } else { parse_params(params)? };
            let offline = params
                .workspace
                .as_deref()
                .filter(|workspace| !workspace.is_empty())
                .is_some_and(|workspace| util::canonical_lenient(&util::expand_home(workspace)).join(crate::sandbox::OFFLINE_MARKER).exists());
            let mut result = schemas::schemas(EditFormat::parse(params.edit_format.as_deref()), offline);
            // The services this machine's apps share, one tool each.
            for spec in crate::services::specs(host.as_ref()).await {
                result.versions[spec.name.as_str()] = json!("service@1");
                result.tools.push(spec);
            }
            to_value(&result)
        }
        methods::EXECUTE => {
            let params: ExecuteParams = parse_params(params)?;
            // Run in its own task so a caller's cancel reaches the shell (which kills the process
            // group) instead of dropping the future mid-command.
            let child_cancel = cancel.child_token();
            let task = tokio::spawn(async move { exec::execute(&core, &host, params, items, child_cancel).await });
            let guard = CancelOnDrop(cancel.clone());
            let outcome = task.await.map_err(|error| RpcError::internal(format!("tool task failed: {error}")))?;
            drop(guard);
            to_value(&outcome)
        }
        methods::NORMALIZE => {
            let params: NormalizeParams = parse_params(params)?;
            let result = match crate::repair::normalize(&params.call.name, &params.call.arguments) {
                Ok(canonical) => NormalizeResult {
                    ok: true,
                    tool: canonical.tool.to_owned(),
                    args: Value::Object(canonical.args),
                    notes: canonical.notes,
                    error: None,
                },
                Err(error) => {
                    NormalizeResult { ok: false, tool: String::new(), args: Value::Null, notes: Vec::new(), error: Some(to_value(&error)?) }
                }
            };
            to_value(&result)
        }
        methods::READ => {
            let params: ReadParams = parse_params(params)?;
            let context = host_context(&params.workspace, params.project_id.as_deref(), params.session_id.as_deref());
            let session =
                Session { workspace: util::canonical_lenient(&util::expand_home(&params.workspace)), context: &context, python_env: false };
            // Without a session, files come back as their exact text (the UI editor saves what it
            // shows); directories, binary files and URIs as the model-facing view. A read made for a
            // session (the host reading ahead for its model) is the model's own read: the anchored
            // view, recorded as read in that session so the model can edit what it was shown.
            let for_session = params.session_id.is_some();
            if !for_session && !crate::uris::is_uri(&params.target) {
                let absolute = util::resolve(&session.workspace, &params.target);
                if core.sandbox.control_read_denied(&absolute) {
                    return Err(RpcError::new(
                        codes::CONTRACT_VIOLATION,
                        "frame credentials are private to the app; use cx for command access",
                    ));
                }
                if absolute.is_file()
                    && let Ok(bytes) = crate::util::read_host_file(&absolute)
                    && !crate::read::is_binary(&bytes)
                {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let text = match &params.range {
                        None => text,
                        Some(range) => raw_lines(&text, range).map_err(RpcError::invalid_params)?,
                    };
                    let total = crate::doc::Doc::parse(&String::from_utf8_lossy(&bytes)).len() as u32;
                    return to_value(&ReadResult { text, total_lines: Some(total) });
                }
            }
            let diagnostics = crate::langs::DiagnosticsRead { wait_ms: params.wait_ms, shown: params.shown.map(|n| n as usize) };
            let mut out = exec::read_target(&core, &host, &session, &params.target, params.range.as_deref(), params.intent.as_deref(), for_session, diagnostics).await;
            // A read for a session (the host reading ahead for its model, e.g. `diagnostics://`)
            // carries the skills of the codes it shows like the model's own read, under the same
            // per-session memory: whichever of the two comes first inlines the text.
            if out.ok && let Some(session_id) = params.session_id.as_deref() {
                exec::attach_code_skills(&core, host.as_ref(), session_id, &mut out).await;
            }
            if !out.ok {
                let code = match out.error_kind.as_deref() {
                    Some("not_found") => codes::NOT_FOUND,
                    Some("unavailable") => codes::UNAVAILABLE,
                    _ => codes::INVALID_PARAMS,
                };
                return Err(RpcError::new(code, out.output));
            }
            let total_lines = out.display.get("total_lines").and_then(Value::as_u64).map(|n| n as u32);
            to_value(&ReadResult { text: out.output, total_lines })
        }
        methods::RUN => {
            let params: RunParams = parse_params(params)?;
            let context = host_context(&params.workspace, None, params.session_id.as_deref());
            let session =
                Session { workspace: util::canonical_lenient(&util::expand_home(&params.workspace)), context: &context, python_env: true };
            let runtime = core
                .runtime(host.as_ref(), &context.session_id, &session.workspace, true)
                .await
                .map_err(|message| RpcError::new(codes::UNAVAILABLE, message))?;
            let venv = runtime.venv.as_ref().map(|venv| venv.path().to_path_buf());
            let mut identity = exec::identity(&core, &session, Some(&runtime));
            let mut command = params.command.clone();
            let mut env: Vec<(String, String)> = params
                .env
                .as_object()
                .map(|map| map.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned()))).collect())
                .unwrap_or_default();
            env.push(("CORTEX_COMMAND_URL".to_owned(), runtime.command_access.url.clone()));
            env.push(("CORTEX_COMMAND_TOKEN".to_owned(), runtime.command_access.token.clone()));
            let mut one_off = params.session_id.is_none();
            // A caller confining a command further (`sandbox-exec -p <profile> <command...>`, e.g. a
            // check run without network): a sandboxed shell cannot apply a second sandbox, so the
            // wrapped command runs in a one-off shell under the intersection of both profiles.
            let cwd = session.workspace.clone();
            if let Some(argv) = crate::approval::simple_argv(&params.command)
                && let Some(wrapped) = crate::sandbox::unwrap_sandbox_exec(&argv, &cwd)
            {
                let wrapped = wrapped.map_err(|error| RpcError::invalid_params(format!("cannot run inside the write sandbox: {error}")))?;
                identity.sandbox = Some(
                    core.sandbox
                        .compose(&wrapped.profile, &session.workspace, venv.as_deref(), &runtime.grants)
                        .ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "the write sandbox is unavailable"))?,
                );
                command = wrapped.argv.iter().map(|word| util::shell_quote(word)).collect::<Vec<_>>().join(" ");
                if let Some(session_id) = &params.session_id {
                    let temporary = format!("{session_id}-wrapped-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
                    core.shells.copy_state(session_id, &temporary).map_err(|error| RpcError::internal(error.to_string()))?;
                    env.push(("CORTEX_SESSION_ID".to_owned(), session_id.clone()));
                    identity.session_id = temporary;
                    one_off = true;
                }
            }
            if params.background {
                let env_file = core.shells.saved_env(&context.session_id);
                let meta = core
                    .jobs
                    .start(crate::jobs::StartRequest {
                        session_id: &context.session_id,
                        project_id: &identity.project_id,
                        command: &command,
                        cwd: &session.workspace,
                        env_file: env_file.as_deref(),
                        identity_env: env,
                        wake: Default::default(),
                        sandbox: identity.sandbox.as_ref(),
                        venv: venv.as_deref(),
                        toolchain: crate::shell::Toolchain::default(),
                    })
                    .map_err(|error| RpcError::internal(format!("could not start job: {error}")))?;
                if one_off && identity.session_id != context.session_id {
                    core.shells.drop_session(&identity.session_id).await;
                }
                return to_value(&RunResult {
                    exit_code: None,
                    output: String::new(),
                    duration_ms: 0,
                    timed_out: false,
                    job_id: Some(meta.job_id),
                    tests: None,
                });
            }
            let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(core.config.default_timeout_ms));
            let sink = items.clone();
            // The step log: what the command changes is the session's step (a check with side
            // effects at home, or a member's command in a copy kept here).
            let steps = core.steps.open(&session.workspace).await;
            steps.begin();
            let result = core
                .shells
                .run(&identity, RunRequest { command: &command, cwd: None, env, toolchain: crate::shell::Toolchain::default(), timeout }, cancel.clone(), move |chunk: &[u8]| {
                    if !chunk.is_empty() {
                        sink.send(json!({ "kind": "output", "text": String::from_utf8_lossy(chunk) }));
                    }
                })
                .await;
            {
                let who = crate::undo::Attribution::session(&context.session_id, "run", &params.command.chars().take(200).collect::<String>());
                let _ = tokio::task::spawn_blocking(move || steps.end(&who)).await;
            }
            if one_off {
                // A one-off command's shell is not kept.
                core.shells.drop_session(&identity.session_id).await;
            }
            let run = result.map_err(|error| RpcError::internal(format!("could not run: {error}")))?;
            let tests = run.output.whole(exec::TEST_VIEW_BYTES).as_deref().and_then(crate::testout::summary).map(|tests| cc_protocol::contracts::tools::TestCounts {
                runner: tests.runner.to_owned(),
                ran: tests.ran,
                failed: tests.failed,
                names: tests.names,
            });
            let (output, _, _) = exec::present_output(&core, &context.session_id, run.output, core.config.run_limits);
            to_value(&RunResult { exit_code: run.exit_code, output, duration_ms: run.duration_ms, timed_out: run.timed_out, job_id: None, tests })
        }
        methods::JOB_STATUS => {
            check_command_job(&core, &params)?;
            let params: JobParams = parse_params(params)?;
            to_value(&job_status(&core, &params.job_id)?)
        }
        methods::JOB_KILL => {
            check_command_job(&core, &params)?;
            // The card's buttons: `mode` "stop" (SIGTERM) or "kill" (SIGKILL now), and the job's
            // output ends saying the user did it; without a mode (`cx job kill`) it escalates.
            let mode = params.get("mode").and_then(Value::as_str).map(str::to_owned);
            let params: JobParams = parse_params(params)?;
            let found = match mode.as_deref() {
                Some("stop") => core.jobs.stop(&params.job_id, "[stopped by the user]", crate::jobs::StopMode::Term).await,
                Some("kill") => core.jobs.stop(&params.job_id, "[killed by the user]", crate::jobs::StopMode::Kill).await,
                _ => core.jobs.kill(&params.job_id).await,
            };
            found.ok_or_else(|| RpcError::not_found(format!("no job `{}`", params.job_id)))?;
            to_value(&job_status(&core, &params.job_id)?)
        }
        methods::JOB_LIST => {
            // The card asks for the running jobs of one project (`running`, `project_id`) and shows
            // each one's command and memory: those come as extra fields of each entry.
            let running_only = params.get("running").and_then(Value::as_bool).unwrap_or(false);
            let project = params.get("project_id").and_then(Value::as_str).map(str::to_owned);
            let command_session = params.get("_command_session").and_then(Value::as_str).map(str::to_owned);
            let mut params: JobListParams = if params.is_null() { JobListParams::default() } else { parse_params(params)? };
            // A command in the model's shells sees its own session's jobs only.
            if command_session.is_some() {
                params.session_id = command_session;
            }
            let metas: Vec<_> = core
                .jobs
                .list(params.session_id.as_deref())
                .into_iter()
                .filter(|meta| !running_only || meta.state == "running")
                .filter(|meta| project.as_deref().is_none_or(|project| meta.project_id == project))
                .collect();
            let mut jobs = Vec::new();
            for meta in metas {
                let Ok(status) = job_status(&core, &meta.job_id) else { continue };
                let mut entry = to_value(&status)?;
                let memory: u64 = if meta.state == "running" { crate::guard::tree(meta.pid).iter().map(|proc| proc.footprint).sum() } else { 0 };
                if let Some(object) = entry.as_object_mut() {
                    object.insert("command".into(), json!(meta.command));
                    object.insert("cwd".into(), json!(meta.cwd));
                    object.insert("session_id".into(), json!(meta.session_id));
                    object.insert("project_id".into(), json!(meta.project_id));
                    object.insert("memory_bytes".into(), json!(memory));
                    object.insert("note".into(), json!(meta.note));
                }
                jobs.push(entry);
            }
            Ok(json!({ "jobs": jobs }))
        }
        methods::ARTIFACT => {
            let params: ArtifactParams = parse_params(params)?;
            let bytes = core.artifacts.read(&params.id).map_err(|_| RpcError::not_found(format!("no artifact `{}`", params.id)))?;
            let total_bytes = bytes.len() as u64;
            let total_lines =
                bytes.iter().filter(|b| **b == b'\n').count() as u64 + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
            let slice: Vec<u8> = match &params.range {
                None => bytes,
                Some(range) => {
                    let segments = crate::read::parse_range(range).map_err(RpcError::invalid_params)?;
                    let lines: Vec<&[u8]> = bytes.split_inclusive(|b| *b == b'\n').collect();
                    let mut out = Vec::new();
                    for segment in segments {
                        let end = segment.end.unwrap_or(lines.len()).min(lines.len());
                        for line in lines.iter().take(end).skip(segment.start - 1) {
                            out.extend_from_slice(line);
                        }
                    }
                    out
                }
            };
            let bytes_len = slice.len() as u64;
            let (encoding, data) = match String::from_utf8(slice) {
                Ok(text) => ("utf8", text),
                Err(error) => {
                    use base64::Engine as _;
                    ("base64", base64::engine::general_purpose::STANDARD.encode(error.into_bytes()))
                }
            };
            to_value(&ArtifactResult { id: params.id, encoding: encoding.to_owned(), data, bytes: bytes_len, total_bytes, total_lines })
        }
        methods::WORKSPACE_CREATE => {
            let params: WorkspaceCreateParams = parse_params(params)?;
            let core = core.clone();
            let workspace = tokio::task::spawn_blocking(move || {
                core.workspaces.create(&params.project_id, &params.source, &params.name, params.branch.as_deref())
            })
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?
            .map_err(|error| RpcError::new(error.code, error.message))?;
            to_value(&workspace)
        }
        methods::WORKSPACE_REMOVE => {
            let params: WorkspaceRemoveParams = parse_params(params)?;
            let core = core.clone();
            let venv = crate::pyenv::auto_dir(&core.home, &util::expand_home(&params.path));
            let remover = core.clone();
            tokio::task::spawn_blocking(move || remover.workspaces.remove(&params.path))
                .await
                .map_err(|error| RpcError::internal(error.to_string()))?
                .map_err(|error| RpcError::new(error.code, error.message))?;
            // The workspace's automatic Python venv goes with it.
            crate::pyenv::remove(&core.graph, &venv);
            Ok(json!({ "removed": true }))
        }
        methods::WORKSPACE_LIST => {
            let params: WorkspaceListParams =
                if params.is_null() { WorkspaceListParams { project_id: None } } else { parse_params(params)? };
            to_value(&WorkspaceListResult { workspaces: core.workspaces.list(params.project_id.as_deref()).await })
        }
        methods::FILES => {
            let params: FilesParams = parse_params(params)?;
            to_value(&list_files(&params)?)
        }
        methods::SAVE => {
            let params: SaveParams = parse_params(params)?;
            let (root, target) = workspace_path(&params.workspace, Some(&params.path))?;
            let before = std::fs::read(&target).ok();
            let log = core.steps.open(&root).await;
            log.begin();
            let result = save(&core, &params);
            if let Ok(result) = &result {
                let mode = std::fs::metadata(&target).ok().map(|meta| { use std::os::unix::fs::PermissionsExt; meta.permissions().mode() & 0o777 }).unwrap_or(0o644);
                log.record_edit(&Attribution { actor: "user".to_owned(), session_id: None, label: "edit".to_owned(), detail: "editor".to_owned() }, &result.path, before.as_deref(), Some(params.content.as_bytes()), mode);
            }
            log.end_quiet();
            let result = result?;
            host.publish(topics::FILES_CHANGED, json!({ "workspace": params.workspace, "paths": [result.path.clone()], "by": "user" }));
            to_value(&result)
        }
        methods::TIMELINE => {
            let params: TimelineParams = parse_params(params)?;
            let log = core.steps.open(&util::expand_home(&params.workspace)).await;
            let result = tokio::task::spawn_blocking(move || {
                log.reconcile(&Attribution::user("hand"));
                log.timeline(&params)
            })
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?;
            to_value(&result)
        }
        methods::TIMELINE_DIFF => {
            let params: TimelineDiffParams = parse_params(params)?;
            let log = core.steps.open(&util::expand_home(&params.workspace)).await;
            let result = tokio::task::spawn_blocking(move || log.diff(&params)).await.map_err(|error| RpcError::internal(error.to_string()))?;
            to_value(&result)
        }
        methods::UNDO => {
            let params: UndoParams = parse_params(params)?;
            let workspace = util::canonical_lenient(&util::expand_home(&params.workspace));
            let log = core.steps.open(&workspace).await;
            let core = core.clone();
            let result = tokio::task::spawn_blocking(move || {
                let allowed = |path: &std::path::Path| util::is_inside(path, &workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(&workspace, path);
                log.undo(&params, &allowed)
            })
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?
            .map_err(RpcError::invalid_params)?;
            to_value(&result)
        }
        methods::REDO => {
            let params: RedoParams = parse_params(params)?;
            let workspace = util::canonical_lenient(&util::expand_home(&params.workspace));
            let log = core.steps.open(&workspace).await;
            let core = core.clone();
            let result = tokio::task::spawn_blocking(move || {
                let allowed = |path: &std::path::Path| util::is_inside(path, &workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(&workspace, path);
                log.redo(params.session_id.as_deref(), params.actor.as_deref(), &allowed)
            })
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?
            .map_err(RpcError::invalid_params)?;
            to_value(&result)
        }
        methods::APPROVE => {
            let params: ApproveParams = parse_params(params)?;
            to_value(&ApproveResult { approved: core.approve(&params.session_id, &params.classes) })
        }
        methods::SKILLS_SHOWN => {
            let params: SkillsShownParams = parse_params(params)?;
            let new = params.uris.iter().filter(|uri| !core.read_state.skill_attached(&params.session_id, uri)).count();
            for uri in &params.uris {
                core.read_state.record_skill(&params.session_id, uri);
            }
            Ok(json!({ "shown": new }))
        }
        methods::CLEANUP => {
            let workspace = params.get("workspace").and_then(Value::as_str).unwrap_or_default().to_owned();
            let session_id = params.get("session_id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let since_ms = params.get("since_ms").and_then(Value::as_u64).unwrap_or(0);
            if workspace.is_empty() || session_id.is_empty() {
                return Err(RpcError::invalid_params("tools.cleanup needs workspace and session_id"));
            }
            let core = core.clone();
            let workspace = util::expand_home(&workspace);
            let log = core.steps.open(&workspace).await;
            let removed = tokio::task::spawn_blocking(move || {
                let removed = crate::cleanup::turn_end(&core, &log, &workspace, &session_id, since_ms);
                // What the turn's commands left and the host took away, and anything else the
                // turn changed that no step recorded yet, is the session's last step of the turn.
                log.reconcile(&Attribution::session(&session_id, "run", "end of turn"));
                removed
            })
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?;
            Ok(json!({ "removed": removed }))
        }
        methods::TESTS_BASELINE => {
            let field = |name: &str| params.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
            let timeout_ms = params.get("timeout_ms").and_then(Value::as_u64);
            crate::baseline::check(
                &core,
                host.as_ref(),
                &field("session_id"),
                &field("workspace"),
                &field("command"),
                timeout_ms,
                cancel.clone(),
            )
            .await
        }
        methods::TESTS_REPORT => {
            let text = params.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (report, notes) = tokio::task::spawn_blocking(move || crate::exec::tests_report(&text)).await.map_err(|error| RpcError::internal(error.to_string()))?;
            Ok(json!({ "report": report, "notes": notes }))
        }
        other => Err(RpcError::unknown_method(other)),
    }
}

fn check_command_job(core: &Core, params: &Value) -> Result<(), RpcError> {
    if let Some(session) = params.get("_command_session").and_then(Value::as_str) {
        let id = params.get("job_id").and_then(Value::as_str).unwrap_or_default();
        if !core.jobs.list(Some(session)).iter().any(|job| job.job_id == id) {
            return Err(RpcError::not_found("no background job in this command's session"));
        }
    }
    Ok(())
}

/// The exact text of the given lines (terminators included).
fn raw_lines(text: &str, range: &str) -> Result<String, String> {
    let segments = crate::read::parse_range(range)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::new();
    for segment in segments {
        let end = segment.end.unwrap_or(lines.len()).min(lines.len());
        for line in lines.iter().take(end).skip(segment.start - 1) {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// Cancels the token if the request future is dropped (caller cancelled or disconnected).
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn workspace_path(workspace: &str, relative: Option<&str>) -> Result<(PathBuf, PathBuf), RpcError> {
    let root =
        util::expand_home(workspace).canonicalize().map_err(|error| RpcError::not_found(format!("workspace {workspace}: {error}")))?;
    let target = match relative {
        Some(relative) if !relative.trim().is_empty() && relative.trim() != "." => util::resolve(&root, relative),
        _ => root.clone(),
    };
    if !util::is_inside(&target, &root) {
        return Err(RpcError::invalid_params(format!("{} is outside the workspace", target.display())));
    }
    Ok((root, target))
}

fn list_files(params: &FilesParams) -> Result<FilesResult, RpcError> {
    let (root, dir) = workspace_path(&params.workspace, params.dir.as_deref())?;
    if !dir.is_dir() {
        return Err(RpcError::not_found(format!("{} is not a directory", dir.display())));
    }
    let mut visible = std::collections::HashSet::new();
    for entry in ignore::WalkBuilder::new(&dir).max_depth(Some(1)).hidden(false).parents(true).build().flatten() {
        if entry.depth() == 1 {
            visible.insert(entry.file_name().to_os_string());
        }
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|error| RpcError::internal(error.to_string()))?.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let meta = std::fs::metadata(entry.path()).or_else(|_| entry.metadata());
        let (is_dir, size) = meta.map(|meta| (meta.is_dir(), meta.len())).unwrap_or((false, 0));
        entries.push(FileEntry {
            path: util::display_rel(&entry.path(), &root),
            is_dir,
            size: if is_dir { 0 } else { size },
            ignored: !visible.contains(&name),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase())));
    Ok(FilesResult { entries })
}

fn save(core: &Core, params: &SaveParams) -> Result<SaveResult, RpcError> {
    let (root, target) = workspace_path(&params.workspace, Some(&params.path))?;
    if target == root || target.is_dir() {
        return Err(RpcError::invalid_params(format!("{} is a directory", params.path)));
    }
    if core.sandbox.protected(&target) || core.sandbox.git_control_write_denied(&root, &target) {
        return Err(RpcError::new(codes::CONTRACT_VIOLATION, "app control files and Git worktree pointers are managed by the app; save a project file instead"));
    }
    util::atomic_write_checked(&target, params.content.as_bytes(), |path| util::is_inside(path, &root) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(&root, path)).map_err(|error| RpcError::internal(format!("{}: {error}", target.display())))?;
    Ok(SaveResult {
        path: util::display_rel(&target, &root),
        bytes: params.content.len() as u64,
        hash: util::content_hash(params.content.as_bytes()),
    })
}

/// The probe exercises the real tools in a scratch directory: hashline read, anchored edit with
/// stale rejection, a shell round trip, output offload with byte-exact read-back, and a parse.
pub async fn probe(core: &Arc<Core>, host: Arc<dyn Host>) -> ProbeReport {
    let mut checks = Vec::new();
    let scratch = std::env::temp_dir().join(format!("cc-tools-probe-{}", uuid::Uuid::new_v4().simple()));
    let _ = std::fs::create_dir_all(&scratch);
    let context = ExecContext {
        project_id: "probe".into(),
        session_id: format!("probe-{}", uuid::Uuid::new_v4().simple()),
        workspace: scratch.to_string_lossy().into_owned(),
        mode: "auto".into(),
        edit_format: None,
    };
    let call = |name: &str, args: Value| ExecuteParams {
        context: context.clone(),
        call: ToolCall { id: "p".into(), name: name.into(), arguments: args.to_string() },
    };
    let mut check = |name: &str, started: Instant, result: Result<String, String>| {
        let (ok, detail) = match result {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        checks.push(ProbeCheck { name: name.to_owned(), ok, detail, ms: started.elapsed().as_millis() as u64 });
    };

    let started = Instant::now();
    let _ = std::fs::write(scratch.join("p.rs"), "fn main() {\n    let x = 1;\n}\n");
    let read = exec::execute_in(
        core,
        &host,
        call("read", json!({"intent": "probe", "path": "p.rs"})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    let anchor = format!("2:{}", cc_protocol::hashline::tag("    let x = 1;"));
    check(
        "read_hashline",
        started,
        if read.ok && read.output.contains(&format!("{anchor}|")) { Ok("hashline tags rendered".into()) } else { Err(read.output.clone()) },
    );

    let started = Instant::now();
    let edit = exec::execute_in(
        core,
        &host,
        call("edit", json!({"intent": "probe", "path": "p.rs", "edits": [{"op": "replace", "at": anchor, "text": "    let x = 2;"}]})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    let written = crate::util::read_host_text(scratch.join("p.rs")).unwrap_or_default();
    check(
        "edit_anchor",
        started,
        if edit.ok && written == "fn main() {\n    let x = 2;\n}\n" {
            Ok("anchored replace applied".into())
        } else {
            Err(edit.output.clone())
        },
    );

    let started = Instant::now();
    let stale = exec::execute_in(
        core,
        &host,
        call("edit", json!({"intent": "probe", "path": "p.rs", "edits": [{"op": "replace", "at": anchor, "text": "x"}]})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    let unchanged = crate::util::read_host_text(scratch.join("p.rs")).unwrap_or_default() == written;
    check(
        "edit_stale_rejected",
        started,
        if !stale.ok && stale.error_kind.as_deref() == Some("stale_anchor") && unchanged {
            Ok("stale anchor rejected, file untouched".into())
        } else {
            Err(stale.output.clone())
        },
    );

    let started = Instant::now();
    let run = exec::execute_in(
        core,
        &host,
        call("run", json!({"intent": "probe", "command": "cd .. && cd - >/dev/null && printf 'ok-%s' $((6*7))"})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    check(
        "run_shell",
        started,
        if run.ok && run.output.starts_with("ok-42") { Ok("persistent bash answered".into()) } else { Err(run.output.clone()) },
    );

    let started = Instant::now();
    let big = exec::execute_in(
        core,
        &host,
        call("run", json!({"intent": "probe", "command": "seq 1 20000"})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    let expected: String = (1..=20000).map(|n| format!("{n}\n")).collect();
    let roundtrip = big.artifact.as_deref().and_then(|id| core.artifacts.read(id).ok());
    check(
        "offload_roundtrip",
        started,
        if roundtrip.as_deref() == Some(expected.as_bytes()) {
            Ok("artifact byte-exact".into())
        } else {
            Err(format!("artifact mismatch: {:?}", big.artifact))
        },
    );

    // The write sandbox holds: a command in the session's sandbox can write its workspace but not a
    // protected file inside CORTEX_HOME (job metadata, in a directory the probe creates and removes),
    // and the refusal is the sandbox's EPERM. Nothing outside CORTEX_HOME is touched even if the
    // sandbox failed.
    let started = Instant::now();
    let canary_dir = core.home.join("jobs").join(format!(".sandbox-probe-{}", uuid::Uuid::new_v4().simple()));
    let canary = canary_dir.join("meta.json");
    let _ = std::fs::create_dir_all(&canary_dir);
    let script = format!(
        "printf in > sandbox-probe.txt; {{ printf out > {}; }} 2>&1; echo \"inside=$(cat sandbox-probe.txt)\"",
        util::shell_quote(&canary.to_string_lossy())
    );
    let confined = exec::execute_in(
        core,
        &host,
        call("run", json!({"intent": "probe", "command": script})),
        cc_protocol::ItemSink::discard(),
        CancellationToken::new(),
        false,
    )
    .await;
    let leaked = canary.exists();
    let _ = std::fs::remove_dir_all(&canary_dir);
    check(
        "write_sandbox",
        started,
        if leaked {
            Err(format!("a sandboxed command wrote {}", canary.display()))
        } else if !confined.output.contains("inside=in") || !crate::sandbox::mentions_eperm(confined.output.as_bytes()) {
            Err(confined.output.clone())
        } else {
            Ok("workspace writable, protected path refused by the sandbox".into())
        },
    );

    let started = Instant::now();
    let parsed = crate::lang::parse(crate::lang::Lang::Rust, "fn a() {}").is_some()
        && crate::lang::parse(crate::lang::Lang::Bash, "echo hi").is_some();
    check("tree_sitter", started, if parsed { Ok("grammars load".into()) } else { Err("a grammar failed to load".into()) });

    core.shells.drop_session(&context.session_id).await;
    if let Some(id) = big.artifact.as_deref() {
        core.artifacts.remove(id);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    ProbeReport::from_checks(checks)
}

impl Plugin for ToolsPlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        let core = self.core(&ctx);
        let host: Arc<dyn Host> = Arc::new(CtxHost(ctx));
        Box::pin(async move { dispatch(core.await, host, call).await })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        let core = self.core(&ctx);
        let host: Arc<dyn Host> = Arc::new(CtxHost(ctx));
        Box::pin(async move { Ok(probe(&core.await, host).await) })
    }

    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move {
            let core = core.await;
            core.jobs.adopt();
            // The approvals, the workspace clones and the venvs come from the graph; the memory
            // plugin may start later, so this does not hold up serving.
            let loading = core.clone();
            tokio::spawn(async move {
                loading.load_approvals().await;
                loading.workspaces.import_files().await;
                // The automatic venvs of workspaces that were deleted meanwhile go.
                crate::pyenv::remove_orphans(&loading.graph, &loading.home).await;
            });
            Ok(())
        })
    }

    fn export_state(&self, ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        let core = self.core(&ctx);
        // A successor takes over: background jobs keep running and it adopts them.
        self.handed_off.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let core = core.await;
            // The successor reads the jobs and steps from the graph: what is queued is written first.
            core.graph.flush(Duration::from_secs(5)).await;
            Ok(json!({ "read_state": core.read_state.export(), "skills_attached": core.read_state.export_skills() }))
        })
    }

    fn import_state(&self, ctx: Ctx, state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        let core = self.core(&ctx);
        Box::pin(async move {
            let core = core.await;
            if let Some(read_state) = state.get("read_state") {
                core.read_state.import(read_state.clone());
            }
            if let Some(skills) = state.get("skills_attached") {
                core.read_state.import_skills(skills.clone());
            }
            core.jobs.adopt();
            Ok(())
        })
    }

    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        if topic == crate::langs::SKILLS_CHANGED
            && let Some(category) = data.get("category").and_then(Value::as_str)
        {
            crate::langs::forget(category);
        }
        // A deleted session or project: this plugin's own data about it goes.
        let core = self.core(&ctx);
        Box::pin(async move {
            if topic == cc_protocol::contracts::vcs::topics::CHANGED
                && let Some(root) = data.get("root").and_then(Value::as_str)
            {
                // A commit, a checkout, a reset: what equals HEAD now leaves the step log.
                let log = core.await.steps.open(std::path::Path::new(root)).await;
                let _ = tokio::task::spawn_blocking(move || log.on_head_change()).await;
            } else if topic == frame_topics::SESSION_DELETED
                && let Ok(deleted) = serde_json::from_value::<SessionDeleted>(data.clone())
            {
                core.await.forget_session(&deleted.session_id, std::path::Path::new(&deleted.workspace)).await;
            } else if topic == frame_topics::PROJECT_DELETED
                && let Ok(deleted) = serde_json::from_value::<ProjectDeleted>(data)
            {
                core.await.forget_project(&deleted.project_id, std::path::Path::new(&deleted.root)).await;
            }
        })
    }

    fn shutdown(&self, ctx: Ctx) -> BoxFuture<'static, ()> {
        let core = self.core(&ctx);
        let handed_off = self.handed_off.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let core = core.await;
            core.shells.shutdown().await;
            // A shutdown without a handoff is a full stop: background jobs end with the session
            // host instead of running on unowned. After a handoff the successor owns them.
            if !handed_off {
                for job in core.jobs.list(None).into_iter().filter(|job| job.state == "running") {
                    core.jobs.kill(&job.job_id).await;
                }
            }
            core.graph.flush(Duration::from_secs(5)).await;
        })
    }
}
