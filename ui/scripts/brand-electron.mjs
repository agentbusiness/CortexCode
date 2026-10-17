// A development run is Electron's own bundle (node_modules/electron/dist/Electron.app), and macOS
// takes the Dock label, the Force Quit entry and the icon from that bundle's Info.plist, not from
// app.setName(). This brands the bundle as CortexCode after install (postinstall). The bundle is
// ad-hoc, linker-signed with Info.plist unbound and no sealed resources, so editing it keeps the
// signature valid. Idempotent; a no-op off macOS or without the bundle.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { UI_DIR } from './shared.mjs';

const NAME = 'CortexCode';
const ICNS = path.join(UI_DIR, 'shell', 'assets', 'CortexCode.icns');
const electronDir = path.join(UI_DIR, 'node_modules', 'electron');
const dist = path.join(electronDir, 'dist');
const bundle = path.join(dist, `${NAME}.app`);
const contents = path.join(bundle, 'Contents');
const plist = path.join(contents, 'Info.plist');

if (process.platform !== 'darwin') process.exit(0);
// The Dock labels a bundle by the name it registered for its path, so the bundle itself is renamed;
// electron's CLI finds the executable through path.txt.
const original = path.join(dist, 'Electron.app');
if (fs.existsSync(original) && !fs.existsSync(bundle)) fs.renameSync(original, bundle);
if (!fs.existsSync(plist)) process.exit(0);
// The Dock tile and Activity Monitor show the executable's name: rename it as well (the frameworks
// load by @executable_path, which does not depend on the file name; the ad-hoc signature stays valid).
const macos = path.join(contents, 'MacOS');
if (fs.existsSync(path.join(macos, 'Electron')) && !fs.existsSync(path.join(macos, NAME))) {
  fs.renameSync(path.join(macos, 'Electron'), path.join(macos, NAME));
}
const pathFile = path.join(electronDir, 'path.txt');
const executable = `${NAME}.app/Contents/MacOS/${NAME}`;
if (fs.readFileSync(pathFile, 'utf8').trim() !== executable) fs.writeFileSync(pathFile, executable);

/** @param {string} command */
function plistBuddy(command) {
  return execFileSync('/usr/libexec/PlistBuddy', ['-c', command, plist], { encoding: 'utf8' }).trim();
}

/** @param {string} key @param {string} value */
function set(key, value) {
  let current = null;
  try {
    current = plistBuddy(`Print :${key}`);
  } catch {
    // Absent.
  }
  if (current === value) return false;
  plistBuddy(current === null ? `Add :${key} string ${value}` : `Set :${key} ${value}`);
  return true;
}

let changed = set('CFBundleName', NAME) | set('CFBundleDisplayName', NAME) | set('CFBundleExecutable', NAME);
if (fs.existsSync(ICNS)) {
  const target = path.join(contents, 'Resources', `${NAME}.icns`);
  if (!fs.existsSync(target) || fs.readFileSync(target).compare(fs.readFileSync(ICNS)) !== 0) {
    fs.copyFileSync(ICNS, target);
    changed = 1;
  }
  changed |= set('CFBundleIconFile', `${NAME}.icns`);
}
if (changed) {
  const now = new Date();
  fs.utimesSync(bundle, now, now);
  console.log(`[brand-electron] ${bundle} is ${NAME}`);
}
// Register the bundle under its name with LaunchServices (what the Dock labels tiles from).
try {
  execFileSync('/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister', ['-f', bundle]);
} catch {
  // Best effort; the Dock reads the plist on the next launch anyway.
}
