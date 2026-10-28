#!/usr/bin/env bash
# Release plugins, each on its own: build its package for every platform it runs on, put it in a
# GitHub release `plugin-<id>-v<version>` and name it in the catalog. Nothing else is built.
#
#   scripts/release-plugin.sh <plugin> [version]     one plugin
#   scripts/release-plugin.sh <plugin> <plugin> ...  several, each with its next version
#   scripts/release-plugin.sh --changed              every plugin whose source changed since its release
#   add --dry-run to build into artifacts/ without uploading
#
# Version: the one given (newer than the released one), else the next after the released one
# (each part 0 to 99: 0.3.99 -> 0.4.0), else plugin.toml's for a first release. The version is
# written into the plugin's plugin.toml (commit it). A plugin without a backend is one package for
# every platform. Linux packages are built in Docker for plugins whose plugin.toml lists linux.
#
# min_frame: the first released frame with the frame interface the plugin is built against (the
# catalog's `frame.interface_since`). When the interface changed since the released frame, the
# frame is released first (scripts/release.sh does).
set -euo pipefail
self="${CORTEX_RELEASE_ORIGINAL:-$0}"
release_name="release-plugin"
# shellcheck source=release/lib.sh
. "$(cd "$(dirname "$self")" && pwd)/release/lib.sh"
release_snapshot "$self" "$@"

dry_run=0
changed=0
plugins=()
given=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --changed) changed=1 ;;
    -*) die "unknown option $arg" ;;
    *)
      if [[ "$arg" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]]; then given="$arg"; else plugins+=("$arg"); fi
      ;;
  esac
