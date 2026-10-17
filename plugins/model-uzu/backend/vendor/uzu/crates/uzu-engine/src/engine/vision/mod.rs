//! CortexCode patch P7: the vision encoder of Qwen3.5/3.8-style vision-language models (the
//! Qwen3-VL tower without DeepStack), on uzu's own kernels.
//!
//! Patch embedding (a linear over the flattened `channel × temporal × 16 × 16` patch), plus the
//! learned position table bilinearly interpolated to the patch grid (on the CPU); then `depth`
//! pre-norm blocks of LayerNorm (with bias) → QKV → 2-D rotary embedding → full non-causal
//! attention over the image → projection → residual, LayerNorm → GELU (tanh) MLP → residual; then
//! the patch merger: LayerNorm, each 2×2 window of patches concatenated (patches arrive in window
//! order), linear → GELU (exact) → linear to the language model's width.
//!
//! uzu's attention kernels support head dimensions 64, 128 and 256; the tower's heads are 72 wide,
//! so the QKV projection's rows and the output projection's columns are zero-padded per head to
//! the next supported size at load. Zero query/key channels add nothing to the scores, zero value
//! channels produce zero outputs that the zero columns of the output projection ignore, and the
//! softmax scale stays `1/sqrt(72)`: the result equals the unpadded attention.

use std::{fs::File, path::Path, sync::Arc};

use half::bf16;
use parking_lot::Mutex;
use thiserror::Error;

use crate::{
    backends::common::{
        Backend, BufferMut, BufferRef, CommandBufferEncoding, CommandBufferExecutable, CommandBufferPending,
        Context, Kernels,
        gpu_types::activation_type::ActivationType,
        kernel::{
            ActivationKernel, AttentionArguments, AttentionKernel, AttentionKernelConfig, AttentionPrepareKernel,
            NormalizationKernel, TensorAddScaleKernel,
            matmul::{MatmulA, MatmulArguments, MatmulB, MatmulDOps, MatmulKernel, MatmulOutput},
        },
    },
    data_type::DataType,
    encodable_block::mixer::attention::KVCacheView,
    engine::Engine,
    safetensors::{ParameterLoader, ParameterLoaderError, ParameterTree},
};

/// Architecture of a Qwen3-VL-style vision tower (no DeepStack).
#[derive(Debug, Clone, PartialEq)]
pub struct VisionModelConfig {
    pub depth: u32,
    pub hidden_size: u32,
    pub intermediate_size: u32,
    pub num_heads: u32,
    pub patch_size: u32,
    pub temporal_patch_size: u32,
    pub spatial_merge_size: u32,
    pub num_position_embeddings: u32,
    pub out_hidden_size: u32,
    pub in_channels: u32,
    pub layer_norm_epsilon: f32,
    pub rope_theta: f32,
}

impl VisionModelConfig {
    pub fn head_dim(&self) -> u32 {
        self.hidden_size / self.num_heads
    }

    /// The head width the attention kernels run at (the next one they support).
    pub fn padded_head_dim(&self) -> Option<u32> {
        [64, 128, 256].into_iter().find(|dim| *dim >= self.head_dim())
    }

    /// Values per flattened patch.
    pub fn patch_dim(&self) -> u32 {
        self.in_channels * self.temporal_patch_size * self.patch_size * self.patch_size
    }

    pub fn merge_unit(&self) -> u32 {
        self.spatial_merge_size * self.spatial_merge_size
    }

    /// Device bytes the loaded tower holds (weights as loaded, including the head padding), an
    /// upper bound.
    pub fn resident_bytes(&self) -> u64 {
        let (hidden, inter, heads) =
            (u64::from(self.hidden_size), u64::from(self.intermediate_size), u64::from(self.num_heads));
        let padded = u64::from(self.padded_head_dim().unwrap_or(self.head_dim()));
        let bf16 = 2;
        let block = (3 * heads * padded) * (hidden + 1) * bf16 // QKV, padded
            + hidden * (heads * padded) * bf16 + hidden * bf16 // output projection, padded
            + (inter * hidden + inter + hidden * inter + hidden) * bf16 // MLP
            + 4 * hidden * 4; // two LayerNorms, f32
        let merged = hidden * u64::from(self.merge_unit());
        let merger = (merged * merged + merged + u64::from(self.out_hidden_size) * merged + u64::from(self.out_hidden_size)) * bf16
            + 2 * hidden * 4;
        let patch = (hidden * u64::from(self.patch_dim()) + hidden) * bf16;
        // Plus allocator rounding and the kernels' own buffers (measured: 1.143 GB for 1.140 GB of
        // weights on Qwen3.8-27B's tower).
        u64::from(self.depth) * block + merger + patch + (16 << 20)
    }

