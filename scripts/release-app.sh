#!/usr/bin/env bash
# Release the app new users download: put the current frame, Electron and every plugin named in
# the catalog together (nothing is built), as CortexCode.app (a .tar.gz; a .dmg too with RELEASE_DMG=yes) for macOS and a
# `cortexcode` folder (.tar.gz) for Linux, in a GitHub release `v<version>`, named in the catalog
# (the npx launcher installs it from there). Release the frame and the plugins first.
#
#   scripts/release-app.sh [version] [--dry-run]
#
# Version: the one given (newer than the released one), else the next after the released one
# (each part 0 to 99), else the frame's version for a first release. Platforms: RELEASE_PLATFORMS.
# The app is not signed by Apple yet (ad-hoc signatures only): macOS asks once, in System Settings >
# Privacy & Security > Open Anyway, when it is opened from a download.
set -euo pipefail
self="${CORTEX_RELEASE_ORIGINAL:-$0}"
release_name="release-app"
# shellcheck source=release/lib.sh
. "$(cd "$(dirname "$self")" && pwd)/release/lib.sh"
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
need_tools hdiutil
build_packer
[ $dry_run = 1 ] || need_publishing

scratch work
catalog="$work/catalog.json"
fetch_catalog "$catalog"
frame_version="$(jq -r '.frame.version // empty' "$catalog")"
[ -n "$frame_version" ] || die "no frame is released yet (scripts/release-frame.sh)"
latest="$(jq -r '.app.version // empty' "$catalog")"
version="$(choose_version "$given" "$latest" "$frame_version")" || exit 1
step "app $version (frame $frame_version)"

