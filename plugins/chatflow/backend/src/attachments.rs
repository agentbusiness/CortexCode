//! Message attachments: pasted screenshots, dropped or picked files, whole folders.
//!
//! - `chatflow.attachment.put` turns what the UI has into an attachment: bytes (a paste that never
//!   was a file, written by the shell to `attachments/incoming/` or sent as base64) are stored content-addressed in `$CORTEX_HOME/attachments/<sha256>.<ext>`; a path inside
//!   the project (its root or the session's workspace) is referenced as it is, never copied; a file
//!   outside the project is copied into the attachment area (the model then reads a stable copy);
//!   a folder is always referenced by its path. Nothing is ever written into the repository.
//! - When a message is journaled, [`prepare`] checks every attachment still exists, records a
//!   folder's top-level listing (within a small budget) and an image's dimensions, and asks the
//!   vision hook ([`vision_hook`]) for a description.
//! - The projection renders them after the message text ([`render`]): paths the model reads with
//!   its tools, sizes, dimensions; an image is described only when a vision capability said what it
//!   shows. The main model has no vision, so without a vision plugin an image is a file and the
//!   text says so.

use crate::data::Dataset;
use crate::host::Host;
use base64::Engine;
use cc_protocol::contracts::chatflow::{Attachment, AttachmentPutParams, AttachmentPutResult, attachment_kinds, limits};
use cc_protocol::contracts::model::{ChatMessage, ContentPart};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The folder under `CORTEX_HOME` where attachment bytes are stored.
pub const AREA: &str = "attachments";
/// A folder's top-level listing: at most this many entries and characters reach the model.
pub const LISTING_ENTRIES: usize = 60;
pub const LISTING_CHARS: usize = 1500;
/// Bytes read from an image to find its dimensions (a JPEG's frame header can follow a large EXIF
/// block).
const HEADER_BYTES: u64 = 1024 * 1024;
/// Pasted text's preview for the model: this many lines from its head and from its tail, each cut
/// at [`PREVIEW_LINE_CHARS`], all within [`PREVIEW_CHARS`] (about 1,000 tokens).
pub const PREVIEW_LINES: usize = 20;
pub const PREVIEW_LINE_CHARS: usize = 200;
pub const PREVIEW_CHARS: usize = 4000;
/// Pasted text up to this size (bytes, about 6k tokens) reaches the model whole: a test run's
/// output is evidence the turn works from, and a folded middle made the model stop to ask for it.
pub const WHOLE_CHARS: usize = 24_000;
/// A vision description longer than this is cut (the model reads the image's path for more).
const DESCRIPTION_CHARS: usize = 4000;

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message.into())
}

/// Human-readable size (mechanical: B, KB, MB, GB with one decimal).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// The MIME type of a file name by its extension (a short table; unknown -> octet-stream).
pub fn mime_of(name: &str) -> &'static str {
    let extension = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
}

/// The extension a stored copy gets: the name's own when it is a plain one, else the MIME type's.
fn stored_extension(name: &str, mime: &str) -> String {
    let own = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !own.is_empty() && own.len() <= 10 && own.chars().all(|c| c.is_ascii_alphanumeric()) {
        return own;
    }
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        "image/tiff" => "tiff",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
    .to_owned()
}

fn kind_of(mime: &str) -> &'static str {
    if mime.starts_with("image/") { attachment_kinds::IMAGE } else { attachment_kinds::FILE }
}

/// Width and height of a PNG, GIF, JPEG, WebP or BMP from its first bytes (structural parse).
pub fn image_size(head: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| head.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as u32);
    let le16 = |at: usize| head.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32);
    let be32 = |at: usize| head.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let le24 = |at: usize| head.get(at..at + 3).map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0]));
    let le32 = |at: usize| head.get(at..at + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if head.starts_with(b"\x89PNG\r\n\x1a\n") && head.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if head.starts_with(b"BM") {
        return Some((le32(18)?.unsigned_abs(), le32(22)?.unsigned_abs()));
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return match head.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8L" => {
                let bits = u32::from_le_bytes([*head.get(21)?, *head.get(22)?, *head.get(23)?, *head.get(24)?]);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            _ => None,
        };
    }
    if head.starts_with(&[0xff, 0xd8]) {
        let mut at = 2;
        while at + 4 <= head.len() {
            if head[at] != 0xff {
                at += 1;
                continue;
            }
            let marker = head[at + 1];
            if marker == 0xff {
                at += 1;
                continue;
            }
            // Markers without a length.
            if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
                at += 2;
                continue;
            }
            let length = be16(at + 2)? as usize;
            let frame = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
            if frame {
                return Some((be16(at + 7)?, be16(at + 5)?));
            }
            at += 2 + length;
        }
    }
    None
}

