//! The GPU Qwen runs on, as llama-server itself finds it (`--list-devices`): CUDA or Vulkan (the
//! backends llama.cpp loads only where their driver is), with at least 16 GB. A machine without
//! one runs no model: it loads nothing, lends nothing, and uses a member's Qwen through the mesh.

use std::path::Path;

/// A GPU smaller than this does not hold the smallest variant with its drafter and cache.
const MIN_MEMORY: u64 = crate::catalog::MIN_GPU_BYTES;

#[derive(Debug, Clone)]
pub struct Gpu {
    /// llama.cpp's device name (`CUDA0`, `Vulkan0`), passed as `--device`.
    pub device: String,
    pub description: String,
    /// `cuda` or `vulkan`.
    pub backend: &'static str,
    pub memory_bytes: u64,
}

/// The best usable GPU: CUDA before Vulkan (an NVIDIA card shows under both), the one with the
/// most memory within a backend. None when llama-server lists none with enough memory.
pub fn find(server: &Path) -> Option<Gpu> {
    let output = std::process::Command::new(server).arg("--list-devices").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<Gpu> = text.lines().filter_map(parse).filter(|gpu| gpu.memory_bytes >= MIN_MEMORY).collect();
    found.sort_by_key(|gpu| (rank(gpu.backend), std::cmp::Reverse(gpu.memory_bytes)));
    let gpu = found.into_iter().next();
    match &gpu {
        Some(gpu) => tracing::info!(device = %gpu.device, gpu = %gpu.description, memory_gb = gpu.memory_bytes >> 30, "the GPU Qwen runs on"),
        None => tracing::info!(devices = %text.trim(), "no usable GPU: this machine uses a member's model through the mesh"),
    }
    gpu
}

fn rank(backend: &str) -> u8 {
    if backend == "cuda" { 0 } else { 1 }
}

/// `  CUDA0: NVIDIA GeForce RTX 4090 (24564 MiB, 23950 MiB free)`.
fn parse(line: &str) -> Option<Gpu> {
    let (device, rest) = line.trim().split_once(": ")?;
    let backend = if device.starts_with("CUDA") {
        "cuda"
    } else if device.starts_with("Vulkan") {
        "vulkan"
    } else {
        return None;
    };
    let (description, sizes) = rest.rsplit_once(" (")?;
    let mib: u64 = sizes.split_once(" MiB")?.0.trim().parse().ok()?;
    Some(Gpu { device: device.to_owned(), description: description.to_owned(), backend, memory_bytes: mib << 20 })
}
