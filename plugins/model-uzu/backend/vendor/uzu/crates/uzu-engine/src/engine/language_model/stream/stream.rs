use std::{
    iter::{once, repeat_n},
    mem::replace,
    range::Range,
    sync::{Arc, mpsc::Sender},
};

use futures::executor::block_on;
use shoji::traits::backend::chat_token::TokenStreamMetrics;


use crate::engine::language_model::grammar::Grammar;
use crate::{
    backends::common::{
        Backend, Buffer, BufferRef, CommandBuffer, CommandBufferEncoding, CommandBufferExecutable, Context, CpuBuffer, GlobalBufferCpuGpu, GpuBuffer,
        TimestampSpan,
        capture_matcher::{CAPTURE_MATCHER, CaptureMatcherSpan},
        gpu_types::trie::TrieNode as GpuTrieNode,
        kernel::ContextRingUpdateKernel,
    },
    data_type::DataType,
    encodable_block::{
        batch_topology::BatchTopology,
        decoder::EmbeddingRows,
        sampling::SamplingMethod,
        transformer::{RopeRows, TransformerBatchSequence},
    },
    engine::language_model::{
        LanguageModel,
        state::LanguageModelState,
        stream::{LanguageModelStreamError, LanguageModelStreamOptions},
    },
    trie::TrieNode,
    utils::{
        size_for_shape::size_for_shape,
        timestamps::{record, wait},
    },
};

enum ForwardPassChaining<B: Backend> {
    Constant {
        token: u64,
        output_norm: Option<B::ScratchBuffer>,
    },
    InFlight(DecodingStatePending<B>),
}

impl<B: Backend> ForwardPassChaining<B> {
    async fn resolve<'a>(
        &'a mut self,
        tokens: &mut Vec<u64>,
        timestamps: Option<&Sender<Box<[TimestampSpan]>>>,
        context: &B::Context,
         grammar: Option<&mut Grammar>,
    ) -> Result<(u64, Option<&'a B::ScratchBuffer>), LanguageModelStreamError<B>> {
        match self {
            Self::Constant {
                token,
                output_norm,
            } => Ok((*token, output_norm.as_ref())),
            Self::InFlight(in_flight) => {
                assert!(in_flight.full_accept);
                for pending in replace(&mut in_flight.pending, Box::new([])) {
                    wait(pending, timestamps).await.map_err(LanguageModelStreamError::Backend)?;
                }
                let output_tokens = context.copyout::<u32>(&*in_flight.output_tokens).await.map_err(LanguageModelStreamError::Backend)?;
                assert_eq!(output_tokens.len(), 1);
                let token_id = output_tokens[0] as u64;
                let output_norm = in_flight.output_norm.take();
                *self = Self::Constant {
                    token: token_id,
                    output_norm,
                };
                tokens.push(token_id);
                
                if let Some(grammar) = grammar {
                    grammar.accept_token(token_id)?;
                }
                let Self::Constant {
                    output_norm,
                    ..
                } = self
                else {
                    unreachable!()
                };
                Ok((token_id, output_norm.as_ref()))
            },
        }
    }

    fn drop_output_norm(&mut self) {
        match self {
            Self::Constant {
                output_norm,
                ..
            } => drop(output_norm.take()),
            Self::InFlight(pending) => drop(pending.output_norm.take()),
        }
    }
}

struct DecodingStatePending<B: Backend> {
    input_trie: TrieNode,
    full_accept: bool,
    pending: Box<[<B::CommandBuffer as CommandBuffer>::Pending]>,
    capture_span: Option<CaptureMatcherSpan>,
    hidden_features: Option<Box<[B::ScratchBuffer]>>,
    output_norm: Option<B::ScratchBuffer>,
    output_tokens: Arc<B::GlobalGpuBuffer>,
    _input_token_source: Option<Arc<B::GlobalGpuBuffer>>,
}

enum DecodingState<B: Backend> {
    Seeded {
        seed_token: u64,
    },
    ForwardPassPending(DecodingStatePending<B>),
    Accepting {
        full: Box<[(usize, u64, u64)]>,
        num_accepted: usize,
        hidden_features: Option<Box<[B::ScratchBuffer]>>,
        output_norm: Option<(B::ScratchBuffer, usize)>,
        capture_span: Option<CaptureMatcherSpan>,
    },
    Halted,
    Invalid,
    Closed,
}

/// CortexCode patch P10: a stream's share of its next forward pass, decided by
/// [`LanguageModelStream::begin`]: the previous output still to hand out, the acceptance already
/// submitted, and the input (one token or a drafted tree).
struct PlannedPass<B: Backend> {
    prev_output: ForwardPassChaining<B>,
    pending: Vec<<B::CommandBuffer as CommandBuffer>::Pending>,
    capture_span: Option<CaptureMatcherSpan>,
    input_trie: TrieNode,
    chain_copy: Option<Arc<B::GlobalGpuBuffer>>,
    full_accept: bool,
}

/// CortexCode patch P10.
enum Begun<B: Backend> {
    /// The step ends without a forward pass: a token of the last accepted run, or the end.
    Done(Option<u64>),
    Pass(PlannedPass<B>),
}

/// CortexCode patch P10: what one step of [`LanguageModel::generate_batch`] gave a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStep {
    /// The next token.
    Token(u64),
    /// The stream ended (context end, closed).
    End,
    /// No token this step: other streams handed out tokens they already had, and this one waits
    /// for the pass they all share.
    Waiting,
}

fn prefill_chunk_parts(
    input_chunk: &[u64],
    last_batch: bool,
    split_logits_row: bool,
) -> [Option<(&[u64], bool)>; 2] {
    if last_batch && split_logits_row && input_chunk.len() > 1 {
        let (prompt_chunk, sample_chunk) = input_chunk.split_at(input_chunk.len() - 1);
        [Some((prompt_chunk, false)), Some((sample_chunk, true))]
    } else {
        [Some((input_chunk, last_batch)), None]
    }
}

pub struct LanguageModelStream<'a, B: Backend> {
    model: &'a LanguageModel<B>,
    model_state: &'a mut LanguageModelState<B>,
    options: LanguageModelStreamOptions,
    allocation_pool: Arc<B::AllocationPool>,
    context_ring: Option<B::GlobalGpuBuffer>,
    decoding_state: DecodingState<B>,
    metrics: TokenStreamMetrics,
}

