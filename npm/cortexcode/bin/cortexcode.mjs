#!/usr/bin/env node
// `npx cortexcode`: start CortexCode. The first run downloads the app for this machine from the
// official releases (the catalog), checks its checksum and unpacks it into ~/.cortexcode/app/<version>/;
// later runs start the installed app at once, without looking at the catalog: the app updates its
// frame and plugins itself, so a newer app release is only for machines that have none. The app runs
// on its own: closing the terminal does not quit it (its start output goes to ~/.cortexcode/logs/start.log).
// Every start also keeps a starter in the Applications folder (macOS) or the app list (Linux).
// Arguments go to the app (`--headless`, `--safe`).

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, closeSync, copyFileSync, createWriteStream, existsSync, mkdirSync, openSync, readFileSync, readdirSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { pipeline } from 'node:stream/promises';

const REPO = process.env.CORTEXCODE_REPO || 'agentbusiness/CortexCode';
// CORTEXCODE_CATALOG: another catalog (a mirror, or a release under test).
const CATALOG_URL = process.env.CORTEXCODE_CATALOG || `https://github.com/${REPO}/releases/download/catalog/catalog.json`;
const HOME = process.env.CORTEX_HOME || join(homedir(), '.cortexcode');
const APPS = join(HOME, 'app');
const READY = '.ready';
const START_LOG = join(HOME, 'logs', 'start.log');
const KEEP = 2;

const say = (text) => process.stderr.write(`cortexcode: ${text}\n`);

function platform() {
  const key = `${process.platform}-${process.arch}`;
  const known = { 'darwin-arm64': 'darwin-arm64', 'linux-x64': 'linux-x64', 'linux-arm64': 'linux-arm64' };
  if (!known[key]) throw new Error(`CortexCode runs on Apple Silicon Macs and on Linux (x64, arm64), not on ${key}`);
  return known[key];
}

/** `0.3.7` as a comparable triple, or null. */
function version(text) {
  const match = /^v?(\d+)\.(\d+)\.(\d+)(\+.*)?$/.exec(String(text ?? '').trim());
  return match ? [Number(match[1]), Number(match[2]), Number(match[3])] : null;
}

function compare(a, b) {
  for (let i = 0; i < 3; i += 1) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

/** The response to a GET of `url`, after redirects (GitHub sends release files from another host). */
function get(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith('https:') ? https : http;
    const request = client.get(url, { headers: { 'user-agent': 'cortexcode-npx' } }, (response) => {
      const status = response.statusCode ?? 0;
      if (status >= 300 && status < 400 && response.headers.location && redirects > 0) {
        response.resume();
        resolve(get(new URL(response.headers.location, url).toString(), redirects - 1));
        return;
      }
      if (status !== 200) {
        response.resume();
        reject(new Error(`${url} answered ${status}`));
        return;
      }
      resolve(response);
    });
    request.on('error', reject);
  });
}

/** The release catalog: the current version of the app and every part, with their files. */
async function catalog() {
  const response = await fetch(CATALOG_URL, { headers: { 'user-agent': 'cortexcode-npx' } });
  if (!response.ok) throw new Error(`${CATALOG_URL} answered ${response.status}`);
  const parsed = await response.json();
  if (parsed.format !== 'cortexcode-catalog/1') throw new Error(`unknown catalog format ${parsed.format}`);
  return parsed;
}

/** Installed app versions, newest first. */
function installed() {
  if (!existsSync(APPS)) return [];
  return readdirSync(APPS)
    .filter((name) => version(name) && existsSync(join(APPS, name, READY)))
    .sort((a, b) => compare(version(b), version(a)));
}

function executable(dir) {
  return process.platform === 'darwin' ? join(dir, 'CortexCode.app', 'Contents', 'MacOS', 'CortexCode') : join(dir, 'cortexcode', 'CortexCode');
}

