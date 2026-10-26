//! Downloads: the catalog, release files checked against their checksum, and unpacking.

use cc_protocol::release::{self, Asset, Catalog};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// The HTTP client (GitHub answers release downloads with a redirect to its file store).
pub fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("CortexCode-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("cannot start the HTTP client: {e}"))
}

/// Download the catalog.
pub async fn catalog(client: &reqwest::Client, url: &str) -> Result<Catalog, String> {
    let response = client.get(url).send().await.map_err(|e| format!("cannot reach {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| format!("cannot read {url}: {e}"))?;
    release::parse_catalog(&bytes).map_err(|e| format!("{url}: {e}"))
}

/// Download `asset` into `dir`, checking its size and SHA-256; `progress` gets the percent done.
pub async fn asset(client: &reqwest::Client, asset: &Asset, dir: &Path, mut progress: impl FnMut(u64)) -> Result<PathBuf, String> {
    if asset.name.is_empty() || asset.name.contains('/') || asset.name.starts_with('.') {
        return Err(format!("invalid file name `{}` in the catalog", asset.name));
    }
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(&asset.name);
    let partial = dir.join(format!(".{}.part", asset.name));
    let mut response = client.get(&asset.url).send().await.map_err(|e| format!("cannot download {}: {e}", asset.name))?;
    if !response.status().is_success() {
        return Err(format!("{} answered {}", asset.url, response.status()));
    }
    let mut file = tokio::fs::File::create(&partial).await.map_err(|e| format!("cannot write {}: {e}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut reported = 0;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("the download of {} broke off: {e}", asset.name))? {
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| format!("cannot write {}: {e}", partial.display()))?;
        received += chunk.len() as u64;
        let percent = if asset.size > 0 { (received * 100 / asset.size).min(100) } else { 0 };
        if percent >= reported + 10 {
            reported = percent;
            progress(percent);
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if received != asset.size || digest != asset.sha256.to_ascii_lowercase() {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(format!("{} does not match the catalog (size or checksum); it was not used", asset.name));
    }
    tokio::fs::rename(&partial, &path).await.map_err(|e| format!("cannot store {}: {e}", path.display()))?;
    Ok(path)
}

/// Unpack a `.tar.gz` (a frame release or a `.ccplugin`) into the new folder `dir`.
pub async fn untar(archive: &Path, dir: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let output = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .output()
        .await
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("cannot unpack {}: {}", archive.display(), String::from_utf8_lossy(&output.stderr).trim()))
    }
}
