//! IronGraph's embedding engine files (never `~/.irongraph`): the embedding model lives in the
//! models folder the Cortex apps share (`~/.cortex-models`, `docs/SHARED-MODELS.md`),
//! `<CORTEX_HOME>/irongraph-cache` holds the SDK's rebuildable passage-vector cache. The app's old
//! model folder, `<CORTEX_HOME>/irongraph-models`, is moved into the shared one at every start.
//!
//! irongraph-sdk 0.1.8 downloads its default model (Bekko a8m) on first open when the model
//! directory lacks it, but no longer verifies the files and reports no progress to embedders. The
//! plugin therefore installs the model itself before opening the database: every file is pinned to
//! the size and digest Hugging Face publishes for the pinned revision (SHA-256 of the LFS object;
//! git blob SHA-1 for the small non-LFS `config.json`), downloads resume from a `.part` file with an
//! HTTP range request, progress is published as `memory.engine` events, and a file that fails
//! verification is never handed to the SDK. Installed files are re-verified on every start. All of
//! it happens with the revision folder's download lock held, so another Cortex app wanting the
//! same model waits and finds it there.

use cc_protocol::RpcError;
use cc_protocol::contracts::memory::EngineStatus;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use cc_protocol::Ctx;
use cc_protocol::contracts::memory::topics;
use std::sync::OnceLock;

/// The SDK's default embedding model and the revision irongraph-sdk 0.1.8 pins.
pub const MODEL_ID: &str = "hotchpotch/bekko-embedding-v1-a8m";
pub const MODEL_REVISION: &str = "c721113d59a1d91b447450324f51c4b3332c924a";
/// The layout irongraph-sdk 0.1.8 reads inside `model_dir`: `<family>/<revision>/<file>`.
const MODEL_FAMILY_DIR: &str = "hotchpotch-bekko-embedding-v1-a8m";

