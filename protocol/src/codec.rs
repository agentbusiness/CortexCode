//! Length-prefixed JSON framing: a 4-byte big-endian length followed by that many bytes of UTF-8 JSON.
//! Bulk data (model files, KV snapshots, large outputs) travels by file path, never in a frame.

use crate::envelope::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest accepted frame. A larger declared length is a protocol error, not an allocation.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Read one message. `Ok(None)` means the peer closed the stream cleanly between frames.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Message>, CodecError> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge(length));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// Encode one message into a complete frame.
pub fn encode_message(message: &Message) -> Result<Vec<u8>, CodecError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge(body.len()));
    }
    let length = u32::try_from(body.len()).map_err(|_| CodecError::TooLarge(body.len()))?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Write one message and flush.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<(), CodecError> {
    let frame = encode_message(message)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}
