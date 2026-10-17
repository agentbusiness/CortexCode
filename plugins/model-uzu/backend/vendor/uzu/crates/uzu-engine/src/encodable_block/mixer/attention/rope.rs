use crate::{
    backends::common::{Backend, CommandBuffer, CommandBufferEncoding},
    config::rope::AnyRoPEConfig,
};

pub struct PrecalculatedRoPE<B: Backend> {
    pub cosines: B::ConstantBuffer,
    pub sines: B::ConstantBuffer,
    pub dim: u32,
}

impl<B: Backend> PrecalculatedRoPE<B> {
    pub fn precalculate(
        rope_config: &AnyRoPEConfig,
        token_positions: &[u32],
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<Self, B::Error> {
        // CortexCode patch P7: the tables come from `rope_tables` (the former body, unchanged).
        let (cosines, sines) = rope_tables(rope_config, token_positions);
        Ok(Self {
            cosines: command_buffer.allocate_constant_from_slice(&cosines)?,
            sines: command_buffer.allocate_constant_from_slice(&sines)?,
            dim: *rope_config.head_dim(),
        })
    }
}

/// Cosine and sine tables (`[token, head_dim]`, rotate-half layout) of 1-D positions.
pub fn rope_tables(
    rope_config: &AnyRoPEConfig,
    token_positions: &[u32],
) -> (Vec<f32>, Vec<f32>) {
    let head_dim = *rope_config.head_dim();
    assert!(head_dim > 0 && head_dim.is_multiple_of(2), "RoPE head_dim must be positive and even");
    let half_dim = head_dim / 2;
    let attention_scaling_factor = attention_scaling_factor(rope_config);

    let element_count = token_positions.len() * head_dim as usize;
    let half_offset = half_dim as usize;
    let mut sines = vec![0.0; element_count];
    let mut cosines = vec![0.0; element_count];
    for pair_index in 0..half_dim {
        let inverse_frequency = inverse_frequency(rope_config, pair_index);
        for (token_index, token_position) in token_positions.iter().enumerate() {
            let embedding = *token_position as f32 * inverse_frequency;
            let sine = embedding.sin() * attention_scaling_factor;
            let cosine = embedding.cos() * attention_scaling_factor;
            let pair_offset = token_index * head_dim as usize + pair_index as usize;
            sines[pair_offset] = sine;
            sines[pair_offset + half_offset] = sine;
            cosines[pair_offset] = cosine;
            cosines[pair_offset + half_offset] = cosine;
        }
    }
    (cosines, sines)
}

/// The inverse frequency of rotary pair `pair_index` (shared by the 1-D and multi-axis tables, so a
/// multi-axis table whose axes agree is bit-identical to the 1-D one).
fn inverse_frequency(
    rope_config: &AnyRoPEConfig,
    pair_index: u32,
) -> f32 {
    let head_dim = *rope_config.head_dim();
    let channel_index = pair_index * 2;
    let inverse_frequency = 1.0 / rope_config.base().powf(channel_index as f32 / head_dim as f32);
    let inverse_frequency = match rope_config {
        AnyRoPEConfig::UnscaledRoPEConfig(_) => inverse_frequency,
        AnyRoPEConfig::LinearScalingRoPEConfig(config) => inverse_frequency / config.scaling_factor,
        AnyRoPEConfig::LlamaRoPEConfig(config) => {
            let low_frequency_wavelength = config.original_context_length as f32 / config.low_frequency_factor;
            let high_frequency_wavelength =
                config.original_context_length as f32 / config.high_frequency_factor;
            let wavelength = 2.0 * std::f32::consts::PI / inverse_frequency;
            let scaled_frequency = inverse_frequency / config.scaling_factor;

            if wavelength < high_frequency_wavelength {
                inverse_frequency
            } else if wavelength > low_frequency_wavelength {
                scaled_frequency
            } else {
                let smoothing_factor =
                    config.original_context_length as f32 / wavelength - config.low_frequency_factor;
                let smoothing_factor =
                    smoothing_factor / (config.high_frequency_factor - config.low_frequency_factor);
                smoothing_factor * inverse_frequency + (1.0 - smoothing_factor) * scaled_frequency
            }
        },
        AnyRoPEConfig::YARNRoPEConfig(config) => {
            let dim = config.head_dim as f64;
            let base = config.base as f64;
            let original_context_length = config.original_context_length as f64;
            let beta_fast = config.beta_fast as f64;
            let beta_slow = config.beta_slow as f64;
            let mut low = dim * (original_context_length / (beta_fast * 2.0 * std::f64::consts::PI)).ln()
                / (2.0 * base.ln());
            let mut high = dim * (original_context_length / (beta_slow * 2.0 * std::f64::consts::PI)).ln()
                / (2.0 * base.ln());
            if config.truncate {
                low = low.floor();
                high = high.ceil();
            }
            let low = low.max(0.0) as f32;
            let mut high = high.min((config.head_dim - 1) as f64) as f32;
            if low == high {
                high += 0.001;
            }
            let ramp = ((pair_index as f32 - low) / (high - low)).clamp(0.0, 1.0);
            let smoothing_factor = 1.0 - ramp;
            let scaled_frequency = inverse_frequency / config.scaling_factor;
            scaled_frequency * (1.0 - smoothing_factor) + inverse_frequency * smoothing_factor
        },
        AnyRoPEConfig::LongRoPEConfig(config) => {
            let factors = if config.max_sequence_length > config.original_context_length {
                &config.long_factor
            } else {
                &config.short_factor
            };
            inverse_frequency / factors[pair_index as usize]
        },
    };
    inverse_frequency
}

fn attention_scaling_factor(rope_config: &AnyRoPEConfig) -> f32 {
    match rope_config {
        AnyRoPEConfig::YARNRoPEConfig(config) => 0.1 * config.scaling_factor.ln() + 1.0,
        AnyRoPEConfig::LongRoPEConfig(config) if config.scaling_factor > 1.0 => {
            (1.0 + config.scaling_factor.ln() / (config.original_context_length as f32).ln()).sqrt()
        },
        _ => 1.0,
    }
}

/// CortexCode patch P7: which position axis (0 = T, 1 = H, 2 = W) rotary pair `pair_index` reads in
/// interleaved multi-axis RoPE (Qwen3-VL/3.5 `apply_interleaved_mrope`): pairs `1, 4, 7, ...`
/// below `3 * section[1]` use H, pairs `2, 5, 8, ...` below `3 * section[2]` use W, the rest T.
pub fn mrope_axis(
    pair_index: u32,
    section: [u32; 3],
) -> usize {
    if pair_index % 3 == 1 && pair_index < 3 * section[1] {
        1
    } else if pair_index % 3 == 2 && pair_index < 3 * section[2] {
        2
    } else {
        0
    }
}

/// CortexCode patch P7: cosine and sine tables (`[token, head_dim]`, rotate-half layout) for
/// interleaved multi-axis RoPE: every token has a (T, H, W) position and each rotary pair reads
/// the axis [`mrope_axis`] assigns it. Rows whose three positions agree equal the 1-D table bit for bit.
pub fn mrope_tables(
    rope_config: &AnyRoPEConfig,
    positions: &[[u32; 3]],
    section: [u32; 3],
) -> (Vec<f32>, Vec<f32>) {
    let head_dim = *rope_config.head_dim();
    assert!(head_dim > 0 && head_dim.is_multiple_of(2), "RoPE head_dim must be positive and even");
    let half_dim = head_dim / 2;
    let attention_scaling_factor = attention_scaling_factor(rope_config);
    let element_count = positions.len() * head_dim as usize;
    let half_offset = half_dim as usize;
    let mut sines = vec![0.0; element_count];
    let mut cosines = vec![0.0; element_count];
    for pair_index in 0..half_dim {
        let inverse_frequency = inverse_frequency(rope_config, pair_index);
        let axis = mrope_axis(pair_index, section);
        for (token_index, position) in positions.iter().enumerate() {
            let embedding = position[axis] as f32 * inverse_frequency;
            let sine = embedding.sin() * attention_scaling_factor;
            let cosine = embedding.cos() * attention_scaling_factor;
            let pair_offset = token_index * head_dim as usize + pair_index as usize;
            sines[pair_offset] = sine;
            sines[pair_offset + half_offset] = sine;
            cosines[pair_offset] = cosine;
            cosines[pair_offset + half_offset] = cosine;
        }
    }
    (cosines, sines)
}

impl<B: Backend> PrecalculatedRoPE<B> {
    /// CortexCode patch P7: tables for explicit (T, H, W) positions (see [`mrope_tables`]).
    pub fn precalculate_mrope(
        rope_config: &AnyRoPEConfig,
        positions: &[[u32; 3]],
        section: [u32; 3],
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<Self, B::Error> {
        let (cosines, sines) = mrope_tables(rope_config, positions, section);
        Self::from_tables(&cosines, &sines, *rope_config.head_dim(), command_buffer)
    }

    /// CortexCode patch P7: tables computed by the caller (`[token, dim]` each, f32).
    pub fn from_tables(
        cosines: &[f32],
        sines: &[f32],
        dim: u32,
        command_buffer: &mut <B::CommandBuffer as CommandBuffer>::Encoding,
    ) -> Result<Self, B::Error> {
        assert_eq!(cosines.len(), sines.len(), "RoPE tables differ in length");
        Ok(Self {
            cosines: command_buffer.allocate_constant_from_slice(cosines)?,
            sines: command_buffer.allocate_constant_from_slice(sines)?,
            dim,
        })
    }
}
