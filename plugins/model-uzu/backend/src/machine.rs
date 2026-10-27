//! Machine facts measured at the moment of a load: physical RAM, the RAM the kernel reports
//! available, the chip and core counts. The GPU working set comes from the engine itself
//! ([`crate::engine::devices`]).

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Machine {
    pub chip: String,
    pub physical_bytes: u64,
    /// The kernel's limit on all wired memory together (`vm.global_user_wire_limit`); zero when
    /// unknown.
    pub wire_limit_bytes: u64,
    /// What the kernel can hand out now without memory pressure.
    pub available_bytes: u64,
    pub cpu_cores: u32,
    pub performance_cores: u32,
}

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
    pub fn measure() -> Self {
        let physical_bytes = sysctl_u64(c"hw.memsize").unwrap_or(0);
        // The kernel's own availability level (what `memory_pressure` prints as "System-wide
        // memory free percentage"): it counts reclaimable file cache, which raw free pages do
        // not, and it is the figure the memory-pressure response acts on.
        let level = sysctl_u64(c"kern.memorystatus_level").unwrap_or(0).min(100);
        Self {
            chip: sysctl_string(c"machdep.cpu.brand_string").unwrap_or_default(),
            physical_bytes,
            wire_limit_bytes: sysctl_u64(c"vm.global_user_wire_limit").unwrap_or(0),
            available_bytes: physical_bytes / 100 * level,
            cpu_cores: sysctl_u64(c"hw.ncpu").and_then(|cores| u32::try_from(cores).ok()).unwrap_or(0),
            performance_cores: sysctl_u64(c"hw.perflevel0.physicalcpu")
                .and_then(|cores| u32::try_from(cores).ok())
                .unwrap_or(0),
        }
    }
}
