# shellcheck shell=bash
# Shared by the release scripts (scripts/release-*.sh). Sourced, never run. See docs/RELEASES.md.
#
# Everything is built on the owner's Mac (Apple Silicon); Linux builds run in Docker on it. Files go
# to GitHub releases of $RELEASE_REPO; the catalog (release `catalog`, file `catalog.json`)
# names the current version of every plugin, the frame and the app.
#
# Configuration: `.env.release` in the repository root (git-ignored), or the environment:
#   GITHUB_TOKEN        a token of the repository's owner that may create releases (required to upload)
#   RELEASE_REPO        default agentbusiness/CortexCode
#   RELEASE_PLATFORMS   default "darwin-arm64"; "darwin-arm64 linux-arm64 linux-x64" for all (Docker)
#   RELEASE_DMG         yes: also a macOS .dmg in the app release (default no: the app is not signed
#                       yet, so macOS users start it with `npx cortexcode`)
#   NPM_TOKEN           for scripts/release-npm.sh (else the npm login of this Mac)
#   RELEASE_FORBIDDEN   names no released file may hold (extended regex, any case); required
#   RELEASE_CATALOG     a local catalog to start from instead of the published one (scripts/release.sh
#                       --dry-run chains its steps through artifacts/catalog.preview.json); files it
#                       names are then taken from artifacts/ when they are there
#
# One release script runs at a time: each rewrites the catalog.

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
pack="$repo/packer/target/release/cortex-pack"
artifacts="$repo/artifacts"
release_name="${release_name:-release}"
# Shared code every plugin builds with (the plugin digest covers it, as in scripts/build-app.sh).
shared_sources=(protocol model-kit lang-kit ui/sdk rust-toolchain.toml)
# What the frame release is built from (the frame digest covers it).
frame_sources=(frame cli packer protocol ui scripts/app rust-toolchain.toml)
# The npx launcher.
npm_sources=(npm/cortexcode)
# Names that must never be in a released file: RELEASE_FORBIDDEN (load_env), kept out of the
# repository so the list itself is never released.
forbidden=""

step() { printf '%s: %s\n' "$release_name" "$*"; }
die() { printf '%s: %s\n' "$release_name" "$*" >&2; exit 1; }

# ----- the run ---------------------------------------------------------------------------------