/// Expected content of one model file.
#[derive(Debug, Clone, Copy)]
pub enum Pin {
    /// SHA-256 of the file (Hugging Face LFS object id).
    Sha256(&'static str),
    /// Git blob SHA-1 (`sha1("blob <len>\0" + content)`), Hugging Face's id for non-LFS files.
    GitBlobSha1(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Artifact {
    pub name: &'static str,
    pub size: u64,
    pub pin: Pin,
}

/// The files the SDK loads, with the sizes and digests Hugging Face publishes for `MODEL_REVISION`
/// (`/api/models/<id>/revision/<rev>?blobs=true`).
pub const ARTIFACTS: [Artifact; 3] = [
    Artifact { name: "model.safetensors", size: 211_952_896, pin: Pin::Sha256("5db74812b69eb6c17845f5ecaab533e6e0816877fcf9639a25dd187862d0e885") },
    Artifact { name: "config.json", size: 1_207, pin: Pin::GitBlobSha1("34d95464b7b2a6440b7d214a7ff66f6ca03f4d14") },
    Artifact { name: "tokenizer.json", size: 34_363_442, pin: Pin::Sha256("8bd47075711f75a143d1b78e01a41cc65c1c591b00d3cfeffc23db07adce1392") },
];

const PUBLISH_EVERY: Duration = Duration::from_millis(500);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Engine directories: the database and vector cache under `CORTEX_HOME`, the model in the shared
/// models folder.
#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub data_dir: PathBuf,
    pub model_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// The app's old model folder, moved into `model_dir` before the model is looked up.
    pub old_model_dir: PathBuf,
}

impl EnginePaths {
    pub fn under(home: &Path) -> Self {
        Self {
            data_dir: home.join("irongraph"),
            model_dir: cc_shared_models::root(),
            cache_dir: home.join("irongraph-cache"),
            old_model_dir: home.join("irongraph-models"),
        }
    }

    /// The directory holding the pinned revision's files.
    pub fn revision_dir(model_dir: &Path) -> PathBuf {
        model_dir.join(MODEL_FAMILY_DIR).join(MODEL_REVISION)
    }
}

/// Shared, observable engine state.
pub struct Engine {
    pub paths: EnginePaths,
    status: Mutex<EngineStatus>,
    last_publish: Mutex<Instant>,
    /// Where `memory.engine` events go once the plugin runtime hands over its context.
    ctx: OnceLock<Ctx>,
}

pub fn total_bytes() -> u64 {
    ARTIFACTS.iter().map(|a| a.size).sum()
}

fn io_error(what: &str, path: &Path, error: std::io::Error) -> RpcError {
    RpcError::internal(format!("{what} {}: {error}", path.display()))
}

impl Engine {
    pub fn new(paths: EnginePaths) -> Arc<Self> {
        let status = EngineStatus {
            state: "installing".into(),
            model_id: MODEL_ID.into(),
            revision: MODEL_REVISION.into(),
            model_dir: paths.model_dir.to_string_lossy().into_owned(),
            cache_dir: paths.cache_dir.to_string_lossy().into_owned(),
            bytes_total: total_bytes(),
            ..EngineStatus::default()
        };
        Arc::new(Self { paths, status: Mutex::new(status), last_publish: Mutex::new(Instant::now()), ctx: OnceLock::new() })
    }

    pub fn status(&self) -> EngineStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Attach the plugin context; the current state is published at once.
    pub fn attach(&self, ctx: &Ctx) {
        if self.ctx.set(ctx.clone()).is_ok() {
            self.publish(&self.status());
        }
    }

    fn publish(&self, status: &EngineStatus) {
        if let (Some(ctx), Ok(data)) = (self.ctx.get(), serde_json::to_value(status)) {
            ctx.publish(topics::ENGINE, data);
        }
    }

    /// Apply `change`; notify subscribers on state changes, and for progress at most every 500 ms.
    pub fn update(&self, force: bool, change: impl FnOnce(&mut EngineStatus)) {
        let snapshot = {
            let Ok(mut status) = self.status.lock() else { return };
            let before = status.state.clone();
            change(&mut status);
            let due = self.last_publish.lock().map(|mut last| {
                let due = force || status.state != before || last.elapsed() >= PUBLISH_EVERY;
                if due {
                    *last = Instant::now();
                }
                due
            });
            if !due.unwrap_or(false) {
                return;
            }
            status.clone()
        };
        self.publish(&snapshot);
    }

    /// Make the pinned model available in `model_dir`: move the old model folder in, re-verify
    /// installed files, else download (resuming a partial download). Returns only when every file
    /// is present and matches its pin.
    pub async fn install(self: &Arc<Self>) -> Result<(), RpcError> {
        let started = Instant::now();
        self.update(true, |s| {
            s.state = "installing".into();
            s.error = None;
            s.bytes_done = 0;
            s.downloaded_bytes = 0;
            s.resumed_from = 0;
        });
        let result = self.install_files().await;
        let ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(source) => {
                tracing::info!(model = MODEL_ID, revision = MODEL_REVISION, %source, ms, dir = %self.paths.model_dir.display(), "embedding model ready");
                self.update(true, |s| {
                    s.state = "loading".into();
                    s.source = Some(source.clone());
                    s.install_ms = Some(ms);
                    s.bytes_done = s.bytes_total;
                });
            }
            Err(error) => {
                tracing::error!(%error, "embedding model installation failed");
                self.update(true, |s| {
                    s.state = "error".into();
                    s.error = Some(error.message.clone());
                });
            }
        }
        result.map(|_| ())
    }

    async fn install_files(self: &Arc<Self>) -> Result<String, RpcError> {
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || {
            cc_shared_models::move_in(&paths.old_model_dir, &paths.model_dir);
            cc_shared_models::ensure_root(&paths.model_dir)
        })
        .await
        .map_err(|e| RpcError::internal(format!("model folder task: {e}")))?
        .map_err(|e| io_error("cannot create", &self.paths.model_dir, e))?;
        let dir = EnginePaths::revision_dir(&self.paths.model_dir);
        std::fs::create_dir_all(&self.paths.cache_dir).map_err(|e| io_error("cannot create", &self.paths.cache_dir, e))?;
        // One installer per revision folder at a time, across the Cortex apps: what is there is
        // looked at with the lock held, since another app may have just fetched it.
        let lock_dir = dir.clone();
        let lock = tokio::task::spawn_blocking(move || cc_shared_models::lock(&lock_dir))
            .await
            .map_err(|e| RpcError::internal(format!("install lock task: {e}")))?
            .map_err(|e| io_error("cannot lock", &dir.join(cc_shared_models::LOCK), e))?;

        let mut origins = Vec::new();
        let mut done = 0u64;
        for artifact in ARTIFACTS {
            let origin = self.install_one(&dir, artifact, done).await?;
            done += artifact.size;
            self.update(true, |s| s.bytes_done = done);
            origins.push(origin);
        }
        drop(lock);
        sync_directory(&dir);
        Ok(if origins.iter().any(|o| matches!(o, Origin::Network)) { "network" } else { "installed" }.to_owned())
    }

