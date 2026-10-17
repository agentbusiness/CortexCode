//! The line store: per-file line-level index kept outside the graph (line hashes and byte offsets),
//! keyed by the file's content hash. Tags are `cc_protocol::hashline::tag` of each line, so the
//! anchors shown by the tools plugin and the spans returned here agree by construction.

use cc_protocol::hashline;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 6] = b"CCLS1\n";

#[derive(Debug, Clone, PartialEq)]
pub struct LineIndex {
    /// Byte offset of the start of each line.
    pub offsets: Vec<u32>,
    /// Hashline tag of each line.
    pub tags: Vec<String>,
}

impl LineIndex {
    pub fn build(text: &str) -> Self {
        let lines = hashline::file_lines(text);
        let mut offsets = Vec::with_capacity(lines.len());
        let mut tags = Vec::with_capacity(lines.len());
        let mut offset = 0usize;
        for line in lines {
            offsets.push(offset as u32);
            tags.push(hashline::tag(line));
            offset += line.len() + 1;
        }
        Self { offsets, tags }
    }

    /// `N:hh` anchor of a 1-based line.
    pub fn anchor(&self, line: u32) -> Option<String> {
        let tag = self.tags.get((line as usize).checked_sub(1)?)?;
        Some(format!("{line}:{tag}"))
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10 + self.offsets.len() * 6);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.offsets.len() as u32).to_le_bytes());
        for (offset, tag) in self.offsets.iter().zip(&self.tags) {
            out.extend_from_slice(&offset.to_le_bytes());
            let bytes = tag.as_bytes();
            out.push(bytes.first().copied().unwrap_or(b'0'));
            out.push(bytes.get(1).copied().unwrap_or(b'0'));
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 10 || &bytes[..6] != MAGIC {
            return None;
        }
        let count = u32::from_le_bytes(bytes[6..10].try_into().ok()?) as usize;
        if bytes.len() != 10 + count * 6 {
            return None;
        }
        let mut offsets = Vec::with_capacity(count);
        let mut tags = Vec::with_capacity(count);
        for i in 0..count {
            let base = 10 + i * 6;
            offsets.push(u32::from_le_bytes(bytes[base..base + 4].try_into().ok()?));
            tags.push(String::from_utf8_lossy(&bytes[base + 4..base + 6]).into_owned());
        }
        Some(Self { offsets, tags })
    }
}

/// The line store of one indexed tree.
#[derive(Debug, Clone)]
pub struct LineStore {
    dir: PathBuf,
}

impl LineStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, hash: &str) -> PathBuf {
        let shard = hash.get(..2).unwrap_or("00");
        self.dir.join(shard).join(format!("{hash}.lines"))
    }

    /// Store the index of `text` under `hash` (no-op when already present).
    pub fn put(&self, hash: &str, text: &str) -> std::io::Result<()> {
        let path = self.path(hash);
        if path.is_file() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&LineIndex::build(text).encode())?;
        file.sync_all()?;
        std::fs::rename(tmp, path)
    }

    pub fn get(&self, hash: &str) -> Option<LineIndex> {
        std::fs::read(self.path(hash)).ok().and_then(|bytes| LineIndex::decode(&bytes))
    }

    /// Remove entries whose hash is no longer referenced by any file.
    pub fn retain(&self, live: &HashSet<String>) -> std::io::Result<usize> {
        let mut removed = 0;
        let Ok(shards) = std::fs::read_dir(&self.dir) else { return Ok(0) };
        for shard in shards.flatten() {
            let Ok(entries) = std::fs::read_dir(shard.path()) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(hash) = name.strip_suffix(".lines")
                    && !live.contains(hash)
                {
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
