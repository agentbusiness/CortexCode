#!/usr/bin/env bash
# Release whatever changed since the last release, in one go: the frame, each changed plugin, the
# app for new users and the npx launcher. Existing installs then update themselves (the updater
# reads the catalog); new users get the app through `npx cortexcode`.
#
#   scripts/release.sh             release what changed
#   scripts/release.sh --dry-run   build what changed into artifacts/, upload nothing
#   scripts/release.sh --plan      only say what would be released
#   add --dirty to release uncommitted changes as well
#
# What changed is decided by content, against the published catalog (scripts/release/digest.mjs
# says what each part is built from):
#   frame    the frame, its Electron shell and the frame's interface in protocol/ differ from the
#            released frame's (scripts/release-frame.sh)
#   plugins  each plugin whose digest differs from its release: its folder, the local crates it
#            builds with, the frame's interface and the protocol contracts its crates name
#            (scripts/release-plugin.sh). A contract change releases the plugins that use it; a
#            change to the frame's interface releases the frame and every plugin with a backend
#   app      the released app carries an older frame than the catalog's, or there is none yet
#            (scripts/release-app.sh); new users start from it, so it follows the frame
#   npx      npm/cortexcode differs from the published launcher (scripts/release-npm.sh); it
#            starts the installed app and downloads one only where there is none
#
# Uncommitted changes in a part to release stop the run (another session may be halfway through
# them), except the version lines the release scripts write; --dirty releases them anyway. Linux
# platforms in RELEASE_PLATFORMS need Docker, which is started when it is not running.
set -euo pipefail
self="${CORTEX_RELEASE_ORIGINAL:-$0}"
release_name="release"
# shellcheck source=release/lib.sh
. "$(cd "$(dirname "$self")" && pwd)/release/lib.sh"
release_snapshot "$self" "$@"

dry_run=0
plan_only=0
dirty=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --plan) plan_only=1 ;;
    --dirty) dirty=1 ;;
    *) die "unknown argument $arg (scripts/release.sh [--dry-run | --plan] [--dirty])" ;;
  esac
done

load_env
need_mac
need_tools node
build_packer
[ $dry_run = 1 ] || [ $plan_only = 1 ] || need_publishing

# ----- what changed ----------------------------------------------------------------------------

scratch work
catalog="$work/catalog.json"
fetch_catalog "$catalog"

frame=0
[ "$(frame_digest)" = "$(jq -r '.frame.digest // empty' "$catalog")" ] || frame=1

plugins=()
for toml in "$repo"/plugins/*/plugin.toml; do
  id="$(basename "$(dirname "$toml")")"
  [ "$(plugin_digest "plugins/$id")" = "$(jq -r --arg id "$id" '.plugins[$id].digest // empty' "$catalog")" ] || plugins+=("$id")
done

app=0
if [ $frame = 1 ] || [ "$(jq -r '.app.frame // empty' "$catalog")" != "$(jq -r '.frame.version // empty' "$catalog")" ] \
  || [ "$(jq -r '.app.version // empty' "$catalog")" = "" ]; then
  app=1
fi

npx=0
[ "$(npm_digest)" = "$(jq -r '.npm.digest // empty' "$catalog")" ] || npx=1

say_part() { printf '  %-8s %s\n' "$1" "$2"; }
step "what changed since the last release ($RELEASE_PLATFORMS):"
if [ $frame = 1 ]; then say_part frame "yes (now $(jq -r '.frame.version // "none"' "$catalog"))"; else say_part frame "no"; fi
if [ ${#plugins[@]} -gt 0 ]; then say_part plugins "${plugins[*]}"; else say_part plugins "none"; fi
if [ $app = 1 ]; then say_part app "yes (follows the frame)"; else say_part app "no"; fi
if [ $npx = 1 ]; then say_part npx "yes"; else say_part npx "no"; fi

if [ $frame = 0 ] && [ ${#plugins[@]} -eq 0 ] && [ $app = 0 ] && [ $npx = 0 ]; then
  step "nothing to release"
  exit 0
fi
[ $plan_only = 0 ] || exit 0

# ----- uncommitted changes ---------------------------------------------------------------------

paths=()
if [ $frame = 1 ]; then while IFS= read -r path; do paths+=("$path"); done < <(part_paths frame); fi
for id in "${plugins[@]+"${plugins[@]}"}"; do
  while IFS= read -r path; do paths+=("$path"); done < <(part_paths plugin "plugins/$id")
done
if [ $npx = 1 ]; then while IFS= read -r path; do paths+=("$path"); done < <(part_paths npm); fi
if [ ${#paths[@]} -gt 0 ]; then
  changes="$(uncommitted "${paths[@]}" | sort -u)"
  if [ -n "$changes" ] && [ $dirty = 0 ]; then
    printf '%s\n' "$changes" | head -40 | sed 's/^/  /' >&2
    die "these files have uncommitted changes: commit them first, or release them as they are with --dirty"
  fi
fi

# ----- Docker for the Linux builds -------------------------------------------------------------

case " $RELEASE_PLATFORMS " in
  *" linux-"*)
    if ! docker info >/dev/null 2>&1; then
      step "starting Docker for the Linux builds"
      open -a Docker
      for _ in $(seq 1 60); do
        docker info >/dev/null 2>&1 && break
        printf '.'
        sleep 2
      done
      printf '\n'
      docker info >/dev/null 2>&1 || die "Docker did not start: start Docker Desktop and run again"
    fi
    ;;
esac

# ----- release, in order -----------------------------------------------------------------------

# Each part is its own script, run as a new process (not as this snapshot).
part() {
  env -u CORTEX_RELEASE_SNAPSHOT -u CORTEX_RELEASE_ORIGINAL "$repo/scripts/$1" "${@:2}"
}
flags=()
if [ $dry_run = 1 ]; then
  flags+=(--dry-run)
  # Each step starts from the catalog the previous one would have published, and takes the files
  # it names from artifacts/.
  preview="$artifacts/catalog.preview.json"
  mkdir -p "$artifacts"
  cp "$catalog" "$preview"
  export RELEASE_CATALOG="$preview"
fi

started=$SECONDS
done_parts=()
# The frame first: plugins name it as their min_frame, and the app carries it.
if [ $frame = 1 ]; then
  part release-frame.sh "${flags[@]+"${flags[@]}"}"
  done_parts+=(frame)
fi
if [ ${#plugins[@]} -gt 0 ]; then
  part release-plugin.sh "${plugins[@]}" "${flags[@]+"${flags[@]}"}"
  done_parts+=("${#plugins[@]} plugins")
fi
if [ $app = 1 ]; then
  part release-app.sh "${flags[@]+"${flags[@]}"}"
  done_parts+=(app)
fi
if [ $npx = 1 ]; then
  part release-npm.sh "${flags[@]+"${flags[@]}"}"
  done_parts+=(npx)
fi

minutes=$(( (SECONDS - started + 59) / 60 ))
if [ $dry_run = 1 ]; then
  step "dry run of ${done_parts[*]} done in ${minutes} min: files in artifacts/, the catalog in artifacts/catalog.preview.json"
else
  step "released ${done_parts[*]} in ${minutes} min; installs update themselves. Commit the version lines the release wrote:"
  (cd "$repo" && git status --short -- frame/Cargo.toml ui/package.json ui/package-lock.json \
    'plugins/*/plugin.toml' npm/cortexcode/package.json) | sed 's/^/  /'
fi
