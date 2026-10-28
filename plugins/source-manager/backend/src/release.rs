//! Releases into the live folder (`docs/RELEASES.md`): whether the user changed a plugin's data away
//! from its package, merging a package's data into it, and putting it back.

use crate::live;
use crate::packages;
use crate::service::Service;
use cc_protocol::contracts::frame::package::valid_id;
use cc_protocol::contracts::source::{ReleaseMergeParams, ReleaseResetParams, ReleaseResetResult, ReleaseStatus, ReleaseStatusParams, UpdateResult};
use cc_protocol::{Ctx, RpcError};
use std::path::Path;
use std::sync::Arc;

fn plugin_id(id: &str) -> Result<String, RpcError> {
    if valid_id(id) { Ok(id.to_owned()) } else { Err(RpcError::invalid_params(format!("`{id}` is not a plugin id"))) }
}

/// The unpacked package in `dir`, for plugin `id`.
fn package(id: &str, dir: &str) -> Result<packages::Package, RpcError> {
    let path = Path::new(dir);
    if !path.is_absolute() || !path.is_dir() {
        return Err(RpcError::invalid_params(format!("`{dir}` must be an absolute path of a folder")));
    }
    packages::read(id, path).map_err(RpcError::invalid_params)
}

/// `source.release.status`.
pub async fn status(service: &Arc<Service>, params: ReleaseStatusParams) -> Result<ReleaseStatus, RpcError> {
    let plugin = plugin_id(&params.plugin)?;
    if !live::is_managed(&service.live_dir) {
        return Ok(ReleaseStatus { plugin, managed: false, edited: false, files: Vec::new() });
    }
    let prefix = format!("plugins/{plugin}/");
    let files = service.git(move |repo| live::edited(repo, &prefix)).await?;
    Ok(ReleaseStatus { plugin, managed: true, edited: !files.is_empty(), files })
}

/// `source.release.merge`: the package's data in, the user's edits kept.
pub async fn merge(service: &Arc<Service>, ctx: &Ctx, params: ReleaseMergeParams) -> Result<UpdateResult, RpcError> {
    let package = package(&plugin_id(&params.plugin)?, &params.dir)?;
    if !live::is_managed(&service.live_dir) {
        return Ok(UpdateResult { state: "unmanaged".into(), conflicts: Vec::new() });
    }
    let version = params.version.clone();
    let updated = service.git(move |repo| live::release(repo, &package, &version)).await?;
    service.changed(ctx);
    Ok(UpdateResult { state: updated.state.into(), conflicts: updated.conflicts })
}

/// `source.release.reset`: one plugin's folder back to its package's data, the user's edits in it
/// dropped.
pub async fn reset(service: &Arc<Service>, ctx: &Ctx, params: ReleaseResetParams) -> Result<ReleaseResetResult, RpcError> {
    let package = package(&plugin_id(&params.plugin)?, &params.dir)?;
    if !live::is_managed(&service.live_dir) {
        return Ok(ReleaseResetResult { state: "unmanaged".into(), files: Vec::new() });
    }
    let version = params.version.clone();
    let (state, files) = service.git(move |repo| live::reset_plugin(repo, &package, &version)).await?;
    service.changed(ctx);
    Ok(ReleaseResetResult { state: state.into(), files })
}
