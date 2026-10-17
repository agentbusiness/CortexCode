//! Machine facts measured once at startup: sysctl (chip, memory, cores), IORegistry (GPU core
//! count of the AGX accelerator) and Metal (`recommendedMaxWorkingSetSize`).

use cc_protocol::contracts::frame::Machine;
use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use serde_json::json;
use std::ffi::{CString, c_char, c_void};

type IoObject = u32;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoObject;
    fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CFStringRef, allocator: *const c_void, options: u32) -> CFTypeRef;
    fn IOObjectRelease(object: IoObject) -> i32;
}

pub fn measure() -> Machine {
    let started = std::time::Instant::now();
    let chip = sysctl_string("machdep.cpu.brand_string").unwrap_or_default();
    let (gpu_working_set_bytes, metal_device, unified_memory) = metal_facts();
    let gpu_cores = gpu_core_count();
    let machine = Machine {
        chip,
        memory_bytes: sysctl_u64("hw.memsize").unwrap_or(0),
        cpu_cores: sysctl_u64("hw.physicalcpu").unwrap_or(0) as u32,
        performance_cores: sysctl_u64("hw.perflevel0.physicalcpu").unwrap_or(0) as u32,
        gpu_cores,
        gpu_working_set_bytes,
        os_version: sysctl_string("kern.osproductversion").unwrap_or_default(),
        extra: json!({
            "model": sysctl_string("hw.model"),
            "logical_cpus": sysctl_u64("hw.logicalcpu"),
            "perf_levels": sysctl_u64("hw.nperflevels"),
            "perflevel0_name": sysctl_string("hw.perflevel0.name"),
            "perflevel1_name": sysctl_string("hw.perflevel1.name"),
            "perflevel1_cores": sysctl_u64("hw.perflevel1.physicalcpu"),
            "os_build": sysctl_string("kern.osversion"),
            "metal_device": metal_device,
            "unified_memory": unified_memory,
            "measure_ms": started.elapsed().as_millis() as u64,
        }),
    };
    tracing::info!(
        chip = %machine.chip,
        memory_gb = machine.memory_bytes / (1 << 30),
        cpu = machine.cpu_cores,
        gpu = machine.gpu_cores,
        gpu_working_set_gb = machine.gpu_working_set_bytes as f64 / f64::from(1u32 << 30),
        "machine measured"
    );
    machine
}

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut size: libc::size_t = 0;
    // SAFETY: size query with a null buffer, then a fill of exactly that size.
    unsafe {
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 || size == 0 {
            return None;
        }
        let mut buffer = vec![0_u8; size];
        if libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buffer.truncate(size);
        Some(buffer)
    }
}

fn sysctl_string(name: &str) -> Option<String> {
    let bytes = sysctl_raw(name)?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).trim().to_owned())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let bytes = sysctl_raw(name)?;
    match bytes.len() {
        4 => Some(u64::from(u32::from_ne_bytes(bytes[..4].try_into().ok()?))),
        8 => Some(u64::from_ne_bytes(bytes[..8].try_into().ok()?)),
        _ => None,
    }
}

/// `gpu-core-count` of the AGX accelerator in the IORegistry; `system_profiler` as fallback.
fn gpu_core_count() -> u32 {
    if let Some(count) = ioreg_gpu_cores() {
        return count;
    }
    system_profiler_gpu_cores().unwrap_or(0)
}

fn ioreg_gpu_cores() -> Option<u32> {
    let class = CString::new("AGXAccelerator").ok()?;
    // SAFETY: IOKit calls with valid arguments; the matching dictionary is consumed by
    // IOServiceGetMatchingService, the property follows the create rule, the service is released.
    unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        let key = CFString::from_static_string("gpu-core-count");
        let property = IORegistryEntryCreateCFProperty(service, key.as_concrete_TypeRef(), std::ptr::null(), 0);
        IOObjectRelease(service);
        if property.is_null() {
            return None;
        }
        let number = CFNumber::wrap_under_create_rule(property as _);
        number.to_i64().and_then(|n| u32::try_from(n).ok())
    }
}

fn system_profiler_gpu_cores() -> Option<u32> {
    let output = std::process::Command::new("/usr/sbin/system_profiler").args(["SPDisplaysDataType", "-json"]).output().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let cores = value["SPDisplaysDataType"].as_array()?.iter().find_map(|gpu| gpu["sppci_cores"].as_str())?;
    cores.trim().parse().ok()
}

fn metal_facts() -> (u64, Option<String>, Option<bool>) {
    use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
    match MTLCreateSystemDefaultDevice() {
        Some(device) => {
            let working_set = device.recommendedMaxWorkingSetSize();
            let name = device.name().to_string();
            let unified = device.hasUnifiedMemory();
            (working_set, Some(name), Some(unified))
        }
        None => (0, None, None),
    }
}
