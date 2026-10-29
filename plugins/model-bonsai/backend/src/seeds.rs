//! Shared leading blocks on disk: a block a conversation of a project starts with (system prompt,
//! tools), saved when the engine keeps it, so a new conversation after a restart (or after the
//! block made room for another project's) starts from it instead of re-reading it.
//!
//! A file is named by the block's tokens and the engine it was made with (model file, KV type,
//! engine revision), so a file never loads into an engine that cannot read it. It holds the
//! block's tokens, the sequence's whole state (attention rows and recurrent state), the hidden
//! state the drafter pairs with the next row, and the draft context's rows. The newest
//! [`KEPT_FILES`] stay.

use crate::llama::KvType;
use sha2::{Digest, Sha256};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Saved blocks kept on disk (the newest by use): the projects opened lately.
pub const KEPT_FILES: usize = 4;

const MAGIC: &[u8] = b"cortexcode bonsai seed 1\n";
const EXTENSION: &str = "seed";

/// A shared leading block as saved.
pub struct Saved {
    pub tokens: Vec<u32>,
    /// The sequence's whole state.
    pub state: Vec<u8>,
    /// The model's hidden state at the block's last token (empty without a drafter).
    pub hidden: Vec<f32>,
    /// The draft context's whole state for the sequence; empty: no drafts from this block.
    pub draft_state: Vec<u8>,
}

pub struct Store {
    dir: PathBuf,
    identity: String,
}

impl Store {
    pub fn new(dir: PathBuf, weights: &Path, kv: KvType) -> Self {
        let size = std::fs::metadata(weights).map_or(0, |meta| meta.len());
        let name = weights.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let identity = format!("{name} {size} {} {}", kv.name(), crate::llama::REVISION);
        Self { dir, identity }
    }

    fn path(&self, tokens: &[u32]) -> PathBuf {
        let mut hash = Sha256::new();
        hash.update(self.identity.as_bytes());
        for token in tokens {
            hash.update(token.to_le_bytes());
        }
        let name: String = hash.finalize().iter().take(16).map(|byte| format!("{byte:02x}")).collect();
        self.dir.join(format!("{name}.{EXTENSION}"))
    }

    /// Whether the block that is exactly `tokens` is saved; its use keeps it among the newest.
    pub fn has(&self, tokens: &[u32]) -> bool {
        let Ok(file) = std::fs::File::options().write(true).open(self.path(tokens)) else { return false };
        let _ = file.set_modified(SystemTime::now());
        true
    }

    /// Write the block on its own thread (the engine goes on), then keep the newest files.
    pub fn save(&self, saved: Saved) {
        let path = self.path(&saved.tokens);
        let identity = self.identity.clone();
        let dir = self.dir.clone();
        let spawned = std::thread::Builder::new().name("bonsai-seed-save".into()).spawn(move || {
            let started = std::time::Instant::now();
            match write(&dir, &path, &identity, &saved) {
                Ok(bytes) => tracing::info!(tokens = saved.tokens.len(), bytes, ms = started.elapsed().as_millis() as u64, "saved a shared leading block"),
                Err(error) => tracing::warn!(%error, path = %path.display(), "saving a shared leading block failed"),
            }
            prune(&dir);
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start saving a shared leading block");
        }
    }

    /// The saved block that is exactly `tokens`, if any. A file that does not read is deleted.
    pub fn load(&self, tokens: &[u32]) -> Option<Saved> {
        let path = self.path(tokens);
        let file = std::fs::File::open(&path).ok()?;
        match read(file, &self.identity) {
            Ok(saved) if saved.tokens == tokens => {
                // Its use keeps it among the newest.
                if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                    let _ = file.set_modified(SystemTime::now());
                }
                Some(saved)
            }
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "a saved shared leading block does not read; deleting it");
                self.forget(tokens);
                None
            }
        }
    }

    /// Delete the saved block that is exactly `tokens` (the engine could not restore it).
    pub fn forget(&self, tokens: &[u32]) {
        let _ = std::fs::remove_file(self.path(tokens));
    }
}

fn write(dir: &Path, path: &Path, identity: &str, saved: &Saved) -> std::io::Result<u64> {
    std::fs::create_dir_all(dir)?;
    let partial = path.with_extension("partial");
    let mut out = BufWriter::new(std::fs::File::create(&partial)?);
    out.write_all(MAGIC)?;
    section(&mut out, identity.as_bytes())?;
    let tokens: Vec<u8> = saved.tokens.iter().flat_map(|token| token.to_le_bytes()).collect();
    section(&mut out, &tokens)?;
    section(&mut out, &saved.state)?;
    let hidden: Vec<u8> = saved.hidden.iter().flat_map(|value| value.to_le_bytes()).collect();
    section(&mut out, &hidden)?;
    section(&mut out, &saved.draft_state)?;
    out.into_inner().map_err(|error| error.into_error())?.sync_all()?;
    std::fs::rename(&partial, path)?;
    Ok(std::fs::metadata(path)?.len())
}

fn section(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(&(bytes.len() as u64).to_le_bytes())?;
    out.write_all(bytes)
}

fn read(file: std::fs::File, identity: &str) -> Result<Saved, String> {
    let mut input = BufReader::new(file);
    let mut magic = vec![0u8; MAGIC.len()];
    input.read_exact(&mut magic).map_err(|error| error.to_string())?;
    if magic != MAGIC {
        return Err("not a saved block".into());
    }
    if next(&mut input)? != identity.as_bytes() {
        return Err("made by another model or engine".into());
    }
    let tokens = next(&mut input)?.chunks_exact(4).map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
    let state = next(&mut input)?;
    let hidden = next(&mut input)?.chunks_exact(4).map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
    let draft_state = next(&mut input)?;
    Ok(Saved { tokens, state, hidden, draft_state })
}

fn next(input: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 8];
    input.read_exact(&mut len).map_err(|error| error.to_string())?;
    let mut bytes = vec![0u8; usize::try_from(u64::from_le_bytes(len)).map_err(|error| error.to_string())?];
    input.read_exact(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// Keep the newest [`KEPT_FILES`] saved blocks.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == EXTENSION))
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(KEPT_FILES) {
        let _ = std::fs::remove_file(path);
    }
}