done
if [ $changed = 1 ] && [ ${#plugins[@]} -gt 0 ]; then die "give plugins or --changed, not both"; fi
if [ $changed = 0 ] && [ ${#plugins[@]} -eq 0 ]; then
  sed -n '2,15p' "$self" | sed 's/^# \{0,1\}//'
  exit 2
fi
if [ -n "$given" ] && [ ${#plugins[@]} -ne 1 ]; then die "a version can be given for one plugin only"; fi

load_env
need_mac
need_tools
build_packer
[ $dry_run = 1 ] || need_publishing

scratch work
catalog="$work/catalog.json"
fetch_catalog "$catalog"
if [ "$(jq -r '.frame.interface // empty' "$catalog")" = "$(interface_digest)" ]; then
  min_frame="$(jq -r '.frame.interface_since' "$catalog")"
else
  die "the frame's interface in protocol/ changed since the released frame: release the frame first (scripts/release.sh)"
fi
commit="$(git -C "$repo" rev-parse --short HEAD)"

if [ $changed = 1 ]; then
  for toml in "$repo"/plugins/*/plugin.toml; do
    id="$(basename "$(dirname "$toml")")"
    released="$(jq -r --arg id "$id" '.plugins[$id].digest // empty' "$catalog")"
    [ "$(plugin_digest "plugins/$id")" = "$released" ] || plugins+=("$id")
  done
  [ ${#plugins[@]} -gt 0 ] || { step "no plugin changed since its release"; exit 0; }
  step "changed since their release: ${plugins[*]}"
fi

# release_one <plugin id>: build, upload and name one plugin in the catalog.
release_one() {
  local id="$1" dir="plugins/$1" info name toml_v platforms has_backend latest released digest version out tag notes rid
  [ -f "$repo/$dir/plugin.toml" ] || die "no plugin $id ($dir/plugin.toml)"
  info="$("$pack" info "$repo/$dir")"
  name="$(jq -r '.plugin.name // .plugin.id' <<<"$info")"
  platforms="$(jq -c '.plugin.platforms // []' <<<"$info")"
  has_backend="$(jq -r 'if .backend then "yes" else "no" end' <<<"$info")"
  toml_v="$(toml_version "$repo/$dir/plugin.toml")"
  latest="$(jq -r --arg id "$id" '.plugins[$id].version // empty' "$catalog")"
  released="$(jq -r --arg id "$id" '.plugins[$id].digest // empty' "$catalog")"
  if [ -z "$given" ] && [ -n "$released" ] && [ "$(plugin_digest "$dir")" = "$released" ]; then
    step "$id is unchanged since its release $latest (give a version to release it anyway)"
    return
  fi
  if [ -z "$given" ] && [ -n "$latest" ] && version_newer "$toml_v" "$latest"; then
    version="$(version_check "$toml_v")" || exit 1
  else
    version="$(choose_version "$given" "$latest" "$toml_v")" || exit 1
  fi
  stamp_toml_version "$repo/$dir/plugin.toml" "$version"
  digest="$(plugin_digest "$dir")"
  out="$artifacts/plugins/$id/$version"
  rm -rf "$out"
  mkdir -p "$out"
  step "$id $version (source b$digest)"

  local files=() keys=() platform os file
  if [ "$has_backend" = no ]; then
    file="$out/$id-$version-any.ccplugin"
    "$pack" build "$repo/$dir" --build-id "b$digest" -o "$file" >/dev/null
    files+=("$file")
    keys+=(any)
  else
    for platform in $RELEASE_PLATFORMS; do
      case "$platform" in darwin-*) os=macos ;; linux-*) os=linux ;; *) die "unknown platform $platform" ;; esac
      if [ "$platforms" != "[]" ] && ! jq -e --arg os "$os" 'index($os) != null' <<<"$platforms" >/dev/null; then
        continue
      fi
      file="$out/$id-$version-$platform.ccplugin"
      case "$platform" in
        darwin-arm64) "$pack" build "$repo/$dir" --build-id "b$digest" -o "$file" >/dev/null ;;
        linux-*) in_docker "$platform" "$out" plugin "$dir" "b$digest" "$(basename "$file")" ;;
      esac
      files+=("$file")
      keys+=("$platform")
    done
  fi
  [ ${#files[@]} -gt 0 ] || die "$id runs on $platforms; none of RELEASE_PLATFORMS ($RELEASE_PLATFORMS)"
  # What it was built from, after the build (which may update the plugin's lock file).
  digest="$(plugin_digest "$dir")"
  leak_scan "${files[@]}"
  (cd "$out" && shasum -a 256 -- *.ccplugin > SHA256SUMS)

  tag="plugin-$id-v$version"
  local entries="{}" i
  if [ $dry_run = 1 ]; then
    for i in "${!files[@]}"; do
      entries="$(jq --arg k "${keys[$i]}" --argjson a "$(asset_json "${files[$i]}" "$tag")" '.[$k] = $a' <<<"$entries")"
    done
  else
    notes="$name $version for CortexCode (source $commit). The app's updater installs it."
    rid="$(gh_release "$tag" "$name $version" "$notes" false false)"
    for i in "${!files[@]}"; do
      step "uploading $(basename "${files[$i]}")"
      gh_upload "$rid" "${files[$i]}"
      entries="$(jq --arg k "${keys[$i]}" --argjson a "$(asset_json "${files[$i]}" "$tag")" '.[$k] = $a' <<<"$entries")"
    done
  fi
  jq --arg id "$id" --arg version "$version" --arg tag "$tag" --arg name "$name" --arg min "$min_frame" \
    --argjson platforms "$platforms" --arg digest "$digest" --argjson files "$entries" \
    '.plugins[$id] = {version: $version, tag: $tag, name: $name, min_frame: $min, platforms: $platforms, digest: $digest, files: $files}' \
    "$catalog" > "$catalog.next"
  mv "$catalog.next" "$catalog"
}

for id in "${plugins[@]}"; do
  release_one "$id"
done

mkdir -p "$artifacts"
if [ $dry_run = 1 ]; then
  cp "$catalog" "$artifacts/catalog.preview.json"
  step "dry run: packages in artifacts/plugins/, the catalog it would publish in artifacts/catalog.preview.json"
else
  publish_catalog "$catalog"
  step "done; commit the plugin.toml versions"
fi
