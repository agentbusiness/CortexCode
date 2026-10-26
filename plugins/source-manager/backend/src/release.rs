//! Releases into the code folder (`docs/RELEASES.md`): whether the user changed a plugin away from
//! the shipped source, and merging a released source (one plugin's, or the frame's) into it.

use crate::code::{self, Scope};
use crate::service::Service;
use crate::{paths, proposals};
use cc_protocol::contracts::frame::package::valid_id;
use cc_protocol::contracts::source::{ReleaseMergeParams, ReleaseStatus, ReleaseStatusParams, UpdateResult};
use cc_protocol::{Ctx, RpcError};
use std::path::PathBuf;
use std::sync::Arc;

fn plugin_id(id: &str) -> Result<String, RpcError> {
    if valid_id(id) { Ok(id.to_owned()) } else { Err(RpcError::invalid_params(format!("`{id}` is not a plugin id"))) }
}

/// `source.release.status`.
pub async fn status(service: &Arc<Service>, params: ReleaseStatusParams) -> Result<ReleaseStatus, RpcError> {
    let plugin = plugin_id(&params.plugin)?;
    if !code::is_managed(&service.code_dir) {
        return Ok(ReleaseStatus { plugin, managed: false, edited: false, files: Vec::new() });
    }
    let prefix = format!("plugins/{plugin}/");
    let files = service.git(move |repo| code::edited(repo, &prefix)).await?;
    Ok(ReleaseStatus { plugin, managed: true, edited: !files.is_empty(), files })
}

/// `source.release.merge`: the released source in, the user's own code on it built again.
pub async fn merge(service: &Arc<Service>, ctx: &Ctx, params: ReleaseMergeParams) -> Result<UpdateResult, RpcError> {
    let scope = match params.scope.as_str() {
        "plugin" => Scope::Plugin(plugin_id(&params.plugin)?),
        "frame" => Scope::Frame,
        other => return Err(RpcError::invalid_params(format!("scope `{other}`: give `plugin` or `frame`"))),
    };
    let dir = PathBuf::from(&params.dir);
    if !dir.is_absolute() || !dir.is_dir() {
        return Err(RpcError::invalid_params(format!("`{}` must be an absolute path of a folder", params.dir)));
    }
    if !code::is_managed(&service.code_dir) {
        return Ok(UpdateResult { state: "unmanaged".into(), conflicts: Vec::new(), rebuilt: Vec::new() });
    }
    let (merge_scope, version) = (scope.clone(), params.version.clone());
    let updated = service.git(move |repo| code::release(repo, &merge_scope, &dir, &version)).await?;
    let mut result = UpdateResult { state: updated.state.into(), conflicts: updated.conflicts, rebuilt: Vec::new() };
    // The user's own code inside the released part sits on a new base: build it again.
    let own: Vec<String> = updated
        .own_code
        .into_iter()
        .filter(|file| match &scope {
            Scope::Plugin(id) => file.starts_with(&format!("plugins/{id}/")),
            Scope::Frame => !file.starts_with("plugins/"),
        })
        .collect();
    if result.state == "merged" && !own.is_empty() {
        let (plugins, _) = paths::affected(&service.code_dir, &own);
        result.rebuilt = proposals::rebuild(service, ctx, &plugins).await.0;
    }
    service.changed(ctx);
    Ok(result)
}