fn read_head(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(HEADER_BYTES).read_to_end(&mut head).ok()?;
    Some(head)
}

/// Whether `path` is inside one of `roots` (both canonicalized by the caller).
fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Store `bytes` content-addressed in the attachment area; returns the path and the hash.
pub fn store(home: &Path, bytes: &[u8], extension: &str) -> Result<(PathBuf, String), RpcError> {
    let sha = format!("{:x}", Sha256::digest(bytes));
    let dir = home.join(AREA);
    std::fs::create_dir_all(&dir).map_err(|error| RpcError::internal(format!("cannot create {}: {error}", dir.display())))?;
    let path = dir.join(format!("{sha}.{extension}"));
    if !path.is_file() {
        // Written beside it, then renamed: a reader never sees half a file.
        let partial = dir.join(format!(".{sha}.{}.partial", uuid::Uuid::new_v4().simple()));
        std::fs::write(&partial, bytes).map_err(|error| RpcError::internal(format!("cannot write {}: {error}", partial.display())))?;
        std::fs::rename(&partial, &path).map_err(|error| {
            let _ = std::fs::remove_file(&partial);
            RpcError::internal(format!("cannot store {}: {error}", path.display()))
        })?;
    }
    Ok((path, sha))
}

fn too_big(name: &str, bytes: u64, what: &str) -> RpcError {
    invalid(format!(
        "{name} is {}; {what} may be at most {}.",
        human_size(bytes),
        human_size(limits::MAX_ATTACHMENT_BYTES)
    ))
}

fn check_thumb(thumb: Option<String>) -> Result<Option<String>, RpcError> {
    match thumb.filter(|thumb| !thumb.is_empty()) {
        None => Ok(None),
        Some(thumb) if !thumb.starts_with("data:image/") => Err(invalid("a preview must be a data:image/ URL")),
        Some(thumb) if thumb.len() > limits::MAX_THUMB_BYTES => {
            Err(invalid(format!("the preview is {}; at most {}", human_size(thumb.len() as u64), human_size(limits::MAX_THUMB_BYTES as u64))))
        }
        Some(thumb) => Ok(Some(thumb)),
    }
}

/// Where the shell writes bytes that never were a file (a pasted screenshot) before they are put:
/// `$CORTEX_HOME/attachments/incoming/`. A put moves them into the store.
pub const INCOMING: &str = "incoming";

/// What a put stored: hash it, keep it once by content (`<sha256>.<ext>`).
fn stored_result(home: &Path, bytes: &[u8], name: String, mime: String, thumb: Option<String>) -> Result<AttachmentPutResult, RpcError> {
    let (path, sha) = store(home, bytes, &stored_extension(&name, &mime))?;
    let size = (kind_of(&mime) == attachment_kinds::IMAGE).then(|| image_size(bytes)).flatten();
    Ok(AttachmentPutResult {
        sha256: Some(sha),
        path: path.to_string_lossy().into_owned(),
        size: bytes.len() as u64,
        width: size.map(|(width, _)| width),
        height: size.map(|(_, height)| height),
        kind: kind_of(&mime).to_owned(),
        name,
        mime,
        stored: true,
        thumb,
    })
}

