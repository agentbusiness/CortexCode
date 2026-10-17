#!/usr/bin/env bash
# Print the newest installed macOS SDK that the build's default linker can use.
set -euo pipefail

default_sdk="$(xcrun --sdk macosx --show-sdk-path)"
sdk_directory="$(dirname "$default_sdk")"
probe_directory="$(mktemp -d "${TMPDIR:-/tmp}/cortexcode-sdk.XXXXXX")"
trap 'rm -rf "$probe_directory"' EXIT
printf 'int main(void) { return 0; }\n' > "$probe_directory/probe.c"

# Ignore alias symlinks (MacOSX.sdk, MacOSX27.sdk); sort real SDKs by version.
while IFS= read -r sdk; do
  if cc -isysroot "$sdk" "$probe_directory/probe.c" \
    -framework Security -framework CoreML -framework Metal -liconv \
    -o "$probe_directory/probe" 2> "$probe_directory/link.log"; then
    printf '%s\n' "$sdk"
    exit 0
  fi
  printf 'Skipping SDK %s: installed compiler/linker failed its link check.\n' "$sdk" >&2
done < <(
  for sdk in "$sdk_directory"/MacOSX[0-9]*.sdk; do
    if [[ -d "$sdk" && ! -L "$sdk" ]]; then
      printf '%s\n' "$sdk"
    fi
  done | sort -Vr
)

printf 'No installed macOS SDK passed the link check. Update the selected Apple developer tools.\n' >&2
if [[ -f "$probe_directory/link.log" ]]; then
  cat "$probe_directory/link.log" >&2
fi
exit 1
