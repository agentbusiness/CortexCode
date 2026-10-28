//! What a path in the live folder is: which plugin it belongs to, and whether it is plugin data in
//! CortexCode's repository layout. Structural: the layout of the repository, never the file's
//! content.

/// The plugin a path belongs to (`plugins/<id>/...`).
pub fn plugin_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("plugins/")?;
    let (id, inner) = rest.split_once('/')?;
    (!id.is_empty() && !inner.is_empty()).then_some(id)
}

/// A plugin's data: `plugins/<id>/data/**` or `plugins/<id>/backend/data/**` (what the code folder
/// of earlier versions held besides the source; read once, to carry the user's edits over).
pub fn is_data(path: &str) -> bool {
    let Some(id) = plugin_of(path) else { return false };
    let inner = &path["plugins/".len() + id.len() + 1..];
    inner.starts_with("data/") || inner.starts_with("backend/data/")
}
