//! The LSP base protocol: each message is a header part (`Content-Length: N`, optionally
//! `Content-Type`, each line ended by `\r\n`), an empty line, then N bytes of UTF-8 JSON.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// The largest message body accepted (a server answering with more is broken or hostile).
pub const MAX_MESSAGE_BYTES: usize = 256 << 20;
/// The longest header line accepted.
const MAX_HEADER_LINE: usize = 4096;

/// One message with its header.
pub fn encode(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    /// The header part is malformed: the stream cannot be resynchronised.
    Header(String),
    /// The body is not JSON; the stream is still in step (the next message can be read).
    Body(String),
}

impl ReadError {
    /// Whether reading cannot continue after this error.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, ReadError::Body(_))
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(error) => write!(f, "read failed: {error}"),
            ReadError::Header(message) => write!(f, "bad header: {message}"),
            ReadError::Body(message) => write!(f, "bad message body: {message}"),
        }
    }
}

/// Read one message; `Ok(None)` when the stream ends between messages.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>, ReadError> {
    let mut length: Option<usize> = None;
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        let n = (&mut *reader).take(MAX_HEADER_LINE as u64 + 1).read_until(b'\n', &mut line).await.map_err(ReadError::Io)?;
        if n == 0 {
            return if first { Ok(None) } else { Err(ReadError::Header("the stream ended inside a header".into())) };
        }
        if !line.ends_with(b"\n") {
            return Err(ReadError::Header(if n > MAX_HEADER_LINE { "header line too long".into() } else { "the stream ended inside a header".into() }));
        }
        first = false;
        let text = std::str::from_utf8(&line).map_err(|_| ReadError::Header("header is not UTF-8".into()))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break;
        }
        let Some((name, value)) = text.split_once(':') else {
            return Err(ReadError::Header(format!("malformed header line `{text}`")));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<usize>().map_err(|_| ReadError::Header(format!("bad Content-Length `{}`", value.trim())))?;
            length = Some(parsed);
        }
    }
    let length = length.ok_or_else(|| ReadError::Header("missing Content-Length".into()))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(ReadError::Header(format!("message of {length} bytes is too large")));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.map_err(ReadError::Io)?;
    serde_json::from_slice(&body).map(Some).map_err(|error| ReadError::Body(error.to_string()))
}
