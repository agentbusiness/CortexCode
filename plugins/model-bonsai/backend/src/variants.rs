//! The model variants this plugin serves and their installation: PrismML's Ternary Bonsai 2 GGUF
//! files (alone, or with Qwen3.8's MTP draft layer added) on Hugging Face at a pinned revision, downloaded file by file with resume, each file
//! verified against the hash Hugging Face publishes for it (SHA-256 `lfs.oid` for LFS files, the git
//! blob SHA-1 `oid` for small files), recorded once per file. Installed variants are used offline.

use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One file of a model directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpec {
    pub path: &'static str,
    pub bytes: u64,
    /// Git blob SHA-1 (`oid` of the Hugging Face tree API).
    pub git_oid: &'static str,
    /// SHA-256 for LFS files (`lfs.oid`).
    pub sha256: Option<&'static str>,
    /// The repository and revision the file comes from, when not the variant's own.
    pub from: Option<(&'static str, &'static str)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub id: &'static str,
    pub label: &'static str,
    pub family: &'static str,
    /// The work the model is for (`cc_protocol::contracts::model::tags`).
    pub tags: &'static [&'static str],
    pub quant: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    /// The files the engine reads (README and images are not fetched).
    pub files: &'static [FileSpec],
    /// The GGUF the engine loads (one of `files`).
    pub weights: &'static str,
    /// The vision projector (image input), fetched with the variant when vision is on.
    pub mmproj: Option<&'static FileSpec>,
    /// Chat template shipped with this plugin (see `templates/README.md`).
    pub template: &'static str,
    /// Small model for functional tests.
    pub test: bool,
    /// The file carries an MTP draft layer.
    pub drafts: bool,
    pub notes: &'static str,
}

pub const HUGGING_FACE: &str = "https://huggingface.co";

/// PrismML's GGUF packs of Ternary Bonsai 2 27B (Qwen3.8-27B, ternary weights, Hadamard-rotated).
const BONSAI_REPO: &str = "prism-ml/Ternary-Bonsai-2-27B-gguf";
const BONSAI_REVISION: &str = "b072e1d3b35a0a630cece372c2127528e0994386";

/// Qwen3.8-27B's own tokenizer: the GGUF vocab tokenizes identically (checked at load), and the
/// template, the output parser and the grammar engine read this file.
const QWEN38_TOKENIZER: FileSpec = FileSpec {
    path: "tokenizer.json",
    bytes: 12_809_320,
    git_oid: "9328ce9c41e80f6dd7bc2c66d8ae1fc93bf87440",
    sha256: Some("0997f410c57a1f4e53b09e4be8f4a172d90edd9564368fb0847030937229b9f3"),
    from: Some(("Qwen/Qwen3.8-27B", "1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0")),
};

const BONSAI_PQ2: FileSpec = FileSpec {
    path: "Ternary-Bonsai-2-27B-PQ2_0.gguf",
    bytes: 7_206_168_928,
    git_oid: "",
    sha256: Some("3907dc1658db1f78a9826bf8d5bcb8dc65db0d466388937af57f2294fae62ec1"),
    from: None,
};

pub const BONSAI_MMPROJ: FileSpec = FileSpec {
    path: "Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf",
    bytes: 629_246_976,
    git_oid: "",
    sha256: Some("6807ede61d570bb86ba34b756a0fa109edc33668604de867c6ea6d8f1d631903"),
    from: Some((BONSAI_REPO, BONSAI_REVISION)),
};

/// PrismML's PQ2_0 file, unchanged (all 851 tensors), with Qwen3.8-27B's MTP layer trained further
/// on Bonsai 2's own output (Q8_0, 0.45 GB): the engine drafts with it (`fit::DRAFT_TOKENS`).
const MTP_REPO: &str = "ProCreations/Ternary-Bonsai-2-27B-MTP";
const MTP_REVISION: &str = "efffdea64c1f9e93cc7fa6bb24f72ae9d66ecf51";

const BONSAI_PQ2_MTP: FileSpec = FileSpec {
    path: "Ternary-Bonsai-2-27B-PQ2_0-MTP-Q8_0.gguf",
    bytes: 7_657_489_728,
    git_oid: "",
    sha256: Some("3cb3f0056d2e34ee44245a64396004a21f8492573d6ce1266ec4b7222c131dd4"),
    from: None,
};

/// PQ2_0 with the MTP draft layer (the owner's choice for this engine).
pub const DEFAULT_VARIANT: &str = "bonsai2-27b-pq2-mtp";

