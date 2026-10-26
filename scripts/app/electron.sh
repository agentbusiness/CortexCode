# Electron branded as CortexCode for macOS, shared by scripts/build-app.sh and the release scripts.
# Source it, then: electron_mac_branded <electron npm package> <destination package folder> <icns>
#
# The destination is a package folder like node_modules/electron: dist/CortexCode.app (renamed,
# its executable, name and icon CortexCode, so the Dock, Force Quit and Activity Monitor show
# CortexCode; ad-hoc signed), path.txt (the executable inside dist/) and version.

electron_mac_branded() {
  local src="$1" dst="$2" icns="$3"
  rm -rf "$dst"
  mkdir -p "$dst/dist"
  local source_app
  source_app="$(ls -d "$src"/dist/*.app | head -1)"
  ditto "$source_app" "$dst/dist/CortexCode.app"
  local macos="$dst/dist/CortexCode.app/Contents/MacOS"
  [ -f "$macos/Electron" ] && mv "$macos/Electron" "$macos/CortexCode"
  local plist="$dst/dist/CortexCode.app/Contents/Info.plist" key
  for key in CFBundleName CFBundleDisplayName CFBundleExecutable; do
    /usr/libexec/PlistBuddy -c "Set :$key CortexCode" "$plist" 2>/dev/null || /usr/libexec/PlistBuddy -c "Add :$key string CortexCode" "$plist"
  done
  cp "$icns" "$dst/dist/CortexCode.app/Contents/Resources/CortexCode.icns"
  /usr/libexec/PlistBuddy -c "Set :CFBundleIconFile CortexCode.icns" "$plist" 2>/dev/null || /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string CortexCode.icns" "$plist"
  codesign --force --deep --sign - "$dst/dist/CortexCode.app" >/dev/null 2>&1
  printf '%s' "CortexCode.app/Contents/MacOS/CortexCode" > "$dst/path.txt"
  /usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"], end="")' "$src/package.json" > "$dst/version"
}
