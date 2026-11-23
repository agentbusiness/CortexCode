use std::{range::Range, sync::Arc};

use derive_more::Debug;
use thiserror::Error;

use crate::{
    backends::common::{Backend, Buffer, BufferRef, CommandBuffer, CommandBufferEncoding},
    config::{rope::AnyRoPEConfig, transformer::TransformerConfig},
    data_type::DataType,
    encodable_block::{
        batch_topology::BatchTopology,
        linear::InputRhtParameters,
        mixer::{
            MixerBatchPart, MixerSnapshot, MixerSnapshotError, MixerState, SnapshotMode, attention::rope::PrecalculatedRoPE,
        },
        normalization::{Normalization, NormalizationNewError, ShortcutMode},
        residual::{ResidualEncodeError, ResidualHead, ResidualNewError},
        transformer_layer::{TransformerLayer, TransformerLayerError},
    },
    safetensors::ParameterTree,
    utils::maybe_mut::MaybeMut,
};

enum TransformerLayerStateType<B: Backend> {
    Owned(Box<dyn MixerState<B>>),
    Shared(u32),
}

pub struct TransformerState<B: Backend> {
    layer_states: Box<[TransformerLayerStateType<B>]>,
    context_length: u32,
    /// CortexCode patch P7: text position minus context index (`rope_deltas` in Qwen's MRoPE).
    /// Image tokens advance the context by their count but the position only by the larger grid
    /// side, so text after an image sits `rope_delta` (<= 0) away from its context index. Zero
    /// for text-only states, which keeps their positions exactly as before.
    rope_delta: i64,
}

/// CortexCode patch P7: explicit rotary positions of a flat batch (multi-axis RoPE). Each row has
/// a (T, H, W) position; text rows have three equal positions.
#[derive(Debug, Clone, Copy)]
pub struct RopeRows<'a> {
    pub positions: &'a [[u32; 3]],
    /// Rotary pairs per axis, interleaved T, H, W (Qwen's `mrope_section`).
    pub section: [u32; 3],
}

impl RopeRows<'_> {
    /// Every row is text (its three positions agree), so the 1-D table applies.
    pub fn is_text(&self) -> bool {
        self.positions.iter().all(|position| position[0] == position[1] && position[0] == position[2])
    }
}

pub struct TransformerEncodeOutput<B: Backend> {
    pub output: Option<B::ScratchBuffer>,
    pub hidden_features: Option<Box<[B::ScratchBuffer]>>,
}

/// CortexCode patch P10: one sequence of a batched pass: its rows' topology (heights relative to
/// its own context) and its state. Sequences take consecutive rows in the order given.
pub struct TransformerBatchSequence<'a, B: Backend> {
    pub topology: &'a BatchTopology,
    pub state: &'a mut TransformerState<B>,
}

/// CortexCode patch P10.
pub struct TransformerBatchOutput<B: Backend> {
    /// Every row, after the output norm.
    pub output: B::ScratchBuffer,
    /// Per sequence, the captured layers' rows of that sequence only.
    pub hidden_features: Option<Box<[Box<[B::ScratchBuffer]>]>>,
}

impl<B: Backend> TransformerState<B> {
    pub fn context_length(&self) -> u32 {
        self.context_length
    }

    /// CortexCode patch P7.
    pub fn rope_delta(&self) -> i64 {
        self.rope_delta
    }

    /// CortexCode patch P7.
    pub fn set_rope_delta(
        &mut self,
        rope_delta: i64,
    ) {
        self.rope_delta = rope_delta;
    }

    pub fn prepare(
        &mut self,
        context_length: u32,
        suffix_length: u32,
    ) -> Result<(), B::Error> {
        for layer_state in &mut self.layer_states {
            let TransformerLayerStateType::Owned(layer_state) = layer_state else {
                continue;
            };

            layer_state.prepare(context_length, suffix_length)?;
        }

        Ok(())
    }