pub const CATALOG: &[Spec] = &[
    Spec {
        id: "bonsai2-27b-pq2-mtp",
        label: "Ternary Bonsai 2 27B (PQ2_0, MTP drafts)",
        family: "qwen3.8",
        tags: &[cc_protocol::contracts::model::tags::CODING],
        quant: "pq2_0-mtp",
        repo: MTP_REPO,
        revision: MTP_REVISION,
        files: &[QWEN38_TOKENIZER, BONSAI_PQ2_MTP],
        weights: BONSAI_PQ2_MTP.path,
        mmproj: Some(&BONSAI_MMPROJ),
        template: "qwen3.8.jinja",
        test: false,
        drafts: true,
        notes: "The PQ2_0 model with Qwen3.8's MTP layer (7.66 GB): it drafts the next token, the model checks it in the same step (lossless).",
    },
    Spec {
        id: "bonsai2-27b-pq2",
        label: "Ternary Bonsai 2 27B (PQ2_0)",
        family: "qwen3.8",
        tags: &[cc_protocol::contracts::model::tags::CODING],
        quant: "pq2_0",
        repo: BONSAI_REPO,
        revision: BONSAI_REVISION,
        files: &[QWEN38_TOKENIZER, BONSAI_PQ2],
        weights: BONSAI_PQ2.path,
        mmproj: Some(&BONSAI_MMPROJ),
        template: "qwen3.8.jinja",
        test: false,
        drafts: false,
        notes: "Ternary weights in 2-bit slots (2.13 bits per weight, 7.2 GB).",
    },
];

pub fn find(id: &str) -> Option<&'static Spec> {
    CATALOG.iter().find(|spec| spec.id == id)
}

impl Spec {
    /// `<models_dir>/<org>-<repo>/<revision>`: the directory the variant's files live in.
    pub fn local_dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.repo.replace('/', "-")).join(self.revision)
    }

    pub fn weights(&self) -> Option<&FileSpec> {
        self.files.iter().find(|file| file.path == self.weights)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    /// The file's URL on a Hugging Face compatible host (`<base>/<repo>/resolve/<rev>/<file>`).
    pub fn url_from(&self, base: &str, file: &FileSpec) -> String {
        let (repo, revision) = file.from.unwrap_or((self.repo, self.revision));
        format!("{}/{}/resolve/{}/{}", base.trim_end_matches('/'), repo, revision, file.path)
    }

    /// `repo@rev7:quant`, the identity reported as `model_id`.
    pub fn model_id(&self) -> String {
        format!("{}@{}:{}", self.repo, &self.revision[..7.min(self.revision.len())], self.quant)
    }
}

/// Whether every file is present at its full size (verification is separate).
pub fn installed(spec: &Spec, dir: &Path) -> bool {
    spec.files.iter().all(|file| {
        std::fs::metadata(dir.join(file.path)).map(|meta| meta.is_file() && meta.len() == file.bytes).unwrap_or(false)
    })
}

/// The expected digest of a file as recorded: `sha256:<hex>` or `gitsha1:<hex>`.
pub fn expected_digest(file: &FileSpec) -> String {
    match file.sha256 {
        Some(sha) => format!("sha256:{sha}"),
        None => format!("gitsha1:{}", file.git_oid),
    }
}

/// One verification record: the digest computed for a file at a size and modification time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Verified {
    bytes: u64,
    modified_ns: u128,
    /// SHA-256 hex (records written by earlier CortexCode plugins) or `gitsha1:<hex>`.
    sha256: String,
}

/// Records of computed digests, so a multi-gigabyte file is hashed once, not on every start. The
/// record lives in the models directory next to the files it describes; a record is valid only for
/// the file's exact size and modification time.
pub struct VerifyCache {
    path: PathBuf,
}

pub const VERIFY_RECORD: &str = ".cortexcode-verified.json";

fn modified_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|age| age.as_nanos())
}

/// Normalize a stored record to the `kind:hex` form (`sha256:` is implied for bare hex).
fn normalized(stored: &str) -> String {
    if stored.contains(':') { stored.to_owned() } else { format!("sha256:{stored}") }
}

impl VerifyCache {
    pub fn new(models_dir: &Path) -> Self {
        Self { path: models_dir.join(VERIFY_RECORD) }
    }

