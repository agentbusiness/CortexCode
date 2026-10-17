use std::io::{Read, Write};

use derive_more::Debug;
use thiserror::Error;

use crate::{
    backends::common::{
        Backend, Buffer, BufferMut, BufferRef, CommandBufferEncoding, CommandBufferExecutable,
        CommandBufferPending, Context,
    },
    encodable_block::{
        dflash::DFlashState,
        mixer::{MixerSnapshot, MixerSnapshotError},
        sampling::PRng,
        transformer::TransformerState,
    },
    engine::language_model::LanguageModel,
};

/// CortexCode patch P1: what a snapshot keeps (re-exported for callers).
pub use crate::encodable_block::mixer::SnapshotMode;

pub struct LanguageModelState<B: Backend> {
    pub(super) tokens: Vec<u64>,
    pub(super) last_output_token: Option<u64>, // TODO: this leaks previous LanguageModelStreamOptions
    pub(super) prng: PRng,
    pub(super) transformer_state: TransformerState<B>,
    pub(super) speculator_state: Option<DFlashState<B>>,
    pub(super) max_context_length: Option<u32>,
}

impl<B: Backend> LanguageModelState<B> {
    pub fn tokens(&self) -> &[u64] {
        &self.tokens
    }

    /// CortexCode patch P4: the context capacity this state was created with.
    pub fn max_context_length(&self) -> Option<u32> {
        self.max_context_length
    }

    /// CortexCode patch P4: re-seed the sampler (seeds derive from this and the token position).
    pub fn set_sampling_seed(
        &mut self,
        seed: u64,
    ) {
        self.prng = PRng::new(seed);
    }

    /// CortexCode patch P4: forget the token a dropped stream sampled but never fed back, so the
    /// next stream must start from new input (its sampling options would otherwise leak).
    pub fn clear_pending_output(&mut self) {
        self.last_output_token = None;
    }

    /// CortexCode patch P4: whether a dropped stream left a sampled, unfed token behind.
    pub fn has_pending_output(&self) -> bool {
        self.last_output_token.is_some()
    }

    /// CortexCode patch P7: rotary position minus context index of the next token (non-zero only
    /// after images; see `images`).
    pub fn rope_delta(&self) -> i64 {
        self.transformer_state.rope_delta()
    }
}

#[derive(Debug, Error)]
pub enum LanguageModelCreateEmptyStateError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
}

impl<B: Backend> LanguageModel<B> {
    pub fn create_empty_state(
        &self,
        max_context_length: Option<u32>,
        sampling_seed: u64,
    ) -> Result<LanguageModelState<B>, LanguageModelCreateEmptyStateError<B>> {
        let tokens = Vec::new();
        let last_output_token = None;

        let prng = PRng::new(sampling_seed);

        let transformer_state = self
            .decoder
            .create_empty_state(max_context_length, &self.engine.context)
            .map_err(LanguageModelCreateEmptyStateError::Backend)?;

        let speculator_state = self
            .speculator
            .as_ref()
            .map(|speculator| {
                speculator.empty_state(max_context_length.expect("speculator doesn't support unlimited state capacity"))
            })
            .transpose()
            .map_err(LanguageModelCreateEmptyStateError::Backend)?;

        Ok(LanguageModelState {
            tokens,
            last_output_token,
            prng,
            transformer_state,
            speculator_state,
            max_context_length,
        })
    }
}

// ---- CortexCode patch P1: state snapshots (context checkpoints and hibernation) -------------------

/// A copy of a language model state: the tokens it holds, every layer's state and the drafter's.
/// `Full` snapshots can be written to disk and restored into any fresh state of the same model;
/// `Checkpoint` snapshots can only roll the state they were taken from back to that point.
pub struct LanguageModelStateSnapshot<B: Backend> {
    mode: SnapshotMode,
    tokens: Vec<u64>,
    max_context_length: Option<u32>,
    transformer: Box<[Option<MixerSnapshot<B>>]>,
    context_length: u32,
    /// CortexCode patch P7: the state's rope delta.
    rope_delta: i64,
    speculator: Option<(Box<[MixerSnapshot<B>]>, u32)>,
}

