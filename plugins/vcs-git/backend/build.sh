#!/bin/sh
# Plugin build (run by cortex-pack in backend/): `cargo build --release`, on macOS in a folder outside
# the home folder. git2's vendored OpenSSL keeps the folder it was built in inside the binary (its
# default engines and modules folders), which scripts/build-env.sh cannot rewrite, so the build runs
# where no user's name is: /Users/Shared/cortex-build/<app>/<plugin>. The executable is then copied to
# target/release/, where plugin.toml names it. Linux builds (in Docker) build in place.
#
#   sh build.sh <executable>
set -eu
cd "$(dirname "$0")"
exe="$1"
case "$(uname -s)" in
  Darwin)
    plugin="$(basename "$(cd .. && pwd)")"
    app="$(basename "$(cd ../../.. && pwd)")"
    out="/Users/Shared/cortex-build/$app/$plugin"
    mkdir -p "$out" target/release
    CARGO_TARGET_DIR="$out" cargo build --release
    cp -p "$out/release/$exe" "target/release/$exe"
    ;;
  *) cargo build --release ;;
esac
