//! CortexCode patch P7: image embeddings in a language model's input.
//!
//! An image occupies `rows × columns` consecutive input tokens (the model's image placeholder
//! token); their embeddings come from the vision encoder instead of the embedding table, and their
//! rotary positions follow Qwen's interleaved multi-axis RoPE: an image starting at text position
//! `s` has position `(s, s + row, s + column)` per token, and the text after it continues at
//! `s + max(rows, columns)`. The state keeps the resulting offset between position and context
//! index (`rope_delta`), so later input and decoding continue from the right position. An image may
//! span several inputs (prefill chunks): `start` is relative to the current input and is negative
//! when the image began in an earlier one.

use std::sync::Arc;

use crate::{
    config::rope::AnyRoPEConfig,
    encodable_block::mixer::attention::rope::{mrope_tables, rope_tables},
};

/// One image's embeddings, placed relative to a stream's input.
#[derive(Debug, Clone)]
pub struct ImageEmbedding {
    /// Index in the input of the image's first token; negative when it began in an earlier input.
    pub start: i64,
    /// Merged token grid (rows, columns); the image has `rows * columns` tokens.
    pub grid: (u32, u32),
    /// `rows * columns` rows of `model_dim` bf16 values (bits), row-major.
    pub rows: Arc<[u16]>,
}

impl ImageEmbedding {
    pub fn tokens(&self) -> u64 {
        u64::from(self.grid.0) * u64::from(self.grid.1)
    }
}

/// The images of one stream input and the decoder's MRoPE layout.
#[derive(Debug, Clone)]
pub struct ImageInput {
    pub images: Vec<ImageEmbedding>,
    /// Rotary pairs per axis, interleaved T, H, W (`mrope_section`).
    pub mrope_section: [u32; 3],
}

/// Rows `[input_row, input_row + count)` of the input take rows `[image_row, image_row + count)` of
/// image `image`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub input_row: usize,
    pub image: usize,
    pub image_row: usize,
    pub count: usize,
}

/// Rotary positions of every input row, the embedding segments, and the state's rope delta after
/// the input.
#[derive(Debug, Clone, PartialEq)]
pub struct InputPlan {
    pub positions: Vec<[u32; 3]>,
    pub segments: Vec<Segment>,
    pub rope_delta_after: i64,
}

/// How much an image shifts the text positions after it: it advances the context by
/// `rows * columns` tokens but the position by `max(rows, columns)`.
pub fn image_rope_delta(grid: (u32, u32)) -> i64 {
    i64::from(grid.0.max(grid.1)) - i64::from(grid.0) * i64::from(grid.1)
}

/// Plan an input of `input_len` tokens at `context_length` with the state's `rope_delta`.
/// `images` are `(start, (rows, columns))`; images entirely before or after the input are ignored.
pub fn plan_input(
    context_length: u32,
    rope_delta: i64,
    input_len: usize,
    images: &[(i64, (u32, u32))],
) -> Result<InputPlan, String> {
    let mut order: Vec<usize> = (0..images.len()).collect();
    order.sort_by_key(|&index| images[index].0);
    let mut previous_end = i64::MIN;
    for &index in &order {
        let (start, (rows, columns)) = images[index];
        if rows == 0 || columns == 0 {
            return Err(format!("image {index} has an empty grid"));
        }
        if start < previous_end {
            return Err(format!("image {index} overlaps another image"));
        }
        previous_end = start + i64::from(rows) * i64::from(columns);
    }
    let position = |index: i64, delta: i64| -> Result<u32, String> {
        u32::try_from(i64::from(context_length) + index + delta).map_err(|_| "rotary position out of range".to_owned())
    };
    let mut positions = Vec::with_capacity(input_len);
    let mut segments = Vec::new();
    let mut delta = rope_delta;
    let mut next = order.iter().copied().filter(|&index| images[index].0 + tokens(images[index].1) > 0).peekable();
    let mut row = 0usize;
    while row < input_len {
        let current = next.peek().copied().filter(|&index| images[index].0 <= row as i64);
        match current {
            Some(index) => {
                let (start, (rows, columns)) = images[index];
                let first = (row as i64 - start) as usize;
                let total = tokens((rows, columns)) as usize;
                let count = (total - first).min(input_len - row);
                // The image's own start position: the text position its first token would have.
                let origin = position(start, delta)?;
                for image_row in first..first + count {
                    let (r, c) = ((image_row / columns as usize) as u32, (image_row % columns as usize) as u32);
                    positions.push([origin, origin + r, origin + c]);
                }
                segments.push(Segment { input_row: row, image: index, image_row: first, count });
                row += count;
                if first + count == total {
                    delta += image_rope_delta((rows, columns));
                    next.next();
                }
            },
            None => {
                let text = position(row as i64, delta)?;
                positions.push([text; 3]);
                row += 1;
            },
        }
    }
    Ok(InputPlan {
        positions,
        segments,
        rope_delta_after: delta,
    })
}

fn tokens(grid: (u32, u32)) -> i64 {
    i64::from(grid.0) * i64::from(grid.1)
}

impl ImageInput {
    pub(crate) fn plan(
        &self,
        context_length: u32,
        rope_delta: i64,
        input_len: usize,
        model_dim: usize,
    ) -> Result<InputPlan, String> {
        for (index, image) in self.images.iter().enumerate() {
            if image.rows.len() as u64 != image.tokens() * model_dim as u64 {
                return Err(format!(
                    "image {index} has {} embedding values; a {}x{} grid needs {}",
                    image.rows.len(),
                    image.grid.0,
                    image.grid.1,
                    image.tokens() * model_dim as u64
                ));
            }
        }
        let spans: Vec<(i64, (u32, u32))> = self.images.iter().map(|image| (image.start, image.grid)).collect();
        plan_input(context_length, rope_delta, input_len, &spans)
    }
}

fn unscaled(
    base: f32,
    rotary_dim: u32,
) -> AnyRoPEConfig {
    serde_json::from_value(serde_json::json!({
        "type": "UnscaledRoPEConfig",
        "base": base,
        "max_sequence_length": u32::MAX,
        "head_dim": rotary_dim,
    }))
    .expect("an unscaled RoPE configuration")
}

/// The cosine and sine tables the decoder uses for 1-D positions of an unscaled rotary
/// embedding (`[token, rotary_dim]`, rotate-half layout), for checks.
pub fn unscaled_rope_tables(
    base: f32,
    rotary_dim: u32,
    positions: &[u32],
) -> (Vec<f32>, Vec<f32>) {
    rope_tables(&unscaled(base, rotary_dim), positions)
}

/// The interleaved multi-axis tables for (T, H, W) positions, for checks (see `rope::mrope_tables`).
pub fn unscaled_mrope_tables(
    base: f32,
    rotary_dim: u32,
    positions: &[[u32; 3]],
    section: [u32; 3],
) -> (Vec<f32>, Vec<f32>) {
    mrope_tables(&unscaled(base, rotary_dim), positions, section)
}
