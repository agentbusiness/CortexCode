//! Builds the bundled notification app (`CortexCodeNotify.app`) next to the `cc-notify` binary.
//!
//! macOS only delivers UNUserNotificationCenter notifications for a process that runs from an app
//! bundle with a bundle identifier; `cc-notify` is a bare executable, so it drives this tiny app
//! (helper/CortexNotify.swift) instead. The app is written to the profile directory
//! (`target/<profile>/CortexCodeNotify.app`), where the plugin looks for it beside its own
//! executable, and is ad-hoc signed. It is rebuilt only when its sources or the Swift compiler
//! change, so its code signature (part of what macOS remembers the user's permission by) stays
//! stable between plugin builds.
//!
//! Without a Swift compiler the plugin still builds; it then delivers through `osascript` only
//! (reported by `notify.status`). The outcome reaches the code as `CC_NOTIFY_HELPER_BUILD`.

use std::path::{Path, PathBuf};
use std::process::Command;

const APP: &str = "CortexCodeNotify.app";
const EXECUTABLE: &str = "cortex-notify-helper";

fn main() {
    println!("cargo:rerun-if-changed=helper/CortexNotify.swift");
    println!("cargo:rerun-if-changed=helper/Info.plist");
    println!("cargo:rerun-if-changed=helper/CortexCode.icns");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    match build() {
        Ok(path) => println!("cargo:rustc-env=CC_NOTIFY_HELPER_BUILD=built:{}", path.display()),
        Err(reason) => {
            println!("cargo:warning=notification app not built ({reason}); cc-notify will deliver through osascript only");
            println!("cargo:rustc-env=CC_NOTIFY_HELPER_BUILD=absent:{reason}");
        }
    }
}

/// FNV-1a over the inputs: a structural change detector, not a security hash.
fn fnv(parts: &[&[u8]]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn run(command: &mut Command, what: &str) -> Result<String, String> {
    let output = command.output().map_err(|error| format!("{what}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{what} failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn build() -> Result<PathBuf, String> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return Err("not building for macOS".into());
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);
    // OUT_DIR is <target>/<profile>/build/<pkg>-<hash>/out: the profile directory holds the binaries.
    let profile_dir = out_dir.ancestors().nth(3).ok_or("unexpected OUT_DIR layout")?.to_path_buf();
    let source = manifest.join("helper/CortexNotify.swift");
    let plist = manifest.join("helper/Info.plist");
    let swift_source = std::fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let plist_bytes = std::fs::read(&plist).map_err(|e| format!("{}: {e}", plist.display()))?;
    // The app's icon (CFBundleIconFile in Info.plist): macOS shows it on every notification.
    let icon = manifest.join("helper/CortexCode.icns");
    let icon_bytes = std::fs::read(&icon).map_err(|e| format!("{}: {e}", icon.display()))?;
    let swiftc_version = run(Command::new("xcrun").args(["swiftc", "--version"]), "xcrun swiftc --version")
        .or_else(|_| run(Command::new("swiftc").arg("--version"), "swiftc --version"))?;
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => return Err(format!("unsupported architecture {other:?}")),
    };
    let target = format!("{arch}-apple-macos13.0");
    let stamp = fnv(&[&swift_source, &plist_bytes, &icon_bytes, swiftc_version.as_bytes(), target.as_bytes()]);

    let app = profile_dir.join(APP);
    let stamp_file = app.join("Contents/Resources/build-stamp");
    if std::fs::read_to_string(&stamp_file).ok().as_deref() == Some(stamp.as_str()) && app.join("Contents/MacOS").join(EXECUTABLE).exists() {
        return Ok(app);
    }

    let staging = profile_dir.join(format!("{APP}.staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let macos = staging.join("Contents/MacOS");
    let resources = staging.join("Contents/Resources");
    std::fs::create_dir_all(&macos).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&resources).map_err(|e| e.to_string())?;
    let executable = macos.join(EXECUTABLE);
    let compiled = compile(&source, &executable, &target);
    if let Err(error) = compiled {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    std::fs::copy(&plist, staging.join("Contents/Info.plist")).map_err(|e| e.to_string())?;
    std::fs::write(resources.join("CortexCode.icns"), &icon_bytes).map_err(|e| e.to_string())?;
    std::fs::write(resources.join("build-stamp"), &stamp).map_err(|e| e.to_string())?;
    // Ad-hoc signature over the whole bundle (seals Info.plist and the icon), which UserNotifications
    // requires; signed after every resource is in place.
    let signed = run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&staging), "codesign");
    if let Err(error) = signed {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let _ = std::fs::remove_dir_all(&app);
    std::fs::rename(&staging, &app).map_err(|e| format!("install {}: {e}", app.display()))?;
    Ok(app)
}

fn compile(source: &Path, executable: &Path, target: &str) -> Result<(), String> {
    let mut command = Command::new("xcrun");
    command
        .arg("swiftc")
        .args(["-swift-version", "5", "-O", "-target", target, "-framework", "AppKit", "-framework", "UserNotifications"])
        .arg("-o")
        .arg(executable)
        .arg(source);
    run(&mut command, "swiftc").map(|_| ())
}