# Every plugin of this repository must be released: the app carries all of them.
missing=()
for toml in "$repo"/plugins/*/plugin.toml; do
  id="$(basename "$(dirname "$toml")")"
  jq -e --arg id "$id" '.plugins[$id]' "$catalog" >/dev/null || missing+=("$id")
done
[ ${#missing[@]} -eq 0 ] || die "release these plugins first (scripts/release-plugin.sh --changed): ${missing[*]}"

out="$artifacts/app/$version"
rm -rf "$out"
mkdir -p "$out"
tag="v$version"
files="{}"
uploads=()

for platform in $RELEASE_PLATFORMS; do
  case "$platform" in darwin-*) os=macos ;; linux-*) os=linux ;; *) die "unknown platform $platform" ;; esac
  base="$work/$platform"
  frame_asset="$(jq -c --arg p "$platform" '.frame.files[$p].frame // empty' "$catalog")"
  electron_asset="$(jq -c --arg p "$platform" '.frame.files[$p].electron // empty' "$catalog")"
  [ -n "$frame_asset" ] && [ -n "$electron_asset" ] || die "the frame $frame_version has no release for $platform"
  step "downloading the frame and Electron for $platform"
  frame_file="$(fetch_asset "$frame_asset" "$base/downloads")" || exit 1
  electron_file="$(fetch_asset "$electron_asset" "$base/downloads")" || exit 1
  mkdir -p "$base/frame" "$base/electron" "$base/packages"
  tar -xzf "$frame_file" -C "$base/frame"
  tar -xzf "$electron_file" -C "$base/electron"

  # Every plugin for this platform, named <id>@<package version>.ccplugin as the frame expects.
  for id in $(jq -r '.plugins | keys[]' "$catalog"); do
    entry="$(jq -c --arg id "$id" '.plugins[$id]' "$catalog")"
    if ! jq -e --arg os "$os" '(.platforms // []) as $p | ($p | length) == 0 or ($p | index($os) != null)' <<<"$entry" >/dev/null; then
      continue
    fi
    asset="$(jq -c --arg p "$platform" '.files[$p] // .files.any // empty' <<<"$entry")"
    [ -n "$asset" ] || continue
    package="$(fetch_asset "$asset" "$base/downloads")" || exit 1
    package_version="$(tar -xzOf "$package" manifest.json | jq -r .plugin.version)"
    cp "$package" "$base/packages/$id@$package_version.ccplugin"
  done
  step "$(find "$base/packages" -name '*.ccplugin' | wc -l | tr -d ' ') plugins for $platform"

  case "$platform" in
    darwin-arm64)
      bundle="$base/bundle"
      app="$bundle/CortexCode.app"
      res="$app/Contents/Resources"
      mkdir -p "$app/Contents/MacOS" "$res"
      cp "$base/frame/CortexCode" "$app/Contents/MacOS/CortexCode"
      cp "$repo/scripts/app/cortex.toml" "$res/cortex.toml"
      cp "$base/frame/ui/shell/assets/CortexCode.icns" "$res/CortexCode.icns"
      ditto "$base/frame/bin" "$res/bin"
      ditto "$base/frame/ui" "$res/ui"
      ditto "$base/electron" "$res/ui/node_modules/electron"
      ditto "$base/packages" "$res/packages"
      sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$frame_version/" "$repo/scripts/app/Info.plist" > "$app/Contents/Info.plist"
      codesign --force --sign - "$app/Contents/MacOS/CortexCode" >/dev/null 2>&1
      archive="$out/CortexCode-$version-darwin-arm64.tar.gz"
      tar_dir "$bundle" "$archive"
      leak_scan "$archive"
      uploads+=("$archive")
      files="$(jq --arg p "$platform" --argjson a "$(asset_json "$archive" "$tag")" '.[$p] = {archive: $a}' <<<"$files")"
      # The disk image only when RELEASE_DMG=yes: an unsigned one is not offered for download.
      if [ "$RELEASE_DMG" = yes ]; then
        dmg_source="$base/dmg"
        mkdir -p "$dmg_source"
        ditto "$app" "$dmg_source/CortexCode.app"
        ln -s /Applications "$dmg_source/Applications"
        dmg="$out/CortexCode-$version-arm64.dmg"
        step "making the disk image"
        hdiutil create -quiet -volname "CortexCode $version" -srcfolder "$dmg_source" -ov -format UDZO "$dmg"
        uploads+=("$dmg")
        files="$(jq --arg p "$platform" --argjson d "$(asset_json "$dmg" "$tag")" '.[$p].dmg = $d' <<<"$files")"
      fi
      ;;
    linux-*)
      folder="$base/bundle/cortexcode"
      mkdir -p "$folder/ui/node_modules"
      cp "$base/frame/CortexCode" "$folder/CortexCode"
      cp "$repo/scripts/app/cortex.toml" "$folder/cortex.toml"
      cp -a "$base/frame/bin" "$folder/bin"
      cp -a "$base/frame/ui/." "$folder/ui/"
      cp -a "$base/electron" "$folder/ui/node_modules/electron"
      cp -a "$base/packages" "$folder/packages"
      archive="$out/cortexcode-$version-$platform.tar.gz"
      tar_dir "$base/bundle" "$archive"
      leak_scan "$archive"
      uploads+=("$archive")
      files="$(jq --arg p "$platform" --argjson a "$(asset_json "$archive" "$tag")" '.[$p] = {archive: $a}' <<<"$files")"
      ;;
  esac
done

(cd "$out" && shasum -a 256 -- * > SHA256SUMS)

if [ $dry_run = 0 ]; then
  notes="CortexCode $version (frame $frame_version). Run it with \`npx cortexcode\` (macOS on Apple silicon, Linux arm64 and x64). On Linux you can also unpack the .tar.gz below and run cortexcode/CortexCode. Updates come by themselves."
  [ "$RELEASE_DMG" != yes ] || notes="$notes On macOS you can also open the .dmg and move CortexCode to Applications (the first start asks in System Settings > Privacy & Security > Open Anyway: the app is not signed by Apple yet)."
  rid="$(gh_release "$tag" "CortexCode $version" "$notes" true false)"
  for file in "${uploads[@]}"; do
    step "uploading $(basename "$file")"
    gh_upload "$rid" "$file"
  done
fi
jq --arg v "$version" --arg tag "$tag" --arg frame "$frame_version" --argjson files "$files" \
  '.app = {version: $v, tag: $tag, frame: $frame, files: $files}' "$catalog" > "$catalog.next"
mv "$catalog.next" "$catalog"

mkdir -p "$artifacts"
if [ $dry_run = 1 ]; then
  cp "$catalog" "$artifacts/catalog.preview.json"
  step "dry run: files in artifacts/app/$version/, the catalog it would publish in artifacts/catalog.preview.json"
else
  publish_catalog "$catalog"
  step "done"
fi
