#!/usr/bin/env bash
# Publish the npx launcher (npm/cortexcode, the npm package `cortexcode`): `npx cortexcode`
# downloads the app named in the catalog for this machine, checks it, and starts it. The launcher
# changes rarely; the app it installs comes from the catalog (scripts/release-app.sh).
#
#   scripts/release-npm.sh [version] [--dry-run]
#
# Version: the one given (newer than the published one), else the next after the published one
# (each part 0 to 99), else package.json's. npm: NPM_TOKEN (.env.release) or this Mac's npm login.
set -euo pipefail
self="${CORTEX_RELEASE_ORIGINAL:-$0}"
release_name="release-npm"
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
need_tools node
build_packer

source_dir="$repo/npm/cortexcode"
name="$(jq -r .name "$source_dir/package.json")"
latest="$(npm view "$name" version 2>/dev/null || true)"
version="$(choose_version "$given" "$latest" "$(jq -r .version "$source_dir/package.json")")" || exit 1
step "$name $version"
stamp_json_version "$source_dir/package.json" "$version"

out="$artifacts/npm/$version"
rm -rf "$out"
mkdir -p "$out"
rsync -a --exclude node_modules "$source_dir/" "$out/package/"
(cd "$out/package" && node --check bin/cortexcode.mjs)
leak_scan "$out/package"

if [ $dry_run = 1 ]; then
  (cd "$out/package" && npm pack --silent --pack-destination "$out" >/dev/null)
  step "dry run: the package is in artifacts/npm/$version/"
  exit 0
fi
if [ -n "${NPM_TOKEN:-}" ]; then
  printf '//registry.npmjs.org/:_authToken=%s\n' "$NPM_TOKEN" > "$out/package/.npmrc"
  release_cleanup+=("$out/package/.npmrc")
fi
(cd "$out/package" && npm publish --access public)
step "published: npx $name"
