//! Builds the vendored PrismML llama.cpp (`vendor/llama.cpp`, see its REVISION and PATCHES.md),
//! its multimodal library and the shim (`native/`) with CMake, and links them statically. Nothing
//! is downloaded and no installed llama.cpp is consulted.
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let vendor = manifest.join("vendor/llama.cpp");
    println!("cargo:rerun-if-changed=native");
    // The whole vendored tree: a local patch (see vendor/llama.cpp/PATCHES.md) must rebuild.
    println!("cargo:rerun-if-changed={}", vendor.display());
    println!("cargo:rerun-if-env-changed=SDKROOT");

    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos");
    // The C/C++ flags otherwise inherit a deployment target newer than the running macOS (derived
    // from the newest installed SDK), and the objects would then fail to load. Match a floor every
    // supported Mac meets.
    if macos && std::env::var_os("MACOSX_DEPLOYMENT_TARGET").is_none() {
        // SAFETY: build scripts are single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("MACOSX_DEPLOYMENT_TARGET", "14.0") };
    }
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    let mut config = cmake::Config::new(manifest.join("native"));
    config.profile("Release").build_target("bonsai_shim");
    if which("ninja") {
        config.generator("Ninja");
    }
    if let Ok(sdk) = std::env::var("SDKROOT") {
        config.define("CMAKE_OSX_SYSROOT", sdk);
    }
    let out = config.build();
    let build = out.join("build");

    // Every static archive the build produced.
    let mut archives = Vec::new();
    collect_archives(&build, &mut archives);
    if archives.is_empty() {
        panic!("the llama.cpp build produced no static libraries under {}", build.display());
    }
    if !macos {
        // GNU ld resolves archives in order: one group lets them refer to each other.
        println!("cargo:rustc-link-arg=-Wl,--start-group");
        for archive in &archives {
            println!("cargo:rustc-link-arg={}", archive.display());
        }
        println!("cargo:rustc-link-arg=-Wl,--end-group");
        println!("cargo:rustc-link-arg=-lstdc++");
        println!("cargo:rustc-link-arg=-lm");
        println!("cargo:rustc-link-arg=-lpthread");
        // GCC's outlined atomics (`__aarch64_*`) live only in the static libgcc.
        println!("cargo:rustc-link-arg=-lgcc");
        return;
    }
    // ld64 resolves archive order itself.
    for archive in &archives {
        let (Some(dir), Some(name)) = (archive.parent(), archive.file_stem().and_then(|stem| stem.to_str())) else {
            continue;
        };
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static={}", name.trim_start_matches("lib"));
    }
    for framework in ["Foundation", "Metal", "MetalKit", "Accelerate"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=c++");
    // ggml-metal's `@available` checks call clang's runtime (`__isPlatformVersionAtLeast`), which
    // rustc does not link by itself.
    if let Some(dir) = clang_runtime_dir() {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    }
    // The binary targets the floor the C++ objects were built for.
    println!("cargo:rustc-link-arg=-mmacosx-version-min=14.0");
}

/// `clang -print-runtime-dir` (where `libclang_rt.osx.a` lives).
fn clang_runtime_dir() -> Option<PathBuf> {
    let output = std::process::Command::new("clang").arg("-print-runtime-dir").output().ok()?;
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    dir.join("libclang_rt.osx.a").is_file().then_some(dir)
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())).unwrap_or(false)
}

fn collect_archives(dir: &Path, archives: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "CMakeFiles") {
                continue;
            }
            collect_archives(&path, archives);
        } else if path.extension().is_some_and(|extension| extension == "a") {
            archives.push(path);
        }
    }
}