impl<'a, B: Backend> LanguageModelStream<'a, B> {
    pub fn new(
        model: &'a LanguageModel<B>,
        input: &[u64],
        model_state: &'a mut LanguageModelState<B>,
        options: LanguageModelStreamOptions,
    ) -> Result<Self, LanguageModelStreamError<B>> {
        let mut options = options;
        if model_state.tokens.is_empty() && input.is_empty() {
            return Err(LanguageModelStreamError::NoSeedToken);
        };

        if model_state
            .max_context_length
            .is_some_and(|max_context_length| model_state.tokens.len() + input.len() > max_context_length as usize)
        {
            return Err(LanguageModelStreamError::ContextOverflow);
        }

        let allocation_pool = model.engine.context.create_allocation_pool();

        let mut context_ring = if let Some(suffix_repetition_length) =
            options.sampling_method.suffix_repetition_length()
        {
            let mut context_ring = model
                .engine
                .context
                .create_buffer_cpu_gpu(size_for_shape(&[2 + suffix_repetition_length], DataType::U32))
                .map_err(LanguageModelStreamError::Backend)?;

            // CortexCode patch P3: an empty window when only generated tokens are penalized.
            let window_start = if options.penalty_window_includes_state {
                model_state.tokens.len().saturating_sub(suffix_repetition_length as usize)
            } else {
                model_state.tokens.len()
            };
            let state_tokens_range = Range::from(window_start..model_state.tokens.len());

            context_ring.copyin(
                &once(0) // offset
                    .chain(once(state_tokens_range.iter().len() as u64)) // length
                    .chain(model_state.tokens[state_tokens_range].iter().copied()) // tokens
                    .chain(repeat_n(0, suffix_repetition_length as usize - state_tokens_range.iter().len())) // pad if not full
                    .map(|x| x as u32)
                    .collect::<Box<[_]>>(),
            );

            Some(context_ring.handoff().map_err(LanguageModelStreamError::Backend)?)
        } else {
            None
        };

        let mut metrics = TokenStreamMetrics::default();

        // CortexCode patch P7: image embeddings and their rotary positions for this input.
        let images = options.images.take().filter(|images| !images.images.is_empty());
        if images.is_some() && input.is_empty() {
            return Err(LanguageModelStreamError::Images("images need input tokens".into()));
        }
        let image_plan = images
            .as_ref()
            .map(|images| {
                images.plan(
                    model_state.transformer_state.context_length(),
                    model_state.transformer_state.rope_delta(),
                    input.len(),
                    model.decoder.model_dim() as usize,
                )
            })
            .transpose()
            .map_err(LanguageModelStreamError::Images)?;

        let decoding_state = if !input.is_empty() {
            let _capture_span = CAPTURE_MATCHER.capture::<B>(&model.engine.context, "prefill")?;
            model_state.last_output_token.take();

            // NOTE: this is required for attention correctness (hardcoded suffix 1024). This is really bad design, attention should be rewritten to allow on-demand suffix length
            let max_batch_size = 1024;
            let number_of_batches = input.len().div_ceil(max_batch_size);

            model_state
                .transformer_state
                .prepare(
                    model_state.transformer_state.context_length() + ((number_of_batches - 1) * max_batch_size) as u32,
                    usize::min(max_batch_size, input.len()) as u32,
                )
                .map_err(LanguageModelStreamError::Backend)?;

            let command_buffer = model
                .engine
                .context
                .create_command_buffer(Some("prefill"), Some(allocation_pool.clone()))
                .map_err(LanguageModelStreamError::Backend)?;
            let mut command_buffer = record(command_buffer, options.timestamps.as_ref());

            command_buffer.debug_point(&format!(
                "prefill at context {}, suffix {} ({} batches)",
                model_state.transformer_state.context_length(),
                input.len(),
                number_of_batches
            ));

            let mut output_tokens = None;
            let mut output_norm = None;
            let split_logits_row = model.decoder.prefill_cache_skips_trailing_layers();
            let hidden_feature_layer_indices =
                model.speculator.as_ref().map(|speculator| speculator.hidden_feature_layer_indices());

            // CortexCode patch P7: index of the current chunk's first row in `input`.
            let mut chunk_offset = 0usize;
            for (input_chunk, sample_last) in input
                .chunks(max_batch_size)
                .enumerate()
                .flat_map(|(batch_idx, input_chunk)| {
                    prefill_chunk_parts(input_chunk, batch_idx == number_of_batches - 1, split_logits_row)
                })
                .flatten()
            {
                let input_trie = TrieNode::flat(model_state.tokens.len(), input_chunk, &model_state.prng);
                let input_flat_trie = input_trie.linearize();

                let token_ids = command_buffer
                    .allocate_constant_from_slice(
                        &input_chunk.iter().map(|token_id| *token_id as u32).collect::<Box<[u32]>>(),
                    )
                    .map_err(LanguageModelStreamError::Backend)?;

                let input_flat_trie_nodes = input_flat_trie.token_subtrie_ranges().collect::<Box<[GpuTrieNode]>>();
                let batch_dim = BatchTopology::new(input_flat_trie_nodes, true);

                // CortexCode patch P7: this chunk's image rows and positions.
                let chunk_range = chunk_offset..chunk_offset + input_chunk.len();
                let model_dim = model.decoder.model_dim() as usize;
                let embedding_rows = match (&images, &image_plan) {
                    (Some(images), Some(plan)) => plan
                        .segments
                        .iter()
                        .filter_map(|segment| {
                            let start = segment.input_row.max(chunk_range.start);
                            let end = (segment.input_row + segment.count).min(chunk_range.end);
                            (start < end).then(|| {
                                let image_row = segment.image_row + (start - segment.input_row);
                                EmbeddingRows {
                                    row: (start - chunk_range.start) as u32,
                                    data: &images.images[segment.image].rows
                                        [image_row * model_dim..(image_row + end - start) * model_dim],
                                }
                            })
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                };
                let rope_rows = match (&images, &image_plan) {
                    (Some(images), Some(plan)) => Some(RopeRows {
                        positions: &plan.positions[chunk_range.clone()],
                        section: images.mrope_section,
                    }),
                    _ => None,
                };
                chunk_offset = chunk_range.end;

                let decoder_output = model.decoder.encode(
                    &token_ids,
                    &batch_dim,
                    sample_last.then(|| (input_chunk.len() as u32 - 1..input_chunk.len() as u32).into()),
                    hidden_feature_layer_indices,
                    &mut model_state.transformer_state,
                    &embedding_rows,
                    rope_rows,
                    &mut command_buffer,
                )?;
                let logits = decoder_output.logits;

                if sample_last {
                    let logits = logits.unwrap();

                    let seeds = if matches!(options.sampling_method, SamplingMethod::Stochastic { .. }) {
                        Some(
                            command_buffer
                                .allocate_constant_from_slice(&[model_state
                                    .prng
                                    .derive((model_state.tokens.len() + input_chunk.len() - 1) as u64)])
                                .map_err(LanguageModelStreamError::Backend)?,
                        )
                    } else {
                        None
                    };

                    
                    let bitmask = if let Some(grammar) = options.grammar.as_mut() {
                        let mut bitmask = command_buffer
                            .allocate_constant(
                                model.vocab_size.div_ceil(DataType::U32.size_in_bits()) * DataType::U32.size_in_bytes(),
                            )
                            .map_err(LanguageModelStreamError::Backend)?;

                        if grammar.next_bitmask(bitmask.as_slice_mut()) {
                            Some(bitmask)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    output_norm = decoder_output.final_hidden;
                    let sampled_row = batch_dim.size() - 1;
                    output_tokens = Some(
                        model
                            .sampling
                            .encode(
                                &logits,
                                seeds.as_ref(),
                                bitmask.as_ref(),
                                context_ring.as_ref(),
                                Some(&token_ids),
                                &batch_dim,
                                (sampled_row..sampled_row + 1).into(),
                                &options.sampling_method,
                                &mut command_buffer,
                            )
                            .map_err(LanguageModelStreamError::Backend)?,
                    );
                }

                model_state
                    .transformer_state
                    .encode_accept(&(0..input_chunk.len() as u32).collect::<Box<[u32]>>(), &mut command_buffer)
                    .map_err(LanguageModelStreamError::Backend)?;

                if let Some(speculator) = model.speculator.as_ref() {
                    let speculator_state = model_state.speculator_state.as_mut().unwrap();
                    speculator.encode_accept(
                        speculator_state,
                        decoder_output.hidden_features.as_ref().unwrap().iter(),
                        &(0..input_chunk.len() as u32).collect::<Box<[u32]>>(),
                        &mut command_buffer,
                    )?;
                }

                if let Some(suffix_repetition_length) = options.sampling_method.suffix_repetition_length() {
                    model.context_ring_update.encode(
                        &token_ids,
                        context_ring.as_mut().unwrap(),
                        suffix_repetition_length,
                        input_chunk.len() as u32,
                        &mut command_buffer,
                    );
                }

                model_state.tokens.extend(input_chunk);
            }

            // CortexCode patch P7: text after this input continues at the shifted position.
            if let Some(plan) = &image_plan {
                model_state.transformer_state.set_rope_delta(plan.rope_delta_after);
            }

            let pending = Box::new([command_buffer.end_encoding().submit()]);

            metrics.num_prefill_forward_passes += 1;
            metrics.num_tokens_prefilled += input.len();
            metrics.num_tokens_proposed += 1;
            metrics.num_tokens_accepted += 1;

            DecodingState::ForwardPassPending(DecodingStatePending {
                input_trie: TrieNode::new(0, 0, 0.0),
                full_accept: true,
                pending,
                capture_span: None,
                hidden_features: None,
                output_norm,
                output_tokens: Arc::new(output_tokens.unwrap()),
                _input_token_source: None,
            })
        } else {
            // TODO: this leaks previous LanguageModelStreamOptions
            DecodingState::Seeded {
                seed_token: model_state.last_output_token.take().unwrap(),
            }
        };

        Ok(LanguageModelStream {
            model,
            model_state,
            options,
            allocation_pool,
            context_ring,
            decoding_state,
            metrics,
        })
    }

    pub async fn generate(&mut self) -> Result<Option<u64>, LanguageModelStreamError<B>> {
        match self.begin(None, false).await? {
            Begun::Done(token) => Ok(token),
            Begun::Pass(pass) => self.encode_single(pass).await,
        }
    }

    /// A verified tree's outcome: which drafted tokens the target model accepted (uzu's code,
    /// moved here from [`Self::begin`] by CortexCode patch P10 so [`Self::settle`] can share it).
    async fn accept_pass(
        &mut self,
        forward_pass_pending: DecodingStatePending<B>,
    ) -> Result<(), LanguageModelStreamError<B>> {
        for pending in forward_pass_pending.pending {
            wait(pending, self.options.timestamps.as_ref()).await.map_err(LanguageModelStreamError::Backend)?;
        }
        let sampled_tokens = self
            .model
            .engine
            .context
            .copyout::<u32>(&*forward_pass_pending.output_tokens)
            .await
            .map_err(LanguageModelStreamError::Backend)?
            .iter()
            .map(|x| *x as u64)
            .collect::<Box<[u64]>>();
        let flat_trie = forward_pass_pending.input_trie.linearize();
        let full = flat_trie.accept(&sampled_tokens, self.options.grammar.as_mut())?;
        let output_norm = forward_pass_pending.output_norm.map(|norm| {
            let row_bytes = norm.size() / flat_trie.len();
            (norm, row_bytes)
        });
        self.metrics.num_tokens_accepted += full.len();
        self.decoding_state = DecodingState::Accepting {
            full,
            num_accepted: 0,
            hidden_features: forward_pass_pending.hidden_features,
            output_norm,
            capture_span: forward_pass_pending.capture_span,
        };
        Ok(())
    }

    /// CortexCode patch P10: take a finished verification pass's outcome now (its accepted tokens
    /// are then known), so [`Self::has_token_without_pass`] can tell what the next step needs.
    async fn settle(&mut self) -> Result<(), LanguageModelStreamError<B>> {
        if matches!(&self.decoding_state, DecodingState::ForwardPassPending(pending) if !pending.full_accept) {
            let DecodingState::ForwardPassPending(pending) = replace(&mut self.decoding_state, DecodingState::Invalid)
            else {
                unreachable!()
            };
            self.accept_pass(pending).await?;
        }
        Ok(())
    }

    /// CortexCode patch P10: whether the next step returns without a forward pass (an accepted
    /// token still to hand out, or the end). Exact after [`Self::settle`].
    fn has_token_without_pass(&self) -> bool {
        match &self.decoding_state {
            DecodingState::Accepting {
                full,
                num_accepted,
                ..
            } => *num_accepted < full.len() - 1,
            DecodingState::Halted | DecodingState::Closed | DecodingState::Invalid => true,
            DecodingState::Seeded {
                ..
            }
            | DecodingState::ForwardPassPending(_) => false,
        }
    }

    /// CortexCode patch P10: everything [`Self::generate`] does before its forward pass: the
    /// previous pass's tokens are taken, and the next pass's input (one token, or the speculator's
    /// tree within `tree_budget` nodes) is decided. `resolve` takes the previous output token on the
    /// CPU now (a batched pass feeds constant tokens only); otherwise a plain decode chains it on the
    /// device, as uzu does.
    async fn begin(
        &mut self,
        tree_budget: Option<u32>,
        resolve: bool,
    ) -> Result<Begun<B>, LanguageModelStreamError<B>> {
        let (mut prev_output, acceptance): (
            ForwardPassChaining<B>,
            Option<(<B::CommandBuffer as CommandBuffer>::Encoding, Option<CaptureMatcherSpan>)>,
        ) = match replace(&mut self.decoding_state, DecodingState::Invalid) {
            DecodingState::Seeded {
                seed_token,
            } => {
                self.model_state.tokens.push(seed_token);
                
                if let Some(grammar) = self.options.grammar.as_mut() {
                    let _ = grammar.accept_token(seed_token); // TODO: this should not be ignored
                }
                self.metrics.num_tokens_returned += 1;
                (
                    ForwardPassChaining::Constant {
                        token: seed_token,
                        output_norm: None,
                    },
                    None,
                )
            },
            DecodingState::ForwardPassPending(forward_pass_pending) => {
                if forward_pass_pending.full_accept {
                    self.metrics.num_tokens_returned += 1;
                    (ForwardPassChaining::InFlight(forward_pass_pending), None)
                } else {
                    self.accept_pass(forward_pass_pending).await?;
                    return Box::pin(self.begin(tree_budget, resolve)).await;
                }
            },
            DecodingState::Accepting {
                full,
                num_accepted,
                hidden_features,
                output_norm,
                capture_span,
            } => {
                let output_token_id = full[num_accepted].2;

                self.metrics.num_tokens_returned += 1;

                if num_accepted < full.len() - 1 {
                    self.decoding_state = DecodingState::Accepting {
                        full,
                        num_accepted: num_accepted + 1,
                        hidden_features,
                        output_norm,
                        capture_span,
                    };
                    return Ok(Begun::Done(Some(output_token_id)));
                } else {
                    let accepted_token_indicies = full.iter().map(|(i, _, _)| *i as u32).collect::<Box<[u32]>>();
                    let accepted_input_token_ids = full.iter().map(|(_, t, _)| *t).collect::<Box<[u64]>>();
                    let accepted_output_token_ids = full.iter().map(|(_, _, t)| *t).collect::<Box<[u64]>>();
                    let command_buffer = self
                        .model
                        .engine
                        .context
                        .create_command_buffer(
                            Some(&format!("decode {} (accept)", self.metrics.num_decode_forward_passes - 1)),
                            Some(self.allocation_pool.clone()),
                        )
                        .map_err(LanguageModelStreamError::Backend)?;
                    let mut command_buffer = record(command_buffer, self.options.timestamps.as_ref());
                    self.model_state
                        .transformer_state
                        .encode_accept(&accepted_token_indicies, &mut command_buffer)
                        .map_err(LanguageModelStreamError::Backend)?;
                    if let Some(speculator) = self.model.speculator.as_ref() {
                        speculator.encode_accept(
                            self.model_state.speculator_state.as_mut().unwrap(),
                            hidden_features.as_ref().unwrap().iter(),
                            &accepted_token_indicies,
                            &mut command_buffer,
                        )?;
                    }
                    let output_norm = if let Some((final_hidden, row_bytes)) = output_norm {
                        let row = full.last().unwrap().0;
                        let mut norm =
                            command_buffer.allocate_scratch(row_bytes).map_err(LanguageModelStreamError::Backend)?;
                        command_buffer
                            .encode_copy(final_hidden.subrange(row * row_bytes..(row + 1) * row_bytes), &mut norm);
                        Some(norm)
                    } else {
                        None
                    };
                    if let Some(suffix_repetition_length) = self.options.sampling_method.suffix_repetition_length() {
                        let accepted_input_token_ids_const = command_buffer
                            .allocate_constant_from_slice(
                                &accepted_input_token_ids
                                    .iter()
                                    .map(|token_id| *token_id as u32)
                                    .collect::<Box<[u32]>>(),
                            )
                            .map_err(LanguageModelStreamError::Backend)?;
                        self.model.context_ring_update.encode(
                            &accepted_input_token_ids_const,
                            self.context_ring.as_mut().unwrap(),
                            suffix_repetition_length,
                            full.len() as u32,
                            &mut command_buffer,
                        );
                    }
                    self.model_state.tokens.extend(accepted_output_token_ids);
                    (
                        ForwardPassChaining::Constant {
                            token: output_token_id,
                            output_norm,
                        },
                        Some((command_buffer, capture_span)),
                    )
                }
            },
            DecodingState::Halted => return Ok(Begun::Done(None)),
            DecodingState::Invalid => unreachable!(),
            DecodingState::Closed => return Ok(Begun::Done(None)),
        };

        let context_length = self.model_state.transformer_state.context_length();

        if self.model_state.max_context_length.is_some_and(|max_context_length| context_length >= max_context_length) {
            self.decoding_state = DecodingState::Halted;
            return Ok(Begun::Done(Some(
                prev_output
                    .resolve(
                        &mut self.model_state.tokens,
                        self.options.timestamps.as_ref(),
                        &self.model.engine.context,

                        self.options.grammar.as_mut(),
                    )
                    .await?
                    .0,
            )));
        }

        let mut pending = Vec::new();
        if let Some((accept_command_buffer, _capture_span)) = acceptance {
            pending.push(accept_command_buffer.end_encoding().submit());
        }

        let capture_span = CAPTURE_MATCHER
            .capture::<B>(&self.model.engine.context, &format!("decode:{}", self.metrics.num_decode_forward_passes))?;

        #[allow(clippy::let_and_return)]
        let (input_trie, chain_copy, full_accept) = if let Some(speculator) = &self.model.speculator
            && self.options.speculation
            && let Some(shape) = speculator.make_shape_within(
                self.model_state.max_context_length.map(|max_context_length| max_context_length - context_length),
                tree_budget,
            )
            && let (root_token, Some(output_norm)) = {
                let resolved = prev_output
                    .resolve(
                        &mut self.model_state.tokens,
                        self.options.timestamps.as_ref(),
                        &self.model.engine.context,
                        
                        self.options.grammar.as_mut(),
                    )
                    .await?;
                resolved
            } {
            let trie = speculator
                .propose_tree(
                    self.model_state.speculator_state.as_mut().unwrap(),
                    output_norm,
                    root_token as u32,
                    self.model.decoder.embedding(),
                    shape,
                    &self.options.sampling_method,
                    
                    self.options.grammar.as_mut(),
                    &self.model_state.prng,
                    self.allocation_pool.clone(),
                    Some(&format!("decode {} (speculate)", self.metrics.num_decode_forward_passes)),
                    self.options.timestamps.as_ref(),
                )
                .await?;
            prev_output.drop_output_norm();
            (trie, None, false)
        } else {
            if resolve {
                prev_output
                    .resolve(
                        &mut self.model_state.tokens,
                        self.options.timestamps.as_ref(),
                        &self.model.engine.context,
                        self.options.grammar.as_mut(),
                    )
                    .await?;
            }
            prev_output.drop_output_norm();
            let (token, chain_copy) = match &prev_output {
                ForwardPassChaining::Constant {
                    token,
                    ..
                } => (*token, None),
                ForwardPassChaining::InFlight(pending) => (0, Some(pending.output_tokens.clone())),
            };
            (TrieNode::new(token, self.model_state.prng.derive(context_length as u64), 0.0), chain_copy, true)
        };

        Ok(Begun::Pass(PlannedPass {
            prev_output,
            pending,
            capture_span,
            input_trie,
            chain_copy,
            full_accept,
        }))
    }

    /// CortexCode patch P10: [`Self::generate`]'s own forward pass for the input [`Self::begin`]
    /// decided (uzu's single-stream path, unchanged).
    async fn encode_single(
        &mut self,
        pass: PlannedPass<B>,
    ) -> Result<Option<u64>, LanguageModelStreamError<B>> {
        let PlannedPass {
            mut prev_output,
            mut pending,
            mut capture_span,
            input_trie,
            chain_copy,
            full_accept,
        } = pass;
        let context_length = self.model_state.transformer_state.context_length();
        let input_flat_trie = input_trie.linearize();

        self.allocation_pool = self.model.engine.context.create_allocation_pool();

        let command_buffer = self
            .model
            .engine
            .context
            .create_command_buffer(
                Some(&format!("decode {} (forward)", self.metrics.num_decode_forward_passes)),
                Some(self.allocation_pool.clone()),
            )
            .map_err(LanguageModelStreamError::Backend)?;
        let mut command_buffer = record(command_buffer, self.options.timestamps.as_ref());

        command_buffer.debug_point(&format!(
            "decode round {} at context {} suffix {}",
            self.metrics.num_decode_forward_passes,
            context_length,
            input_flat_trie.len()
        ));

        let scratch_token_ids;
        let mut constant_token_ids = None;
        let token_ids: &dyn GpuBuffer<Backend = B> = if let Some(chain_copy) = chain_copy.as_deref() {
            let mut token_ids = command_buffer
                .allocate_scratch(DataType::U32.size_in_bytes())
                .map_err(LanguageModelStreamError::Backend)?;
            command_buffer.encode_copy(chain_copy, &mut token_ids);
            scratch_token_ids = token_ids;
            &scratch_token_ids
        } else {
            constant_token_ids.insert(
                command_buffer
                    .allocate_constant_from_slice(
                        &input_flat_trie.token_ids().map(|token_id| token_id as u32).collect::<Box<[u32]>>(),
                    )
                    .map_err(LanguageModelStreamError::Backend)?,
            )
        };

        let input_flat_trie_nodes = input_flat_trie.token_subtrie_ranges().collect::<Box<[GpuTrieNode]>>();
        let batch_dim = BatchTopology::new(input_flat_trie_nodes, full_accept);

        self.model_state
            .transformer_state
            .prepare(self.model_state.transformer_state.context_length(), batch_dim.size())
            .map_err(LanguageModelStreamError::Backend)?;

        let hidden_feature_layer_indices =
            self.model.speculator.as_ref().map(|speculator| speculator.hidden_feature_layer_indices());

        let decoder_output = self.model.decoder.encode(
            token_ids,
            &batch_dim,
            Some((0..batch_dim.size()).into()),
            hidden_feature_layer_indices,
            &mut self.model_state.transformer_state,
            &[],
            None,
            &mut command_buffer,
        )?;
        let logits = decoder_output.logits.unwrap();

        
        let (bitmask, mut command_buffer) = if let Some(grammar) = self.options.grammar.as_mut() {
            if chain_copy.is_some() {
                pending.push(command_buffer.end_encoding().submit());

                let command_buffer = self
                    .model
                    .engine
                    .context
                    .create_command_buffer(
                        Some(&format!("decode {} (sample and accept)", self.metrics.num_decode_forward_passes)),
                        Some(self.allocation_pool.clone()),
                    )
                    .map_err(LanguageModelStreamError::Backend)?;
                let mut command_buffer = record(command_buffer, self.options.timestamps.as_ref());

                let mut bitmask = command_buffer
                    .allocate_constant(
                        self.model.vocab_size.div_ceil(DataType::U32.size_in_bits()) * DataType::U32.size_in_bytes(),
                    )
                    .map_err(LanguageModelStreamError::Backend)?;

                prev_output
                    .resolve(&mut self.model_state.tokens, self.options.timestamps.as_ref(), &self.model.engine.context, Some(grammar))
                    .await?;
                if grammar.next_bitmask(bitmask.as_slice_mut()) {
                    (Some(bitmask), command_buffer)
                } else {
                    (None, command_buffer)
                }
            } else {
                let mut bitmasks = command_buffer
                    .allocate_constant(
                        input_flat_trie.len()
                            * self.model.vocab_size.div_ceil(DataType::U32.size_in_bits())
                            * DataType::U32.size_in_bytes(),
                    )
                    .map_err(LanguageModelStreamError::Backend)?;

                if input_flat_trie.fill_bitmasks(bitmasks.as_slice_mut(), self.model.vocab_size, grammar) {
                    (Some(bitmasks), command_buffer)
                } else {
                    (None, command_buffer)
                }
            }
        } else {
            (None, command_buffer)
        };

        let seeds = if matches!(self.options.sampling_method, SamplingMethod::Stochastic { .. }) {
            Some(
                command_buffer
                    .allocate_constant_from_slice(&input_flat_trie.token_seeds().collect::<Box<[u64]>>())
                    .map_err(LanguageModelStreamError::Backend)?,
            )
        } else {
            None
        };

        let output_tokens = self
            .model
            .sampling
            .encode(
                &logits,
                seeds.as_ref(),
                bitmask.as_ref(),
                self.context_ring.as_ref(),
                Some(token_ids),
                &batch_dim,
                (0..batch_dim.size()).into(),
                &self.options.sampling_method,
                &mut command_buffer,
            )
            .map_err(LanguageModelStreamError::Backend)?;

        drop(seeds);
        drop(bitmask);
        drop(logits);

        if full_accept {
            self.model_state
                .transformer_state
                .encode_accept(&(0..batch_dim.size()).collect::<Box<[u32]>>(), &mut command_buffer)
                .map_err(LanguageModelStreamError::Backend)?;

            if let Some(speculator) = self.model.speculator.as_ref() {
                let speculator_state = self.model_state.speculator_state.as_mut().unwrap();
                speculator.encode_accept(
                    speculator_state,
                    decoder_output.hidden_features.as_ref().unwrap().iter(),
                    &(0..batch_dim.size()).collect::<Box<[u32]>>(),
                    &mut command_buffer,
                )?;
            }

            if let Some(suffix_repetition_length) = self.options.sampling_method.suffix_repetition_length() {
                self.model.context_ring_update.encode(
                    token_ids,
                    self.context_ring.as_mut().unwrap(),
                    suffix_repetition_length,
                    batch_dim.size(),
                    &mut command_buffer,
                );
            }
        }

        drop(constant_token_ids);
        pending.push(command_buffer.end_encoding().submit());

        if full_accept {
            capture_span.take();
        }

        self.metrics.num_decode_forward_passes += 1;
        self.metrics.num_tokens_proposed += input_flat_trie.len();
        if full_accept {
            self.metrics.num_tokens_accepted += input_flat_trie.len();
        }

        self.decoding_state = DecodingState::ForwardPassPending(DecodingStatePending {
            input_trie,
            full_accept,
            pending: pending.into_boxed_slice(),
            capture_span,
            hidden_features: if full_accept {
                None
            } else {
                decoder_output.hidden_features
            },
            output_norm: decoder_output.final_hidden,
            output_tokens: Arc::new(output_tokens),
            _input_token_source: chain_copy,
        });

        Ok(Some(
            prev_output
                .resolve(
                    &mut self.model_state.tokens,
                    self.options.timestamps.as_ref(),
                    &self.model.engine.context,
                    
                    self.options.grammar.as_mut(),
                )
                .await?
                .0,
        ))
    }

    pub fn metrics(&self) -> &TokenStreamMetrics {
        &self.metrics
    }

    pub async fn close(&mut self) -> Result<(), LanguageModelStreamError<B>> {
        let last_output_token = match replace(&mut self.decoding_state, DecodingState::Closed) {
            DecodingState::Seeded {
                seed_token,
            } => Some(seed_token),
            DecodingState::ForwardPassPending(in_flight) => {
                for pending in in_flight.pending {
                    wait(pending, self.options.timestamps.as_ref()).await.map_err(LanguageModelStreamError::Backend)?;
                }

                if !in_flight.full_accept {
                    let command_buffer = self
                        .model
                        .engine
                        .context
                        .create_command_buffer(
                            Some(&format!("decode {} (accept on drop)", self.metrics.num_decode_forward_passes - 1)),
                            Some(self.allocation_pool.clone()),
                        )
                        .map_err(LanguageModelStreamError::Backend)?;
                    let mut command_buffer = record(command_buffer, self.options.timestamps.as_ref());
                    self.model_state
                        .transformer_state
                        .encode_accept(&[0], &mut command_buffer)
                        .map_err(LanguageModelStreamError::Backend)?;
                    if let Some(speculator) = self.model.speculator.as_ref() {
                        speculator.encode_accept(
                            self.model_state.speculator_state.as_mut().unwrap(),
                            in_flight.hidden_features.as_ref().unwrap().iter(),
                            &[0],
                            &mut command_buffer,
                        )?;
                    }
                    wait(command_buffer.end_encoding().submit(), self.options.timestamps.as_ref())
                        .await
                        .map_err(LanguageModelStreamError::Backend)?;
                }

                Some(self.model.engine.context.copyout::<u32>(&*in_flight.output_tokens).await.map_err(LanguageModelStreamError::Backend)?[0] as u64)
            },
            DecodingState::Accepting {
                full,
                num_accepted,
                hidden_features,
                output_norm: _,
                capture_span,
            } => {
                // CortexCode patch P10: a stream waiting in a batch is closed with nothing handed
                // out yet (`settle` took its pass's outcome): only the root is accepted then, as
                // a pending tree's close does.
                assert!(num_accepted < full.len());

                let command_buffer = self
                    .model
                    .engine
                    .context
                    .create_command_buffer(
                        Some(&format!("decode {} (accept on drop)", self.metrics.num_decode_forward_passes - 1)),
                        Some(self.allocation_pool.clone()),
                    )
                    .map_err(LanguageModelStreamError::Backend)?;
                let mut command_buffer = record(command_buffer, self.options.timestamps.as_ref());
                let accepted_token_indicies =
                    full.iter().take(num_accepted + 1).map(|(i, _, _)| *i as u32).collect::<Box<[u32]>>();
                self.model_state
                    .transformer_state
                    .encode_accept(&accepted_token_indicies, &mut command_buffer)
                    .map_err(LanguageModelStreamError::Backend)?;
                if let Some(speculator) = self.model.speculator.as_ref() {
                    speculator.encode_accept(
                        self.model_state.speculator_state.as_mut().unwrap(),
                        hidden_features.as_ref().unwrap().iter(),
                        &accepted_token_indicies,
                        &mut command_buffer,
                    )?;
                }
                wait(command_buffer.end_encoding().submit(), self.options.timestamps.as_ref())
                    .await
                    .map_err(LanguageModelStreamError::Backend)?;

                drop(capture_span);

                self.model_state.tokens.extend(full.iter().take(num_accepted).map(|(_, _, t)| *t));

                Some(full[num_accepted].2)
            },
            DecodingState::Halted => None,
            DecodingState::Invalid => None, // TODO: proper error handling
            DecodingState::Closed => self.model_state.last_output_token,
        };

        self.model_state.last_output_token = last_output_token;

        Ok(())
    }
}

impl<B: Backend> LanguageModel<B> {
    /// CortexCode patch P10: whether [`Self::generate_batch`] can run several streams in one
    /// forward pass (otherwise it steps them one after another).
    pub fn batch_supported(&self) -> bool {
        self.decoder.batch_supported()
    }

    /// CortexCode patch P10: one step of every stream, their forward passes shared.
    ///
    /// The model's weights are read once per pass whatever the number of rows, so one pass that
    /// verifies every stream's input costs about what one stream's pass costs, until the rows make
    /// it compute-bound. The device's tree budget (the rows uzu tuned one verification pass for) is
    /// therefore shared: each stream drafts a tree of `budget / streams` nodes, and decodes without
    /// drafting when that leaves less than a root and one token.
    ///
    /// Streams run in lockstep. A stream holding accepted tokens hands one out per step without a
    /// pass; while any stream does, the others wait ([`BatchStep::Waiting`]), and the pass runs once
    /// every stream needs one. Those steps cost the CPU only, so all streams share every pass.
    pub async fn generate_batch(
        &self,
        streams: &mut [&mut LanguageModelStream<'_, B>],
    ) -> Vec<Result<BatchStep, LanguageModelStreamError<B>>> {
        let step = |token: Option<u64>| token.map_or(BatchStep::End, BatchStep::Token);
        let mut results: Vec<Option<Result<BatchStep, LanguageModelStreamError<B>>>> =
            streams.iter().map(|_| None).collect();

        if streams.len() < 2 || !self.batch_supported() {
            for (stream, result) in streams.iter_mut().zip(results.iter_mut()) {
                *result = Some(stream.generate().await.map(step));
            }
            return results.into_iter().map(|result| result.expect("every stream stepped")).collect();
        }

        // The last pass completed before it returned, so its outcome is read without a wait.
        for (stream, result) in streams.iter_mut().zip(results.iter_mut()) {
            if let Err(error) = stream.settle().await {
                *result = Some(Err(error));
            }
        }
        let ready: Vec<bool> = streams.iter().map(|stream| stream.has_token_without_pass()).collect();
        if ready.iter().zip(results.iter()).any(|(&ready, result)| ready && result.is_none()) {
            for ((stream, result), ready) in streams.iter_mut().zip(results.iter_mut()).zip(ready) {
                if result.is_some() {
                    continue;
                }
                *result = Some(if ready {
                    match stream.begin(None, true).await {
                        Ok(Begun::Done(token)) => Ok(step(token)),
                        // Ready streams hand out a token without a pass (see `has_token_without_pass`).
                        Ok(Begun::Pass(pass)) => stream.encode_single(pass).await.map(step),
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(BatchStep::Waiting)
                });
            }
            return results.into_iter().map(|result| result.expect("every stream stepped")).collect();
        }

        let live = results.iter().filter(|result| result.is_none()).count() as u32;
        let share = self.speculator.as_ref().map(|speculator| speculator.tree_budget() / live.max(1));
        let mut planned: Vec<(usize, PlannedPass<B>)> = Vec::new();
        for (index, (stream, result)) in streams.iter_mut().zip(results.iter_mut()).enumerate() {
            if result.is_some() {
                continue;
            }
            match stream.begin(share, true).await {
                Ok(Begun::Done(token)) => *result = Some(Ok(step(token))),
                Ok(Begun::Pass(pass)) => planned.push((index, pass)),
                Err(error) => *result = Some(Err(error)),
            }
        }

        match planned.len() {
            0 => {},
            1 => {
                let (index, pass) = planned.pop().expect("one planned pass");
                results[index] = Some(streams[index].encode_single(pass).await.map(step));
            },
            _ => {
                let indices: Vec<usize> = planned.iter().map(|(index, _)| *index).collect();
                let passes: Vec<PlannedPass<B>> = planned.into_iter().map(|(_, pass)| pass).collect();
                // The planned streams, mutably and in order.
                let mut members: Vec<&mut LanguageModelStream<'_, B>> = Vec::with_capacity(indices.len());
                let mut wanted = indices.iter().copied().peekable();
                for (index, stream) in streams.iter_mut().enumerate() {
                    if wanted.peek() == Some(&index) {
                        wanted.next();
                        members.push(&mut **stream);
                    }
                }
                match self.encode_shared_pass(&mut members, passes).await {
                    Ok(tokens) => {
                        for (index, token) in indices.into_iter().zip(tokens) {
                            results[index] = Some(token.map(step));
                        }
                    },
                    Err(message) => {
                        for index in indices {
                            results[index] = Some(Err(LanguageModelStreamError::Batch(message.clone())));
                        }
                    },
                }
            },
        }

        results.into_iter().map(|result| result.expect("every stream stepped")).collect()
    }

    /// CortexCode patch P10: one forward pass over the planned streams' inputs, then each stream's
    /// sampling and acceptance as [`LanguageModelStream::encode_single`] does them; the pass is
    /// waited for. Returns each stream's previous output token. A failure of the shared pass
    /// leaves every member's stream unusable, as a failed pass does to a single stream.
    async fn encode_shared_pass(
        &self,
        members: &mut [&mut LanguageModelStream<'_, B>],
        passes: Vec<PlannedPass<B>>,
    ) -> Result<Vec<Result<Option<u64>, LanguageModelStreamError<B>>>, String> {
        assert_eq!(members.len(), passes.len());
        let allocation_pool = self.engine.context.create_allocation_pool();
        let command_buffer = self
            .engine
            .context
            .create_command_buffer(Some("decode (shared pass)"), Some(allocation_pool.clone()))
            .map_err(|error| error.to_string())?;
        let mut command_buffer = record(command_buffer, None);
        let mut pending_acceptances = Vec::new();

        // Each member's input rows and topology (constant tokens: `begin` resolved them).
        let mut inputs = Vec::with_capacity(passes.len());
        for (stream, pass) in members.iter_mut().zip(passes) {
            assert!(pass.chain_copy.is_none(), "a shared pass takes constant input tokens");
            let PlannedPass {
                prev_output,
                pending,
                capture_span,
                input_trie,
                chain_copy: _,
                full_accept,
            } = pass;
            pending_acceptances.extend(pending);
            let flat = input_trie.linearize();
            let token_ids = flat.token_ids().map(|token_id| token_id as u32).collect::<Box<[u32]>>();
            let topology = BatchTopology::new(flat.token_subtrie_ranges().collect::<Box<[GpuTrieNode]>>(), full_accept);
            let context_length = stream.model_state.transformer_state.context_length();
            stream
                .model_state
                .transformer_state
                .prepare(context_length, topology.size())
                .map_err(|error| error.to_string())?;
            drop(flat);
            inputs.push((prev_output, capture_span, input_trie, full_accept, token_ids, topology));
        }
        command_buffer.debug_point(&format!(
            "shared decode pass: {} streams, {} rows",
            inputs.len(),
            inputs.iter().map(|input| input.5.size()).sum::<u32>()
        ));

        let all_ids: Vec<u32> = inputs.iter().flat_map(|input| input.4.iter().copied()).collect();
        let all_ids = command_buffer.allocate_constant_from_slice(&all_ids).map_err(|error| error.to_string())?;
        let hidden_feature_layer_indices =
            self.speculator.as_ref().map(|speculator| speculator.hidden_feature_layer_indices());
        let output = {
            let mut sequences = members
                .iter_mut()
                .zip(inputs.iter())
                .map(|(stream, input)| TransformerBatchSequence {
                    topology: &input.5,
                    state: &mut stream.model_state.transformer_state,
                })
                .collect::<Vec<_>>();
            self.decoder
                .encode_batch(&all_ids, &mut sequences, hidden_feature_layer_indices, &mut command_buffer)
                .map_err(|error| error.to_string())?
        };
        let total: u32 = inputs.iter().map(|input| input.5.size()).sum();
        let logits_row_bytes = output.logits.size() / total as usize;
        let mut final_hidden = output.final_hidden.map(|rows| rows.into_vec().into_iter().map(Some).collect::<Vec<_>>());
        let mut hidden_features =
            output.hidden_features.map(|rows| rows.into_vec().into_iter().map(Some).collect::<Vec<_>>());

        let mut outputs = Vec::with_capacity(inputs.len());
        let mut row = 0usize;
        for (member, (stream, input)) in members.iter_mut().zip(inputs.iter()).enumerate() {
            let (_, _, input_trie, full_accept, token_ids, topology) = input;
            let rows = topology.size() as usize;
            let flat = input_trie.linearize();
            let part_ids = command_buffer.allocate_constant_from_slice(token_ids).map_err(|error| error.to_string())?;
            // This stream's logits in a buffer of its own (the sampling kernels read from offset zero).
            let mut logits = command_buffer.allocate_scratch(rows * logits_row_bytes).map_err(|error| error.to_string())?;
            command_buffer
                .encode_copy((&output.logits).subrange(row * logits_row_bytes..(row + rows) * logits_row_bytes), &mut logits);
            row += rows;

            let bitmask = if let Some(grammar) = stream.options.grammar.as_mut() {
                let mut bitmasks = command_buffer
                    .allocate_constant(
                        flat.len() * self.vocab_size.div_ceil(DataType::U32.size_in_bits()) * DataType::U32.size_in_bytes(),
                    )
                    .map_err(|error| error.to_string())?;
                if flat.fill_bitmasks(bitmasks.as_slice_mut(), self.vocab_size, grammar) {
                    Some(bitmasks)
                } else {
                    None
                }
            } else {
                None
            };
            let seeds = if matches!(stream.options.sampling_method, SamplingMethod::Stochastic { .. }) {
                Some(
                    command_buffer
                        .allocate_constant_from_slice(&flat.token_seeds().collect::<Box<[u64]>>())
                        .map_err(|error| error.to_string())?,
                )
            } else {
                None
            };
            let output_tokens = self
                .sampling
                .encode(
                    &logits,
                    seeds.as_ref(),
                    bitmask.as_ref(),
                    stream.context_ring.as_ref(),
                    Some(&part_ids),
                    topology,
                    (0..topology.size()).into(),
                    &stream.options.sampling_method,
                    &mut command_buffer,
                )
                .map_err(|error| error.to_string())?;
            drop(seeds);
            drop(bitmask);
            drop(logits);

            let member_features = hidden_features.as_mut().and_then(|features| features[member].take());
            if *full_accept {
                stream
                    .model_state
                    .transformer_state
                    .encode_accept(&(0..topology.size()).collect::<Box<[u32]>>(), &mut command_buffer)
                    .map_err(|error| error.to_string())?;
                if let Some(speculator) = self.speculator.as_ref() {
                    let speculator_state = stream.model_state.speculator_state.as_mut().unwrap();
                    speculator
                        .encode_accept(
                            speculator_state,
                            member_features.as_ref().expect("hidden features for the speculator").iter(),
                            &(0..topology.size()).collect::<Box<[u32]>>(),
                            &mut command_buffer,
                        )
                        .map_err(|error| error.to_string())?;
                }
                if let Some(suffix_repetition_length) = stream.options.sampling_method.suffix_repetition_length() {
                    self.context_ring_update.encode(
                        &part_ids,
                        stream.context_ring.as_mut().unwrap(),
                        suffix_repetition_length,
                        topology.size(),
                        &mut command_buffer,
                    );
                }
            }
            drop(part_ids);
            outputs.push((
                output_tokens,
                if *full_accept {
                    None
                } else {
                    member_features
                },
                final_hidden.as_mut().and_then(|rows| rows[member].take()),
            ));
        }
        drop(all_ids);

        // The acceptances the members submitted while beginning ran first on the queue.
        for pending in pending_acceptances {
            wait(pending, None).await.map_err(|error| error.to_string())?;
        }
        wait(command_buffer.end_encoding().submit(), None).await.map_err(|error| error.to_string())?;

        let mut tokens = Vec::with_capacity(members.len());
        for ((stream, (prev_output, capture_span, input_trie, full_accept, _, topology)), (output_tokens, features, norm)) in
            members.iter_mut().zip(inputs).zip(outputs)
        {
            let mut prev_output = prev_output;
            let mut capture_span = capture_span;
            if full_accept {
                capture_span.take();
            }
            stream.allocation_pool = allocation_pool.clone();
            stream.metrics.num_decode_forward_passes += 1;
            stream.metrics.num_tokens_proposed += topology.size() as usize;
            if full_accept {
                stream.metrics.num_tokens_accepted += topology.size() as usize;
            }
            stream.decoding_state = DecodingState::ForwardPassPending(DecodingStatePending {
                input_trie,
                full_accept,
                // Completed above: the next step reads its outcome without a wait.
                pending: Box::new([]),
                capture_span,
                hidden_features: features,
                output_norm: norm,
                output_tokens: Arc::new(output_tokens),
                _input_token_source: None,
            });
            let token = prev_output
                .resolve(
                    &mut stream.model_state.tokens,
                    stream.options.timestamps.as_ref(),
                    &self.engine.context,
                    stream.options.grammar.as_mut(),
                )
                .await
                .map(|(token, _)| Some(token));
            tokens.push(token);
        }
        Ok(tokens)
    }
}

impl<'a, B: Backend> Drop for LanguageModelStream<'a, B> {
    fn drop(&mut self) {
        if !matches!(self.decoding_state, DecodingState::Closed) {
            let _ = block_on(self.close());
        }
    }
}
