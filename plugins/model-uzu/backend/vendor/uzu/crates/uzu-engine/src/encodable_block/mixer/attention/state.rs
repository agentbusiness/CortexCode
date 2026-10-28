use crate::{
    backends::common::{
        Backend, BufferMut, BufferRef, CommandBuffer, CommandBufferEncoding, Context,
        gpu_types::{kv_cache_update::Copy, ring::RingParams},
        kernel::{AttentionKernel, KVFormat},
    },
    encodable_block::mixer::{
        MixerSnapshot, MixerSnapshotError, MixerState, SnapshotMode,
        attention::{Attention, kv_storage::KVStorage},
    },
};

pub const ATTENTION_SUFFIX_CAPACITY: u32 = 1024; // TODO: remove hardcoded suffix capacity

#[derive(Clone, Copy)]
pub struct KVCacheView {
    prefix_len: u32,
    ring: Option<RingParams>,
}

impl KVCacheView {
    pub fn full(prefix_len: u32) -> Self {
        Self {
            prefix_len,
            ring: None,
        }
    }

    pub fn ring(
        prefix_len: u32,
        offset: u32,
    ) -> Self {
        Self {
            prefix_len,
            ring: Some(RingParams {
                ring_offset: offset,
                ring_length: prefix_len,
            }),
        }
    }

    pub fn prefix_len(self) -> u32 {
        self.prefix_len
    }

    pub fn ring_params(self) -> Option<RingParams> {
        self.ring
    }
}

enum KVCacheState {
    Full {
        length: u32,
    },
    Ring {
        offset: u32,
        length: u32,
        capacity: u32,
    },
}

impl KVCacheState {
    fn full() -> Self {
        Self::Full {
            length: 0,
        }
    }

    fn ring(capacity: u32) -> Self {
        assert!(capacity > 0, "zero ring capacity");
        Self::Ring {
            offset: 0,
            length: 0,
            capacity,
        }
    }

    pub fn view(&self) -> KVCacheView {
        match self {
            Self::Full {
                length,
            } => KVCacheView::full(*length),
            Self::Ring {
                offset,
                length,
                capacity: _,
            } => KVCacheView::ring(*length, *offset),
        }
    }

    fn required_prefix_len(
        &self,
        context_length: u32,
    ) -> u32 {
        match self {
            Self::Full {
                ..
            } => context_length,
            Self::Ring {
                capacity,
                ..
            } => context_length.min(*capacity),
        }
    }

    fn accept(
        &mut self,
        accepted_indices: &[u32],
    ) -> Vec<Copy> {
        assert!(accepted_indices.is_sorted_by(|a, b| a < b), "unsorted accept");
        let suffix_base = self.view().prefix_len();

        match self {
            Self::Full {
                length,
            } => {
                let mut copies = Vec::with_capacity(accepted_indices.len());
                for (destination_index, accepted_index) in accepted_indices.iter().copied().enumerate() {
                    let source = suffix_base + accepted_index;
                    let destination = suffix_base + destination_index as u32;
                    if source != destination {
                        copies.push(Copy {
                            source,
                            destination,
                        });
                    }
                }
                *length += accepted_indices.len() as u32;
                copies
            },
            Self::Ring {
                offset,
                length,
                capacity,
            } => {
                let count = accepted_indices.len() as u32;
                let mut copies = Vec::with_capacity(accepted_indices.len().min(*capacity as usize));
                for (index, accepted_index) in accepted_indices.iter().copied().enumerate() {
                    let source = suffix_base + accepted_index;
                    let destination = (*offset + *length) % *capacity;
                    // A copy is dead when a later accept in the same batch overwrites its
                    // destination; emitting it would alias in a single kernel dispatch.
                    if index as u32 + *capacity >= count && source != destination {
                        copies.push(Copy {
                            source,
                            destination,
                        });
                    }

                    if length < capacity {
                        *length += 1;
                    } else {
                        *offset = (*offset + 1) % *capacity;
                    }
                }
                copies
            },
        }
    }
}

pub struct AttentionState<B: Backend> {
    cache: KVCacheState,
    pub kv: KVStorage<B>,
}

impl<B: Backend> AttentionState<B> {
    pub fn view(&self) -> KVCacheView {
        self.cache.view()
    }

    pub fn create_empty(
        attention: &Attention<B>,
        max_context_length: Option<u32>,
        context: &B::Context,
    ) -> Result<Self, B::Error> {
        if let Some(max_context_length) = max_context_length {
            assert!(
                attention.max_rope_length.is_none_or(|max_rope_length| max_context_length <= max_rope_length),
                "context exceeds RoPE length"
            );
        }

        let max_prefix_elements = attention.ring_capacity.unwrap_or_else(|| {
            max_context_length
                .or(attention.max_rope_length)
                .expect("Cannot create full attention state with unlimited length for with no RoPE")
        });

        let cache = attention.ring_capacity.map_or_else(KVCacheState::full, KVCacheState::ring);

        let format = if attention.kernel.supports_int8() {
            KVFormat::INT8
        } else {
            KVFormat::BF16
        };
        let kv = KVStorage::create_empty(
            format,
            attention.num_kv_heads.unwrap(),
            attention.head_dim,
            max_prefix_elements + ATTENTION_SUFFIX_CAPACITY,
            attention.data_type,
            context,
        )?;

        Ok(Self {
            cache,
            kv,
        })
    }
}