    pub fn encode_accept(
        &mut self,
        accepted_indices: &[u32],
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<(), B::Error> {
        {
            let command_buffer = &mut *command_buffer.debug_group("transformer accept");

            for layer_state in &mut self.layer_states {
                let TransformerLayerStateType::Owned(layer_state) = layer_state else {
                    continue;
                };

                layer_state.encode_accept(accepted_indices, command_buffer)?;
            }

            self.context_length += accepted_indices.len() as u32;
        }

        command_buffer.debug_point(&format!("accepted {} tokens", accepted_indices.len()));

        Ok(())
    }

    /// CortexCode patch P1: snapshot every owned layer state (`None` for layers sharing another's KV).
    /// CortexCode patch P5: attention KV bytes per token across the owned layers.
    pub fn kv_bytes_per_token(&self) -> u64 {
        self.layer_states
            .iter()
            .map(|layer_state| match layer_state {
                TransformerLayerStateType::Owned(state) => state.kv_bytes_per_token(),
                TransformerLayerStateType::Shared(_) => 0,
            })
            .sum()
    }

    pub fn encode_snapshot(
        &self,
        mode: SnapshotMode,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<(Box<[Option<MixerSnapshot<B>>]>, u32), MixerSnapshotError<B>> {
        let layers = self
            .layer_states
            .iter()
            .map(|layer_state| match layer_state {
                TransformerLayerStateType::Owned(state) => state.encode_snapshot(mode, command_buffer).map(Some),
                TransformerLayerStateType::Shared(_) => Ok(None),
            })
            .collect::<Result<Box<[_]>, _>>()?;
        Ok((layers, self.context_length))
    }

    /// CortexCode patch P1: restore every owned layer state from a snapshot of the same model.
    pub fn encode_restore(
        &mut self,
        layers: &[Option<MixerSnapshot<B>>],
        context_length: u32,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<(), MixerSnapshotError<B>> {
        if layers.len() != self.layer_states.len() {
            return Err(MixerSnapshotError::Mismatch(format!(
                "{} layer snapshots for {} layers",
                layers.len(),
                self.layer_states.len()
            )));
        }
        for (layer_state, snapshot) in self.layer_states.iter_mut().zip(layers) {
            match (layer_state, snapshot) {
                (TransformerLayerStateType::Owned(state), Some(snapshot)) => {
                    state.encode_restore(snapshot, command_buffer)?
                },
                (TransformerLayerStateType::Shared(_), None) => {},
                _ => return Err(MixerSnapshotError::Mismatch("layer ownership differs".into())),
            }
        }
        self.context_length = context_length;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TransformerNewError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Layer error: {0}")]
    Layer(#[from] TransformerLayerError<B>),
    #[error("Normalization error: {0}")]
    Normalization(#[from] NormalizationNewError<B>),
    #[error("Residual error: {0}")]
    Residual(#[from] ResidualNewError<B>),
    #[error("Readout expects RHT-rotated input but there is no output norm to apply it")]
    UnappliedReadoutRht,
}

#[derive(Debug, Error)]
pub enum TransformerEncodeError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Layer error: {0}")]
    Layer(#[from] TransformerLayerError<B>),
    #[error("Residual encoding error: {0}")]
    Residual(#[from] ResidualEncodeError<B>),
    #[error("Hidden features require an additive residual")]
    HiddenFeaturesRequireAdditiveResidual,
}

pub struct Transformer<B: Backend> {
    ropes: Box<[AnyRoPEConfig]>,
    layers: Box<[(TransformerLayer<B>, Option<usize>)]>,
    output_norm: Option<Normalization<B>>,
    model_dim: u32,
    residual_head: ResidualHead<B>,
}

impl<B: Backend> Transformer<B> {
    pub async fn new(
        input_data_type: DataType,
        output_data_type: DataType,
        output_norm_hadamard_factors: Option<Arc<B::GlobalGpuBuffer>>,
        transformer_config: &TransformerConfig,
        parameter_tree: &ParameterTree<'_, B>,
        context: &B::Context,
    ) -> Result<Self, TransformerNewError<B>> {
        let mut ropes: Vec<AnyRoPEConfig> = Vec::new();

        let residual_config = transformer_config.residual_config.clone();
        let residual_head = ResidualHead::new(
            &residual_config,
            transformer_config.model_dim,
            input_data_type,
            &parameter_tree.subtree("residual_head"),
            context,
        )
        .await?;

        let num_layers = transformer_config.layer_configs.len() as u32;
        let mut layers = Vec::with_capacity(num_layers as usize);
        for (layer_config, layer_index) in transformer_config.layer_configs.iter().zip(0u32..) {
            let layer_loader = parameter_tree.subtree(&format!("layers.{}", layer_index));

            let rope = layer_config.rope_config.as_ref().map(|layer_rope_config| {
                ropes.iter().position(|rope_config| rope_config == layer_rope_config).unwrap_or_else(|| {
                    ropes.push(layer_rope_config.clone());
                    ropes.len() - 1
                })
            });

            let layer = TransformerLayer::new(
                transformer_config.model_dim,
                transformer_config.hidden_dim,
                num_layers,
                layer_index,
                input_data_type,
                layer_config,
                &residual_config,
                &layer_loader,
                context,
            )
            .await?;

            layers.push((layer, rope));
        }

        let output_norm = match (&transformer_config.output_norm_config, output_norm_hadamard_factors) {
            (Some(output_norm_config), hadamard_factors) => Some(
                Normalization::new(
                    transformer_config.model_dim,
                    input_data_type,
                    output_data_type,
                    hadamard_factors.map(InputRhtParameters::h32),
                    ShortcutMode::None,
                    output_norm_config,
                    &parameter_tree.subtree("output_norm"),
                    context,
                )
                .await?,
            ),
            (None, Some(_)) => return Err(TransformerNewError::UnappliedReadoutRht),
            (None, None) => None,
        };

        Ok(Self {
            ropes: ropes.into_boxed_slice(),
            layers: layers.into_boxed_slice(),
            output_norm,
            model_dim: transformer_config.model_dim,
            residual_head,
        })
    }

    fn capture_hidden_feature(
        &self,
        stream: impl BufferRef<Backend = B>,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<B::ScratchBuffer, B::Error> {
        let mut copy = command_buffer.allocate_scratch(stream.size())?;
        command_buffer.encode_copy(stream, &mut copy);
        Ok(copy)
    }

    pub fn model_dim(&self) -> u32 {
        self.model_dim
    }

    pub fn speculation_supported(&self) -> bool {
        matches!(self.residual_head, ResidualHead::Additive)
            && self.layers.iter().all(|(layer, _rope)| layer.mixer.speculation_supported())
    }

    pub fn max_context_length(&self) -> Option<u32> {
        self.layers.iter().map(|(layer, _rope_index)| layer.mixer.max_context_length()).fold(None, |acc, el| {
            match (acc, el) {
                (Some(a), Some(b)) => Some(u32::min(a, b)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        })
    }

    pub fn prefill_cache_layer_count(&self) -> usize {
        let num_layers = self.layers.len();
        let Some(last_owned_kv_layer_index) =
            self.layers.iter().rposition(|(layer, _rope_index)| layer.kv_source_layer_index.is_none())
        else {
            return num_layers;
        };

        last_owned_kv_layer_index + 1
    }

    pub fn prefill_cache_skips_trailing_layers(&self) -> bool {
        self.prefill_cache_layer_count() < self.layers.len()
    }

    pub fn create_empty_state(
        &self,
        max_context_length: Option<u32>,
        context: &B::Context,
    ) -> Result<TransformerState<B>, B::Error> {
        let layer_states = self
            .layers
            .iter()
            .map(|(layer, _rope)| match layer.kv_source_layer_index {
                None => {
                    layer.mixer.create_empty_state(max_context_length, context).map(TransformerLayerStateType::Owned)
                },
                Some(kv_source_layer_index) => Ok(TransformerLayerStateType::Shared(kv_source_layer_index)),
            })
            .collect::<Result<_, B::Error>>()?;

        let context_length = 0;

        Ok(TransformerState {
            layer_states,
            context_length,
            rope_delta: 0,
        })
    }

    pub fn encode(
        &self,
        input: B::ScratchBuffer,
        per_layer_inputs: Option<impl BufferRef<Backend = B>>,
        batch_dim: &BatchTopology,
        output_range: Option<Range<u32>>,
        hidden_feature_layer_indices: Option<&[u32]>,
        state: Option<&mut TransformerState<B>>,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<TransformerEncodeOutput<B>, TransformerEncodeError<B>> {
        self.encode_with_rope_rows(
            input,
            per_layer_inputs,
            batch_dim,
            output_range,
            hidden_feature_layer_indices,
            state,
            None,
            command_buffer,
        )
    }

    /// CortexCode patch P7: [`Transformer::encode`] with explicit rotary positions for the batch
    /// (multi-axis RoPE for image tokens). Without them, positions are the context index plus the
    /// state's `rope_delta`.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_with_rope_rows(
        &self,
        input: B::ScratchBuffer,
        per_layer_inputs: Option<impl BufferRef<Backend = B>>,
        batch_dim: &BatchTopology,
        output_range: Option<Range<u32>>,
        hidden_feature_layer_indices: Option<&[u32]>,
        mut state: Option<&mut TransformerState<B>>,
        rope_rows: Option<RopeRows<'_>>,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<TransformerEncodeOutput<B>, TransformerEncodeError<B>> {
        if hidden_feature_layer_indices.is_some() && !matches!(self.residual_head, ResidualHead::Additive) {
            return Err(TransformerEncodeError::HiddenFeaturesRequireAdditiveResidual);
        }

        let mut stream = self
            .residual_head
            .expand(input, batch_dim.size(), command_buffer)
            .map_err(TransformerEncodeError::Backend)?;
        let layer_count = if output_range.is_none() && hidden_feature_layer_indices.is_none() {
            self.prefill_cache_layer_count()
        } else {
            self.layers.len()
        };

        let mut hidden_features =
            hidden_feature_layer_indices.map(|indices| (0..indices.len()).map(|_| None).collect::<Vec<_>>());

        let context_length = state.as_ref().map(|state| state.context_length).unwrap_or(0);
        // CortexCode patch P7: positions shifted by the state's rope delta, or given per row.
        let rope_delta = state.as_ref().map(|state| state.rope_delta).unwrap_or(0);
        if let Some(rows) = &rope_rows {
            assert_eq!(rows.positions.len(), batch_dim.size() as usize, "one rotary position per batch row");
        }
        let precalculated_ropes = match rope_rows {
            Some(rows) if !rows.is_text() => self
                .ropes
                .iter()
                .map(|rope_config| {
                    PrecalculatedRoPE::precalculate_mrope(rope_config, rows.positions, rows.section, command_buffer)
                })
                .collect::<Result<Box<[_]>, B::Error>>(),
            _ => {
                let token_positions = match rope_rows {
                    Some(rows) => rows.positions.iter().map(|position| position[0]).collect::<Box<[u32]>>(),
                    None if rope_delta == 0 => {
                        batch_dim.heights().map(|rel_pos| context_length + rel_pos).collect::<Box<[u32]>>()
                    },
                    None => batch_dim
                        .heights()
                        .map(|rel_pos| {
                            u32::try_from(i64::from(context_length + rel_pos) + rope_delta)
                                .expect("rotary position out of range")
                        })
                        .collect::<Box<[u32]>>(),
                };
                self.ropes
                    .iter()
                    .map(|rope_config| PrecalculatedRoPE::precalculate(rope_config, &token_positions, command_buffer))
                    .collect::<Result<Box<[_]>, B::Error>>()
            },
        }
        .map_err(TransformerEncodeError::Backend)?;

        for (layer, layer_rope_index) in self.layers.iter().take(layer_count) {
            let precalculated_rope = layer_rope_index.map(|i| &precalculated_ropes[i]);

            let layer_state = if let Some(state) = &mut state {
                Some(match &mut state.layer_states[layer.layer_index as usize] {
                    TransformerLayerStateType::Owned(layer_state) => MaybeMut::Mut(layer_state.as_mut()),
                    TransformerLayerStateType::Shared(owned_layer_index) => {
                        let TransformerLayerStateType::Owned(owned_layer) =
                            &state.layer_states[*owned_layer_index as usize]
                        else {
                            panic!("shared layer doesn't point to an owned layer");
                        };
                        MaybeMut::Const(owned_layer.as_ref())
                    },
                })
            } else {
                None
            };

            stream =
                layer.encode(stream, per_layer_inputs, precalculated_rope, batch_dim, layer_state, command_buffer)?;

            if let (Some(hidden_features), Some(indices)) = (&mut hidden_features, hidden_feature_layer_indices) {
                for (feature_index, &layer_index) in indices.iter().enumerate() {
                    if layer_index == layer.layer_index {
                        let feature = self
                            .capture_hidden_feature(&stream, command_buffer)
                            .map_err(TransformerEncodeError::Backend)?;
                        hidden_features[feature_index] = Some(feature);
                    }
                }
            }
        }

        let hidden_features = hidden_features.map(|hidden_features| {
            hidden_features
                .into_iter()
                .enumerate()
                .map(|(feature_index, feature)| {
                    feature.unwrap_or_else(|| {
                        panic!(
                            "requested hidden feature for missing layer {}",
                            hidden_feature_layer_indices.unwrap()[feature_index]
                        )
                    })
                })
                .collect::<Box<[_]>>()
        });

        let Some(output_range) = output_range else {
            return Ok(TransformerEncodeOutput {
                output: None,
                hidden_features,
            });
        };

        let output_row_count = output_range.end - output_range.start;
        let (hidden, row_offset) =
            match self.residual_head.readout(&stream, output_range.start, output_row_count, command_buffer)? {
                Some(hidden) => (hidden, 0),
                None => (stream, output_range.start),
            };
        let output = match &self.output_norm {
            Some(output_norm) => output_norm
                .encode(&hidden, row_offset, output_row_count, None::<&mut B::ScratchBuffer>, command_buffer)
                .map_err(TransformerEncodeError::Backend)?,
            None if row_offset == 0 => hidden,
            None => {
                let row_size = hidden.size() / batch_dim.size() as usize;
                let rows = row_offset as usize * row_size..(row_offset + output_row_count) as usize * row_size;
                let mut output =
                    command_buffer.allocate_scratch(rows.end - rows.start).map_err(TransformerEncodeError::Backend)?;
                command_buffer.encode_copy((&hidden).subrange(rows), &mut output);
                output
            },
        };

        Ok(TransformerEncodeOutput {
            output: Some(output),
            hidden_features,
        })
    }

    /// CortexCode patch P10: whether several sequences can share one pass: an additive residual
    /// stream and every layer row-wise apart from mixers that run each sequence on its own state.
    pub fn batch_supported(&self) -> bool {
        matches!(self.residual_head, ResidualHead::Additive) && self.layers.iter().all(|(layer, _)| layer.batch_supported())
    }

    /// CortexCode patch P10: one forward pass over several sequences' rows (`input` holds them in
    /// the order of `sequences`). The weights are read once for all rows; attention and the
    /// recurrent mixers run per sequence on its state. Text positions only (no image rows):
    /// context index plus the state's rope delta, as [`Self::encode_with_rope_rows`] without rows.
    pub fn encode_batch(
        &self,
        input: B::ScratchBuffer,
        per_layer_inputs: Option<impl BufferRef<Backend = B>>,
        sequences: &mut [TransformerBatchSequence<'_, B>],
        hidden_feature_layer_indices: Option<&[u32]>,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<TransformerBatchOutput<B>, TransformerEncodeError<B>> {
        assert!(self.batch_supported(), "this model cannot run several sequences in one pass");
        let starts: Box<[u32]> = sequences
            .iter()
            .scan(0u32, |next, sequence| {
                let start = *next;
                *next += sequence.topology.size();
                Some(start)
            })
            .collect();
        let total: u32 = sequences.iter().map(|sequence| sequence.topology.size()).sum();
        let all_rows = BatchTopology::flat(total, true);

        let mut stream = self.residual_head.expand(input, total, command_buffer).map_err(TransformerEncodeError::Backend)?;

        // Each sequence's rotary tables, from its own context and rope delta.
        let ropes = sequences
            .iter()
            .map(|sequence| {
                let context_length = sequence.state.context_length;
                let rope_delta = sequence.state.rope_delta;
                let positions = sequence
                    .topology
                    .heights()
                    .map(|rel_pos| {
                        u32::try_from(i64::from(context_length + rel_pos) + rope_delta).expect("rotary position out of range")
                    })
                    .collect::<Box<[u32]>>();
                self.ropes
                    .iter()
                    .map(|rope_config| PrecalculatedRoPE::precalculate(rope_config, &positions, command_buffer))
                    .collect::<Result<Box<[_]>, B::Error>>()
            })
            .collect::<Result<Box<[_]>, B::Error>>()
            .map_err(TransformerEncodeError::Backend)?;

        let mut hidden_features = hidden_feature_layer_indices.map(|indices| {
            sequences.iter().map(|_| (0..indices.len()).map(|_| None).collect::<Vec<Option<B::ScratchBuffer>>>()).collect::<Vec<_>>()
        });

        for (layer, layer_rope_index) in self.layers.iter() {
            let layer_index = layer.layer_index as usize;
            let mut parts = Vec::with_capacity(sequences.len());
            for ((sequence, &row), sequence_ropes) in sequences.iter_mut().zip(starts.iter()).zip(ropes.iter()) {
                let state = &mut *sequence.state;
                // Which layer holds the KV this layer reads, looked up before any borrow is taken.
                let source = match &state.layer_states[layer_index] {
                    TransformerLayerStateType::Owned(_) => None,
                    TransformerLayerStateType::Shared(owned_layer_index) => Some(*owned_layer_index as usize),
                };
                let layer_state = match source {
                    None => {
                        let TransformerLayerStateType::Owned(layer_state) = &mut state.layer_states[layer_index] else {
                            unreachable!()
                        };
                        MaybeMut::Mut(layer_state.as_mut())
                    },
                    Some(owned_layer_index) => {
                        let TransformerLayerStateType::Owned(owned_layer) = &state.layer_states[owned_layer_index] else {
                            panic!("shared layer doesn't point to an owned layer");
                        };
                        MaybeMut::Const(owned_layer.as_ref())
                    },
                };
                parts.push(MixerBatchPart {
                    row,
                    topology: sequence.topology,
                    rope: layer_rope_index.map(|index| &sequence_ropes[index]),
                    state: layer_state,
                });
            }

            stream = layer.encode_batch(stream, per_layer_inputs, &all_rows, &mut parts, command_buffer)?;
            drop(parts);

            if let (Some(hidden_features), Some(indices)) = (&mut hidden_features, hidden_feature_layer_indices) {
                let row_bytes = stream.size() / total as usize;
                for (feature_index, &feature_layer_index) in indices.iter().enumerate() {
                    if feature_layer_index != layer.layer_index {
                        continue;
                    }
                    for (sequence_index, sequence) in sequences.iter().enumerate() {
                        let start = starts[sequence_index] as usize * row_bytes;
                        let size = sequence.topology.size() as usize * row_bytes;
                        let mut copy = command_buffer.allocate_scratch(size).map_err(TransformerEncodeError::Backend)?;
                        command_buffer.encode_copy((&stream).subrange(start..start + size), &mut copy);
                        hidden_features[sequence_index][feature_index] = Some(copy);
                    }
                }
            }
        }

        let hidden_features = hidden_features.map(|per_sequence| {
            per_sequence
                .into_iter()
                .map(|features| {
                    features
                        .into_iter()
                        .enumerate()
                        .map(|(feature_index, feature)| {
                            feature.unwrap_or_else(|| {
                                panic!(
                                    "requested hidden feature for missing layer {}",
                                    hidden_feature_layer_indices.unwrap()[feature_index]
                                )
                            })
                        })
                        .collect::<Box<[_]>>()
                })
                .collect::<Box<[_]>>()
        });

        let (hidden, row_offset) = match self.residual_head.readout(&stream, 0, total, command_buffer)? {
            Some(hidden) => (hidden, 0),
            None => (stream, 0),
        };
        let output = match &self.output_norm {
            Some(output_norm) => output_norm
                .encode(&hidden, row_offset, total, None::<&mut B::ScratchBuffer>, command_buffer)
                .map_err(TransformerEncodeError::Backend)?,
            None => hidden,
        };

        Ok(TransformerBatchOutput {
            output,
            hidden_features,
        })
    }
}