    async fn install_one(self: &Arc<Self>, dir: &Path, artifact: Artifact, base: u64) -> Result<Origin, RpcError> {
        let target = dir.join(artifact.name);
        // Installed: re-verified on every start; a file that no longer matches is replaced.
        if let Ok(meta) = std::fs::metadata(&target) {
            if meta.is_file() && meta.len() == artifact.size && verify(&target, artifact).await? {
                return Ok(Origin::Installed);
            }
            tracing::warn!(file = %target.display(), "installed model file does not match its pinned digest; reinstalling");
            std::fs::remove_file(&target).map_err(|e| io_error("cannot remove", &target, e))?;
        }
        // Network, resumable: a `.part` file keeps what earlier attempts (or processes) fetched.
        let part = dir.join(format!("{}.part", artifact.name));
        let url = format!("https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{}", artifact.name);
        let partial = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0).min(artifact.size);
        self.update(true, |s| s.resumed_from += partial);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.download(&url, &part, artifact, base).await {
                Ok(()) => break,
                Err(error) if error.retryable && attempt < 6 => {
                    let wait = Duration::from_secs(2u64.pow(attempt.min(5)));
                    tracing::warn!(file = artifact.name, attempt, %error, ?wait, "model download interrupted; resuming");
                    tokio::time::sleep(wait).await;
                }
                Err(error) => {
                    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    return Err(RpcError::internal(format!(
                        "embedding model {MODEL_ID}@{MODEL_REVISION} is not installed in {} and {} could not be downloaded ({have} of {} bytes kept; the next start resumes): {}",
                        self.paths.model_dir.display(),
                        artifact.name,
                        artifact.size,
                        error.message
                    )));
                }
            }
        }
        if !verify(&part, artifact).await? {
            let _ = std::fs::remove_file(&part);
            return Err(RpcError::internal(format!("downloaded {} does not match its pinned digest; it was discarded", artifact.name)));
        }
        std::fs::rename(&part, &target).map_err(|e| io_error("cannot install", &target, e))?;
        Ok(Origin::Network)
    }

    /// Fetch the rest of `artifact` into `part`. Errors marked retryable leave `part` resumable.
    async fn download(self: &Arc<Self>, url: &str, part: &Path, artifact: Artifact, base: u64) -> Result<(), RpcError> {
        let mut existing = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if existing > artifact.size {
            // Longer than the pinned file: not a prefix of it.
            std::fs::remove_file(part).map_err(|e| io_error("cannot remove", part, e))?;
            existing = 0;
        }
        if existing == artifact.size {
            return Ok(());
        }
        self.update(true, |s| s.bytes_done = base + existing);
        tracing::info!(file = artifact.name, bytes = artifact.size, resume_from = existing, "downloading embedding model file");
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(NETWORK_TIMEOUT)
            .read_timeout(NETWORK_TIMEOUT)
            .build()
            .map_err(|e| RpcError::internal(format!("http client: {e}")))?;
        let mut request = client.get(url).header(reqwest::header::ACCEPT_ENCODING, "identity");
        if existing > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
        }
        let transient = |e: reqwest::Error| RpcError::internal(format!("{url}: {e}")).retryable();
        let mut response = request.send().await.map_err(transient)?;
        let offset = match response.status().as_u16() {
            206 => {
                // Structural check of `Content-Range: bytes <first>-<last>/<total>`.
                let range = response.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
                let expected = format!("bytes {existing}-{}/{}", artifact.size - 1, artifact.size);
                if range != expected {
                    return Err(RpcError::internal(format!("{url}: Content-Range {range:?}, expected {expected:?}")));
                }
                existing
            }
            200 => 0,
            416 => {
                // The server has nothing past what we hold: the partial file is not a prefix.
                std::fs::remove_file(part).map_err(|e| io_error("cannot remove", part, e))?;
                return Err(RpcError::internal(format!("{url}: range not satisfiable; partial file discarded")).retryable());
            }
            status => {
                let error = RpcError::internal(format!("{url}: HTTP {status}"));
                return Err(if status >= 500 || status == 429 { error.retryable() } else { error });
            }
        };
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(part).map_err(|e| io_error("cannot open", part, e))?;
        file.set_len(offset).map_err(|e| io_error("cannot truncate", part, e))?;
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(offset)).map_err(|e| io_error("cannot seek", part, e))?;
        let mut have = offset;
        while let Some(chunk) = response.chunk().await.map_err(transient)? {
            have += chunk.len() as u64;
            if have > artifact.size {
                drop(file);
                let _ = std::fs::remove_file(part);
                return Err(RpcError::internal(format!("{url}: body exceeds the pinned size {}", artifact.size)));
            }
            file.write_all(&chunk).map_err(|e| io_error("cannot write", part, e))?;
            let n = chunk.len() as u64;
            self.update(false, |s| {
                s.bytes_done = base + have;
                s.downloaded_bytes += n;
            });
        }
        file.sync_all().map_err(|e| io_error("cannot sync", part, e))?;
        if have != artifact.size {
            return Err(RpcError::internal(format!("{url}: transfer ended at {have} of {} bytes", artifact.size)).retryable());
        }
        tracing::info!(file = artifact.name, bytes = artifact.size, "embedding model file downloaded");
        Ok(())
    }
}

