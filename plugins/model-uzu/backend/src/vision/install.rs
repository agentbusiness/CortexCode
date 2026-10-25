//! Installing a vision tower: only the vision tensors of the upstream checkpoint, fetched with HTTP
//! range requests from the one safetensors shard that holds them, written as a standalone
//! safetensors file into `<variant dir>/vision/`.
//!
//! 1. The shard's 8-byte header length and its JSON header (two small range requests); the header
//!    must list exactly the pinned tensor count and parameter count under the vision prefix, as one
//!    contiguous byte range.
//! 2. A new header with just those tensors (offsets rebased, keys sorted, so the file is
//!    deterministic), then the tensor bytes appended by one range request, resumable from a
//!    `.part` file.
//! 3. The assembled file's SHA-256 is computed, compared with the pinned value when one is pinned,
//!    and recorded (`.cortexcode-verified.json`) with a `manifest.json` describing the source.
//!    `config.json` and `preprocessor_config.json` are fetched whole and checked by git blob id.

use super::{DIR, SmallFile, TowerSpec, WEIGHTS};
use crate::variants::{self, FileSpec, HUGGING_FACE, Progress, VerifyCache};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The tower's directory under a variant's model directory.
pub fn tower_dir(variant_dir: &Path) -> PathBuf {
    variant_dir.join(DIR)
}

/// Every file present at its expected size (verification is separate).
pub fn installed(tower: &TowerSpec, variant_dir: &Path) -> bool {
    let dir = tower_dir(variant_dir);
    let sized = |name: &str, bytes: Option<u64>| {
        std::fs::metadata(dir.join(name)).is_ok_and(|meta| meta.is_file() && bytes.is_none_or(|bytes| meta.len() == bytes))
    };
    tower.files.iter().all(|file| sized(file.path, Some(file.bytes))) && sized(WEIGHTS, None) && sized("manifest.json", None)
}

fn small_spec(file: &SmallFile) -> FileSpec {
    FileSpec { path: file.path, bytes: file.bytes, git_oid: file.git_oid, sha256: None }
}

fn url(base: &str, tower: &TowerSpec, path: &str) -> String {
    format!("{}/{}/resolve/{}/{}", base.trim_end_matches('/'), tower.repo, tower.revision, path)
}

/// The selection of the shard header: the vision tensors, rebased, and their byte range in the
/// shard's data section.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// The standalone file's header (8-byte length prefix included), padded to 8 bytes.
    pub prefix: Vec<u8>,
    /// `[start, end)` of the tensor data within the shard's data section.
    pub data: (u64, u64),
    pub tensors: usize,
    pub parameters: u64,
}

/// Select the vision tensors from a shard header and build the standalone file's header.
pub fn select(tower: &TowerSpec, header: &[u8]) -> Result<Selection, String> {
    let header: Map<String, Value> = serde_json::from_slice(header).map_err(|error| format!("shard header: {error}"))?;
    let mut chosen = BTreeMap::new();
    let mut parameters = 0u64;
    for (name, info) in &header {
        if !name.starts_with(tower.prefix) {
            continue;
        }
        let offsets = info["data_offsets"].as_array().filter(|offsets| offsets.len() == 2).ok_or_else(|| format!("{name}: no data_offsets"))?;
        let (start, end) = (offsets[0].as_u64().unwrap_or(0), offsets[1].as_u64().unwrap_or(0));
        let shape: Vec<u64> = info["shape"].as_array().map(|dims| dims.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        if info["dtype"] != "BF16" || end < start || end - start != shape.iter().product::<u64>() * 2 {
            return Err(format!("{name}: unexpected {} tensor of shape {shape:?}", info["dtype"]));
        }
        parameters += shape.iter().product::<u64>();
        chosen.insert(name.clone(), (start, end, info["dtype"].clone(), shape));
    }
    if chosen.len() != tower.tensors || parameters != tower.parameters {
        return Err(format!(
            "the shard lists {} vision tensors with {parameters} parameters; expected {} with {}",
            chosen.len(),
            tower.tensors,
            tower.parameters
        ));
    }
    let low = chosen.values().map(|(start, ..)| *start).min().unwrap_or(0);
    let high = chosen.values().map(|(_, end, ..)| *end).max().unwrap_or(0);
    let covered: u64 = chosen.values().map(|(start, end, ..)| end - start).sum();
    if covered != high - low || high - low != tower.data_bytes {
        return Err(format!("the vision tensors are not one contiguous {}-byte range ({covered} of {})", tower.data_bytes, high - low));
    }
    // Contiguous and non-overlapping: no other tensor of the shard falls inside the range.
    for (name, info) in &header {
        if name.starts_with(tower.prefix) || name == "__metadata__" {
            continue;
        }
        if let Some(start) = info["data_offsets"][0].as_u64()
            && (low..high).contains(&start)
        {
            return Err(format!("{name} lies inside the vision range"));
        }
    }
    let mut out = Map::new();
    out.insert(
        "__metadata__".into(),
        json!({"format": "pt", "source": format!("{}@{} {} (vision tensors only)", tower.repo, tower.revision, tower.shard)}),
    );
    for (name, (start, end, dtype, shape)) in chosen {
        out.insert(name, json!({"dtype": dtype, "shape": shape, "data_offsets": [start - low, end - low]}));
    }
    let mut text = serde_json::to_vec(&Value::Object(out)).map_err(|error| error.to_string())?;
    while !text.len().is_multiple_of(8) {
        text.push(b' ');
    }
    let mut prefix = (text.len() as u64).to_le_bytes().to_vec();
    prefix.extend_from_slice(&text);
    Ok(Selection { prefix, data: (low, high), tensors: tower.tensors, parameters })
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| format!("http client: {error}"))
}

