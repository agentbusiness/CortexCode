//! Installing a vision tower: only the vision tensors of the upstream checkpoint, fetched with HTTP
//! range requests from the one safetensors shard that holds them, written as a standalone
//! safetensors file into `<variant dir>/vision/`.
//!
//! 1. The shard's 8-byte header length and its JSON header (two small range requests); the header
//!    must list exactly the pinned tensor count and parameter count under the vision prefix, as one
//!    contiguous byte range.
//! 2. A new header with just those tensors (offsets rebased, keys sorted, so the file is
//!    deterministic), then the tensor bytes appended by one range request, resumable from a
//!    `.part` file. A complete file already in place is kept. All of it happens with the variant's
//!    revision folder's download lock held (`~/.cortex-models` is shared by the Cortex apps).
//! 3. The assembled file's SHA-256 is computed, compared with the pinned value when one is pinned,
//!    and recorded in the graph with an install record describing the source (`records`:
//!    `(:VisionInstall)-[:ON]->(:Machine)`). `config.json` and `preprocessor_config.json` are
//!    fetched whole and checked by git blob id.
//!
//! The tower's files are bytes and stay files; the install record is the graph's.

use super::{DIR, SmallFile, TowerSpec, WEIGHTS};
use crate::records::VisionInstall;
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

/// Every file present at its expected size, and the install recorded in the graph (verification
/// is separate). While the graph has not been read (the memory plugin did not answer), the files
/// decide.
pub fn installed(tower: &TowerSpec, variant_dir: &Path, cache: &VerifyCache) -> bool {
    let dir = tower_dir(variant_dir);
    let sized = |name: &str, bytes: Option<u64>| {
        std::fs::metadata(dir.join(name)).is_ok_and(|meta| meta.is_file() && bytes.is_none_or(|bytes| meta.len() == bytes))
    };
    tower.files.iter().all(|file| sized(file.path, Some(file.bytes))) && sized(WEIGHTS, None) && cache.vision_installed(&dir) != Some(false)
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
    cache.wait().await;
    let dir = tower_dir(variant_dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let total = tower.data_bytes + tower.files.iter().map(|file| file.bytes).sum::<u64>();
    let report = |phase: &str, file: &str, bytes: u64, detail: String| {
        on_progress(Progress { variant: label.to_owned(), phase: phase.into(), file: format!("{DIR}/{file}"), bytes, total, detail })
    };
    // The tower lives in the variant's revision folder: its download lock covers it, and what is
    // there is looked at with the lock held.
    let _lock = variants::lock_revision(variant_dir, &|detail| report("download", "", 0, detail)).await?;
    // The small files, whole, checked by git blob id.
    let mut done = 0u64;
    for file in &tower.files {
        let target = dir.join(file.path);
        let spec = small_spec(file);
        let checked = if std::fs::metadata(&target).is_ok_and(|meta| meta.len() == file.bytes) {
            cache.verify(&spec, &target, &mut |_, _| {}).inspect_err(|_| {
                let _ = std::fs::remove_file(&target);
            })
        } else {
            let progress = |phase: &str, bytes: u64, detail: String| report(phase, file.path, done + bytes, detail);
            variants::fetch(&url(base, tower, file.path), &spec, &target, cache, &progress).await
        };
        if let Err(error) = checked {
            report("failed", file.path, done, error.clone());
            return Err(error);
        }
        done += file.bytes;
    }
    let weights = dir.join(WEIGHTS);
    // No install record (and the graph read): the source is fetched again; a complete file in
    // place is kept, only its header is read.
    let install = if !weights.exists() || cache.vision_installed(&dir) == Some(false) {
        Some(fetch_tensors(base, tower, &dir, &|bytes, detail| report("download", WEIGHTS, done + bytes, detail)).await?)
    } else {
        None
    };
    // Verify: the pinned digest when there is one (the record makes later checks instant).
    let digest = {
        let path = weights.clone();
        let cache = cache.clone();
        let data_bytes = tower.data_bytes;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let task = tokio::task::spawn_blocking(move || {
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
    if let Some(mut install) = install {
        install.sha256 = digest.trim_start_matches("sha256:").to_owned();
        cache.record_vision(install);
    }
    report("done", WEIGHTS, total, dir.display().to_string());
    Ok(dir)
}

/// Fetch the shard header, select the vision tensors and append their bytes to a resumable
/// `.part` file that becomes `vision/model.safetensors` (a complete file in place is kept). The
/// install record describes the source; its digest is added once verified.
async fn fetch_tensors(base: &str, tower: &TowerSpec, dir: &Path, report: &(dyn Fn(u64, String) + Send + Sync)) -> Result<VisionInstall, String> {
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
    let install = VisionInstall {
        dir: dir.to_path_buf(),
        repo: tower.repo.to_owned(),
        revision: tower.revision.to_owned(),
        shard: tower.shard.to_owned(),
        shard_bytes: tower.shard_bytes,
        shard_sha256: tower.shard_sha256.to_owned(),
        shard_range: [data_start, data_end],
        prefix: tower.prefix.to_owned(),
        tensors: selection.tensors as u64,
        parameters: selection.parameters,
        header_bytes: selection.prefix.len() as u64,
        sha256: String::new(),
    };
    if starts_with(&dir.join(WEIGHTS), &selection.prefix, selection.prefix.len() as u64 + (data_end - data_start)) {
        return Ok(install);
    }
    let part = dir.join(format!("{WEIGHTS}.part"));
    // A part file is valid only when it starts with this exact header.
    let existing = std::fs::metadata(&part).ok().map(|metadata| metadata.len());
    let reusable = existing.is_some_and(|len| len >= selection.prefix.len() as u64) && starts_with(&part, &selection.prefix, 0);
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
    std::fs::rename(&part, dir.join(WEIGHTS)).map_err(|error| format!("cannot move {} into place: {error}", part.display()))?;
    Ok(install)
}

/// Whether the file starts with `head` (and, when `bytes` is not 0, has exactly that size).
fn starts_with(path: &Path, head: &[u8], bytes: u64) -> bool {
    if bytes != 0 && std::fs::metadata(path).ok().map(|meta| meta.len()) != Some(bytes) {
        return false;
    }
    std::fs::File::open(path).ok().is_some_and(|mut file| {
        let mut start = vec![0u8; head.len()];
        std::io::Read::read_exact(&mut file, &mut start).is_ok() && start == head
    })
}