/// `chatflow.attachment.put`: bytes or a path into an attachment. `roots` are the project's root
/// and the session's workspace (canonical); a path inside one is referenced, never copied.
pub fn put(home: &Path, roots: &[PathBuf], params: AttachmentPutParams) -> Result<AttachmentPutResult, RpcError> {
    let thumb = check_thumb(params.thumb)?;
    match (params.path.filter(|path| !path.trim().is_empty()), params.data) {
        (Some(_), Some(_)) => Err(invalid("give either a path or data, not both")),
        (None, None) => Err(invalid("give a path or data to attach")),
        (None, Some(data)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|error| invalid(format!("the data is not base64: {error}")))?;
            let name = if params.name.trim().is_empty() { "attachment".to_owned() } else { params.name.trim().to_owned() };
            if bytes.len() as u64 > limits::MAX_ATTACHMENT_BYTES {
                return Err(too_big(&name, bytes.len() as u64, "an attachment"));
            }
            let mime = if params.mime.trim().is_empty() { mime_of(&name).to_owned() } else { params.mime.trim().to_owned() };
            stored_result(home, &bytes, name, mime, thumb)
        }
        (Some(path), None) => {
            let given = PathBuf::from(path.trim());
            if !given.is_absolute() {
                return Err(invalid(format!("{} is not an absolute path", given.display())));
            }
            let canonical = given.canonicalize().map_err(|error| invalid(format!("{} cannot be attached: {error}", given.display())))?;
            let file_name = canonical.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| canonical.to_string_lossy().into_owned());
            let name = if params.name.trim().is_empty() { file_name } else { params.name.trim().to_owned() };
            let metadata = std::fs::metadata(&canonical).map_err(|error| invalid(format!("{} cannot be attached: {error}", canonical.display())))?;
            if metadata.is_dir() {
                return Ok(AttachmentPutResult {
                    path: canonical.to_string_lossy().into_owned(),
                    mime: "inode/directory".into(),
                    kind: attachment_kinds::FOLDER.into(),
                    name,
                    ..AttachmentPutResult::default()
                });
            }
            let mime = if params.mime.trim().is_empty() { mime_of(&name).to_owned() } else { params.mime.trim().to_owned() };
            let incoming = home.join(AREA).join(INCOMING).canonicalize().ok();
            if incoming.as_ref().is_some_and(|incoming| canonical.starts_with(incoming)) {
                // Bytes the shell wrote for this put: they move into the store.
                let bytes = std::fs::read(&canonical).map_err(|error| invalid(format!("{} cannot be read: {error}", canonical.display())))?;
                let _ = std::fs::remove_file(&canonical);
                if bytes.len() as u64 > limits::MAX_ATTACHMENT_BYTES {
                    return Err(too_big(&name, bytes.len() as u64, "an attachment"));
                }
                return stored_result(home, &bytes, name, mime, thumb);
            }
            let kind = kind_of(&mime);
            if inside(&canonical, roots) {
                // The user's own project file: referenced where it is (hashed when it is not huge,
                // so an image can be cached by content; never copied).
                let small = metadata.len() <= limits::MAX_ATTACHMENT_BYTES;
                let bytes = if small { std::fs::read(&canonical).ok() } else { None };
                let sha256 = bytes.as_deref().map(|bytes| format!("{:x}", Sha256::digest(bytes)));
                let size = if kind == attachment_kinds::IMAGE {
                    bytes.as_deref().and_then(image_size).or_else(|| read_head(&canonical).and_then(|head| image_size(&head)))
                } else {
                    None
                };
                return Ok(AttachmentPutResult {
                    sha256,
                    path: canonical.to_string_lossy().into_owned(),
                    mime,
                    size: metadata.len(),
                    width: size.map(|(width, _)| width),
                    height: size.map(|(_, height)| height),
                    kind: kind.into(),
                    name,
                    stored: false,
                    thumb,
                });
            }
            if metadata.len() > limits::MAX_ATTACHMENT_BYTES {
                return Err(too_big(&name, metadata.len(), "a file from outside the project (it is copied for the model)"));
            }
            let bytes = std::fs::read(&canonical).map_err(|error| invalid(format!("{} cannot be read: {error}", canonical.display())))?;
            stored_result(home, &bytes, name, mime, thumb)
        }
    }
}

