//! `cc-source` executable: serve `source@1` to the frame.

use cc_protocol::contracts::lifecycle::{Hello, ProbeReport};
use cc_protocol::contracts::frame::{PluginUnit, topics as frame_topics};
use cc_protocol::contracts::source::{BackupSetParams, DiffParams, HistoryParams, RestoreParams, RevertParams, RevertResult, ShareParams, Status, methods};
use cc_protocol::{BoxFuture, Call, Ctx, Plugin, PluginEnv, RpcError, codes, parse_params, run_plugin, to_value};
use cc_source::service::{Service, rpc};
use cc_source::{PROVIDES, backup, live, probe, record, share, storage, watch};
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

async fn status(service: &Arc<Service>) -> Result<Status, RpcError> {
    let settings = service.settings()?;
    let shipped = if live::is_managed(&service.live_dir) { service.git(|repo| Ok(live::shipped(repo))).await? } else { None };
    Ok(Status {
        live_dir: service.live_dir.display().to_string(),
        managed: service.managed(),
        shipped,
        update: if settings.update.is_empty() { "up_to_date".into() } else { settings.update },
        conflicts: settings.conflicts,
    })
}

async fn revert(service: &Arc<Service>, ctx: &Ctx, params: RevertParams) -> Result<RevertResult, RpcError> {
    if !service.managed() {
        return Err(RpcError::invalid_params("the live folder is a developer checkout: revert with git there"));
    }
    let target = params.commit.clone();
    let (commit, _) = service.git(move |repo| live::revert(repo, &target)).await?;
    record_now(service).await;
    service.changed(ctx);
    backup::after_change(service, ctx).await;
    Ok(RevertResult { commit: commit.to_string() })
}

/// Record a change in the graph before listeners refetch (the watch loop records it otherwise).
async fn record_now(service: &Arc<Service>) {
    if let Err(error) = record::sync(service, true).await {
        tracing::warn!(error = %error.message, "the change is not recorded in the graph yet; the next look at the live folder records it");
    }
}

async fn diff(service: &Arc<Service>, params: DiffParams) -> Result<Value, RpcError> {
    let oid = Oid::from_str(&params.commit).map_err(|e| RpcError::invalid_params(e.message().to_owned()))?;
    let files = service.git(move |repo| live::diff_commit(repo, oid)).await?;
    to_value(&cc_protocol::contracts::source::Diff { files })
}

impl Plugin for SourcePlugin {
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.remember(&ctx);
        let service = self.service.clone();
        Box::pin(async move {
            match call.method.as_str() {
                methods::STATUS => to_value(&status(&service).await?),
                methods::UPDATE => to_value(&watch::update(&service, &ctx).await),
                methods::HISTORY => {
                    let params: HistoryParams = params(call.params)?;
                    let limit = params.limit.unwrap_or(200).clamp(1, 1000);
                    if !live::is_managed(&service.live_dir) {
                        return to_value(&cc_protocol::contracts::source::History { changes: Vec::new() });
                    }
                    let mut changes = record::history(&service, limit).await?;
                    let shared = service.settings()?.shared;
                    for change in &mut changes {
                        change.shared = shared.iter().find(|s| s.commits.contains(&change.commit)).map(|s| s.url.clone());
                    }
                    to_value(&cc_protocol::contracts::source::History { changes })
                }
                methods::REVERT => to_value(&revert(&service, &ctx, parse_params(call.params)?).await?),
                methods::DIFF => diff(&service, parse_params(call.params)?).await,
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
                    let (home, live) = (service.home.clone(), service.live_dir.clone());
                    let storage = tokio::task::spawn_blocking(move || storage::measure(&home, &live)).await.map_err(|e| rpc(e.to_string()))?;
                    to_value(&storage)
                }
                methods::RELEASE_STATUS => to_value(&cc_source::release::status(&service, parse_params(call.params)?).await?),
                methods::RELEASE_MERGE => to_value(&cc_source::release::merge(&service, &ctx, parse_params(call.params)?).await?),
                methods::RELEASE_RESET => to_value(&cc_source::release::reset(&service, &ctx, parse_params(call.params)?).await?),
                methods::FILE_HISTORY => to_value(&record::file_history(&service, parse_params(call.params)?).await?),
                methods::FILE_RESTORE => {
                    let restored = record::file_restore(&service, parse_params(call.params)?).await?;
                    if restored.state != "up_to_date" {
                        service.changed(&ctx);
                        backup::after_change(&service, &ctx).await;
                    }
                    to_value(&restored)
                }
                other => Err(RpcError::new(codes::UNKNOWN_METHOD, format!("cc-source does not serve `{other}`"))),
            }
        })
    }

    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>> {
        self.remember(&ctx);
        let service = self.service.clone();
        Box::pin(async move { Ok(probe::run(&service).await) })
    }

    /// Create or update the live folder, then watch it.
    fn activate(&self, ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        self.remember(&ctx);
        let (service, stop) = (self.service.clone(), self.stop.clone());
        Box::pin(async move {
            tokio::spawn(async move {
                if let Err(error) = service.load_settings(&ctx).await {
                    tracing::error!(%error.message, "the settings could not be read from the graph; changes are still written");
                }
                // The start-up merge publishes `source.changed`: plugins that read live data (they
                // started beside this one) read it again.
                watch::update(&service, &ctx).await;
                watch::run(service, ctx, stop).await;
            });
            Ok(())
        })
    }

    /// `plugin.unit`: the plugin's folder in the live folder follows the package it now runs from.
    fn event(&self, ctx: Ctx, topic: String, data: Value) -> BoxFuture<'static, ()> {
        let service = self.service.clone();
        Box::pin(async move {
            if topic == frame_topics::PLUGIN_UNIT
                && let Ok(unit) = serde_json::from_value::<PluginUnit>(data)
            {
                watch::follow(&service, &ctx, unit).await;
            }
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
        let live_dir = env.live_root.clone().unwrap_or_else(|| env.home.join("live"));
        let service = Arc::new(Service::new(env.data_dir.clone(), env.home.clone(), live_dir));
        let mut hello = Hello::new("cc-source", env!("CARGO_PKG_VERSION"), "source", "source@1");
        hello.provides = PROVIDES.iter().map(|m| (*m).to_owned()).collect();
        hello.subscribes = vec![frame_topics::PLUGIN_UNIT.to_owned()];
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