/** Download the app archive, check size and SHA-256, unpack it beside the others. */
async function install(app, asset) {
  mkdirSync(APPS, { recursive: true });
  const archive = join(APPS, `.download-${app.version}.tar.gz`);
  const staging = join(APPS, `.staging-${app.version}`);
  say(`downloading CortexCode ${app.version} (${Math.round(asset.size / 1e6)} MB)`);
  // node:http rather than fetch: fetch's stream fails when a server closes the connection while the
  // file write is behind.
  const body = await get(asset.url);
  const hash = createHash('sha256');
  let received = 0;
  let reported = 0;
  body.on('data', (chunk) => {
    hash.update(chunk);
    received += chunk.length;
    const percent = asset.size ? Math.floor((received * 100) / asset.size) : 0;
    if (percent >= reported + 10) {
      reported = percent;
      process.stderr.write(`\rcortexcode: ${percent}%`);
    }
  });
  await pipeline(body, createWriteStream(archive));
  process.stderr.write('\n');
  if (received !== asset.size || hash.digest('hex') !== String(asset.sha256).toLowerCase()) {
    rmSync(archive, { force: true });
    throw new Error(`${asset.name} does not match the catalog (size or checksum); it was not used`);
  }
  rmSync(staging, { recursive: true, force: true });
  mkdirSync(staging, { recursive: true });
  await run('tar', ['-xzf', archive, '-C', staging]);
  rmSync(archive, { force: true });
  const target = join(APPS, app.version);
  rmSync(target, { recursive: true, force: true });
  renameSync(staging, target);
  writeFileSync(join(target, READY), '');
  say(`CortexCode ${app.version} is installed in ${target}`);
}

function run(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'inherit'] });
    child.on('error', reject);
    child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`${command} failed (${code})`))));
  });
}

/** The pid of a CortexCode running for this home, if any (the frame writes run/frame.json). */
function running() {
  try {
    const { pid } = JSON.parse(readFileSync(join(HOME, 'run', 'frame.json'), 'utf8'));
    process.kill(pid, 0);
    return pid;
  } catch {
    return null;
  }
}

// The starter: a small app in the Applications folder (macOS) or the desktop's app list (Linux) that
// starts the newest installed version, so the app opens without a terminal. It copies nothing of the
// app, so it keeps working when updates add versions and prune old ones.
const STARTER_ID = 'ai.cortexcode.starter';

/** `text` quoted for sh. */
const shq = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;

/** Write `text` to `path` unless it already holds it; true when it wrote. */
function writeIfChanged(path, text, mode = 0o644) {
  mkdirSync(dirname(path), { recursive: true });
  if (existsSync(path) && readFileSync(path, 'utf8') === text) return false;
  writeFileSync(path, text);
  chmodSync(path, mode);
  return true;
}

/** The starter's script: the newest ready version in APPS, started as this launcher starts it. */
function starterScript() {
  const start =
    process.platform === 'darwin'
      ? 'exec open "$apps/$version/CortexCode.app" --stdout "$log" --stderr "$log" --args "$@"'
      : 'exec "$apps/$version/cortexcode/CortexCode" "$@" >>"$log" 2>&1';
  const missing =
    process.platform === 'darwin'
      ? `osascript -e 'display alert "CortexCode is not installed" message "Run npx cortexcode in a terminal to install it."' >/dev/null 2>&1`
      : 'command -v notify-send >/dev/null 2>&1 && notify-send CortexCode "CortexCode is not installed: run npx cortexcode in a terminal."';
  return `#!/bin/sh
# CortexCode starter, written by npx cortexcode: starts the newest app installed in the folder below.
apps=${shq(APPS)}
log=${shq(START_LOG)}
mkdir -p "$(dirname "$log")"
version="$(ls "$apps" 2>/dev/null | grep -E '^[0-9]+\\.[0-9]+\\.[0-9]+$' | sort -t. -k1,1n -k2,2n -k3,3n |
  while read -r v; do [ -e "$apps/$v/${READY}" ] && echo "$v"; done | tail -n 1)"
if [ -z "$version" ]; then
  ${missing}
  exit 1
fi
${start}
`;
}

