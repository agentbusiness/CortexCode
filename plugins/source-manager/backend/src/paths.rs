//! What a path in the code folder is: which plugin it belongs to, whether it is data (live, no
//! approval) or code (built after the user approves), and which parts need a restart instead of a
//! hot swap. Structural: the layout of the repository, never the file's content.

use cc_protocol::contracts::frame::package::PluginToml;
use std::collections::BTreeMap;
use std::path::Path;

/// Data that is still treated as code: changing it needs the user's approval, and the plugin reads
/// it from its package only (the chat flow's safety policy).
pub const PROTECTED: &[&str] = &["plugins/chatflow/backend/data/prompts/policy.md"];

/// Parts that are not plugins: built into the app itself, so a change reaches the running app with
/// the next update rather than by a hot swap.
const APP_PARTS: &[(&str, &str)] =
    &[("frame/", "frame"), ("ui/shell/", "shell"), ("ui/renderer/", "shell"), ("packer/", "packer"), ("cli/", "cli")];

/// Shared libraries: a change rebuilds every plugin that uses them.
pub const SHARED: &[&str] = &["protocol/", "lang-kit/", "model-kit/", "ui/sdk/"];

/// The plugin a path belongs to (`plugins/<id>/...`).
pub fn plugin_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("plugins/")?;
    let (id, inner) = rest.split_once('/')?;
    (!id.is_empty() && !inner.is_empty()).then_some(id)
}

/// A plugin's data: `plugins/<id>/data/**` or `plugins/<id>/backend/data/**`, except protected files.
pub fn is_data(path: &str) -> bool {
    if PROTECTED.contains(&path) {
        return false;
    }
    let Some(id) = plugin_of(path) else { return false };
    let inner = &path["plugins/".len() + id.len() + 1..];
    inner.starts_with("data/") || inner.starts_with("backend/data/")
}

/// The app part a path belongs to, when it is one.
pub fn app_part(path: &str) -> Option<&'static str> {
    APP_PARTS.iter().find(|(prefix, _)| path.starts_with(prefix)).map(|(_, part)| *part)
}

/// The shared library a path belongs to, when it is one.
pub fn shared_of(path: &str) -> Option<&'static str> {
    SHARED.iter().find(|prefix| path.starts_with(**prefix)).copied()
}

/// Plugin ids and their names, from the plugin.toml files in the code folder.
pub fn plugin_names(code: &Path) -> BTreeMap<String, String> {
    plugin_manifests(code).into_iter().map(|(id, manifest)| (id, manifest.plugin.name)).collect()
}

/// Every plugin.toml in the code folder that parses, by plugin id (test-only plugins left out).
pub fn plugin_manifests(code: &Path) -> BTreeMap<String, PluginToml> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(code.join("plugins")) else { return out };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("plugin.toml")) else { continue };
        let Ok(manifest) = toml::from_str::<PluginToml>(&text) else { continue };
        out.insert(manifest.plugin.id.clone(), manifest);
    }
    out
}

/// The plugins whose build must run for a change to `files`: the plugins the files are in, plus,
/// for a shared library, every plugin that builds against it. App parts are returned separately.
pub fn affected(code: &Path, files: &[String]) -> (Vec<String>, Vec<String>) {
    let manifests = plugin_manifests(code);
    let mut plugins: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let add = |list: &mut Vec<String>, item: &str| {
        if !list.iter().any(|existing| existing == item) {
            list.push(item.to_owned());
        }
    };
    for file in files {
        if is_data(file) {
            continue;
        }
        if let Some(id) = plugin_of(file) {
            if manifests.contains_key(id) {
                add(&mut plugins, id);
            }
        } else if let Some(part) = app_part(file) {
            add(&mut parts, part);
        } else if let Some(library) = shared_of(file) {
            for (id, manifest) in &manifests {
                if uses_library(code, id, manifest, library) {
                    add(&mut plugins, id);
                }
            }
        }
    }
    plugins.sort();
    parts.sort();
    (plugins, parts)
}

/// Whether plugin `id` builds against a shared library: its backend's
/// Cargo.toml or one of its UI package.json files names the library's folder.
fn uses_library(code: &Path, id: &str, manifest: &PluginToml, library: &str) -> bool {
    let dir = code.join("plugins").join(id);
    let folder = library.trim_end_matches('/');
    let mentions = |file: &Path| std::fs::read_to_string(file).is_ok_and(|text| text.contains(&format!("/{folder}")));
    if library == "ui/sdk/" {
        return manifest.ui.as_ref().is_some_and(|ui| {
            mentions(&dir.join(&ui.dir).join("package.json")) || ui.package.iter().any(|package| mentions(&dir.join(&package.dir).join("package.json")))
        });
    }
    // lang-kit itself builds against the protocol, so its users do too.
    let cargo = |backend_dir: &str| {
        let file = dir.join(backend_dir).join("Cargo.toml");
        mentions(&file) || (library == "protocol/" && std::fs::read_to_string(&file).is_ok_and(|text| text.contains("/lang-kit")))
    };
    manifest.backend.as_ref().is_some_and(|backend| cargo(&backend.dir))
}