    fn load(&self) -> BTreeMap<String, Verified> {
        std::fs::read(&self.path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    /// The recorded digest (`kind:hex`) of `path`, if computed for the file as it is now.
    pub fn recorded(&self, path: &Path) -> Option<String> {
        let entry = self.load().remove(&path.display().to_string())?;
        let bytes = std::fs::metadata(path).ok()?.len();
        (entry.bytes == bytes && Some(entry.modified_ns) == modified_ns(path)).then(|| normalized(&entry.sha256))
    }

    /// A cache whose record file is `path`.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The record file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record a digest (`kind:hex`) computed for `path` as it is now.
    pub fn record_digest(&self, path: &Path, digest: &str) {
        self.record(path, digest);
    }

    fn record(&self, path: &Path, digest: &str) {
        let mut all = self.load();
        if let (Ok(metadata), Some(modified)) = (std::fs::metadata(path), modified_ns(path)) {
            // SHA-256 records keep the bare-hex form other plugins wrote, so they stay shared.
            let stored = digest.strip_prefix("sha256:").unwrap_or(digest).to_owned();
            all.insert(
                path.display().to_string(),
                Verified { bytes: metadata.len(), modified_ns: modified, sha256: stored },
            );
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_vec_pretty(&all) {
            let temporary = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
            if std::fs::write(&temporary, text).is_ok() {
                let _ = std::fs::rename(temporary, &self.path);
            }
        }
    }

    /// Hash the file (or reuse the record) and compare with the catalog. `on_progress(done, total)`.
    pub fn verify(&self, file: &FileSpec, path: &Path, on_progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
        let expected = expected_digest(file);
        let digest = match self.recorded(path).filter(|recorded| recorded.split(':').next() == expected.split(':').next())
        {
            Some(digest) => digest,
            None => {
                let digest = digest_file(path, file.sha256.is_some(), on_progress)?;
                self.record(path, &digest);
                digest
            }
        };
        if digest == expected {
            Ok(digest)
        } else {
            Err(format!("{} has {digest}, expected {expected}", path.display()))
        }
    }
}

/// `sha256:<hex>` of the content, or `gitsha1:<hex>` (SHA-1 of `blob <len>\0<content>`, git's
/// object id) for files Hugging Face stores in git directly.
pub fn digest_file(path: &Path, sha256: bool, on_progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let total = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut sha2 = Sha256::new();
    let mut sha1 = Sha1::new();
    if !sha256 {
        sha1.update(format!("blob {total}\0").as_bytes());
    }
    let mut buffer = vec![0u8; 8 << 20];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let read = file.read(&mut buffer).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if sha256 {
            sha2.update(&buffer[..read]);
        } else {
            sha1.update(&buffer[..read]);
        }
        done += read as u64;
        if last.elapsed() >= Duration::from_millis(500) {
            on_progress(done, total);
            last = Instant::now();
        }
    }
    on_progress(done, total);
    Ok(if sha256 { format!("sha256:{}", hex(&sha2.finalize())) } else { format!("gitsha1:{}", hex(&sha1.finalize())) })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Installation progress, published as `model.install` events.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Progress {
    pub variant: String,
    /// download | verify | done | failed
    pub phase: String,
    /// The file being worked on.
    pub file: String,
    /// Bytes done of the whole variant.
    pub bytes: u64,
    pub total: u64,
    #[serde(default)]
    pub detail: String,
}

/// Verify every file of an installed variant (cached records make this instant after the first time).
pub async fn verify_installed(spec: &'static Spec, models_dir: &Path, cache_path: PathBuf) -> Result<(), String> {
    let dir = spec.local_dir(models_dir);
    tokio::task::spawn_blocking(move || {
        let cache = VerifyCache { path: cache_path };
        for file in spec.files {
            cache.verify(file, &dir.join(file.path), &mut |_, _| {})?;
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("verification task failed: {error}"))?
}

/// Whether every file of the variant has a matching verification record (no hashing).
pub fn verified_by_record(spec: &Spec, models_dir: &Path) -> bool {
    let cache = VerifyCache::new(models_dir);
    let dir = spec.local_dir(models_dir);
    spec.files.iter().all(|file| cache.recorded(&dir.join(file.path)).as_deref() == Some(expected_digest(file).as_str()))
}

/// Make `spec` present and verified under `models_dir`: download missing files (resuming partial
/// `.part` files), check every digest against the catalog, and move files into place.
pub async fn install(
    spec: &'static Spec,
    models_dir: &Path,
    cache: &VerifyCache,
    on_progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<PathBuf, String> {
    install_from(HUGGING_FACE, spec, models_dir, cache, on_progress).await
}

/// [`install`] from a Hugging Face compatible host.
pub async fn install_from(
    base: &str,
    spec: &'static Spec,
    models_dir: &Path,
    cache: &VerifyCache,
    on_progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<PathBuf, String> {
    let dir = spec.local_dir(models_dir);
    let total = spec.total_bytes();
    let mut before = 0u64;
    for file in spec.files {
        let target = dir.join(file.path);
        let report = |phase: &str, done: u64, detail: String| {
            on_progress(Progress {
                variant: spec.id.to_owned(),
                phase: phase.to_owned(),
                file: file.path.to_owned(),
                bytes: before + done,
                total,
                detail,
            })
        };
        let present = std::fs::metadata(&target).map(|meta| meta.len() == file.bytes).unwrap_or(false);
        if !present {
            download(&spec.url_from(base, file), file, &target, &report).await?;
        }
        let path = target.clone();
        let cache_path = cache.path.clone();
        let file_spec = *file;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let task = tokio::task::spawn_blocking(move || {
            let cache = VerifyCache { path: cache_path };
            let mut tick = |done: u64, _total: u64| {
                let _ = tx.send(done);
            };
            cache.verify(&file_spec, &path, &mut tick)
        });
        while let Some(done) = rx.recv().await {
            report("verify", done, String::new());
        }
        match task.await.map_err(|error| format!("verification task failed: {error}"))? {
            Ok(_) => {}
            Err(error) => {
                // A file this call downloaded is discarded so the next attempt fetches it again. A
                // file that was already there is never touched: it is reported, not deleted.
                if !present {
                    let _ = std::fs::remove_file(&target);
                }
                report("failed", 0, error.clone());
                return Err(error);
            }
        }
        before += file.bytes;
    }
    on_progress(Progress {
        variant: spec.id.to_owned(),
        phase: "done".into(),
        file: String::new(),
        bytes: total,
        total,
        detail: dir.display().to_string(),
    });
    Ok(dir)
}

pub(crate) async fn download(
    url: &str,
    file: &FileSpec,
    target: &Path,
    report: &(dyn Fn(&str, u64, String) + Send + Sync),
) -> Result<(), String> {
    use futures_util::StreamExt;
    let parent = target.parent().ok_or("model path has no directory")?;
    std::fs::create_dir_all(parent).map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let part = target.with_file_name(format!("{name}.part"));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    // Several attempts: each resumes from what is already on disk.
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let have = std::fs::metadata(&part).map(|metadata| metadata.len()).unwrap_or(0);
        if have > file.bytes {
            std::fs::remove_file(&part).map_err(|error| format!("cannot reset {}: {error}", part.display()))?;
            continue;
        }
        if have == file.bytes {
            break;
        }
        let mut request = client.get(url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let outcome: Result<(), String> = async {
            let response = request.send().await.map_err(|error| format!("download request failed: {error}"))?;
            let status = response.status();
            let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
            if !status.is_success() {
                return Err(format!("download of {url} returned HTTP {status}"));
            }
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(resumed)
                .write(true)
                .truncate(!resumed)
                .open(&part)
                .map_err(|error| format!("cannot write {}: {error}", part.display()))?;
            let mut written = if resumed { have } else { 0 };
            let mut stream = response.bytes_stream();
            let mut last = Instant::now();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| format!("download interrupted: {error}"))?;
                if chunk.len() as u64 > file.bytes.saturating_sub(written) {
                    return Err("the server sent more than the expected file size".into());
                }
                out.write_all(&chunk).map_err(|error| format!("cannot write {}: {error}", part.display()))?;
                written += chunk.len() as u64;
                if last.elapsed() >= Duration::from_secs(1) {
                    report("download", written, String::new());
                    last = Instant::now();
                }
            }
            out.flush().map_err(|error| format!("cannot write {}: {error}", part.display()))?;
            report("download", written, String::new());
            if written != file.bytes {
                return Err(format!("download ended at {written} of {} bytes", file.bytes));
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => {}
            Err(error) if attempts < 20 => {
                report("download", have, format!("retrying after: {error}"));
                tokio::time::sleep(Duration::from_secs(u64::from(attempts.min(10)) * 2)).await;
            }
            Err(error) => return Err(error),
        }
    }
    // The digest is checked by the caller on the final path; move into place only at full size.
    std::fs::rename(&part, target).map_err(|error| format!("cannot move {} into place: {error}", part.display()))
}