/** macOS: CortexCode.app in /Applications, or in ~/Applications when /Applications is not writable. */
function macStarter(appDir) {
  const places = ['/Applications', join(homedir(), 'Applications')].map((place) => join(place, 'CortexCode.app'));
  const ours = (bundle) => {
    try {
      return readFileSync(join(bundle, 'Contents', 'Info.plist'), 'utf8').includes(STARTER_ID);
    } catch {
      return false;
    }
  };
  // A CortexCode.app that is not a starter (the disk image's app) is the user's own: leave it.
  if (places.some((bundle) => existsSync(join(bundle, 'Contents', 'Info.plist')) && !ours(bundle))) return;
  let bundle = places.find(ours);
  const fresh = !bundle;
  if (!bundle) {
    for (const place of places) {
      try {
        mkdirSync(join(place, 'Contents', 'MacOS'), { recursive: true });
        bundle = place;
        break;
      } catch {
        // not writable: the next place
      }
    }
  }
  if (!bundle) throw new Error('neither /Applications nor ~/Applications is writable');
  // LSUIElement: the starter only hands over to the app, so it never shows in the Dock itself.
  writeIfChanged(
    join(bundle, 'Contents', 'Info.plist'),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>CortexCode</string>
	<key>CFBundleDisplayName</key>
	<string>CortexCode</string>
	<key>CFBundleIdentifier</key>
	<string>${STARTER_ID}</string>
	<key>CFBundleExecutable</key>
	<string>CortexCode</string>
	<key>CFBundleIconFile</key>
	<string>CortexCode.icns</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>1.0</string>
	<key>LSUIElement</key>
	<true/>
</dict>
</plist>
`,
  );
  writeIfChanged(join(bundle, 'Contents', 'MacOS', 'CortexCode'), starterScript(), 0o755);
  const icon = join(bundle, 'Contents', 'Resources', 'CortexCode.icns');
  if (!existsSync(icon)) {
    mkdirSync(dirname(icon), { recursive: true });
    copyFileSync(join(appDir, 'CortexCode.app', 'Contents', 'Resources', 'CortexCode.icns'), icon);
  }
  if (fresh) say(`CortexCode is in ${dirname(bundle)}: open it from there next time`);
}

/** Linux: a desktop entry in the app list ($XDG_DATA_HOME/applications) that runs APPS/start. */
function linuxStarter(appDir) {
  const script = join(APPS, 'start');
  const icon = join(APPS, 'cortexcode.png');
  writeIfChanged(script, starterScript(), 0o755);
  if (!existsSync(icon)) copyFileSync(join(appDir, 'cortexcode', 'ui', 'shell', 'assets', 'cortexcode-512.png'), icon);
  const data = process.env.XDG_DATA_HOME || join(homedir(), '.local', 'share');
  const entry = join(data, 'applications', 'cortexcode.desktop');
  const fresh = !existsSync(entry);
  writeIfChanged(
    entry,
    `[Desktop Entry]
Type=Application
Name=CortexCode
Comment=Local coding agent
Exec="${script}"
Icon=${icon}
Terminal=false
Categories=Development;
`,
  );
  if (fresh) say('CortexCode is in your applications: open it from there next time');
}

/** Keep the newest versions (and the one starting now). */
function prune(keep) {
  installed()
    .slice(KEEP)
    .filter((name) => name !== keep)
    .forEach((name) => rmSync(join(APPS, name), { recursive: true, force: true }));
}

async function main() {
  const args = process.argv.slice(2);
  if (args.includes('--help') || args.includes('-h')) {
    process.stdout.write('npx cortexcode [--headless] [--safe]\n\nStarts CortexCode; the first run downloads it into ~/.cortexcode/app and adds it to your applications.\n');
    return 0;
  }
  const pid = running();
  if (pid) {
    say(`CortexCode is already running (pid ${pid})`);
    return 0;
  }
  const key = platform();
  // The installed app updates itself (frame and plugins): only a machine without one downloads.
  if (installed().length === 0) {
    try {
      const listed = await catalog();
      const files = listed.app?.files?.[key];
      if (listed.app && files?.archive) await install(listed.app, files.archive);
      else say(`no CortexCode app is released for ${key} yet`);
    } catch (error) {
      say(`cannot download CortexCode (${error.message})`);
    }
  }
  const chosen = installed()[0];
  if (!chosen) {
    say('CortexCode is not installed and could not be downloaded');
    return 1;
  }
  prune(chosen);
  // Only for the user's own home: a scratch home (CORTEX_HOME) must not take over the starter.
  if (!process.env.CORTEX_HOME) {
    try {
      if (process.platform === 'darwin') macStarter(join(APPS, chosen));
      else linuxStarter(join(APPS, chosen));
    } catch (error) {
      say(`cannot add CortexCode to the applications (${error.message})`);
    }
  }
  // Its own session, detached from this terminal: closing the terminal does not quit it.
  mkdirSync(join(HOME, 'logs'), { recursive: true });
  const log = openSync(START_LOG, 'a');
  const child = spawn(executable(join(APPS, chosen)), args, { detached: true, stdio: ['ignore', log, log] });
  closeSync(log);
  return await new Promise((resolve) => {
    child.on('error', (error) => {
      say(`cannot start CortexCode: ${error.message}`);
      resolve(1);
    });
    child.on('spawn', () => {
      child.unref();
      say(`CortexCode ${chosen} is starting; you can close this terminal (start output: ${START_LOG})`);
      resolve(0);
    });
  });
}

main().then(
  (code) => process.exit(code),
  (error) => {
    say(error.message);
    process.exit(1);
  },
);
