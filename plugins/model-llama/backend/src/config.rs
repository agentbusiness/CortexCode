//! `[backend.config]` of plugin.toml (the frame config's `[plugin.config]` wins).

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "" = by the GPU's memory (`catalog::for_memory`).
    pub variant: String,
    /// Context tokens per slot.
    pub ctx: u32,
    pub parallel: u32,
    pub ubatch: u32,
    /// f16 | q8_0 (q8_0 needs flash attention; checked once, f16 otherwise).
    pub kv_type: String,
    pub idle_unload_s: u64,
    /// Tokens DFlash 2 drafts per step at most (`--spec-draft-n-max`).
    pub draft_max: u32,
    /// Use the Qwen of another app on this machine (a local member of the mesh) instead of loading
    /// one here. A machine without a usable GPU always uses a member's.
    pub borrow: bool,
    /// The llama-server binary (default: `llama/llama-server` beside this executable).
    pub server_bin: Option<String>,
    /// Where models live (default `~/.cortex-models`, the folder every Cortex app shares).
    pub models_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            variant: String::new(),
            ctx: 32768,
            parallel: 2,
            ubatch: 512,
            kv_type: "q8_0".into(),
            idle_unload_s: 600,
            draft_max: 7,
            borrow: true,
            server_bin: None,
            models_dir: None,
        }
    }
}

impl Config {
    pub fn from_value(value: &serde_json::Value) -> Self {
        if value.is_null() {
            return Self::default();
        }
        serde_json::from_value(value.clone()).unwrap_or_else(|error| {
            tracing::warn!(%error, "plugin config not understood; using the defaults");
            Self::default()
        })
    }
}
