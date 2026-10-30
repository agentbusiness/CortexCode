//! Offloaded output: anything too large to show inline is stored byte-exact under
//! `<CORTEX_HOME>/artifacts/<id>`; the model sees the head, the tail and the id, and reads any part
//! back with `read artifact://<id>:<range>`. Nothing is dropped.
//!
//! ```text
//! (:Artifact {key: id, path, kind: 'output' | 'job', bytes, created_ms})-[:ON]->(:Machine)
//!     -[:FROM]->(:Session)
//! ```
//!
//! The bytes stay the file; the node is its metadata. A job's output is written while the job
//! runs: its node is put with the job's (see [`crate::jobs`]).

use crate::read::parse_range;
use crate::util;
use std::path::{Path, PathBuf};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::fd::{AsRawFd, FromRawFd};

#[derive(Clone)]
pub struct Artifacts {
    dir: PathBuf,
    home: PathBuf,
    graph: std::sync::Arc<crate::graph::Graph>,
}

/// Limits for inline output (bytes). Output above `inline` is offloaded; the inline view then
/// shows about `head` bytes from the start and `tail` bytes from the end, cut at line boundaries.
#[derive(Debug, Clone, Copy)]
pub struct InlineLimits {
    pub inline: usize,
    pub head: usize,
    pub tail: usize,
}

impl Default for InlineLimits {
    fn default() -> Self {
        Self { inline: 16 * 1024, head: 4 * 1024, tail: 8 * 1024 }
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Artifacts {
    pub fn new(home: &Path, graph: std::sync::Arc<crate::graph::Graph>) -> Self {
        let home = util::canonical_lenient(home);
        Self { dir: home.join("artifacts"), home, graph }
    }

    /// The node of artifact `id`.
    pub fn batch(&self, id: &str, kind: &str, bytes: u64, created_ms: u64, session: Option<&str>) -> crate::graph::Batch {
        use crate::graph;
        let me = graph::gref("Artifact", id);
        let node = graph::node(
            "Artifact",
            id,
            graph::props(serde_json::json!({ "path": self.dir.join(id).to_string_lossy(), "kind": kind, "bytes": bytes, "created_ms": created_ms })),
        );
        let soft = session.map(|session| graph::edge(&me, "FROM", &graph::gref("Session", session))).into_iter().collect();
        graph::Batch::workspace(vec![node], vec![graph::edge(&me, "ON", &graph::machine())], soft)
    }

    fn record(&self, id: &str, bytes: u64, session: Option<&str>) {
        self.graph.put(self.batch(id, "output", bytes, util::now_ms(), session));
    }

    /// Remove an artifact's file; its node is retired.
    pub fn remove(&self, id: &str) {
        if let Some(path) = self.path(id) {
            let _ = std::fs::remove_file(path);
            self.graph.retire(vec![crate::graph::gref("Artifact", id)], "removed");
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, id: &str) -> Option<PathBuf> {
        valid_id(id).then(|| self.dir.join(id))
    }

    /// Keep `bytes` as an artifact of `session`'s.
    pub fn store(&self, bytes: &[u8], session: Option<&str>) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let id = util::new_id("a");
        std::fs::write(self.dir.join(&id), bytes)?;
        self.record(&id, bytes.len() as u64, session);
        Ok(id)
    }

    /// Adopt an already written file (e.g. a spooled command output) as an artifact.
    pub fn adopt(&self, file: &Path, session: Option<&str>) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let id = util::new_id("a");
        let target = self.dir.join(&id);
        std::fs::rename(file, &target)?;
        self.record(&id, std::fs::metadata(&target).map(|meta| meta.len()).unwrap_or(0), session);
        Ok(id)
    }

    pub fn open(&self, id: &str) -> std::io::Result<std::fs::File> {
        let path =
            self.path(id).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("`{id}` is not an artifact id")))?;
        let resolved = std::fs::canonicalize(&path)?;
        if !resolved.starts_with(&self.dir) || crate::sandbox::control_read_path(&self.home, &resolved) {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "artifact aliases may not read app control files or leave the artifact directory"));
        }
        // Check the opened object too: a command can replace an artifact while it is being read.
        let directory = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(&self.dir)?;
        let name = std::ffi::CString::new(id).expect("validated ASCII artifact id");
        // SAFETY: the directory descriptor stays alive, the validated name is NUL-terminated,
        // and the returned descriptor is owned by the File exactly once.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
        if fd == -1 { return Err(std::io::Error::last_os_error()); }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "artifact must be a regular file without aliases"));
        }
        Ok(file)
    }

    pub fn read(&self, id: &str) -> std::io::Result<Vec<u8>> {
        let mut file = self.open(id)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Model-facing view of a large output: head, an explicit marker with the artifact id, and tail.
pub fn inline_view(bytes: &[u8], id: &str, limits: InlineLimits) -> String {
    let text = String::from_utf8_lossy(bytes);
    let head_end = cut_forward(&text, limits.head);
    let tail_start = cut_backward(&text, limits.tail).max(head_end);
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    let head_lines = head.lines().count();
    let omitted_lines = text[head_end..tail_start].lines().count();
    let mut out = String::with_capacity(head.len() + tail.len() + 256);
    out.push_str(head);
    if !head.ends_with('\n') && !head.is_empty() {
        out.push('\n');
    }
    out.push_str(&omitted_marker(id, head_lines, omitted_lines));
    out.push_str(tail);
    out
}

/// The line that stands for output left out of a result: how many lines, and one concrete range
/// of them to read (at most 200 lines from the first one left out).
pub fn omitted_marker(id: &str, head_lines: usize, omitted_lines: usize) -> String {
    let first = head_lines + 1;
    let last = (head_lines + omitted_lines.max(1)).min(head_lines + 200);
    format!("[{omitted_lines} lines not shown: read artifact://{id}:{first}-{last} for them]\n")
}

/// Largest char boundary at a line end within `limit` bytes from the start (at least one line).
fn cut_forward(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\n') {
        Some(newline) => newline + 1,
        None => end,
    }
}

/// Smallest line start such that the tail is within `limit` bytes.
fn cut_backward(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return 0;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(newline) if start + newline + 1 < text.len() => start + newline + 1,
        _ => start,
    }
}

/// Render part of an artifact as numbered lines (`N|text`) for the model.
pub fn render_range(bytes: &[u8], id: &str, range: Option<&str>, page_lines: usize) -> Result<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total = lines.len();
    let segments = match range {
        Some(range) => parse_range(range)?,
        None => vec![crate::read::Segment { start: 1, end: None }],
    };
    let mut out = String::new();
    for segment in segments {
        if segment.start > total.max(1) {
            out.push_str(&format!("[artifact://{id} has {total} lines; line {} is past the end]\n", segment.start));
            continue;
        }
        let (end, more) = match segment.end {
            Some(end) => (end.min(total), false),
            None => {
                let end = (segment.start + page_lines - 1).min(total);
                (end, end < total)
            }
        };
        out.push_str(&format!("artifact://{id} lines {}-{end} of {total}:\n", segment.start));
        for number in segment.start..=end {
            let line = lines[number - 1];
            let line = line.strip_suffix('\n').unwrap_or(line);
            out.push_str(&format!("{number}|{line}\n"));
        }
        if more {
            out.push_str(&format!("[more: read artifact://{id}:{}]\n", end + 1));
        }
    }
    Ok(out)
}
