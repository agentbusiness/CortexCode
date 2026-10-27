//! Machine facts measured at the moment of a load: physical RAM, the RAM the kernel reports
//! available, the chip and core counts. The GPU working set comes from the engine itself
//! (`crate::engine`).

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Machine {
    pub chip: String,
    pub physical_bytes: u64,
    /// What the kernel can hand out now without memory pressure.
    pub available_bytes: u64,
    pub cpu_cores: u32,
    pub performance_cores: u32,
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut value = 0u64;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: the key is NUL-terminated, the output points to a writable u64 whose size is passed,
    // and no new value is supplied (read-only query).
    let status =
        unsafe { libc::sysctlbyname(name.as_ptr(), (&raw mut value).cast(), &mut length, std::ptr::null_mut(), 0) };
    if status != 0 {
        return None;
    }
    match length {
        8 => Some(value),
        // A 32-bit value lands in the low half on little-endian machines.
        4 => Some(value & 0xffff_ffff),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut length = 0usize;
    // SAFETY: size query with a null output buffer.
    let status =
        unsafe { libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut length, std::ptr::null_mut(), 0) };
    if status != 0 || length == 0 {
        return None;
    }
    let mut buffer = vec![0u8; length];
    // SAFETY: the buffer is `length` bytes as the previous query reported.
    let status =
        unsafe { libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut length, std::ptr::null_mut(), 0) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

impl Machine {
    #[cfg(target_os = "macos")]
    pub fn measure() -> Self {
        let physical_bytes = sysctl_u64(c"hw.memsize").unwrap_or(0);
        // The kernel's own availability level (what `memory_pressure` prints as "System-wide
        // memory free percentage"): it counts reclaimable file cache, which raw free pages do
        // not, and it is the figure the memory-pressure response acts on.
        let level = sysctl_u64(c"kern.memorystatus_level").unwrap_or(0).min(100);
        Self {
            chip: sysctl_string(c"machdep.cpu.brand_string").unwrap_or_default(),
            physical_bytes,
            available_bytes: physical_bytes / 100 * level,
            cpu_cores: sysctl_u64(c"hw.ncpu").and_then(|cores| u32::try_from(cores).ok()).unwrap_or(0),
            performance_cores: sysctl_u64(c"hw.perflevel0.physicalcpu")
                .and_then(|cores| u32::try_from(cores).ok())
                .unwrap_or(0),
        }
    }

    /// /proc/meminfo (`MemTotal`, and `MemAvailable`, which counts reclaimable cache) and the
    /// CPU's model name; the cores are the physical ones from /sys.
    #[cfg(not(target_os = "macos"))]
    pub fn measure() -> Self {
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let field = |key: &str| {
            meminfo
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                .map_or(0, |kib| kib * 1024)
        };
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let chip = cpuinfo
            .lines()
            .find_map(|line| line.split_once(':').filter(|(key, _)| key.trim() == "model name").map(|(_, value)| value.trim().to_owned()))
            .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
        Self {
            chip,
            physical_bytes: field("MemTotal"),
            available_bytes: field("MemAvailable"),
            cpu_cores: physical_cores(),
            performance_cores: 0,
        }
    }
}

/// The CPU threads the engine decodes with: on macOS the GPU does the work and llama.cpp's own
/// default stands (0); on Linux the CPU is the engine, one thread per physical core.
pub fn engine_threads() -> i32 {
    if cfg!(target_os = "macos") { 0 } else { i32::try_from(physical_cores()).unwrap_or(0) }
}

/// Distinct (package, core) pairs in /sys, else the logical CPU count.
#[cfg(not(target_os = "macos"))]
fn physical_cores() -> u32 {
    let mut cores = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.strip_prefix("cpu").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())) {
                continue;
            }
            let topology = entry.path().join("topology");
            let read = |file: &str| std::fs::read_to_string(topology.join(file)).ok().map(|text| text.trim().to_owned());
            if let (Some(package), Some(core)) = (read("physical_package_id"), read("core_id")) {
                cores.insert((package, core));
            }
        }
    }
    if cores.is_empty() {
        std::thread::available_parallelism().map_or(0, |n| u32::try_from(n.get()).unwrap_or(0))
    } else {
        u32::try_from(cores.len()).unwrap_or(0)
    }
}

#[cfg(target_os = "macos")]
fn physical_cores() -> u32 {
    0
}