impl<B: Backend> MixerState<B> for AttentionState<B> {
    fn prepare(
        &mut self,
        context_length: u32,
        suffix_length: u32,
    ) -> Result<(), B::Error> {
        assert!(suffix_length <= ATTENTION_SUFFIX_CAPACITY, "suffix exceeds capacity");

        self.kv.prepare(self.cache.required_prefix_len(context_length) + suffix_length)
    }

    fn encode_accept(
        &mut self,
        accepted_indices: &[u32],
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<(), B::Error> {
        let copies = self.cache.accept(accepted_indices);

        if !copies.is_empty() {
            self.kv.encode_accept(&copies, command_buffer);
        }

        Ok(())
    }

    fn kv_bytes_per_token(&self) -> u64 {
        // CortexCode patch P5: int8 is laid out by blocks of keys, so measure one whole block.
        let block = crate::backends::common::gpu_types::kv_layout::TILE_KEYS;
        let (_, buffers) = self.kv.row_buffers(block);
        buffers.iter().map(|(_, bytes)| *bytes as u64).sum::<u64>() / u64::from(block)
    }

    fn encode_snapshot(
        &self,
        mode: SnapshotMode,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<MixerSnapshot<B>, MixerSnapshotError<B>> {
        // CortexCode patch P1. Scalars: [kind (0 full, 1 ring), length, offset, capacity, rows copied,
        // KV format (0 bf16, 1 int8)].
        let (kind, length, offset, capacity) = match self.cache {
            KVCacheState::Full {
                length,
            } => (0, length, 0, 0),
            KVCacheState::Ring {
                offset,
                length,
                capacity,
            } => (1, length, offset, capacity),
        };
        // A ring overwrites its own rows, so even a checkpoint must copy it; a full cache only
        // appends, so a checkpoint records its length.
        let rows = match (mode, kind) {
            (SnapshotMode::Checkpoint, 0) => 0,
            // Full: rows [0, length); ring: its `length` (<= capacity) occupied slots.
            _ => length,
        };
        let (format, sources) = self.kv.row_buffers(rows);
        let mut buffers = Vec::new();
        if rows > 0 {
            for (source, bytes) in sources {
                let mut copy =
                    command_buffer.context().create_buffer(bytes.max(1)).map_err(MixerSnapshotError::Backend)?;
                command_buffer.encode_copy(source.subrange(..bytes), (&mut copy).subrange_mut(..bytes));
                buffers.push(copy);
            }
        }
        Ok(MixerSnapshot {
            scalars: Box::new([kind, u64::from(length), u64::from(offset), u64::from(capacity), u64::from(rows), format]),
            buffers: buffers.into_boxed_slice(),
        })
    }

    fn encode_restore(
        &mut self,
        snapshot: &MixerSnapshot<B>,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<(), MixerSnapshotError<B>> {
        // A snapshot of another KV format (or of a build before int8 KV, which had five scalars)
        // does not restore: the conversation starts cold.
        let [kind, length, offset, capacity, rows, format] = *snapshot.scalars else {
            return Err(MixerSnapshotError::Mismatch("attention snapshot is of another KV layout".into()));
        };
        let (length, offset, capacity, rows) = (length as u32, offset as u32, capacity as u32, rows as u32);
        let cache = match (kind, &self.cache) {
            (0, KVCacheState::Full { .. }) => KVCacheState::Full {
                length,
            },
            (1, KVCacheState::Ring {
                capacity: own_capacity,
                ..
            }) if *own_capacity == capacity => KVCacheState::Ring {
                offset,
                length,
                capacity,
            },
            _ => return Err(MixerSnapshotError::Mismatch("attention cache kind or capacity differs".into())),
        };
        if rows > 0 {
            if self.kv.row_buffers(rows).0 != format {
                return Err(MixerSnapshotError::Mismatch("attention snapshot is of another KV format".into()));
            }
            self.kv.prepare(rows).map_err(MixerSnapshotError::Backend)?;
            let (_, targets) = self.kv.row_buffers_mut(rows);
            if targets.len() != snapshot.buffers.len() {
                return Err(MixerSnapshotError::Mismatch("attention snapshot buffers are missing".into()));
            }
            for ((target, bytes), source) in targets.into_iter().zip(snapshot.buffers.iter()) {
                if bytes > target.size() {
                    return Err(MixerSnapshotError::Mismatch(format!("{rows} rows exceed this cache")));
                }
                if source.size() < bytes {
                    return Err(MixerSnapshotError::Mismatch("attention snapshot buffers are short".into()));
                }
                command_buffer.encode_copy(source.subrange(..bytes), target.subrange_mut(..bytes));
            }
        } else if kind == 1 && length > 0 {
            return Err(MixerSnapshotError::Mismatch("a ring cache cannot be restored without its rows".into()));
        }
        self.cache = cache;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../../unit/encodable_block/kv_cache_state_test.rs"]
mod tests;