release_cleanup=()
release_on_exit() {
  local path
  for path in "${release_cleanup[@]+"${release_cleanup[@]}"}"; do rm -rf "$path"; done
}
trap release_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Run from a copy of the calling script, so an edit to it during the build (CortexCode edits its
# own code) cannot break the run. Call first: release_snapshot "$self" "$@".
release_snapshot() {
  local script="$1"
  shift
  if [ "${CORTEX_RELEASE_SNAPSHOT:-}" = "1" ]; then
    release_cleanup+=("$0")
    return
  fi
  local copy
  copy="$(mktemp "${TMPDIR:-/tmp}/cortexcode-release.XXXXXX")"
  cp "$script" "$copy"
  chmod +x "$copy"
  CORTEX_RELEASE_SNAPSHOT=1 CORTEX_RELEASE_ORIGINAL="$script" exec bash "$copy" "$@"
}

# scratch <variable>: a new folder, removed at the end of the run, into <variable> (not in a
# subshell, so the run remembers to remove it).
scratch() {
  local dir
  dir="$(mktemp -d "${TMPDIR:-/tmp}/cortexcode-release-work.XXXXXX")"
  release_cleanup+=("$dir")
  printf -v "$1" '%s' "$dir"
}

load_env() {
  if [ -f "$repo/.env.release" ]; then
    set -a
    # shellcheck disable=SC1091
    . "$repo/.env.release"
    set +a
  fi
  RELEASE_REPO="${RELEASE_REPO:-agentbusiness/CortexCode}"
  RELEASE_PLATFORMS="${RELEASE_PLATFORMS:-darwin-arm64}"
  RELEASE_DMG="${RELEASE_DMG:-no}"
  forbidden="${RELEASE_FORBIDDEN:-}"
}

need_mac() {
  [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || die "releases are built on an Apple Silicon Mac"
}

need_tools() {
  local tool
  for tool in git curl jq shasum tar cargo npm "$@"; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
  done
}

# Fail before building when the upload could not work.
need_publishing() {
  [ -n "${GITHUB_TOKEN:-}" ] || die "GITHUB_TOKEN is not set (.env.release): uploads to GitHub need a token of the repository's owner"
  local allowed
  allowed="$(gh_api GET "/repos/$RELEASE_REPO" | jq -r '.permissions.push // false')" || die "cannot reach $RELEASE_REPO with GITHUB_TOKEN"
  [ "$allowed" = true ] || die "GITHUB_TOKEN may not publish to $RELEASE_REPO"
}

build_packer() {
  step "building cortex-pack"
  (cd "$repo/packer" && cargo build --release --quiet)
}

# ----- versions --------------------------------------------------------------------------------

version_check() { "$pack" release check "$1"; }
version_next() { "$pack" release next "$1"; }
version_newer() { "$pack" release newer "$1" "$2" >/dev/null 2>&1; }

# choose_version <given or ""> <released or ""> <first>: the given version (newer than the
# released one), else the next after the released one, else <first>.
choose_version() {
  local given="$1" latest="$2" first="$3"
  if [ -n "$given" ]; then
    given="$(version_check "$given")" || exit 1
    if [ -n "$latest" ] && ! version_newer "$given" "$latest"; then
      die "$given is not newer than the released $latest"
    fi
    printf '%s\n' "$given"
  elif [ -n "$latest" ]; then
    version_next "$latest"
  else
    version_check "$first"
  fi
}

# The first `version = "..."` line of a TOML file ([package] of a Cargo.toml, [plugin] of a plugin.toml).
toml_version() { awk -F'"' '/^version = "/ { print $2; exit }' "$1"; }
stamp_toml_version() { perl -0pi -e 's/^version = "[^"]*"/version = "'"$2"'"/m' "$1"; }
stamp_json_version() {
  local file="$1" version="$2" tmp
  tmp="$(mktemp)"
  jq --arg v "$version" '.version = $v | if .packages and .packages[""] then .packages[""].version = $v else . end' "$file" > "$tmp"
  mv "$tmp" "$file"
}

# ----- sources and digests ---------------------------------------------------------------------

# The source files git tracks or would track under the given paths, that exist (NUL-separated).
sources() {
  (cd "$repo" && git ls-files -co --exclude-standard -z -- "$@") | while IFS= read -r -d '' file; do
    if [ -f "$repo/$file" ]; then printf '%s\0' "$file"; fi
  done
}

# The digest of a plugin's source and the shared code it builds with (as scripts/build-app.sh).
plugin_digest() {
  (cd "$repo" && sources "$1" "${shared_sources[@]}" | xargs -0 shasum -a 256 | shasum -a 256 | cut -c1-12)
}

# The digest of what the frame release is built from.
frame_digest() {
  (cd "$repo" && sources "${frame_sources[@]}" | xargs -0 shasum -a 256 | shasum -a 256 | cut -c1-12)
}

# The digest of the npx launcher's source.
npm_digest() {
  (cd "$repo" && sources "${npm_sources[@]}" | xargs -0 shasum -a 256 | shasum -a 256 | cut -c1-12)
}

# Files under <paths> with uncommitted changes other than version lines (the release scripts write
# those), one per line.
uncommitted() {
  local file
  (cd "$repo" && git status --porcelain -z --untracked-files=all -- "$@") | while IFS= read -r -d '' entry; do
    file="${entry:3}"
    case "$entry" in
      " M"*|"M "*|"MM"*)
        # Only version lines changed: `version = "..."` (TOML) or `"version": "..."` (JSON).
        if ! (cd "$repo" && git diff -U0 HEAD -- "$file") | grep -E '^[-+][^-+]' | grep -qvE '^[-+](version = "|  "version": ")'; then
          continue
        fi
        ;;
    esac
    printf '%s\n' "$file"
  done
}

# Copy the source outside plugins/ (the frame's part) into <dir>.
copy_frame_source() {
  mkdir -p "$1"
  (cd "$repo" && git ls-files -co --exclude-standard -z -- . ':(exclude)plugins/**' | while IFS= read -r -d '' file; do
    if [ -f "$file" ]; then printf '%s\0' "$file"; fi
  done) | (cd "$repo" && rsync -a --from0 --files-from=- ./ "$1/")
}

# ----- files -----------------------------------------------------------------------------------

sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
size_of() { stat -f%z "$1"; }

# A .tar.gz of the contents of <dir> (no ./ prefix, no macOS metadata files or extended attributes:
# GNU tar on Linux warns about every macOS attribute header).
tar_dir() {
  local dir="$1" out="$2"
  rm -f "$out"
  (cd "$dir" && COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs --no-acls --no-fflags -czf "$out" -- *)
}

# Refuse a file or folder that holds a forbidden name or the release configuration.
leak_scan() {
  local item found
  [ -n "$forbidden" ] || die "RELEASE_FORBIDDEN is not set (.env.release): the names no released file may hold, e.g. 'name1|name2'"
  for item in "$@"; do
    if [ -d "$item" ]; then
      found="$(LC_ALL=C grep -r -a -i -l -E "$forbidden" "$item" 2>/dev/null | head -3 || true)"
      [ -z "$found" ] || die "$item holds a forbidden name ($forbidden): $found"
      [ -z "$(find "$item" -name '.env.release' -print -quit)" ] || die "$item holds .env.release"
    else
      case "$item" in
        *.tar.gz|*.ccplugin)
          # grep -c reads to the end: tar is never cut off (an early exit would hide a match).
          found="$(tar -xzOf "$item" | LC_ALL=C grep -a -i -c -E "$forbidden" || true)"
          [ "${found:-0}" = 0 ] || die "$item holds a forbidden name ($forbidden)"
          found="$(tar -tzf "$item" | grep -c '\.env\.release$' || true)"
          [ "${found:-0}" = 0 ] || die "$item holds .env.release"
          ;;
        *)
          if LC_ALL=C grep -a -i -q -E "$forbidden" "$item"; then die "$item holds a forbidden name ($forbidden)"; fi
          ;;
      esac
    fi
  done
}

