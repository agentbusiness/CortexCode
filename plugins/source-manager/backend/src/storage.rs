//! What CortexCode keeps on disk, by kind, for the Storage settings tab.

use cc_protocol::contracts::source::{Storage, StorageItem};
use std::path::Path;

/// Plain names for the home's folders, in display order.
const ITEMS: &[(&str, &str)] = &[
    ("Installed plugins", "plugins"),
    ("Plugin data (caches, saved model states)", "plugins-data"),
    ("Store (projects, conversations, settings, code graph)", "irongraph"),
    ("Attachments", "attachments"),
    ("Logs", "logs"),
];

fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.flatten().map(|entry| size(&entry.path())).sum()
}

/// Sizes of the home's folders and the live folder (a walk; run off the async threads).
pub fn measure(home: &Path, live: &Path) -> Storage {
    let item = |name: &str, path: &Path| StorageItem { name: name.into(), bytes: size(path), path: path.display().to_string() };
    let mut items: Vec<StorageItem> = ITEMS.iter().map(|(name, folder)| item(name, &home.join(folder))).collect();
    // The models live outside the home, in the folder every Cortex app shares.
    items.insert(0, item("Models (shared with the other Cortex apps)", &cc_shared_models::root()));
    items.insert(1, item("Live data (skills, prompts, texts)", live));
    Storage { home: home.display().to_string(), items }
}
