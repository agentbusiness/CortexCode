#!/usr/bin/env bash
# Release the frame: the frame and its Electron shell (ui/shell), plus Electron when its version
# changed, in a GitHub release `frame-v<version>`, named in the catalog. The app downloads it, and it
# runs after "Restart to update". Everything else is a plugin, released on its own
# (scripts/release-plugin.sh): the window is the workbench plugin's, cx the tools plugin's.
#
#   scripts/release-frame.sh [version] [--dry-run]
#
# Version: the one given (newer than the released one), else the next after the released one
# (each part 0 to 99), else frame/Cargo.toml's for a first release. It is written into
# frame/Cargo.toml and ui/package.json (commit them). Platforms: RELEASE_PLATFORMS.
#
# The catalog records the frame's interface (the protocol files that are not features) and the
# first frame version with it (`interface_since`): plugins built against it name that version as
# their min_frame, so a frame release that keeps the interface never holds plugin updates back.
set -euo pipefail
self="${CORTEX_RELEASE_ORIGINAL:-$0}"
release_name="release-frame"
# shellcheck source=release/lib.sh
. "$(cd "$(dirname "$self")" && pwd)/release/lib.sh"
# shellcheck source=app/electron.sh
. "$repo/scripts/app/electron.sh"
release_snapshot "$self" "$@"

dry_run=0
given=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -*) die "unknown option $arg" ;;
    *) given="$arg" ;;
  esac
done

load_env
need_mac
need_tools node
build_packer
[ $dry_run = 1 ] || need_publishing

scratch work
catalog="$work/catalog.json"
fetch_catalog "$catalog"
latest="$(jq -r '.frame.version // empty' "$catalog")"
first="$(toml_version "$repo/frame/Cargo.toml")"
if [ -z "$given" ] && [ -n "$latest" ] && version_newer "$first" "$latest"; then
  version="$(version_check "$first")" || exit 1
else
  version="$(choose_version "$given" "$latest" "$first")" || exit 1
fi
step "frame $version"

# --- the version everywhere the frame reports it ----------------------------------------------
stamp_toml_version "$repo/frame/Cargo.toml" "$version"
stamp_json_version "$repo/ui/package.json" "$version"
[ ! -f "$repo/ui/package-lock.json" ] || stamp_json_version "$repo/ui/package-lock.json" "$version"

# --- the shell's runtime packages (the same on every platform) ---------------------------------
[ -f "$repo/ui/node_modules/.package-lock.json" ] || (cd "$repo/ui" && npm ci --no-audit --no-fund)
electron_version="$(jq -r .version "$repo/ui/node_modules/electron/package.json")"

out="$artifacts/frame/$version"
rm -rf "$out"
mkdir -p "$out"
tag="frame-v$version"
files="{}"
uploads=()

for platform in $RELEASE_PLATFORMS; do
  stage="$work/$platform/frame"
  mkdir -p "$stage/ui/node_modules"
  case "$platform" in
    darwin-arm64)
      step "building the frame"
      (cd "$repo/frame" && cargo build --release --quiet)
      cp "$repo/frame/target/release/cortex-frame" "$stage/CortexCode"
      codesign --force --sign - "$stage/CortexCode" >/dev/null 2>&1
      ;;
    linux-*)
      step "building the frame for $platform (Docker)"
      in_docker "$platform" "$work/$platform/built" frame
      cp "$work/$platform/built/bin/CortexCode" "$stage/CortexCode"
      ;;
    *) die "unknown platform $platform" ;;
  esac
  cp "$repo/scripts/app/cortex.frame.toml" "$stage/cortex.toml"
  cp "$repo/ui/package.json" "$stage/ui/package.json"
  rsync -a "$repo/ui/shell/" "$stage/ui/shell/"
  rsync -a "$repo/ui/node_modules/ws/" "$stage/ui/node_modules/ws/"
  frame_file="$out/frame-$version-$platform.tar.gz"
  tar_dir "$stage" "$frame_file"
  uploads+=("$frame_file")

  # Electron: the released one when its version did not change.
  electron_asset="$(jq -c --arg p "$platform" --arg e "$electron_version" 'if .frame.electron == $e then (.frame.files[$p].electron // empty) else empty end' "$catalog")"
  if [ -z "$electron_asset" ]; then
    step "Electron $electron_version for $platform"
    electron_dir="$work/$platform/electron"
    case "$platform" in
      darwin-arm64) electron_mac_branded "$repo/ui/node_modules/electron" "$electron_dir" "$repo/ui/shell/assets/CortexCode.icns" ;;
      linux-*) electron_linux "$platform" "$electron_version" "$electron_dir" ;;
    esac
    electron_file="$out/electron-$electron_version-$platform.tar.gz"
    tar_dir "$electron_dir" "$electron_file"
    uploads+=("$electron_file")
    electron_asset="$(asset_json "$electron_file" "$tag")"
  fi
  files="$(jq --arg p "$platform" --argjson f "$(asset_json "$frame_file" "$tag")" --argjson e "$electron_asset" '.[$p] = {frame: $f, electron: $e}' <<<"$files")"
done

leak_scan "${uploads[@]}"
(cd "$out" && shasum -a 256 -- *.tar.gz > SHA256SUMS)

if [ $dry_run = 0 ]; then
  rid="$(gh_release "$tag" "CortexCode frame $version" "The CortexCode frame $version (source $(git -C "$repo" rev-parse --short HEAD)). The app's updater installs it; it runs after Restart to update." false false)"
  for file in "${uploads[@]}"; do
    step "uploading $(basename "$file")"
    gh_upload "$rid" "$file"
  done
fi
# What it was built from (after the builds: they write the version into the lock files), and the
# first frame with this interface.
interface="$(interface_digest)"
since="$(jq -r --arg i "$interface" 'if .frame.interface == $i then (.frame.interface_since // empty) else empty end' "$catalog")"
[ -n "$since" ] || since="$version"
jq --arg v "$version" --arg tag "$tag" --arg e "$electron_version" --arg digest "$(frame_digest)" \
  --arg interface "$interface" --arg since "$since" --argjson files "$files" \
  '.frame = {version: $v, tag: $tag, electron: $e, digest: $digest, interface: $interface, interface_since: $since, files: $files}' "$catalog" > "$catalog.next"
mv "$catalog.next" "$catalog"

mkdir -p "$artifacts"
if [ $dry_run = 1 ]; then
  cp "$catalog" "$artifacts/catalog.preview.json"
  step "dry run: files in artifacts/frame/$version/, the catalog it would publish in artifacts/catalog.preview.json"
else
  publish_catalog "$catalog"
  step "done; commit the version changes"
fi
