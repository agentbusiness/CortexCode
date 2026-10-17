//! Image understanding (vendor patch P7, called P5 in `docs/VISION-RESEARCH.md`): the model's own
//! vision tower, fetched from the upstream Hugging Face repository, run on demand.
//!
//! - [`install`]: fetch only the vision tensors of the upstream checkpoint (HTTP range requests on
//!   one safetensors shard) into `<variant dir>/vision/`, with digests recorded and verified.
//! - [`preprocess`]: decoding, resizing to the patch grid within a pixel budget, normalization
//!   and patchification on the CPU, exactly as Qwen's image processor does.
//! - [`prompt`]: image content parts in a rendered prompt (the `<|image_pad|>` run per image), the
//!   identity of image tokens for prefix matching, and token counts from image dimensions alone.
//!
//! - [`worker`]: the encoder's own thread (own engine context and command queue), started with
//!   the tower when an image needs encoding and stopped after an idle period (see `engine.rs`).

pub mod install;
pub mod preprocess;
pub mod prompt;
pub mod worker;

use serde_json::Value;
use std::path::Path;

/// The directory under a variant's model directory that holds its vision tower.
pub const DIR: &str = "vision";
/// The vision tensors, as a standalone safetensors file (upstream names, `model.visual.*`).
pub const WEIGHTS: &str = "model.safetensors";
/// Default pixel budget per image (about 2,340 image tokens of 32×32 px).
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 2_400_000;
/// Default seconds without an image after which the tower is unloaded.
pub const DEFAULT_IDLE_UNLOAD_S: u64 = 300;

/// A small upstream file fetched whole (verified by its git blob id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmallFile {
    pub path: &'static str,
    pub bytes: u64,
    pub git_oid: &'static str,
}

/// Where a model's vision tower comes from: the upstream (non-Mirai) checkpoint at a pinned
/// revision, of which only the tensors under `prefix` are fetched, from one shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerSpec {
    pub repo: &'static str,
    pub revision: &'static str,
    /// The shard holding every vision tensor.
    pub shard: &'static str,
    pub shard_bytes: u64,
    /// The shard's SHA-256 as Hugging Face publishes it (`lfs.oid`); recorded in the manifest.
    pub shard_sha256: &'static str,
    /// Length of the shard's safetensors header (the 8-byte length prefix's value).
    pub shard_header_bytes: u64,
    pub prefix: &'static str,
    pub tensors: usize,
    pub parameters: u64,
    /// Bytes of tensor data fetched (one contiguous range of the shard).
    pub data_bytes: u64,
    /// SHA-256 of the assembled `vision/model.safetensors` (deterministic: sorted header).
    pub sha256: &'static str,
    /// `config.json` (vision and MRoPE configuration) and `preprocessor_config.json`.
    pub files: [SmallFile; 2],
}

/// Qwen3.8-27B's own vision tower (Qwen3.5-27B shape): 27 blocks, 1152 wide, 16 heads, merged
/// to the decoder width 5120. Read from shard 1 of `Qwen/Qwen3.8-27B` at `1d4bf0f`.
pub const QWEN38_27B_TOWER: TowerSpec = TowerSpec {
    repo: "Qwen/Qwen3.8-27B",
    revision: "1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0",
    shard: "model-00001-of-00018.safetensors",
    shard_bytes: 3_966_730_552,
    shard_sha256: "ba0ce20aae489ad196733da5064bcdf159a1fe84f53336648196e1ebb7751b1c",
    shard_header_bytes: 45_392,
    prefix: "model.visual.",
    tensors: 333,
    parameters: 460_730_096,
    data_bytes: 921_460_192,
    // Measured on the first install (2026-10-15): the assembled file, header sorted by name.
    sha256: "ad17b11caacd2f6518c47705ceab84540765a0296c2a43118d1b5b4931f421ee",
    files: [
        SmallFile { path: "config.json", bytes: 4_312, git_oid: "706cebd746c4b6f2b1d1f892630867acfdfd3df8" },
        SmallFile { path: "preprocessor_config.json", bytes: 390, git_oid: "2ea84a437d448ff71b08df68fdd949d5cc4ebb64" },
    ],
};

/// The vision tower's architecture and the language model's image tokens and MRoPE layout, read
/// from the upstream `config.json` (and `preprocessor_config.json`).
#[derive(Debug, Clone, PartialEq)]
pub struct VisionConfig {
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
    pub image_token_id: u64,
    pub vision_start_token_id: u64,
    pub vision_end_token_id: u64,
    /// Rotary pairs per axis (T, H, W) of the decoder's interleaved MRoPE.
    pub mrope_section: [u32; 3],
    pub image_mean: [f32; 3],
    pub image_std: [f32; 3],
    /// The processor's lower pixel bound (`size.shortest_edge`).
    pub min_pixels: u64,
}

