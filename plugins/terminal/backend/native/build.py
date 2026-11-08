#!/usr/bin/env python3
"""Native + Cargo package entry point. Run only in an owner-authorized build.
Downloads are digest-checked before extraction or compilation. Nothing is installed.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
NATIVE = ROOT / "native"
PIN = json.loads((NATIVE / "ghostty-pin.json").read_text())
zig = os.environ.get("ZIG", "zig")
version = subprocess.check_output([zig, "version"], text=True).strip()
if version != PIN["zig"]:
    raise SystemExit(f"Terminal needs Zig {PIN['zig']}; found {version}. No automatic toolchain installation.")
arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}.get(platform.machine())
system = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu"}.get(platform.system())
if arch is None or system is None:
    raise SystemExit("Terminal's pinned native build supports macOS/Linux on ARM64/x86-64.")
host = f"{arch}-{system}"
target = os.environ.get("CARGO_BUILD_TARGET", host)
zig_targets = {
    "aarch64-apple-darwin": "aarch64-macos",
    "x86_64-apple-darwin": "x86_64-macos",
    "aarch64-unknown-linux-gnu": "aarch64-linux-gnu",
    "x86_64-unknown-linux-gnu": "x86_64-linux-gnu",
}
if target not in zig_targets:
    raise SystemExit(f"Unsupported Rust target: {target}")
if target != host:
    raise SystemExit("Cross compilation is not configured: build the plugin on its target platform.")
work = ROOT / "target" / "ghostty" / target
work.mkdir(parents=True, exist_ok=True)
archive = work / f"{PIN['commit']}.tar.gz"
if not archive.exists():
    with urllib.request.urlopen(PIN["archive"], timeout=120) as response:
        data = response.read(64 * 1024 * 1024 + 1)
    if len(data) > 64 * 1024 * 1024:
        raise SystemExit("Ghostty source archive exceeds 64 MiB")
else:
    data = archive.read_bytes()
if hashlib.sha256(data).hexdigest() != PIN["sha256"]:
    raise SystemExit("Ghostty source digest mismatch; refusing extraction and build")
archive.write_bytes(data)
# Recreate only generated source, never the checkout or an owner-managed directory.
source = work / "source"
if source.exists():
    shutil.rmtree(source)
source.mkdir()
with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
    for member in tar.getmembers():
        parts = Path(member.name).parts[1:]
        if not parts:
            continue
        if member.issym() or member.islnk() or member.isdev() or ".." in parts or Path(*parts).is_absolute():
            raise SystemExit(f"Unsafe archive entry: {member.name}")
        destination = source.joinpath(*parts)
        if member.isdir():
            destination.mkdir(parents=True, exist_ok=True)
        elif member.isfile():
            destination.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(member) as incoming:
                destination.write_bytes(incoming.read())
            destination.chmod(0o755 if member.mode & 0o111 else 0o644)
# Keep the unstable API adaptation small and inside the exact upstream module.
shutil.copyfile(NATIVE / "cortex_terminal.zig", source / "src" / "cortex_terminal.zig")
with (source / "src" / "lib_vt.zig").open("a") as output:
    output.write('\ncomptime { if (@import("terminal_options").c_abi) {\n'
                 '    @export(&@import("cortex_terminal.zig").read, .{ .name = "cortex_terminal_read" });\n'
                 '} }\n')
prefix = work / "install"
cache = work / "global-cache"
native_environment = dict(os.environ, ZIG_GLOBAL_CACHE_DIR=str(cache), ZIG_LOCAL_CACHE_DIR=str(work / "local-cache"))
subprocess.run([zig, "build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "-Dsimd=false", "-Dvt-features=-kitty_graphics",
                f"-Dtarget={zig_targets[target]}", "--prefix", str(prefix)], cwd=source, env=native_environment, check=True)
archive_lib = prefix / "lib" / "libghostty-vt.a"
if not archive_lib.is_file():
    raise SystemExit(f"Ghostty did not produce {archive_lib}")
# Collect notices from the exact downloaded dependency sources, not from an
# unrelated machine-wide Zig cache. Refuse packaging when linked Unicode code
# has no inspectable notice. This is completed only by an authorized build.
notices = []
for base in [source, cache / "p"]:
    if not base.is_dir():
        continue
    for path in sorted(base.rglob("*")):
        if path.is_file() and path.name.lower().startswith(("license", "copying", "notice", "copyright")):
            notices.append((str(path.relative_to(base)), path.read_text(errors="replace")))
if not any("uucode" in path.lower() for path, _ in notices):
    raise SystemExit("Missing uucode dependency notice; refusing Cargo/package build")
if not any("unicode" in text.lower() and "copyright" in text.lower() for _, text in notices):
    raise SystemExit("Missing Unicode data notice; refusing Cargo/package build")
notice_file = ROOT.parent / "data" / "licenses" / "GHOSTTY-THIRD-PARTY.txt"
notice_file.parent.mkdir(parents=True, exist_ok=True)
notice_file.write_text("Ghostty pinned dependency notices; source " + PIN["commit"] + "\n\n" +
    "\n\n".join("=== " + path + " ===\n" + text for path, text in notices))
# Install/license evidence travels with the archive; the shipped binary links it statically.
shutil.copyfile(source / "LICENSE", work / "GHOSTTY-LICENSE")
(NATIVE / "GHOSTTY-LICENSE").read_text()  # checked-in attribution must remain present
stamp = {
    "source": PIN["commit"], "target": target, "zig": version,
    "adapter": hashlib.sha256((NATIVE / "cortex_terminal.zig").read_bytes()).hexdigest(),
}
shutil.copyfile(NATIVE / "cortex_terminal.zig", prefix / "cortex-adapter.zig")
(prefix / "cortex-build.txt").write_text(f"{PIN['commit']}\n{version}\n{target}\nsimd=false;kitty_graphics=false\n")
(prefix / "cortex-build.json").write_text(json.dumps(stamp, indent=2) + "\n")
environment = dict(os.environ, CORTEX_GHOSTTY_LIB_DIR=str(prefix / "lib"))
subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, env=environment, check=True)