/// Pasted text's line count and its preview: the first and last [`PREVIEW_LINES`] lines (all of it
/// when it is that short), long lines cut, the whole within [`PREVIEW_CHARS`].
pub fn text_preview(ds: &Dataset, text: &str) -> (u64, String) {
    let lines: Vec<&str> = text.lines().collect();
    if text.len() <= WHOLE_CHARS {
        return (lines.len() as u64, lines.join("\n"));
    }
    let cut = |line: &str| -> String {
        let count = line.chars().count();
        if count <= PREVIEW_LINE_CHARS {
            line.to_owned()
        } else {
            let kept: String = line.chars().take(PREVIEW_LINE_CHARS).collect();
            format!("{kept}{}", ds.text("attachment_preview_cut", &[("count", &(count - PREVIEW_LINE_CHARS).to_string())]))
        }
    };
    // Each half gets half the budget, so the tail is never squeezed out by long head lines.
    let half = PREVIEW_CHARS / 2;
    let take = |iter: &mut dyn Iterator<Item = &&str>| -> Vec<String> {
        let mut out = Vec::new();
        let mut used = 0;
        for line in iter.take(PREVIEW_LINES) {
            let line = cut(line);
            if used + line.len() + 1 > half {
                break;
            }
            used += line.len() + 1;
            out.push(line);
        }
        out
    };
    let head = take(&mut lines.iter());
    let shown_head = head.len();
    let mut tail = take(&mut lines.iter().skip(shown_head).rev());
    tail.reverse();
    let omitted = lines.len() - shown_head - tail.len();
    let mut preview = head.join("\n");
    if omitted > 0 {
        preview.push('\n');
        preview.push_str(&ds.text("attachment_preview_gap", &[("count", &omitted.to_string())]));
    }
    if !tail.is_empty() {
        preview.push('\n');
        preview.push_str(&tail.join("\n"));
    }
    (lines.len() as u64, preview)
}

/// A folder's top-level entries, folders first, within [`LISTING_ENTRIES`] and [`LISTING_CHARS`].
pub fn listing(ds: &Dataset, dir: &Path) -> String {
    let Ok(read) = std::fs::read_dir(dir) else { return ds.text("attachment_listing_unreadable", &[]) };
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => folders.push(format!("{name}/")),
            Ok(metadata) => files.push(format!("{name} ({})", human_size(metadata.len()))),
            Err(_) => files.push(name),
        }
    }
    folders.sort();
    files.sort();
    let total = folders.len() + files.len();
    if total == 0 {
        return ds.text("attachment_listing_empty", &[]);
    }
    let mut out = Vec::new();
    let mut chars = 0;
    for line in folders.into_iter().chain(files) {
        if out.len() >= LISTING_ENTRIES || chars + line.len() + 1 > LISTING_CHARS {
            break;
        }
        chars += line.len() + 1;
        out.push(line);
    }
    let shown = out.len();
    let mut text = out.join("\n");
    if shown < total {
        text.push('\n');
        text.push_str(&ds.text("attachment_listing_more", &[("count", &(total - shown).to_string())]));
    }
    text
}

/// The vision hook. The main model cannot see; a `vision` plugin (being designed) will serve
/// [`vision_hook::METHOD`] and answer what an image shows in words. Until one is present this
/// returns `None` and the image goes to the model as a file (path, size, dimensions). Nothing here
/// ever makes up a description.
pub mod vision_hook {
    use super::*;

    /// The method a vision capability serves: `{path, name, mime, width?, height?}` ->
    /// `{text, model?}`. When the main model itself gains vision (model.info will say so), this is
    /// also the place to hand it the image instead.
    pub const METHOD: &str = "vision.describe";

    /// Ask the vision capability, once, without retries: when nothing serves it the frame answers
    /// at once (`unavailable`) and the image stays undescribed. Returns `(text, by)`.
    pub async fn describe(host: &Host, image: &Attachment) -> Option<(String, String)> {
        let params = json!({"path": image.path, "name": image.name, "mime": image.mime, "width": image.width, "height": image.height});
        match host.ctx().call(METHOD, params).await {
            Ok(answer) => {
                let text = answer.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if text.is_empty() {
                    return None;
                }
                let by = answer.get("model").and_then(Value::as_str).filter(|by| !by.is_empty()).unwrap_or("vision").to_owned();
                Some((text.chars().take(DESCRIPTION_CHARS).collect(), by))
            }
            Err(error) => {
                if error.code != codes::UNAVAILABLE && error.code != codes::UNKNOWN_METHOD {
                    tracing::warn!(%error, path = %image.path, "the vision capability could not describe an image");
                }
                None
            }
        }
    }
}

