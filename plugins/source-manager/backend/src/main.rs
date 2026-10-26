//! `cc-source` executable: serve `source@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::contracts::source::{
    BackupSetParams, DecideParams, DiffParams, HistoryParams, ProposeParams, Proposals, RestoreParams, RevertParams, RevertResult, ShareParams, Status, methods,
};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, codes, parse_params, run_plugin, to_value};
use cc_source::service::{Service, rpc};
use cc_source::{PROVIDES, backup, capability, code, paths, probe, proposals, share, storage, watch};
use git2::Oid;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::{Arc, Once};
use tokio_util::sync::CancellationToken;

struct SourcePlugin {
    service: Arc<Service>,
    configured: Once,
    stop: CancellationToken,
}

impl SourcePlugin {
    /// Configuration arrives in the hello acknowledgement; apply it with the first call.
    fn remember(&self, ctx: &Ctx) {
        self.configured.call_once(|| self.service.configure(ctx.config()));
    }
}

/// Params, with `null` meaning the defaults.
fn params<T: DeserializeOwned + Default>(value: Value) -> Result<T, RpcError> {
    if value.is_null() { Ok(T::default()) } else { parse_params(value) }
}

async fn status(service: &Arc<Service>, ctx: &Ctx) -> Result<Status, RpcError> {
    let tools = capability::detect().await;
    // What each installed plugin can have edited: its plugin.toml in the code folder, else in its
    // installed package.
    let mut manifests = paths::plugin_manifests(&service.code_dir);
    if let Ok(value) = ctx.call(cc_protocol::contracts::frame::methods::PLUGINS_LIST, serde_json::json!({})).await
        && let Some(units) = value.get("units").and_then(Value::as_array)
    {
        for unit in units {
            let (Some(id), Some(dir)) = (unit.get("id").and_then(Value::as_str), unit.get("dir").and_then(Value::as_str)) else { continue };
            if !manifests.contains_key(id)
                && let Ok(text) = std::fs::read_to_string(std::path::Path::new(dir).join("plugin.toml"))
                && let Ok(manifest) = toml::from_str(&text)
            {
                manifests.insert(id.to_owned(), manifest);
            }
        }
    }
    let plugins = manifests.iter().map(|(id, manifest)| capability::editing(id, manifest, &tools)).collect();
    let settings = service.settings()?;
    let shipped = if code::is_managed(&service.code_dir) { service.git(|repo| Ok(code::shipped(repo))).await? } else { None };
    let pending = service.proposals()?.iter().filter(|p| p.state == "pending").count();
    Ok(Status {
        code_dir: service.code_dir.display().to_string(),
        managed: service.managed(),
        shipped,
        update: if settings.update.is_empty() { "up_to_date".into() } else { settings.update },
        conflicts: settings.conflicts,
        plugins,
        tools,
        pending,
    })
}

async fn revert(service: &Arc<Service>, ctx: &Ctx, params: RevertParams) -> Result<RevertResult, RpcError> {
    if !service.managed() {
        return Err(RpcError::invalid_params("the code folder is a developer checkout: revert with git there"));
    }
    let target = params.commit.clone();
    let (commit, files) = service.git(move |repo| code::revert(repo, &target)).await?;
    let (plugins, next_start) = paths::affected(&service.code_dir, &files);
    let (rebuilt, failed) = proposals::rebuild(service, ctx, &plugins).await;
    if !failed.is_empty() {
        tracing::warn!(?failed, "plugins could not be rebuilt after a revert");
    }
    service.changed(ctx);
    backup::after_change(service, ctx).await;
    Ok(RevertResult { commit: commit.to_string(), rebuilt, next_start })
}

async fn diff(service: &Arc<Service>, params: DiffParams) -> Result<Value, RpcError> {
    let target = match (params.proposal, params.commit) {
        (Some(id), _) => service.proposals()?.into_iter().find(|p| p.id == id).map(|p| p.draft).ok_or_else(|| RpcError::invalid_params(format!("no proposal `{id}`")))?,
        (None, Some(commit)) => commit,
        (None, None) => return Err(RpcError::invalid_params("give a proposal or a commit")),
    };
    let oid = Oid::from_str(&target).map_err(|e| RpcError::invalid_params(e.message().to_owned()))?;
    let files = service.git(move |repo| code::diff_commit(repo, oid)).await?;
    to_value(&cc_protocol::contracts::source::Diff { files })
}