# Download a catalog asset (JSON {name, url, sha256, size}) into <dir>, checking it.
fetch_asset() {
  local asset="$1" dir="$2" name url sha
  name="$(jq -r .name <<<"$asset")"
  url="$(jq -r .url <<<"$asset")"
  sha="$(jq -r .sha256 <<<"$asset")"
  mkdir -p "$dir"
  local copy=""
  [ -z "${RELEASE_CATALOG:-}" ] || copy="$(find "$artifacts" -type f -name "$name" -not -path "*/downloads/*" 2>/dev/null | head -1)"
  if [ -n "$copy" ]; then
    cp "$copy" "$dir/$name"
  else
    curl -fsSL -o "$dir/$name" "$url" || die "cannot download $url"
  fi
  [ "$(sha256_of "$dir/$name")" = "$sha" ] || die "$name does not match the catalog's checksum"
  printf '%s' "$dir/$name"
}

# ----- GitHub ----------------------------------------------------------------------------------

# gh_api <METHOD> <path> [json body]: the response body; fails on an HTTP error.
gh_api() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-fsSL -X "$method" -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28")
  [ -z "$body" ] || args+=(-H "Content-Type: application/json" --data "$body")
  curl "${args[@]}" "https://api.github.com$path"
}

# gh_release <tag> <title> <notes> <make_latest: true|false> <prerelease: true|false>: the release
# id, created when the tag has none (the tag is made on the repository's default branch).
gh_release() {
  local tag="$1" title="$2" notes="$3" latest="$4" prerelease="$5" found body
  found="$(curl -sSL -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $GITHUB_TOKEN" "https://api.github.com/repos/$RELEASE_REPO/releases/tags/$tag")"
  if [ "$found" = 200 ]; then
    gh_api GET "/repos/$RELEASE_REPO/releases/tags/$tag" | jq -r .id
    return
  fi
  body="$(jq -n --arg tag "$tag" --arg title "$title" --arg notes "$notes" --arg latest "$latest" --argjson pre "$prerelease" \
    '{tag_name: $tag, name: $title, body: $notes, make_latest: $latest, prerelease: $pre}')"
  gh_api POST "/repos/$RELEASE_REPO/releases" "$body" | jq -r .id
}

# gh_upload <release id> <file> [asset name]: replaces an asset of the same name.
gh_upload() {
  local id="$1" file="$2" name="${3:-$(basename "$2")}" existing
  existing="$(gh_api GET "/repos/$RELEASE_REPO/releases/$id/assets?per_page=100" | jq -r --arg n "$name" '.[] | select(.name == $n) | .id')"
  [ -z "$existing" ] || gh_api DELETE "/repos/$RELEASE_REPO/releases/assets/$existing" >/dev/null
  curl -fsSL -X POST -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" \
    -H "Content-Type: application/octet-stream" --data-binary "@$file" \
    "https://uploads.github.com/repos/$RELEASE_REPO/releases/$id/assets?name=$name" >/dev/null || die "cannot upload $name"
}

# asset_json <file> <tag>: the catalog entry of an uploaded file.
asset_json() {
  local file="$1" tag="$2" name
  name="$(basename "$file")"
  jq -n --arg name "$name" --arg url "https://github.com/$RELEASE_REPO/releases/download/$tag/$name" \
    --arg sha "$(sha256_of "$file")" --argjson size "$(size_of "$file")" '{name: $name, url: $url, sha256: $sha, size: $size}'
}

# ----- the catalog -----------------------------------------------------------------------------

catalog_url() { printf 'https://github.com/%s/releases/download/catalog/catalog.json' "$RELEASE_REPO"; }

# fetch_catalog <out>: the published catalog (an empty one before the first release).
fetch_catalog() {
  local out="$1" code
  if [ -n "${RELEASE_CATALOG:-}" ] && [ -f "$RELEASE_CATALOG" ]; then
    cp "$RELEASE_CATALOG" "$out"
    return
  fi
  code="$(curl -sSL -o "$out" -w '%{http_code}' "$(catalog_url)")"
  case "$code" in
    200) jq -e '.format == "cortexcode-catalog/1"' "$out" >/dev/null || die "the published catalog is not a catalog" ;;
    404) step "no catalog published yet: starting one"
         jq -n --arg repo "$RELEASE_REPO" '{format: "cortexcode-catalog/1", updated: "", repo: $repo, plugins: {}}' > "$out" ;;
    *) die "cannot download the catalog ($code)" ;;
  esac
}

# publish_catalog <catalog.json>: swap it in (uploaded beside the old one, then renamed).
publish_catalog() {
  local catalog="$1" work id old next
  scratch work
  jq --arg now "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg repo "$RELEASE_REPO" '.updated = $now | .repo = $repo' "$catalog" > "$work/catalog.json"
  cp "$work/catalog.json" "$work/catalog.json.next"
  id="$(gh_release catalog "Catalog" "The list of the current CortexCode releases, read by the app's updater." false true)"
  gh_upload "$id" "$work/catalog.json.next"
  old="$(gh_api GET "/repos/$RELEASE_REPO/releases/$id/assets?per_page=100" | jq -r '.[] | select(.name == "catalog.json") | .id')"
  next="$(gh_api GET "/repos/$RELEASE_REPO/releases/$id/assets?per_page=100" | jq -r '.[] | select(.name == "catalog.json.next") | .id')"
  [ -z "$old" ] || gh_api DELETE "/repos/$RELEASE_REPO/releases/assets/$old" >/dev/null
  gh_api PATCH "/repos/$RELEASE_REPO/releases/assets/$next" '{"name":"catalog.json"}' >/dev/null
  cp "$work/catalog.json" "$artifacts/catalog.json"
  step "catalog published: $(catalog_url)"
}

# ----- Linux builds in Docker ------------------------------------------------------------------

docker_arch() {
  case "$1" in
    linux-x64) printf amd64 ;;
    linux-arm64) printf arm64 ;;
    *) die "$1 is not a Linux platform" ;;
  esac
}

# in_docker <platform> <out dir> <in-docker.sh arguments...>: run scripts/release/in-docker.sh in a
# Debian container of that architecture, with this repository read-only at /src and <out> at /out.
in_docker() {
  local platform="$1" out="$2" arch image
  shift 2
  command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 || die "Docker is needed (and running) for $platform"
  arch="$(docker_arch "$platform")"
  image="cortexcode-release:$arch"
  docker build --quiet --platform "linux/$arch" -t "$image" -f "$repo/scripts/release/linux.Dockerfile" "$repo/scripts/release" >/dev/null
  mkdir -p "$out"
  docker run --rm --platform "linux/$arch" \
    -v "$repo:/src:ro" -v "$out:/out" \
    -v "cortexcode-release-work-$arch:/work" -v "cortexcode-release-cargo-$arch:/usr/local/cargo/registry" \
    "$image" bash /src/scripts/release/in-docker.sh "$@"
}

# Electron for a Linux platform as a package folder (dist/ with the `electron` executable,
# path.txt, version) in <dir>.
electron_linux() {
  local platform="$1" version="$2" dir="$3" arch zip
  case "$platform" in linux-x64) arch=x64 ;; linux-arm64) arch=arm64 ;; esac
  zip="$(cd "$repo/ui" && node --input-type=module -e "
    const { downloadArtifact } = await import('@electron/get');
    console.log(await downloadArtifact({ version: '$version', platform: 'linux', arch: '$arch', artifactName: 'electron' }));
  ")" || die "cannot download Electron $version for $platform"
  rm -rf "$dir"
  mkdir -p "$dir/dist"
  unzip -q "$zip" -d "$dir/dist"
  printf '%s' electron > "$dir/path.txt"
  printf '%s' "$version" > "$dir/version"
}
