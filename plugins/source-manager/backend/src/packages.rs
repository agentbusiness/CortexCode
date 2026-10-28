//! The installed packages' data: what the live folder is made from and follows. A package names
//! its data folders in its `manifest.json` (`data`) and carries them at those paths.

use cc_protocol::contracts::frame::package::{CcPluginManifest, PACKAGE_MANIFEST};
use cc_protocol::contracts::frame::{PluginListResult, PluginUnit, methods as frame_methods};
use cc_protocol::{Ctx, RpcError};
use std::path::{Path, PathBuf};

/// One plugin's package and the data folders it carries.
#[derive(Debug, Clone)]
pub struct Package {
    pub id: String,
    /// The unpacked package.
    pub dir: PathBuf,
    /// Its data folders, relative to the package (`data`, `backend/data`).
    pub dirs: Vec<String>,
}

/// The package unpacked in `dir`, for plugin `id`.
pub fn read(id: &str, dir: &Path) -> Result<Package, String> {
    let path = dir.join(PACKAGE_MANIFEST);
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let manifest: CcPluginManifest = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if manifest.plugin.id != id {
        return Err(format!("{} is a package of `{}`, not `{id}`", dir.display(), manifest.plugin.id));
    }
    Ok(Package { id: id.to_owned(), dir: dir.to_path_buf(), dirs: manifest.data })
}

/// The package a unit runs from (`frame.plugins.list`, `plugin.unit`).
pub fn of_unit(unit: &PluginUnit) -> Result<Package, String> {
    read(&unit.id, Path::new(&unit.dir))
}

/// Every installed plugin: its package, and its name for commit messages.
pub async fn installed(ctx: &Ctx) -> Result<Vec<(Package, String)>, RpcError> {
    let value = ctx.call(frame_methods::PLUGINS_LIST, serde_json::json!({})).await?;
    let list: PluginListResult = serde_json::from_value(value).map_err(|e| RpcError::internal(e.to_string()))?;
    let mut out = Vec::new();
    for unit in &list.units {
        match of_unit(unit) {
            Ok(package) => out.push((package, if unit.name.is_empty() { unit.id.clone() } else { unit.name.clone() })),
            Err(error) => tracing::warn!(plugin = %unit.id, %error, "the installed package could not be read"),
        }
    }
    Ok(out)
}