/// Check and complete a message's attachments before they are journaled: each must still exist and
/// be what its kind says; folders get their listing, images their dimensions. Facts a caller sent
/// for the fields the chat flow sets are replaced. (The vision hook runs after, in [`prepare`].)
pub fn check(ds: &Dataset, attachments: Vec<Attachment>) -> Result<Vec<Attachment>, RpcError> {
    if attachments.len() > limits::MAX_ATTACHMENTS {
        return Err(invalid(format!("{} attachments; a message may carry at most {}", attachments.len(), limits::MAX_ATTACHMENTS)));
    }
    let mut out = Vec::with_capacity(attachments.len());
    for mut attachment in attachments {
        attachment.listing = None;
        attachment.description = None;
        attachment.described_by = None;
        attachment.lines = None;
        attachment.preview = None;
        attachment.thumb = check_thumb(attachment.thumb.take())?;
        let path = PathBuf::from(&attachment.path);
        if !path.is_absolute() {
            return Err(invalid(format!("{}: {} is not an absolute path", attachment.name, attachment.path)));
        }
        let metadata = std::fs::metadata(&path).map_err(|error| invalid(format!("{} ({}) cannot be attached: {error}", attachment.name, attachment.path)))?;
        match attachment.kind.as_str() {
            attachment_kinds::FOLDER => {
                if !metadata.is_dir() {
                    return Err(invalid(format!("{} is not a folder", attachment.path)));
                }
                attachment.bytes = 0;
                attachment.listing = Some(listing(ds, &path));
            }
            attachment_kinds::IMAGE | attachment_kinds::FILE => {
                if !metadata.is_file() {
                    return Err(invalid(format!("{} is not a file", attachment.path)));
                }
                attachment.bytes = metadata.len();
                if attachment.mime.is_empty() {
                    attachment.mime = mime_of(&attachment.name).to_owned();
                }
                if attachment.pasted_text {
                    if metadata.len() > limits::MAX_ATTACHMENT_BYTES {
                        return Err(too_big(&attachment.name, metadata.len(), "pasted text"));
                    }
                    let bytes = std::fs::read(&path).map_err(|error| invalid(format!("{} cannot be read: {error}", attachment.path)))?;
                    let (lines, preview) = text_preview(ds, &String::from_utf8_lossy(&bytes));
                    attachment.lines = Some(lines);
                    attachment.preview = Some(preview);
                }
                if attachment.kind == attachment_kinds::IMAGE && attachment.sha256.is_none() && metadata.len() <= limits::MAX_ATTACHMENT_BYTES {
                    attachment.sha256 = std::fs::read(&path).ok().map(|bytes| format!("{:x}", Sha256::digest(&bytes)));
                }
                if attachment.kind == attachment_kinds::IMAGE && (attachment.width.is_none() || attachment.height.is_none()) {
                    let size = read_head(&path).and_then(|head| image_size(&head));
                    attachment.width = size.map(|(width, _)| width);
                    attachment.height = size.map(|(_, height)| height);
                }
            }
            other => return Err(invalid(format!("unknown attachment kind `{other}` (image, file or folder)"))),
        }
        out.push(attachment);
    }
    Ok(out)
}

/// [`check`], then the vision hook for each image.
pub async fn prepare(host: &Host, ds: &Dataset, attachments: Vec<Attachment>) -> Result<Vec<Attachment>, RpcError> {
    let mut checked = check(ds, attachments)?;
    for attachment in checked.iter_mut().filter(|attachment| attachment.kind == attachment_kinds::IMAGE) {
        if let Some((text, by)) = vision_hook::describe(host, attachment).await {
            attachment.description = Some(text);
            attachment.described_by = Some(by);
        }
    }
    Ok(checked)
}

/// The attachments of a journal entry (`data.attachments`), as journaled.
pub fn of_entry(data: &Value) -> Vec<Attachment> {
    data.get("attachments").and_then(|value| serde_json::from_value(value.clone()).ok()).unwrap_or_default()
}

