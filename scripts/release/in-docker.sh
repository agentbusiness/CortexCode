#!/usr/bin/env bash
# Runs inside the Linux build container (scripts/release/linux.Dockerfile), started by `in_docker`
# in scripts/release/lib.sh: the repository is at /src (read-only), the output goes to /out.
#
#   in-docker.sh plugin <plugin dir> <build id> <output file name>   a plugin package
#   in-docker.sh frame                                                 bin/CortexCode, bin/cx, bin/cortex-pack
set -euo pipefail

# The source into the build copy; build outputs and installs there stay between runs.
rsync -a --delete --exclude target --exclude node_modules --exclude dist /src/ /work/
cd /work

# The UI core and SDK (plugin UI packages build against them); installed once per volume.
ui_core() {
  (cd ui && { [ -f node_modules/.package-lock.json ] || npm ci --no-audit --no-fund; } && npm run build --silent)
}

case "${1:-}" in
  plugin)
    dir="$2" build_id="$3" name="$4"
    ui_core
    (cd packer && cargo build --release --quiet)
    packer/target/release/cortex-pack build "$dir" --build-id "$build_id" -o "/out/$name"
    ;;
  frame)
    for crate in frame cli packer; do
      (cd "$crate" && cargo build --release --quiet)
    done
    mkdir -p /out/bin
    cp frame/target/release/cortex-frame /out/bin/CortexCode
    cp cli/target/release/cx /out/bin/cx
    cp packer/target/release/cortex-pack /out/bin/cortex-pack
    ;;
  *)
    echo "in-docker.sh: give plugin or frame" >&2
    exit 2
    ;;
esac
