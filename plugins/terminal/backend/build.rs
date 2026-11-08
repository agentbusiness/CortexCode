//! Linking only: cargo check/build must never download or execute native build hooks.
use std::{env, fs, path::PathBuf};
fn main() {
    for path in [
        "native/ghostty-pin.json",
        "native/cortex_terminal.zig",
        "native/build.py",
        "native/GHOSTTY-LICENSE",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=CORTEX_GHOSTTY_LIB_DIR");
    let target = env::var("TARGET").expect("Cargo target");
    let directory = env::var_os("CORTEX_GHOSTTY_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("target/ghostty")
                .join(&target)
                .join("install/lib")
        });
    let directory = fs::canonicalize(&directory).unwrap_or_else(|_| panic!(
        "Ghostty native archive is absent. An owner-authorized plugin build must use python3 native/build.py first."));
    assert!(
        directory.join("libghostty-vt.a").is_file(),
        "Ghostty static archive is absent"
    );
    let stamp = fs::read_to_string(directory.parent().unwrap().join("cortex-build.txt"))
        .expect("Missing Ghostty build stamp; use the plugin build entry point");
    assert_eq!(
        stamp,
        format!(
            "83fb95624b0439ee660c2679b43b38c8b63769cd\n0.17.0\n{target}\nsimd=false;kitty_graphics=false\n"
        ),
        "Ghostty source/toolchain/target mismatch"
    );
    let adapter = directory.parent().unwrap().join("cortex-adapter.zig");
    assert_eq!(
        fs::read(&adapter).expect("Missing native adapter stamp"),
        fs::read("native/cortex_terminal.zig").unwrap(),
        "Ghostty adapter changed; rebuild native archive"
    );
    println!("cargo:rerun-if-changed={}", adapter.display());
    println!(
        "cargo:rerun-if-changed={}",
        directory.join("libghostty-vt.a").display()
    );
    println!("cargo:rustc-link-search=native={}", directory.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:rustc-link-lib=c");
    if target.contains("linux") {
        println!("cargo:rustc-link-lib=m");
    }
}