enum Origin {
    Installed,
    Network,
}

/// Whether `path` has exactly the pinned size and digest (hashing on a blocking thread).
pub async fn verify(path: &Path, artifact: Artifact) -> Result<bool, RpcError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || verify_blocking(&path, artifact)).await.map_err(|e| RpcError::internal(format!("verify task: {e}")))?
}

fn verify_blocking(path: &Path, artifact: Artifact) -> Result<bool, RpcError> {
    let mut file = std::fs::File::open(path).map_err(|e| io_error("cannot open", path, e))?;
    let len = file.metadata().map_err(|e| io_error("cannot stat", path, e))?.len();
    if len != artifact.size {
        return Ok(false);
    }
    let mut buffer = vec![0u8; 1 << 20];
    let digest = match artifact.pin {
        Pin::Sha256(expected) => {
            let mut hasher = Sha256::new();
            hash_all(&mut file, &mut buffer, path, |b| hasher.update(b))?;
            (hex(&hasher.finalize()), expected)
        }
        Pin::GitBlobSha1(expected) => {
            let mut hasher = Sha1::new();
            hasher.update(format!("blob {len}\0").as_bytes());
            hash_all(&mut file, &mut buffer, path, |b| hasher.update(b))?;
            (hex(&hasher.finalize()), expected)
        }
    };
    Ok(digest.0 == digest.1)
}

fn hash_all(file: &mut std::fs::File, buffer: &mut [u8], path: &Path, mut sink: impl FnMut(&[u8])) -> Result<(), RpcError> {
    loop {
        let n = file.read(buffer).map_err(|e| io_error("cannot read", path, e))?;
        if n == 0 {
            return Ok(());
        }
        sink(&buffer[..n]);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sync_directory(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}