/// Copy `attachments` from one entry's data onto another's (a steer becoming a user message).
pub fn carry(mut data: Value, from: &Value) -> Value {
    if let Some(attachments) = from.get("attachments").filter(|value| value.as_array().is_some_and(|items| !items.is_empty()))
        && let Some(object) = data.as_object_mut()
    {
        object.insert("attachments".into(), attachments.clone());
    }
    data
}

/// Entry data for a message with its attachments (left out when there are none).
pub fn with(mut data: Value, attachments: &[Attachment]) -> Value {
    if !attachments.is_empty()
        && let Some(object) = data.as_object_mut()
    {
        object.insert("attachments".into(), json!(attachments));
    }
    data
}

/// Whether an attachment goes to an image-input model as an image part (it needs its content hash).
fn as_image_part(attachment: &Attachment) -> bool {
    attachment.kind == attachment_kinds::IMAGE && attachment.sha256.is_some()
}

/// What the model reads for a message's attachments (empty when there are none). `image_input`:
/// the model takes image parts (`model.info`), so images are shown to it, not only named.
pub fn render(ds: &Dataset, attachments: &[Attachment], image_input: bool) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let items: Vec<String> = attachments
        .iter()
        .map(|attachment| {
            let size = human_size(attachment.bytes);
            match attachment.kind.as_str() {
                attachment_kinds::FOLDER => {
                    let listing = attachment.listing.as_deref().unwrap_or("").lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n");
                    ds.text("attachment_folder", &[("name", &attachment.name), ("path", &attachment.path), ("listing", &listing)])
                }
                attachment_kinds::IMAGE => {
                    let dims = match (attachment.width, attachment.height) {
                        (Some(width), Some(height)) => ds.text("attachment_image_dims", &[("width", &width.to_string()), ("height", &height.to_string())]),
                        _ => String::new(),
                    };
                    let seen = match (&attachment.description, &attachment.described_by) {
                        _ if image_input && as_image_part(attachment) => ds.text("attachment_shown", &[]),
                        (Some(description), by) => ds.text("attachment_seen", &[("by", by.as_deref().unwrap_or("vision")), ("description", description)]),
                        (None, _) => ds.text("attachment_unseen", &[]),
                    };
                    ds.text("attachment_image", &[("name", &attachment.name), ("path", &attachment.path), ("size", &size), ("dims", &dims), ("seen", &seen)])
                }
                _ if attachment.pasted_text => {
                    let lines = attachment.lines.unwrap_or(0).to_string();
                    let preview = attachment.preview.as_deref().unwrap_or("").lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n");
                    let key = if attachment.bytes as usize <= WHOLE_CHARS { "attachment_pasted_whole" } else { "attachment_pasted" };
                    ds.text(key, &[("name", &attachment.name), ("path", &attachment.path), ("size", &size), ("lines", &lines), ("preview", &preview)])
                }
                _ => ds.text("attachment_file", &[("name", &attachment.name), ("path", &attachment.path), ("size", &size)]),
            }
        })
        .collect();
    ds.text("attachments", &[("count", &attachments.len().to_string()), ("items", &items.join("\n"))])
}

/// A message as the model reads it: the text, then its attachments.
pub fn message_text(ds: &Dataset, text: &str, attachments: &[Attachment], image_input: bool) -> String {
    let block = render(ds, attachments, image_input);
    match (text.trim().is_empty(), block.is_empty()) {
        (_, true) => text.to_owned(),
        (true, false) => block,
        (false, false) => format!("{text}\n\n{block}"),
    }
}

/// A user-role message with its attachments: the text (with the attachment block) as `content` and,
/// for a model that takes images, the images as parts before the text (Qwen's own order).
pub fn user_message(content: String, attachments: &[Attachment], image_input: bool) -> ChatMessage {
    let mut message = ChatMessage::new("user", content);
    if image_input && attachments.iter().any(as_image_part) {
        message.parts = attachments
            .iter()
            .filter(|attachment| as_image_part(attachment))
            .map(|image| ContentPart::Image {
                sha256: image.sha256.clone().unwrap_or_default(),
                path: image.path.clone(),
                mime: image.mime.clone(),
                width: image.width,
                height: image.height,
            })
            .collect();
        message.parts.push(ContentPart::Text { text: message.content.clone() });
    }
    message
}