    /// Device bytes an encode of `patches` patches may hold at once beyond the weights (an upper
    /// bound: every buffer of one block counted as live, plus the inputs and the output).
    pub fn scratch_bytes(
        &self,
        patches: u64,
    ) -> u64 {
        let (hidden, inter, heads) =
            (u64::from(self.hidden_size), u64::from(self.intermediate_size), u64::from(self.num_heads));
        let padded = u64::from(self.padded_head_dim().unwrap_or(self.head_dim()));
        let bf16 = 2;
        let per_patch = bf16
            * (hidden * 3 // residual, normed, projected
                + 3 * heads * padded * 2 // QKV and the prepared Q, K, V
                + heads * padded // attention output
                + inter // MLP inner
                + u64::from(self.patch_dim()) // patches
                + hidden) // position rows
            + 2 * u64::from(self.head_dim()) * 4; // rotary tables
        let merged = patches / u64::from(self.merge_unit()).max(1);
        patches * per_patch + merged * (hidden * u64::from(self.merge_unit()) + u64::from(self.out_hidden_size)) * bf16
    }
}

#[derive(Debug, Error)]
pub enum VisionError<B: Backend> {
    #[error("Backend error: {0}")]
    Backend(#[source] B::Error),
    #[error("Matmul error: {0}")]
    Matmul(#[source] <<B::Kernels as Kernels>::MatmulKernel as MatmulKernel>::Error),
    #[error("Attention error: {0}")]
    Attention(#[source] <<B::Kernels as Kernels>::AttentionKernel as AttentionKernel>::Error),
    #[error("Parameter error: {0}")]
    Parameter(#[from] ParameterLoaderError<B>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid input: {0}")]
    Invalid(String),
}

struct Linear<B: Backend> {
    weights: B::GlobalBuffer,
    biases: B::GlobalBuffer,
    input_dim: u32,
    output_dim: u32,
}

struct LayerNorm<B: Backend> {
    scales: B::GlobalBuffer,
    biases: B::GlobalBuffer,
}

struct Block<B: Backend> {
    norm1: LayerNorm<B>,
    qkv: Linear<B>,
    proj: Linear<B>,
    norm2: LayerNorm<B>,
    fc1: Linear<B>,
    fc2: Linear<B>,
}

struct VisionKernels<B: Backend> {
    matmul: Mutex<<B::Kernels as Kernels>::MatmulKernel>,
    norm: <B::Kernels as Kernels>::NormalizationKernel,
    activation: <B::Kernels as Kernels>::ActivationKernel,
    add: <B::Kernels as Kernels>::TensorAddScaleKernel,
    prepare: <B::Kernels as Kernels>::AttentionPrepareKernel,
    attention: <B::Kernels as Kernels>::AttentionKernel,
}

/// A loaded vision tower.
pub struct VisionModel<B: Backend> {
    context: Arc<B::Context>,
    config: VisionModelConfig,
    padded_head_dim: u32,
    patch: Linear<B>,
    /// The learned position table, kept on the CPU for interpolation (`[positions, hidden]`).
    position_table: Box<[bf16]>,
    blocks: Box<[Block<B>]>,
    merger_norm: LayerNorm<B>,
    merger_fc1: Linear<B>,
    merger_fc2: Linear<B>,
    kernels: VisionKernels<B>,
}

fn buffer_from<B: Backend, T: bytemuck::NoUninit + bytemuck::AnyBitPattern>(
    context: &B::Context,
    data: &[T],
) -> Result<B::GlobalBuffer, B::Error> {
    let mut buffer = context.create_buffer(std::mem::size_of_val(data).max(1))?;
    (&mut buffer).subrange_mut(..std::mem::size_of_val(data)).copyin(data);
    Ok(buffer)
}

impl<B: Backend> Engine<B> {
    /// CortexCode patch P7: load a vision tower from a safetensors file with upstream names
    /// (`model.visual.*`).
    pub fn load_vision_model(
        &self,
        weights: &Path,
        config: VisionModelConfig,
    ) -> Result<VisionModel<B>, VisionError<B>> {
        let file = File::open(weights)?;
        let loader = ParameterLoader::new(&file, &*self.context)?;
        let model = VisionModel::load(self.context.clone(), config, &loader.tree().subtree("model").subtree("visual"))?;
        loader.assert_all_tensors_read()?;
        Ok(model)
    }
}

impl<B: Backend> VisionModel<B> {
    fn load(
        context: Arc<B::Context>,
        config: VisionModelConfig,
        tree: &ParameterTree<B>,
    ) -> Result<Self, VisionError<B>> {
        let hidden = config.hidden_size;
        let head_dim = config.head_dim();
        if head_dim * config.num_heads != hidden || !head_dim.is_multiple_of(2) {
            return Err(VisionError::Invalid(format!("hidden size {hidden} does not split into {} heads", config.num_heads)));
        }
        let padded = config
            .padded_head_dim()
            .ok_or_else(|| VisionError::Invalid(format!("head dimension {head_dim} exceeds the attention kernels")))?;
        let heads = config.num_heads as usize;
        let backend = VisionError::Backend;
        let linear = |tree: &ParameterTree<B>, input_dim: u32, output_dim: u32| -> Result<Linear<B>, VisionError<B>> {
            Ok(Linear {
                weights: tree.read_buffer("weight", &[output_dim, input_dim], DataType::BF16)?,
                biases: tree.read_buffer("bias", &[output_dim], DataType::BF16)?,
                input_dim,
                output_dim,
            })
        };
        let layer_norm = |tree: &ParameterTree<B>, dim: u32| -> Result<LayerNorm<B>, VisionError<B>> {
            let to_f32 = |values: Box<[bf16]>| values.iter().map(|value| value.to_f32()).collect::<Vec<f32>>();
            let scales = to_f32(tree.read_slice::<bf16>("weight", &[dim])?);
            let biases = to_f32(tree.read_slice::<bf16>("bias", &[dim])?);
            Ok(LayerNorm {
                scales: buffer_from::<B, f32>(&context, &scales).map_err(backend)?,
                biases: buffer_from::<B, f32>(&context, &biases).map_err(backend)?,
            })
        };

        let patch_tree = tree.subtree("patch_embed").subtree("proj");
        let patch_weights = patch_tree.read_slice::<bf16>(
            "weight",
            &[hidden, config.in_channels, config.temporal_patch_size, config.patch_size, config.patch_size],
        )?;
        let patch = Linear {
            weights: buffer_from::<B, bf16>(&context, &patch_weights).map_err(backend)?,
            biases: patch_tree.read_buffer("bias", &[hidden], DataType::BF16)?,
            input_dim: config.patch_dim(),
            output_dim: hidden,
        };
        let position_table =
            tree.subtree("pos_embed").read_slice::<bf16>("weight", &[config.num_position_embeddings, hidden])?;

        let mut blocks = Vec::with_capacity(config.depth as usize);
        for index in 0..config.depth {
            let block = tree.subtree("blocks").subtree(&index.to_string());
            let attention = block.subtree("attn");
            // QKV: rows [q heads, k heads, v heads] × 72, each head padded to `padded` rows.
            let qkv_tree = attention.subtree("qkv");
            let qkv_weights = qkv_tree.read_slice::<bf16>("weight", &[3 * hidden, hidden])?;
            let qkv_biases = qkv_tree.read_slice::<bf16>("bias", &[3 * hidden])?;
            let (hd, pd, width) = (head_dim as usize, padded as usize, hidden as usize);
            let mut weights = vec![bf16::ZERO; 3 * heads * pd * width];
            let mut biases = vec![bf16::ZERO; 3 * heads * pd];
            for group in 0..3 * heads {
                for row in 0..hd {
                    let source = (group * hd + row) * width;
                    let target = (group * pd + row) * width;
                    weights[target..target + width].copy_from_slice(&qkv_weights[source..source + width]);
                    biases[group * pd + row] = qkv_biases[group * hd + row];
                }
            }
            let qkv = Linear {
                weights: buffer_from::<B, bf16>(&context, &weights).map_err(backend)?,
                biases: buffer_from::<B, bf16>(&context, &biases).map_err(backend)?,
                input_dim: hidden,
                output_dim: 3 * config.num_heads * padded,
            };
            // Output projection: columns per head padded the same way.
            let proj_tree = attention.subtree("proj");
            let proj_weights = proj_tree.read_slice::<bf16>("weight", &[hidden, hidden])?;
            let mut padded_proj = vec![bf16::ZERO; width * heads * pd];
            for row in 0..width {
                for head in 0..heads {
                    let source = row * width + head * hd;
                    let target = row * heads * pd + head * pd;
                    padded_proj[target..target + hd].copy_from_slice(&proj_weights[source..source + hd]);
                }
            }
            let proj = Linear {
                weights: buffer_from::<B, bf16>(&context, &padded_proj).map_err(backend)?,
                biases: proj_tree.read_buffer("bias", &[hidden], DataType::BF16)?,
                input_dim: config.num_heads * padded,
                output_dim: hidden,
            };
            let mlp = block.subtree("mlp");
            blocks.push(Block {
                norm1: layer_norm(&block.subtree("norm1"), hidden)?,
                qkv,
                proj,
                norm2: layer_norm(&block.subtree("norm2"), hidden)?,
                fc1: linear(&mlp.subtree("linear_fc1"), hidden, config.intermediate_size)?,
                fc2: linear(&mlp.subtree("linear_fc2"), config.intermediate_size, hidden)?,
            });
        }
        let merger = tree.subtree("merger");
        let merged = hidden * config.merge_unit();
        let merger_norm = layer_norm(&merger.subtree("norm"), hidden)?;
        let merger_fc1 = linear(&merger.subtree("linear_fc1"), merged, merged)?;
        let merger_fc2 = linear(&merger.subtree("linear_fc2"), merged, config.out_hidden_size)?;

        let context_ref = context.as_ref();
        let kernels = VisionKernels {
            matmul: Mutex::new(
                <B::Kernels as Kernels>::MatmulKernel::new(context_ref, DataType::BF16, DataType::BF16, DataType::BF16)
                    .map_err(VisionError::Matmul)?,
            ),
            norm: <B::Kernels as Kernels>::NormalizationKernel::new(
                context_ref,
                DataType::BF16,
                DataType::F32,
                DataType::BF16,
                DataType::F32,
                false,
                true,
                true,
                false,
                false,
                false,
                false,
                false,
                true,
                true,
            )
            .map_err(backend)?,
            activation: <B::Kernels as Kernels>::ActivationKernel::new(context_ref, DataType::BF16, true).map_err(backend)?,
            add: <B::Kernels as Kernels>::TensorAddScaleKernel::new(context_ref, DataType::BF16, true).map_err(backend)?,
            prepare: <B::Kernels as Kernels>::AttentionPrepareKernel::new(
                context_ref,
                DataType::BF16,
                DataType::F32,
                true,
                true,
            )
            .map_err(backend)?,
            attention: <B::Kernels as Kernels>::AttentionKernel::new(
                context_ref,
                AttentionKernelConfig {
                    head_dim: padded,
                    num_groups: config.num_heads,
                    num_q_heads: config.num_heads,
                    has_sinks: false,
                    gate_row_stride: None,
                    is_kv_cache_ring: false,
                    is_causal: false,
                    sliding_window_size: None,
                    scale: Some(1.0 / (head_dim as f32).sqrt()),
                    data_type: DataType::BF16,
                },
            )
            .map_err(VisionError::Attention)?,
        };
        Ok(Self {
            context,
            config,
            padded_head_dim: padded,
            patch,
            position_table,
            blocks: blocks.into_boxed_slice(),
            merger_norm,
            merger_fc1,
            merger_fc2,
            kernels,
        })
    }

    pub fn config(&self) -> &VisionModelConfig {
        &self.config
    }

    fn matmul(
        &self,
        input: impl BufferRef<Backend = B>,
        rows: u32,
        linear: &Linear<B>,
        output: impl BufferMut<Backend = B>,
        command_buffer: &mut <B::CommandBuffer as crate::backends::common::CommandBuffer>::Encoding,
    ) -> Result<(), VisionError<B>> {
        self.kernels
            .matmul
            .lock()
            .encode(
                MatmulArguments {
                    a: MatmulA::FullPrecision {
                        values: input,
                        offset: 0,
                    },
                    b: MatmulB::FullPrecision {
                        b: &linear.weights,
                    },
                    b_leading_dimension: None,
                    b_transpose: true,
                    output: MatmulOutput::new(
                        output,
                        MatmulDOps {
                            bias: Some(&linear.biases),
                            ..MatmulDOps::none()
                        },
                    ),
                    gather_indices: None::<&B::GlobalBuffer>,
                    m: rows,
                    n: linear.output_dim,
                    k: linear.input_dim,
                },
                command_buffer,
            )
            .map_err(VisionError::Matmul)
    }

    fn layer_norm(
        &self,
        input: impl BufferRef<Backend = B>,
        rows: u32,
        norm: &LayerNorm<B>,
        command_buffer: &mut <B::CommandBuffer as crate::backends::common::CommandBuffer>::Encoding,
    ) -> Result<B::ScratchBuffer, VisionError<B>> {
        let mut output = command_buffer
            .allocate_scratch_for_shape(&[rows, self.config.hidden_size], DataType::BF16)
            .map_err(VisionError::Backend)?;
        self.kernels.norm.encode(
            Some(input),
            Some(&norm.scales),
            Some(&norm.biases),
            &mut output,
            None::<&mut B::ScratchBuffer>,
            None::<&B::GlobalBuffer>,
            rows,
            self.config.hidden_size,
            self.config.layer_norm_epsilon,
            0.0,
            1.0,
            command_buffer,
        );
        Ok(output)
    }

    /// Encode one image: `patches` are `grid.0 × grid.1` flattened patches (`patch_dim` values
    /// each) in merge-window order, as Qwen's image processor emits them. Returns the merged
    /// embeddings: `grid.0 × grid.1 / merge_unit` rows of `out_hidden_size` bf16 values, in
    /// row-major merged-grid order.
    pub fn encode(
        &self,
        patches: &[bf16],
        grid: (u32, u32),
    ) -> Result<Vec<bf16>, VisionError<B>> {
        let config = &self.config;
        let merge = config.spatial_merge_size;
        if grid.0 == 0 || grid.1 == 0 || !grid.0.is_multiple_of(merge) || !grid.1.is_multiple_of(merge) {
            return Err(VisionError::Invalid(format!("patch grid {grid:?} is not a multiple of {merge}")));
        }
        let count = grid.0 * grid.1;
        if patches.len() != (count * config.patch_dim()) as usize {
            return Err(VisionError::Invalid(format!(
                "{} patch values for a {}x{} grid of {}-value patches",
                patches.len(),
                grid.0,
                grid.1,
                config.patch_dim()
            )));
        }
        let hidden = config.hidden_size;
        let elements = count * hidden;
        let positions = interpolated_positions(config, &self.position_table, grid);
        let (cosines, sines) = rotary_tables(config, grid);
        let backend = VisionError::Backend;

        // The residual stream lives across command buffers.
        let mut residual =
            self.context.create_buffer((elements as usize) * DataType::BF16.size_in_bytes()).map_err(backend)?;
        let pool = self.context.create_allocation_pool();
        let submit = |command_buffer: <B::CommandBuffer as crate::backends::common::CommandBuffer>::Encoding| {
            command_buffer.end_encoding().submit().wait_until_completed().map(|_| ()).map_err(VisionError::Backend)
        };

        // Patch embedding plus the interpolated position embedding.
        let mut command_buffer =
            self.context.create_command_buffer(Some("vision patch embed"), Some(pool.clone())).map_err(backend)?;
        let input = command_buffer.allocate_constant_from_slice(patches).map_err(backend)?;
        self.matmul(&input, count, &self.patch, &mut residual, &mut command_buffer)?;
        let position_rows = command_buffer.allocate_constant_from_slice(&positions).map_err(backend)?;
        self.kernels.add.encode(None::<&B::GlobalBuffer>, &position_rows, &mut residual, elements, elements, 1.0, &mut command_buffer);
        drop(input);
        drop(position_rows);
        submit(command_buffer)?;

        let heads = config.num_heads;
        let padded = self.padded_head_dim;
        for (index, block) in self.blocks.iter().enumerate() {
            let mut command_buffer = self
                .context
                .create_command_buffer(Some(&format!("vision block {index}")), Some(pool.clone()))
                .map_err(backend)?;
            let cos = command_buffer.allocate_constant_from_slice(&cosines).map_err(backend)?;
            let sin = command_buffer.allocate_constant_from_slice(&sines).map_err(backend)?;
            // Attention.
            let normed = self.layer_norm(&residual, count, &block.norm1, &mut command_buffer)?;
            let mut qkv = command_buffer
                .allocate_scratch_for_shape(&[count, block.qkv.output_dim], DataType::BF16)
                .map_err(backend)?;
            self.matmul(&normed, count, &block.qkv, &mut qkv, &mut command_buffer)?;
            drop(normed);
            let mut queries =
                command_buffer.allocate_scratch_for_shape(&[heads, count, padded], DataType::BF16).map_err(backend)?;
            let mut keys =
                command_buffer.allocate_scratch_for_shape(&[count, heads, padded], DataType::BF16).map_err(backend)?;
            let mut values =
                command_buffer.allocate_scratch_for_shape(&[count, heads, padded], DataType::BF16).map_err(backend)?;
            self.kernels.prepare.encode(
                &qkv,
                &mut queries,
                Some(&mut keys),
                Some(&mut values),
                Some(&cos),
                Some(&sin),
                heads,
                Some(heads),
                padded,
                Some(config.head_dim()),
                Some(0),
                block.qkv.output_dim,
                count,
                &mut command_buffer,
            );
            drop(qkv);
            let attended = self
                .kernels
                .attention
                .encode(
                    AttentionArguments {
                        queries: &queries,
                        keys: &keys,
                        values: &values,
                        suffix_length: count,
                        trie: None::<&B::GlobalBuffer>,
                        sinks: None,
                        cache: KVCacheView::full(0),
                        gate: None::<&B::GlobalBuffer>,
                    },
                    &mut command_buffer,
                )
                .map_err(VisionError::Attention)?;
            drop((queries, keys, values));
            let mut projected =
                command_buffer.allocate_scratch_for_shape(&[count, hidden], DataType::BF16).map_err(backend)?;
            self.matmul(&attended, count, &block.proj, &mut projected, &mut command_buffer)?;
            drop(attended);
            self.kernels.add.encode(None::<&B::GlobalBuffer>, &projected, &mut residual, elements, elements, 1.0, &mut command_buffer);
            drop(projected);
            // MLP.
            let normed = self.layer_norm(&residual, count, &block.norm2, &mut command_buffer)?;
            let mut inner = command_buffer
                .allocate_scratch_for_shape(&[count, config.intermediate_size], DataType::BF16)
                .map_err(backend)?;
            self.matmul(&normed, count, &block.fc1, &mut inner, &mut command_buffer)?;
            drop(normed);
            self.kernels.activation.encode(
                None::<&B::GlobalBuffer>,
                &mut inner,
                count * config.intermediate_size,
                ActivationType::GELUApprox,
                &mut command_buffer,
            );
            let mut out =
                command_buffer.allocate_scratch_for_shape(&[count, hidden], DataType::BF16).map_err(backend)?;
            self.matmul(&inner, count, &block.fc2, &mut out, &mut command_buffer)?;
            drop(inner);
            self.kernels.add.encode(None::<&B::GlobalBuffer>, &out, &mut residual, elements, elements, 1.0, &mut command_buffer);
            drop((out, cos, sin));
            submit(command_buffer)?;
        }

        // Merger: LayerNorm per patch, 2x2 windows concatenated (contiguous rows), MLP.
        let merged_rows = count / config.merge_unit();
        let mut command_buffer =
            self.context.create_command_buffer(Some("vision merger"), Some(pool.clone())).map_err(backend)?;
        let normed = self.layer_norm(&residual, count, &self.merger_norm, &mut command_buffer)?;
        let mut inner = command_buffer
            .allocate_scratch_for_shape(&[merged_rows, self.merger_fc1.output_dim], DataType::BF16)
            .map_err(backend)?;
        self.matmul(&normed, merged_rows, &self.merger_fc1, &mut inner, &mut command_buffer)?;
        drop(normed);
        self.kernels.activation.encode(
            None::<&B::GlobalBuffer>,
            &mut inner,
            merged_rows * self.merger_fc1.output_dim,
            ActivationType::GELUExact,
            &mut command_buffer,
        );
        let mut output = self
            .context
            .create_buffer((merged_rows * config.out_hidden_size) as usize * DataType::BF16.size_in_bytes())
            .map_err(backend)?;
        self.matmul(&inner, merged_rows, &self.merger_fc2, &mut output, &mut command_buffer)?;
        drop(inner);
        submit(command_buffer)?;
        drop(residual);
        let values = (&output).as_slice::<bf16>().to_vec();
        drop(output);
        Ok(values)
    }
}

/// The patch order of Qwen's image processor: merge windows row-major, patches within a window
/// row-major. Yields `(row, column)` patch coordinates.
pub fn window_order(
    grid: (u32, u32),
    merge: u32,
) -> impl Iterator<Item = (u32, u32)> {
    let (rows, columns) = grid;
    (0..rows / merge).flat_map(move |block_row| {
        (0..columns / merge).flat_map(move |block_column| {
            (0..merge).flat_map(move |inner_row| {
                (0..merge).map(move |inner_column| (block_row * merge + inner_row, block_column * merge + inner_column))
            })
        })
    })
}

/// `torch.linspace(start, end, steps)` in f32, as PyTorch computes it on the CPU.
fn linspace(
    start: f32,
    end: f32,
    steps: u32,
) -> Vec<f32> {
    if steps == 1 {
        return vec![start];
    }
    let step = (end - start) / (steps - 1) as f32;
    let halfway = steps / 2;
    (0..steps)
        .map(|index| {
            if index < halfway {
                start + step * index as f32
            } else {
                end - step * (steps - index - 1) as f32
            }
        })
        .collect()
}

/// The learned position table bilinearly interpolated to the patch grid, in window order, with
/// the reference's bf16 arithmetic (weights rounded to bf16, products and the running sum too).
pub fn interpolated_positions(
    config: &VisionModelConfig,
    table: &[bf16],
    grid: (u32, u32),
) -> Vec<bf16> {
    let side = (config.num_position_embeddings as f64).sqrt().round() as u32;
    let hidden = config.hidden_size as usize;
    let (heights, widths) = (linspace(0.0, (side - 1) as f32, grid.0), linspace(0.0, (side - 1) as f32, grid.1));
    let corners = |value: f32| {
        let floor = value as u32;
        (floor, (floor + 1).min(side - 1), value - floor as f32)
    };
    let mut out = Vec::with_capacity(grid.0 as usize * grid.1 as usize * hidden);
    for (row, column) in window_order(grid, config.spatial_merge_size) {
        let (h0, h1, dh) = corners(heights[row as usize]);
        let (w0, w1, dw) = corners(widths[column as usize]);
        let weighted = [
            (h0 * side + w0, (1.0 - dh) * (1.0 - dw)),
            (h0 * side + w1, (1.0 - dh) * dw),
            (h1 * side + w0, dh * (1.0 - dw)),
            (h1 * side + w1, dh * dw),
        ];
        for channel in 0..hidden {
            let mut sum: Option<bf16> = None;
            for (index, weight) in weighted {
                let product = bf16::from_f32(table[index as usize * hidden + channel].to_f32() * bf16::from_f32(weight).to_f32());
                sum = Some(match sum {
                    None => product,
                    Some(total) => bf16::from_f32(total.to_f32() + product.to_f32()),
                });
            }
            out.push(sum.unwrap_or(bf16::ZERO));
        }
    }
    out
}

/// The 2-D rotary tables (`[patch, head_dim]`, f32, rotate-half layout) in window order: the first
/// half of each row's frequencies use the patch row, the second half the patch column.
pub fn rotary_tables(
    config: &VisionModelConfig,
    grid: (u32, u32),
) -> (Vec<f32>, Vec<f32>) {
    let head_dim = config.head_dim() as usize;
    let quarter = head_dim / 4;
    let dim = (head_dim / 2) as f32;
    let inverse: Vec<f32> =
        (0..quarter).map(|index| 1.0 / config.rope_theta.powf((2 * index) as f32 / dim)).collect();
    let count = grid.0 as usize * grid.1 as usize;
    let mut cosines = Vec::with_capacity(count * head_dim);
    let mut sines = Vec::with_capacity(count * head_dim);
    for (row, column) in window_order(grid, config.spatial_merge_size) {
        let mut angles = Vec::with_capacity(head_dim);
        angles.extend(inverse.iter().map(|frequency| row as f32 * frequency));
        angles.extend(inverse.iter().map(|frequency| column as f32 * frequency));
        let doubled: Vec<f32> = angles.iter().chain(angles.iter()).copied().collect();
        cosines.extend(doubled.iter().map(|angle| angle.cos()));
        sines.extend(doubled.iter().map(|angle| angle.sin()));
    }
    (cosines, sines)
}