impl VisionConfig {
    pub fn read(dir: &Path) -> Result<Self, String> {
        let read = |name: &str| -> Result<Value, String> {
            let path = dir.join(name);
            let text = std::fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            serde_json::from_slice(&text).map_err(|error| format!("cannot parse {}: {error}", path.display()))
        };
        Self::from_values(&read("config.json")?, &read("preprocessor_config.json")?)
    }

    pub fn from_values(config: &Value, preprocessor: &Value) -> Result<Self, String> {
        let vision = &config["vision_config"];
        let number = |value: &Value, name: &str| -> Result<u64, String> {
            value[name].as_u64().ok_or_else(|| format!("vision config has no `{name}`"))
        };
        let small = |value: &Value, name: &str| -> Result<u32, String> {
            number(value, name).and_then(|value| u32::try_from(value).map_err(|_| format!("`{name}` is out of range")))
        };
        if vision["hidden_act"].as_str() != Some("gelu_pytorch_tanh") {
            return Err(format!("unsupported vision activation {}", vision["hidden_act"]));
        }
        if vision["deepstack_visual_indexes"].as_array().is_some_and(|indexes| !indexes.is_empty()) {
            return Err("vision towers with DeepStack layers are not supported".into());
        }
        let rope = &config["text_config"]["rope_parameters"];
        let section = rope["mrope_section"].as_array().ok_or("text config has no mrope_section")?;
        if section.len() != 3 || rope["mrope_interleaved"].as_bool() != Some(true) {
            return Err("only interleaved three-axis MRoPE is supported".into());
        }
        let mut mrope_section = [0u32; 3];
        for (slot, value) in mrope_section.iter_mut().zip(section) {
            *slot = value.as_u64().and_then(|value| u32::try_from(value).ok()).ok_or("bad mrope_section")?;
        }
        let triple = |name: &str| -> Result<[f32; 3], String> {
            let values = preprocessor[name].as_array().filter(|values| values.len() == 3).ok_or_else(|| format!("preprocessor has no `{name}`"))?;
            let mut out = [0f32; 3];
            for (slot, value) in out.iter_mut().zip(values) {
                *slot = value.as_f64().ok_or_else(|| format!("bad `{name}`"))? as f32;
            }
            Ok(out)
        };
        let config = Self {
            depth: small(vision, "depth")?,
            hidden_size: small(vision, "hidden_size")?,
            intermediate_size: small(vision, "intermediate_size")?,
            num_heads: small(vision, "num_heads")?,
            patch_size: small(vision, "patch_size")?,
            temporal_patch_size: small(vision, "temporal_patch_size")?,
            spatial_merge_size: small(vision, "spatial_merge_size")?,
            num_position_embeddings: small(vision, "num_position_embeddings")?,
            out_hidden_size: small(vision, "out_hidden_size")?,
            in_channels: small(vision, "in_channels")?,
            image_token_id: number(config, "image_token_id")?,
            vision_start_token_id: number(config, "vision_start_token_id")?,
            vision_end_token_id: number(config, "vision_end_token_id")?,
            mrope_section,
            image_mean: triple("image_mean")?,
            image_std: triple("image_std")?,
            min_pixels: number(&preprocessor["size"], "shortest_edge")?,
        };
        if config.patch_size != preprocessor["patch_size"].as_u64().unwrap_or(0) as u32
            || config.spatial_merge_size != preprocessor["merge_size"].as_u64().unwrap_or(0) as u32
        {
            return Err("the processor's patch geometry differs from the vision config".into());
        }
        Ok(config)
    }

    /// Pixels per side of one language-model token (patch size × spatial merge).
    pub fn factor(&self) -> u32 {
        self.patch_size * self.spatial_merge_size
    }

    /// The engine's view of this configuration.
    pub fn engine(&self) -> uzu_engine::engine::vision::VisionModelConfig {
        uzu_engine::engine::vision::VisionModelConfig {
            depth: self.depth,
            hidden_size: self.hidden_size,
            intermediate_size: self.intermediate_size,
            num_heads: self.num_heads,
            patch_size: self.patch_size,
            temporal_patch_size: self.temporal_patch_size,
            spatial_merge_size: self.spatial_merge_size,
            num_position_embeddings: self.num_position_embeddings,
            out_hidden_size: self.out_hidden_size,
            in_channels: self.in_channels,
            layer_norm_epsilon: 1e-6,
            rope_theta: 10_000.0,
        }
    }
}