#[derive(Debug, Error)]
pub enum LanguageModelSnapshotError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Snapshot error: {0}")]
    Snapshot(#[from] MixerSnapshotError<B>),
    #[error("The state has a pending output token; drop the stream with new input first")]
    PendingOutput,
    #[error("Snapshot does not fit this state: {0}")]
    Mismatch(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Malformed snapshot file: {0}")]
    Malformed(String),
}

const SNAPSHOT_MAGIC: &[u8; 8] = b"UZUSNAP3";
/// CortexCode patch P7: snapshots written before the rope delta existed (read as delta 0).
const SNAPSHOT_MAGIC_V2: &[u8; 8] = b"UZUSNAP2";

impl<B: Backend> LanguageModelStateSnapshot<B> {
    pub fn tokens(&self) -> &[u64] {
        &self.tokens
    }

    pub fn mode(&self) -> SnapshotMode {
        self.mode
    }

    /// Bytes held by the snapshot's buffer copies.
    pub fn buffer_bytes(&self) -> usize {
        let transformer: usize =
            self.transformer.iter().flatten().flat_map(|layer| layer.buffers.iter()).map(|buffer| buffer.size()).sum();
        let speculator: usize = self
            .speculator
            .iter()
            .flat_map(|(layers, _)| layers.iter())
            .flat_map(|layer| layer.buffers.iter())
            .map(|buffer| buffer.size())
            .sum();
        transformer + speculator
    }

    /// Serialize a snapshot (little-endian, self-describing; see `read_state_snapshot`).
    pub fn write_to(
        &self,
        out: &mut impl Write,
    ) -> Result<u64, LanguageModelSnapshotError<B>> {
        let mut written = 0u64;
        let mut put = |out: &mut dyn Write, bytes: &[u8]| -> std::io::Result<()> {
            written += bytes.len() as u64;
            out.write_all(bytes)
        };
        put(out, SNAPSHOT_MAGIC)?;
        // A checkpoint carries no attention rows; it restores only into a state holding them.
        put(out, &[u8::from(self.mode == SnapshotMode::Checkpoint)])?;
        put(out, &self.max_context_length.map_or(u64::MAX, u64::from).to_le_bytes())?;
        put(out, &(self.tokens.len() as u64).to_le_bytes())?;
        for token in &self.tokens {
            put(out, &token.to_le_bytes())?;
        }
        put(out, &u64::from(self.context_length).to_le_bytes())?;
        put(out, &self.rope_delta.to_le_bytes())?;
        put(out, &(self.transformer.len() as u64).to_le_bytes())?;
        for layer in self.transformer.iter() {
            match layer {
                None => put(out, &[0])?,
                Some(layer) => {
                    put(out, &[1])?;
                    write_layer(&mut put, out, layer)?;
                },
            }
        }
        match &self.speculator {
            None => put(out, &[0])?,
            Some((layers, context_length)) => {
                put(out, &[1])?;
                put(out, &u64::from(*context_length).to_le_bytes())?;
                put(out, &(layers.len() as u64).to_le_bytes())?;
                for layer in layers.iter() {
                    write_layer(&mut put, out, layer)?;
                }
            },
        }
        Ok(written)
    }
}

fn write_layer<B: Backend>(
    put: &mut impl FnMut(&mut dyn Write, &[u8]) -> std::io::Result<()>,
    out: &mut dyn Write,
    layer: &MixerSnapshot<B>,
) -> std::io::Result<()> {
    put(out, &(layer.scalars.len() as u64).to_le_bytes())?;
    for scalar in layer.scalars.iter() {
        put(out, &scalar.to_le_bytes())?;
    }
    put(out, &(layer.buffers.len() as u64).to_le_bytes())?;
    for buffer in layer.buffers.iter() {
        let bytes: &[u8] = buffer.as_slice();
        put(out, &(bytes.len() as u64).to_le_bytes())?;
        put(out, bytes)?;
    }
    Ok(())
}

struct SnapshotReader<'a, R: Read> {
    input: &'a mut R,
    read: u64,
}