/// One range request answered in full; a server that ignores `Range` is an error (it would send
/// the whole multi-gigabyte shard).
async fn get_range(client: &reqwest::Client, url: &str, start: u64, end_inclusive: u64) -> Result<reqwest::Response, String> {
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end_inclusive}"))
        .send()
        .await
        .map_err(|error| format!("range request failed: {error}"))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!("{url} answered a range request with HTTP {} (range requests are required)", response.status()));
    }
    Ok(response)
}

async fn get_bytes(client: &reqwest::Client, url: &str, start: u64, length: u64) -> Result<Vec<u8>, String> {
    let bytes = get_range(client, url, start, start + length - 1)
        .await?
        .bytes()
        .await
        .map_err(|error| format!("range read failed: {error}"))?;
    if bytes.len() as u64 != length {
        return Err(format!("expected {length} bytes from {url}, got {}", bytes.len()));
    }
    Ok(bytes.to_vec())
}

/// Install `tower` into `variant_dir/vision` from Hugging Face.
pub async fn install(
    tower: &'static TowerSpec,
    label: &str,
    variant_dir: &Path,
    cache: &VerifyCache,
    on_progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<PathBuf, String> {
    install_from(HUGGING_FACE, tower, label, variant_dir, cache, on_progress).await
}

/// [`install`] from a Hugging Face compatible host.
pub async fn install_from(
    base: &str,
    tower: &'static TowerSpec,
    label: &str,
    variant_dir: &Path,
    cache: &VerifyCache,
    on_progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<PathBuf, String> {
    let dir = tower_dir(variant_dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let total = tower.data_bytes + tower.files.iter().map(|file| file.bytes).sum::<u64>();
    let report = |phase: &str, file: &str, bytes: u64, detail: String| {
        on_progress(Progress { variant: label.to_owned(), phase: phase.into(), file: format!("{DIR}/{file}"), bytes, total, detail })
    };
    // The small files, whole, checked by git blob id.
    let mut done = 0u64;
    for file in &tower.files {
        let target = dir.join(file.path);
        let spec = small_spec(file);
        if !std::fs::metadata(&target).is_ok_and(|meta| meta.len() == file.bytes) {
            let progress = |phase: &str, bytes: u64, detail: String| report(phase, file.path, done + bytes, detail);
            variants::download(&url(base, tower, file.path), &spec, &target, &progress).await?;
        }
        if let Err(error) = cache.verify(&spec, &target, &mut |_, _| {}) {
            let _ = std::fs::remove_file(&target);
            report("failed", file.path, done, error.clone());
            return Err(error);
        }
        done += file.bytes;
    }
    let weights = dir.join(WEIGHTS);
    if !weights.exists() || !dir.join("manifest.json").exists() {
        fetch_tensors(base, tower, &dir, &|bytes, detail| report("download", WEIGHTS, done + bytes, detail)).await?;
    }
    // Verify: the pinned digest when there is one (the record makes later checks instant).
    let digest = {
        let path = weights.clone();
        let cache_path = cache.path().to_path_buf();
        let data_bytes = tower.data_bytes;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let task = tokio::task::spawn_blocking(move || {
            let cache = VerifyCache::at(cache_path);
            if let Some(recorded) = cache.recorded(&path) {
                return Ok(recorded);
            }
            let digest = variants::digest_file(&path, true, &mut |bytes, total| {
                // The file is the header plus the data; progress counts the data share.
                let _ = tx.send(bytes.saturating_sub(total.saturating_sub(data_bytes)));
            })?;
            cache.record_digest(&path, &digest);
            Ok::<_, String>(digest)
        });
        while let Some(bytes) = rx.recv().await {
            report("verify", WEIGHTS, done + bytes, String::new());
        }
        task.await.map_err(|error| format!("verification task failed: {error}"))??
    };
    let expected = format!("sha256:{}", tower.sha256);
    if !tower.sha256.is_empty() && digest != expected {
        let _ = std::fs::remove_file(&weights);
        let message = format!("{} has {digest}, expected {expected}", weights.display());
        report("failed", WEIGHTS, done, message.clone());
        return Err(message);
    }
    let manifest_path = dir.join("manifest.json");
    let mut manifest: Value = std::fs::read(&manifest_path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(json!({}));
    manifest["sha256"] = Value::String(digest.trim_start_matches("sha256:").to_owned());
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?)
        .map_err(|error| format!("cannot write {}: {error}", manifest_path.display()))?;
    report("done", WEIGHTS, total, dir.display().to_string());
    Ok(dir)
}

/// Fetch the shard header, select the vision tensors and append their bytes to a resumable
/// `.part` file that becomes `vision/model.safetensors`.
async fn fetch_tensors(base: &str, tower: &TowerSpec, dir: &Path, report: &(dyn Fn(u64, String) + Send + Sync)) -> Result<(), String> {
    let client = client()?;
    let shard = url(base, tower, tower.shard);
    let length = u64::from_le_bytes(
        get_bytes(&client, &shard, 0, 8).await?.try_into().map_err(|_| "short header length".to_owned())?,
    );
    if length != tower.shard_header_bytes {
        return Err(format!("{} has a {length}-byte header; the pinned revision has {}", tower.shard, tower.shard_header_bytes));
    }
    let header = get_bytes(&client, &shard, 8, length).await?;
    let selection = select(tower, &header)?;
    let data_start = 8 + length + selection.data.0;
    let data_end = 8 + length + selection.data.1;
    let part = dir.join(format!("{WEIGHTS}.part"));
    // A part file is valid only when it starts with this exact header.
    let existing = std::fs::metadata(&part).ok().map(|metadata| metadata.len());
    let reusable = existing.is_some_and(|len| len >= selection.prefix.len() as u64)
        && std::fs::File::open(&part).ok().is_some_and(|mut file| {
            let mut head = vec![0u8; selection.prefix.len()];
            std::io::Read::read_exact(&mut file, &mut head).is_ok() && head == selection.prefix
        });
    if !reusable {
        std::fs::write(&part, &selection.prefix).map_err(|error| format!("cannot write {}: {error}", part.display()))?;
    }
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let have = std::fs::metadata(&part).map(|meta| meta.len()).unwrap_or(0).saturating_sub(selection.prefix.len() as u64);
        let wanted = data_end - data_start;
        if have > wanted {
            std::fs::write(&part, &selection.prefix).map_err(|error| format!("cannot reset {}: {error}", part.display()))?;
            continue;
        }
        if have == wanted {
            break;
        }
        let outcome: Result<(), String> = async {
            use futures_util::StreamExt;
            let response = get_range(&client, &shard, data_start + have, data_end - 1).await?;
            let mut out = std::fs::OpenOptions::new().append(true).open(&part).map_err(|error| format!("cannot write {}: {error}", part.display()))?;
            let mut written = have;
            let mut stream = response.bytes_stream();
            let mut last = Instant::now();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| format!("download interrupted: {error}"))?;
                if written + chunk.len() as u64 > wanted {
                    return Err("the server sent more than the requested range".into());
                }
                out.write_all(&chunk).map_err(|error| format!("cannot write {}: {error}", part.display()))?;
                written += chunk.len() as u64;
                if last.elapsed() >= Duration::from_secs(1) {
                    report(written, String::new());
                    last = Instant::now();
                }
            }
            out.flush().map_err(|error| format!("cannot write {}: {error}", part.display()))?;
            report(written, String::new());
            if written != wanted {
                return Err(format!("download ended at {written} of {wanted} bytes"));
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => {}
            Err(error) if attempts < 20 => {
                report(have, format!("retrying after: {error}"));
                tokio::time::sleep(Duration::from_secs(u64::from(attempts.min(10)) * 2)).await;
            }
            Err(error) => return Err(error),
        }
    }
    let manifest = json!({
        "repo": tower.repo,
        "revision": tower.revision,
        "shard": tower.shard,
        "shard_bytes": tower.shard_bytes,
        "shard_sha256": tower.shard_sha256,
        "shard_range": [data_start, data_end],
        "prefix": tower.prefix,
        "tensors": selection.tensors,
        "parameters": selection.parameters,
        "header_bytes": selection.prefix.len(),
    });
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?)
        .map_err(|error| format!("cannot write the manifest: {error}"))?;
    std::fs::rename(&part, dir.join(WEIGHTS)).map_err(|error| format!("cannot move {} into place: {error}", part.display()))
}
