use std::range::Range;

use derive_more::Debug;
use thiserror::Error;

use crate::{
    backends::common::{
        Backend, Buffer, BufferRef, CommandBuffer, CommandBufferEncoding, Kernels, kernel::TensorAddScaleKernel,
    },
    config::{rope::AnyRoPEConfig, transformer::TransformerConfig},
    data_type::DataType,
    encodable_block::{
        batch_topology::BatchTopology,
        mixer::{MixerSnapshot, MixerSnapshotError, MixerState, SnapshotMode, attention::rope::PrecalculatedRoPE},
        normalization::{Normalization, NormalizationNewError, PostLayerScalar, ShortcutMode},
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
}

#[derive(Debug, Error)]
pub enum TransformerEncodeError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Layer error: {0}")]
    Layer(#[from] TransformerLayerError<B>),
}

pub struct Transformer<B: Backend> {
    ropes: Box<[AnyRoPEConfig]>,
    layers: Box<[(TransformerLayer<B>, Option<usize>)]>,
    output_norm: Normalization<B>,
    model_dim: u32,
    residual_add: <B::Kernels as Kernels>::TensorAddScaleKernel,
}

impl<B: Backend> Transformer<B> {
    pub fn new(
        data_type: DataType,
        output_norm_hadamard_factors: Option<B::GlobalBuffer>,
        transformer_config: &TransformerConfig,
        parameter_tree: &ParameterTree<B>,
        context: &B::Context,
    ) -> Result<Self, TransformerNewError<B>> {
        let mut ropes: Vec<AnyRoPEConfig> = Vec::new();

        let num_layers = transformer_config.layer_configs.len() as u32;
        let layers = transformer_config
            .layer_configs
            .iter()
            .zip(0u32..)
            .map(|(layer_config, layer_index)| {
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
                    data_type,
                    layer_config,
                    &layer_loader,
                    context,
                )?;

                Ok((layer, rope))
            })
            .collect::<Result<Box<[_]>, TransformerNewError<B>>>()?;

        let output_norm = Normalization::new(
            transformer_config.model_dim,
            data_type,
            output_norm_hadamard_factors,
            ShortcutMode::Add,
            PostLayerScalar::None,
            &transformer_config.output_norm_config,
            &parameter_tree.subtree("output_norm"),
            context,
        )?;

        let residual_add = <B::Kernels as Kernels>::TensorAddScaleKernel::new(context, data_type, false)
            .map_err(TransformerNewError::Backend)?;

        Ok(Self {
            ropes: ropes.into_boxed_slice(),
            layers,
            output_norm,
            model_dim: transformer_config.model_dim,
            residual_add,
        })
    }

    fn capture_residual(
        &self,
        shortcut: impl BufferRef<Backend = B>,
        hidden: impl BufferRef<Backend = B>,
        batch_size: u32,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<B::ScratchBuffer, B::Error> {
        let mut output = command_buffer.allocate_scratch(hidden.size())?;
        let elements = batch_size * self.model_dim;
        self.residual_add.encode(Some(shortcut), hidden, &mut output, elements, elements, 1.0, command_buffer);
        Ok(output)
    }

    pub fn speculation_supported(&self) -> bool {
        self.layers.iter().all(|(layer, _rope)| layer.mixer.speculation_supported())
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
        let mut hidden = input;
        let layer_count = if output_range.is_none() && hidden_feature_layer_indices.is_none() {
            self.prefill_cache_layer_count()
        } else {
            self.layers.len()
        };

        let mut shortcut = command_buffer.allocate_scratch(hidden.size()).map_err(TransformerEncodeError::Backend)?;
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

            hidden = layer.encode(
                hidden,
                &mut shortcut,
                per_layer_inputs,
                precalculated_rope,
                batch_dim,
                layer_state,
                command_buffer,
            )?;

            if let (Some(hidden_features), Some(indices)) = (&mut hidden_features, hidden_feature_layer_indices) {
                for (feature_index, &layer_index) in indices.iter().enumerate() {
                    if layer_index == layer.layer_index {
                        let feature = self
                            .capture_residual(&shortcut, &hidden, batch_dim.size(), command_buffer)
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

        let output_normalized = self
            .output_norm
            .encode(
                &hidden,
                output_range.start,
                output_range.end - output_range.start,
                Some(&mut shortcut),
                command_buffer,
            )
            .map_err(TransformerEncodeError::Backend)?;

        Ok(TransformerEncodeOutput {
            output: Some(output_normalized),
            hidden_features,
        })
    }
}
