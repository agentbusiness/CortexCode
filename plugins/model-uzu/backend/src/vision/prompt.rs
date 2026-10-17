//! Images in a rendered prompt. The chat template renders each image part as
//! `<|vision_start|><|image_pad|><|vision_end|>`; after tokenization each `<|image_pad|>` becomes
//! the image's run of `rows × columns` pad tokens, whose embeddings the vision encoder supplies.
//! Every pad token has the same id, so the prompt's identity for prefix matching (the per-
//! conversation cache, hibernation, checkpoints) replaces each image's tokens with a value derived
//! from the image's SHA-256 and grid.

use std::borrow::Cow;
use std::path::PathBuf;

/// One image of a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptImage {
    /// Index of the image's first pad token in the prompt.
    pub start: usize,
    /// Merged token grid (rows, columns).
    pub grid: (u32, u32),
    pub sha256: String,
    pub path: PathBuf,
}

impl PromptImage {
    pub fn tokens(&self) -> usize {
        self.grid.0 as usize * self.grid.1 as usize
    }

    pub fn span(&self) -> ImageSpan {
        ImageSpan { start: self.start, len: self.tokens(), key: identity_key(&self.sha256, self.grid) }
    }
}

/// Where an image sits in a token sequence and what it is (for prefix matching).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageSpan {
    pub start: usize,
    pub len: usize,
    pub key: u64,
}

/// A value no real token id has (token ids are below 2^32), derived from the image and its grid
/// (the same picture at another resolution is different input). FNV-1a over the bytes.
pub fn identity_key(sha256: &str, grid: (u32, u32)) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sha256.bytes().chain(grid.0.to_le_bytes()).chain(grid.1.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash | (1 << 63)
}

/// The sequence compared for prefix reuse: `tokens` with each image's positions replaced by its
/// identity key (borrowed unchanged when there are no images).
pub fn identity<'a>(tokens: &'a [u64], spans: &[ImageSpan]) -> Cow<'a, [u64]> {
    if spans.is_empty() {
        return Cow::Borrowed(tokens);
    }
    let mut out = tokens.to_vec();
    for span in spans {
        let end = (span.start + span.len).min(out.len());
        if span.start < end {
            out[span.start..end].fill(span.key);
        }
    }
    Cow::Owned(out)
}

/// An image part before expansion: its file, digest and merged grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePart {
    pub sha256: String,
    pub path: PathBuf,
    pub grid: (u32, u32),
}

/// Replace the `n`-th `image_token` of `tokens` with image `n`'s run of pad tokens. The number of
/// placeholders must equal the number of images (text that spells the placeholder is refused).
pub fn expand(tokens: &[u64], image_token: u64, parts: &[ImagePart]) -> Result<(Vec<u64>, Vec<PromptImage>), String> {
    let placeholders = tokens.iter().filter(|token| **token == image_token).count();
    if placeholders != parts.len() {
        return Err(format!(
            "the prompt has {placeholders} image placeholder tokens for {} images (text may not contain the image placeholder)",
            parts.len()
        ));
    }
    let extra: usize = parts.iter().map(|part| (part.grid.0 * part.grid.1) as usize).sum();
    let mut out = Vec::with_capacity(tokens.len() + extra);
    let mut images = Vec::with_capacity(parts.len());
    let mut next = parts.iter();
    for token in tokens {
        if *token == image_token {
            let part = next.next().ok_or("more placeholders than images")?;
            images.push(PromptImage { start: out.len(), grid: part.grid, sha256: part.sha256.clone(), path: part.path.clone() });
            out.extend(std::iter::repeat_n(image_token, (part.grid.0 * part.grid.1) as usize));
        } else {
            out.push(*token);
        }
    }
    Ok((out, images))
}