impl Plugin for SourcePlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.remember(&ctx);
        let service = self.service.clone();
        Box::pin(async move {
            match call.method.as_str() {
                methods::STATUS => to_value(&status(&service, &ctx).await?),
                methods::UPDATE => to_value(&watch::update(&service, &ctx).await),
                methods::HISTORY => {
                    let params: HistoryParams = params(call.params)?;
                    let limit = params.limit.unwrap_or(200).clamp(1, 1000);
                    if !code::is_managed(&service.code_dir) {
                        return to_value(&cc_protocol::contracts::source::History { changes: Vec::new() });
                    }
                    let mut changes = service.git(move |repo| code::history(repo, limit)).await?;
                    let shared = service.settings()?.shared;
                    for change in &mut changes {
                        change.shared = shared.iter().find(|s| s.commits.contains(&change.commit)).map(|s| s.url.clone());
                    }
                    to_value(&cc_protocol::contracts::source::History { changes })
                }
                methods::REVERT => to_value(&revert(&service, &ctx, parse_params(call.params)?).await?),
                methods::DIFF => diff(&service, parse_params(call.params)?).await,
                methods::PROPOSALS => {
                    let mut proposals = service.proposals()?;
                    proposals.reverse();
                    to_value(&Proposals { proposals })
                }
                methods::PROPOSE => {
                    let params: ProposeParams = params(call.params)?;
                    to_value(&proposals::propose(&service, &ctx, params).await?)
                }
                methods::DECIDE => {
                    let params: DecideParams = parse_params(call.params)?;
                    to_value(&proposals::decide(&service, &ctx, params).await?)
                }
                methods::BACKUP_STATUS => to_value(&backup::status(&service)?),
                methods::BACKUP_SET => {
                    let params: BackupSetParams = params(call.params)?;
                    let result = backup::set(&service, params)?;
                    service.changed(&ctx);
                    to_value(&result)
                }
                methods::BACKUP_PUSH => to_value(&backup::push(&service, &ctx).await?),
                methods::BACKUP_RESTORE => {
                    let params: RestoreParams = parse_params(call.params)?;
                    to_value(&backup::restore(&service, &ctx, params).await?)
                }
                methods::SHARE_CANDIDATES => to_value(&share::candidates(&service, &ctx).await?),
                methods::SHARE_SEND => {
                    let params: ShareParams = parse_params(call.params)?;
                    to_value(&share::send(&service, &ctx, params).await?)
                }
                methods::SHARE_STATUS => to_value(&share::status(&service, &ctx).await?),
                methods::STORAGE => {
                    let (home, code) = (service.home.clone(), service.code_dir.clone());
                    let storage = tokio::task::spawn_blocking(move || storage::measure(&home, &code)).await.map_err(|e| rpc(e.to_string()))?;
                    to_value(&storage)
                }
                methods::RELEASE_STATUS => to_value(&cc_source::release::status(&service, parse_params(call.params)?).await?),
                methods::RELEASE_MERGE => to_value(&cc_source::release::merge(&service, &ctx, parse_params(call.params)?).await?),
                other => Err(RpcError::new(codes::UNKNOWN_METHOD, format!("cc-source does not serve `{other}`"))),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.remember(&ctx);
        let service = self.service.clone();
        Box::pin(async move { Ok(probe::run(&service).await) })
    }

    /// Create or update the code folder, then watch it.
    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        let (service, stop) = (self.service.clone(), self.stop.clone());
        Box::pin(async move {
            tokio::spawn(async move {
                if let Err(error) = service.load_stored(&ctx).await {
                    tracing::error!(%error.message, "the settings and proposals could not be read from the store");
                }
                proposals::recover(&service, &ctx).await;
                watch::update(&service, &ctx).await;
                // The start-up merge is done: plugins that read live data from the code folder
                // (they started beside this one) read it again now.
                service.changed(&ctx);
                watch::run(service, ctx, stop).await;
            });
            Ok(())
        })
    }

    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        self.stop.cancel();
        Box::pin(async {})
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CORTEX_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cc-source: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let env = match PluginEnv::from_env() {
            Ok(env) => env,
            Err(error) => {
                eprintln!("cc-source: {error}");
                return 2;
            }
        };
        if let Err(error) = std::fs::create_dir_all(&env.data_dir) {
            eprintln!("cc-source: cannot create {}: {error}", env.data_dir.display());
            return 1;
        }
        let code_dir = env.code_dir.clone().unwrap_or_else(|| env.home.join("code"));
        let service = Arc::new(Service::new(env.data_dir.clone(), env.home.clone(), code_dir));
        let mut hello = Hello::new("cc-source", env!("CARGO_PKG_VERSION"), "source", "source@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        let plugin = Arc::new(SourcePlugin { service, configured: Once::new(), stop: CancellationToken::new() });
        match run_plugin(hello, plugin).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("cc-source: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}
