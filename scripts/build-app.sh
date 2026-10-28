#!/bin/bash
# Build CortexCode.app: the frame, the Electron UI, every plugin built and packed, cx and
# cortex-pack, the full source and the app's configuration.
#
#   scripts/build-app.sh [out-dir] [--live]     # default dist/  ->  <out-dir>/CortexCode.app
#
# The macOS release is this app. `./cortexcode` builds the same app into build/ and starts it, so a
# development start runs exactly what a user downloads. A plugin whose source did not change keeps
# its package (its version carries a digest of its source), so a rebuild only builds what changed.
#
# `--live` is the build `./cortexcode` runs beside an app already started from <out-dir>: each
# changed plugin is installed into the running frame as it is packed (a hot-swap), the frame and
# the window's core take effect at the next start, and nothing the app runs is overwritten in
# place. Its progress lines go to fd 3 (the terminal), everything else to the caller's output.
#
# Contents/MacOS/CortexCode          the frame; it reads Contents/Resources/cortex.toml
# Contents/Resources/cortex.toml     every path inside the bundle (scripts/app/cortex.toml)
# Contents/Resources/packages/       <id>@<version>.ccplugin for every plugin, installed at start
# Contents/Resources/source/         the full source, synced into ~/.cortexcode/code at start
# Contents/Resources/bin/            cx and cortex-pack, copied into ~/.cortexcode/bin at start
# Contents/Resources/ui/             the Electron shell, the renderer and Electron (as CortexCode.app)
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
live=0
args=()
for arg in "$@"; do
  case "$arg" in --live) live=1 ;; *) args+=("$arg") ;; esac
done
out="${args[0]:-$repo/dist}"
case "$out" in /*) ;; *) out="$repo/$out" ;; esac
app="$out/CortexCode.app"
contents="$app/Contents"
res="$contents/Resources"
cd "$repo"

step() {
  printf 'build-app: %s\n' "$*"
  if [ $live = 1 ]; then printf 'build-app: %s\n' "$*" >&3 2>/dev/null || true; fi
}
if [ $live = 1 ]; then
  trap 'status=$?; if [ $status != 0 ]; then step "the build failed (exit $status); the app keeps running the last build. Log: $out/build.log"; fi' EXIT
fi
# Put a file in place by a rename, so a running program never sees it rewritten under it.
place() {
  cp "$1" "$2.next"
  mv -f "$2.next" "$2"
}

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
  # The packer writes the package under a temporary name and renames it into place.
  if [ $live = 0 ]; then
    "$pack" build "$dir" --build-id "b$digest" -o "$res/packages/$name" >/dev/null
    continue
  fi
  log="$out/.pack-$id.log"
  if "$pack" build "$dir" --build-id "b$digest" -o "$res/packages/$name" --install >"$log" 2>&1; then
    step "$(grep -E '^installed |not running' "$log" | tail -1)"
  elif [ -f "$res/packages/$name" ]; then
    # Packed, but the running frame kept its version: the next start installs it.
    step "$id was packed but not swapped in: $(tail -1 "$log")"
  else
    cat "$log"
    rm -f "$log"
    exit 1
  fi
  cat "$log"
  rm -f "$log"
done
# Packages of an older source go.
for file in "$res/packages"/*.ccplugin; do
  [ -e "$file" ] || continue
  keep=0
  for name in "${wanted[@]}"; do [ "$(basename "$file")" = "$name" ] && keep=1; done
  [ $keep = 1 ] || rm -f "$file"
done

# --- cx and cortex-pack ------------------------------------------------------------------------
place cli/target/release/cx "$res/bin/cx"
place packer/target/release/cortex-pack "$res/bin/cortex-pack"

# --- the full source ---------------------------------------------------------------------------
step "copying the source"
rm -rf "$res/.source-next" "$res/.source-old"
mkdir -p "$res/.source-next"
sources . | rsync -a --from0 --files-from=- ./ "$res/.source-next/"
[ ! -d "$res/source" ] || mv "$res/source" "$res/.source-old"
mv "$res/.source-next" "$res/source"
rm -rf "$res/.source-old"

# --- the UI: shell, renderer, its runtime packages and Electron ---------------------------------
mkdir -p "$res/ui/renderer"
place ui/package.json "$res/ui/package.json"
# Compared by content (the UI build rewrites unchanged files); a file sent or deleted is a change.
ui_sent="$(
  rsync -aci --delete ui/shell/ "$res/ui/shell/" || exit 1
  rsync -aci --delete ui/renderer/dist/ "$res/ui/renderer/dist/" || exit 1
  rsync -aci --delete ui/node_modules/ws/ "$res/ui/node_modules/ws/" || exit 1
)"
ui_changed="$(printf '%s\n' "$ui_sent" | grep -E '^(>f|\*deleting)' || true)"
electron_src="ui/node_modules/electron"
electron_dst="$res/ui/node_modules/electron"
if [ "$(cat "$electron_dst/version" 2>/dev/null)" != "$(cat "$electron_src/package.json" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])')" ]; then
  step "copying Electron"
  electron_mac_branded "$electron_src" "$electron_dst" ui/shell/assets/CortexCode.icns
fi

# --- the frame and the app's own files ---------------------------------------------------------
# The signature changes the frame's bytes, so whether it changed is told from the copy it was
# built from ($out/.frame-built), not from the app's.
frame_exe="$contents/MacOS/CortexCode"
frame_changed=0
if ! cmp -s frame/target/release/cortex-frame "$out/.frame-built" 2>/dev/null; then
  frame_changed=1
  place frame/target/release/cortex-frame "$frame_exe"
  cp frame/target/release/cortex-frame "$out/.frame-built"
fi
place scripts/app/cortex.toml "$res/cortex.toml"
place ui/shell/assets/CortexCode.icns "$res/CortexCode.icns"
version="$(grep -m1 '^version' frame/Cargo.toml | cut -d'"' -f2)"
build="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"
sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$build/" scripts/app/Info.plist > "$contents/Info.plist.next"
mv -f "$contents/Info.plist.next" "$contents/Info.plist"
# Signed in the bundle (its identifier and the sealed resources); codesign writes a new file, so a
# frame running from it keeps its own.
codesign --force --sign - "$frame_exe" >/dev/null 2>&1

if [ $live = 1 ]; then
  [ $frame_changed = 0 ] || step "the frame changed: it runs from the next start"
  [ -z "$ui_changed" ] || step "the window's core changed: it loads at the next start"
fi
step "done: $app"

# The build ran through: remember what it was built from.
printf '%s\n' "$stamp" > "$out/.stamp"
