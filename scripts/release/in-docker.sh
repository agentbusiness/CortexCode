#!/usr/bin/env bash
# Runs inside the Linux build container (scripts/release/linux.Dockerfile), started by `in_docker`
# in scripts/release/lib.sh: the repository is at /src (read-only), the output goes to /out.
#
#   in-docker.sh plugin <plugin dir> <build id> <output file name>   a plugin package
#   in-docker.sh frame                                                 bin/CortexCode
set -euo pipefail

# The source into the build copy; build outputs and installs there stay between runs.
rsync -a --delete --exclude target --exclude node_modules --exclude dist /src/ /work/
cd /work

# The UI core and SDK (plugin UI packages build against them); installed again when the lock file
# changed since the volume's install (cortex-pack does the same for plugin UIs).
ui_core() {
  (cd ui && {
    { [ -f node_modules/.package-lock.json ] && cmp -s package-lock.json node_modules/.cortex-pack-lock.json; } ||
      { npm ci --no-audit --no-fund && cp package-lock.json node_modules/.cortex-pack-lock.json; }
  } && npm run build --silent)
}

case "${1:-}" in
  plugin)
    dir="$2" build_id="$3" name="$4"
    ui_core
    (cd packer && cargo build --release --quiet)
    packer/target/release/cortex-pack build "$dir" --build-id "$build_id" -o "/out/$name"
    ;;
  frame)
    (cd frame && cargo build --release --quiet)
    mkdir -p /out/bin
    cp frame/target/release/cortex-frame /out/bin/CortexCode
    ;;
  *)
    echo "in-docker.sh: give plugin or frame" >&2
    exit 2
    ;;
esac
