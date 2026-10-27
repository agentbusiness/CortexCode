#!/bin/bash
# Build CortexCode.app: the frame, the Electron UI, every plugin built and packed, cx and
# cortex-pack, the full source and the app's configuration.
#
#   scripts/build-app.sh [out-dir]     # default dist/  ->  <out-dir>/CortexCode.app
#
# The macOS release is this app. `./cortexcode` builds the same app into build/ and starts it, so a
# development start runs exactly what a user downloads. A plugin whose source did not change keeps
# its package (its version carries a digest of its source), so a rebuild only builds what changed.
#
# Contents/MacOS/CortexCode          the frame; it reads Contents/Resources/cortex.toml
# Contents/Resources/cortex.toml     every path inside the bundle (scripts/app/cortex.toml)
# Contents/Resources/packages/       <id>@<version>.ccplugin for every plugin, installed at start
# Contents/Resources/source/         the full source, synced into ~/.cortexcode/code at start
# Contents/Resources/bin/            cx and cortex-pack, copied into ~/.cortexcode/bin at start
# Contents/Resources/ui/             the Electron shell, the renderer and Electron (as CortexCode.app)
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$repo/dist}"
case "$out" in /*) ;; *) out="$repo/$out" ;; esac
app="$out/CortexCode.app"
contents="$app/Contents"
res="$contents/Resources"
cd "$repo"

step() { printf 'build-app: %s\n' "$*"; }

# Nothing changed since the last complete build: the app is current, there is nothing to do. The
# stamp covers every source file (tracked or not, ignored ones left out) by path, size and mtime,
# and HEAD; it is written only when a build ran through.
stamp_of() {
  { git rev-parse HEAD 2>/dev/null; git ls-files -co --exclude-standard -z | xargs -0 stat -f '%N %z %m' 2>/dev/null; } | shasum -a 256 | cut -c1-16
}
stamp="$(stamp_of)"
if [ -x "$contents/MacOS/CortexCode" ] && [ "$(cat "$out/.stamp" 2>/dev/null)" = "$stamp" ]; then
  step "nothing changed since the last build; the app is current"
  exit 0
fi
rm -f "$out/.stamp"
# shellcheck source=app/electron.sh
. "$repo/scripts/app/electron.sh"
# The source files git tracks or would track under the given paths, that exist (NUL-separated).
sources() {
  git ls-files -co --exclude-standard -z -- "$@" | while IFS= read -r -d '' file; do
    [ -f "$file" ] && printf '%s\0' "$file"
  done
}

mkdir -p "$contents/MacOS" "$res/packages" "$res/bin" "$res/ui/node_modules"

# --- the frame, cx and cortex-pack -------------------------------------------------------------
for crate in frame cli packer; do
  step "building $crate"
  (cd "$crate" && cargo build --release --quiet)
done
pack="$repo/packer/target/release/cortex-pack"

# --- the UI core (renderer and SDK) ------------------------------------------------------------
[ -d ui/node_modules ] || (cd ui && npm ci --no-audit --no-fund)
step "building the UI core"
(cd ui && npm run build --silent >/dev/null)

# --- every plugin, built and packed -------------------------------------------------------------
# The digest covers the plugin's own files and the shared code it builds with.
shared=(protocol model-kit lang-kit ui/sdk rust-toolchain.toml)
wanted=()
for toml in plugins/*/plugin.toml; do
  dir="$(dirname "$toml")"
  read -r id version < <("$pack" info "$dir" | /usr/bin/python3 -c 'import json,sys; p=json.load(sys.stdin)["plugin"]; print(p["id"], p["version"].split("+")[0])')
  digest="$(sources "$dir" "${shared[@]}" | xargs -0 shasum -a 256 | shasum -a 256 | cut -c1-12)"
  name="$id@$version+b$digest.ccplugin"
  wanted+=("$name")
  if [ -f "$res/packages/$name" ]; then
    continue
  fi
  step "packing $id"
  "$pack" build "$dir" --build-id "b$digest" -o "$res/packages/.$name" >/dev/null
  mv "$res/packages/.$name" "$res/packages/$name"
done
# Packages of an older source go.
for file in "$res/packages"/*.ccplugin; do
  [ -e "$file" ] || continue
  keep=0
  for name in "${wanted[@]}"; do [ "$(basename "$file")" = "$name" ] && keep=1; done
  [ $keep = 1 ] || rm -f "$file"
done

# --- cx and cortex-pack ------------------------------------------------------------------------
cp cli/target/release/cx "$res/bin/cx"
cp packer/target/release/cortex-pack "$res/bin/cortex-pack"

# --- the full source ---------------------------------------------------------------------------
step "copying the source"
rm -rf "$res/source"
mkdir -p "$res/source"
sources . | rsync -a --from0 --files-from=- ./ "$res/source/"

# --- the UI: shell, renderer, its runtime packages and Electron ---------------------------------
rm -rf "$res/ui/shell" "$res/ui/renderer"
mkdir -p "$res/ui/renderer"
cp ui/package.json "$res/ui/package.json"
rsync -a ui/shell/ "$res/ui/shell/"
rsync -a --delete ui/renderer/dist/ "$res/ui/renderer/dist/"
rsync -a --delete ui/node_modules/ws/ "$res/ui/node_modules/ws/"
electron_src="ui/node_modules/electron"
electron_dst="$res/ui/node_modules/electron"
if [ "$(cat "$electron_dst/version" 2>/dev/null)" != "$(cat "$electron_src/package.json" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')" ]; then
  step "copying Electron"
  electron_mac_branded "$electron_src" "$electron_dst" ui/shell/assets/CortexCode.icns
fi

# --- the frame and the app's own files ---------------------------------------------------------
cp frame/target/release/cortex-frame "$contents/MacOS/CortexCode"
cp scripts/app/cortex.toml "$res/cortex.toml"
cp ui/shell/assets/CortexCode.icns "$res/CortexCode.icns"
version="$(grep -m1 '^version' frame/Cargo.toml | cut -d'"' -f2)"
build="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"
sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$build/" scripts/app/Info.plist > "$contents/Info.plist"
codesign --force --sign - "$contents/MacOS/CortexCode" >/dev/null 2>&1

step "done: $app"

# The build ran through: remember what it was built from.
printf '%s\n' "$stamp" > "$out/.stamp"