impl<R: Read> SnapshotReader<'_, R> {
    fn bytes(
        &mut self,
        into: &mut [u8],
    ) -> std::io::Result<()> {
        self.read += into.len() as u64;
        self.input.read_exact(into)
    }

    fn u64(&mut self) -> std::io::Result<u64> {
        let mut bytes = [0u8; 8];
        self.bytes(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn u8(&mut self) -> std::io::Result<u8> {
        let mut byte = [0u8; 1];
        self.bytes(&mut byte)?;
        Ok(byte[0])
    }

    /// A count that must be plausible (structural bound against corrupt files).
    fn count(
        &mut self,
        limit: u64,
        what: &str,
    ) -> Result<usize, String> {
        let value = self.u64().map_err(|error| error.to_string())?;
        if value > limit {
            return Err(format!("{what} count {value} exceeds {limit}"));
        }
        Ok(value as usize)
    }
}

impl<B: Backend> LanguageModel<B> {
    /// CortexCode patch P1: copy an idle state (no stream alive on it). Blocks until the copies
    /// completed on the device.
    pub fn snapshot_state(
        &self,
        state: &LanguageModelState<B>,
        mode: SnapshotMode,
    ) -> Result<LanguageModelStateSnapshot<B>, LanguageModelSnapshotError<B>> {
        let mut command_buffer = self
            .engine
            .context
            .create_command_buffer(Some("state snapshot"), None)
            .map_err(LanguageModelSnapshotError::Backend)?;
        let (transformer, context_length) = state.transformer_state.encode_snapshot(mode, &mut command_buffer)?;
        let speculator = state
            .speculator_state
            .as_ref()
            .map(|speculator| speculator.encode_snapshot(mode, &mut command_buffer))
            .transpose()?;
        command_buffer.end_encoding().submit().wait_until_completed().map_err(LanguageModelSnapshotError::Backend)?;
        Ok(LanguageModelStateSnapshot {
            mode,
            tokens: state.tokens.clone(),
            max_context_length: state.max_context_length,
            transformer,
            context_length,
            rope_delta: state.transformer_state.rope_delta(),
            speculator,
        })
    }

    /// CortexCode patch P1: replace an idle state's contents with a snapshot. A `Checkpoint` must
    /// come from this same state and be no longer than it. Blocks until the copies completed.
    pub fn restore_state(
        &self,
        state: &mut LanguageModelState<B>,
        snapshot: &LanguageModelStateSnapshot<B>,
    ) -> Result<(), LanguageModelSnapshotError<B>> {
        if snapshot.mode == SnapshotMode::Checkpoint
            && (snapshot.tokens.len() > state.tokens.len() || state.tokens[..snapshot.tokens.len()] != snapshot.tokens[..])
        {
            return Err(LanguageModelSnapshotError::Mismatch("checkpoint is not a prefix of this state".into()));
        }
        if let Some(capacity) = state.max_context_length
            && snapshot.tokens.len() as u64 > u64::from(capacity)
        {
            return Err(LanguageModelSnapshotError::Mismatch("snapshot exceeds this state's capacity".into()));
        }
        if snapshot.speculator.is_some() != state.speculator_state.is_some() {
            return Err(LanguageModelSnapshotError::Mismatch("drafter presence differs".into()));
        }
        let mut command_buffer = self
            .engine
            .context
            .create_command_buffer(Some("state restore"), None)
            .map_err(LanguageModelSnapshotError::Backend)?;
        state.transformer_state.encode_restore(&snapshot.transformer, snapshot.context_length, &mut command_buffer)?;
        state.transformer_state.set_rope_delta(snapshot.rope_delta);
        if let (Some(speculator), Some((layers, context_length))) =
            (state.speculator_state.as_mut(), snapshot.speculator.as_ref())
        {
            speculator.encode_restore(layers, *context_length, &mut command_buffer)?;
        }
        command_buffer.end_encoding().submit().wait_until_completed().map_err(LanguageModelSnapshotError::Backend)?;
        state.tokens = snapshot.tokens.clone();
        state.last_output_token = None;
        Ok(())
    }

    /// CortexCode patch P1: read a snapshot written by `write_to` into device-visible buffers.
    /// Returns the snapshot and the number of bytes read.
    pub fn read_state_snapshot(
        &self,
        input: &mut impl Read,
    ) -> Result<(LanguageModelStateSnapshot<B>, u64), LanguageModelSnapshotError<B>> {
        let malformed = LanguageModelSnapshotError::Malformed;
        let mut reader = SnapshotReader {
            input,
            read: 0,
        };
        let mut magic = [0u8; 8];
        reader.bytes(&mut magic)?;
        let has_rope_delta = match &magic {
            SNAPSHOT_MAGIC => true,
            SNAPSHOT_MAGIC_V2 => false,
            _ => return Err(malformed("bad magic".into())),
        };
        let mode = match reader.u8()? {
            0 => SnapshotMode::Full,
            1 => SnapshotMode::Checkpoint,
            other => return Err(malformed(format!("bad mode {other}"))),
        };
        let max_context_length = match reader.u64()? {
            u64::MAX => None,
            value => Some(u32::try_from(value).map_err(|_| malformed("context length out of range".into()))?),
        };
        // Structural bounds only: a corrupt length must not allocate unbounded memory.
        let n_tokens = reader.count(1 << 32, "token").map_err(malformed)?;
        let mut tokens = Vec::with_capacity(n_tokens);
        for _ in 0..n_tokens {
            tokens.push(reader.u64()?);
        }
        let context_length =
            u32::try_from(reader.u64()?).map_err(|_| malformed("context length out of range".into()))?;
        let rope_delta = if has_rope_delta {
            reader.u64()? as i64
        } else {
            0
        };
        let n_layers = reader.count(1 << 16, "layer").map_err(malformed)?;
        let mut transformer = Vec::with_capacity(n_layers);
        for _ in 0..n_layers {
            transformer.push(match reader.u8()? {
                0 => None,
                1 => Some(self.read_layer(&mut reader)?),
                other => return Err(malformed(format!("bad layer tag {other}"))),
            });
        }
        let speculator = match reader.u8()? {
            0 => None,
            1 => {
                let context_length = u32::try_from(reader.u64()?)
                    .map_err(|_| malformed("drafter context length out of range".into()))?;
                let n = reader.count(1 << 16, "drafter layer").map_err(malformed)?;
                let mut layers = Vec::with_capacity(n);
                for _ in 0..n {
                    layers.push(self.read_layer(&mut reader)?);
                }
                Some((layers.into_boxed_slice(), context_length))
            },
            other => return Err(malformed(format!("bad drafter tag {other}"))),
        };
        Ok((
            LanguageModelStateSnapshot {
                mode,
                tokens,
                max_context_length,
                transformer: transformer.into_boxed_slice(),
                context_length,
                rope_delta,
                speculator,
            },
            reader.read,
        ))
    }

    fn read_layer<R: Read>(
        &self,
        reader: &mut SnapshotReader<'_, R>,
    ) -> Result<MixerSnapshot<B>, LanguageModelSnapshotError<B>> {
        let malformed = LanguageModelSnapshotError::Malformed;
        let n_scalars = reader.count(64, "scalar").map_err(malformed)?;
        let mut scalars = Vec::with_capacity(n_scalars);
        for _ in 0..n_scalars {
            scalars.push(reader.u64()?);
        }
        let n_buffers = reader.count(64, "buffer").map_err(malformed)?;
        let mut buffers = Vec::with_capacity(n_buffers);
        for _ in 0..n_buffers {
            let size = reader.count(1 << 40, "buffer byte").map_err(malformed)?;
            let mut buffer = self.engine.context.create_buffer(size.max(1)).map_err(LanguageModelSnapshotError::Backend)?;
            reader.bytes((&mut buffer).subrange_mut(..size).as_slice_mut::<u8>())?;
            buffers.push(buffer);
        }
        Ok(MixerSnapshot {
            scalars: scalars.into_boxed_slice(),
            buffers: buffers.into_boxed_slice(),
        })
    }
}
