// Toolchains for the Aider Polyglot languages beyond Python/JavaScript/Rust, matching aider's
// benchmark Dockerfile (benchmark/Dockerfile at the pinned aider commit): Go 1.21.5, OpenJDK 21
// (Temurin build) with the exercises' own Gradle wrapper (8.7), CMake + the system C++ compiler with
// Boost (only Boost.DateTime is used, by 2 exercises; see LANG_PINS.boost for the version).
// Everything is installed under eval/cache/toolchains (pinned URL + SHA-256), never system-wide.
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { fetchPinned } from '../../lib/fetch.mjs';
import { SRC, tmpRoot, TOOLCHAINS } from '../../lib/paths.mjs';
import { T } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, exists, log, rmrf, run, runOk, sha256File, writeText } from '../../lib/util.mjs';

const DL = path.join(SRC, 'datasets', 'polyglot-toolchains');

export const LANG_PINS = {
  go: {
    version: '1.21.5',
    url: 'https://go.dev/dl/go1.21.5.darwin-arm64.tar.gz',
    sha256: 'd0f8ac0c4fb3efc223a833010901d02954e3923cfe2c9a2ff0e4254a777cc9cc',
  },
  jdk: {
    version: '21.0.11+10',
    url: 'https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.11%2B10/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.11_10.tar.gz',
    sha256: '6ebcf221c9b41507b14c098e93c6ead6440b8d9bd154f8ec666c4c73abbdb201',
  },
  gradle: {
    version: '8.7',
    url: 'https://services.gradle.org/distributions/gradle-8.7-bin.zip',
    sha256: '544c35d6bd849ae8a5ed0bcea39ba677dc40f49df7d1835561582da2009b961d',
  },
  // aider's image has jammy's Boost 1.74, whose Boost.MPL does not compile with current clang
  // (enum out-of-range constant, an error since clang 16; measured on this machine). 1.83.0 (Ubuntu
  // noble's version) is the nearest distribution release that does.
  boost: {
    version: '1.83.0',
    url: 'https://archives.boost.io/release/1.83.0/source/boost_1_83_0.tar.bz2',
    sha256: '6478edfe2f3305127cffe8caf73ea0176c53769f4bf1585be237eb30798c3b8e',
  },
};

export const JAVA_HOME = path.join(T.jdk, 'Contents', 'Home');
export const GO_BIN = path.join(T.go, 'bin');

async function extractTar(archive, dest, strip = 1) {
  const tmp = `${dest}.part`;
  await rmrf(tmp);
  await ensureDir(tmp);
  await runOk('/usr/bin/tar', ['-x', '-f', archive, '-C', tmp, `--strip-components=${strip}`], { timeoutMs: 3600000 });
  await rmrf(dest);
  fs.renameSync(tmp, dest);
}

export async function ensureGo() {
  const rec = await fetchPinned({ ...LANG_PINS.go, dest: path.join(DL, path.basename(LANG_PINS.go.url)), name: `Go ${LANG_PINS.go.version} darwin-arm64`, revision: LANG_PINS.go.version });
  if (!await exists(path.join(GO_BIN, 'go'))) {
    log('polyglot: installing Go into the eval cache');
    await extractTar(rec.path, T.go);
  }
  await ensureDir(T.goAgentCache);
  return rec;
}

export async function ensureJdk() {
  const rec = await fetchPinned({ ...LANG_PINS.jdk, dest: path.join(DL, 'OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.11_10.tar.gz'), name: `Temurin OpenJDK ${LANG_PINS.jdk.version} aarch64 mac`, revision: LANG_PINS.jdk.version });
  if (!await exists(path.join(JAVA_HOME, 'bin', 'java'))) {
    log('polyglot: installing the JDK into the eval cache');
    await extractTar(rec.path, T.jdk);
  }
  return rec;
}

/**
 * Gradle: the exercises' wrapper (gradle-wrapper.properties: gradle-8.7-bin.zip) looks for its
 * distribution under GRADLE_USER_HOME/wrapper/dists/gradle-8.7-bin/<base36(md5(url))>/. The pinned,
 * hash-checked zip is placed there, so the wrapper only unpacks it (it downloads only when the zip is
 * absent). Maven dependencies (JUnit BOM 5.10.0, AssertJ 3.25.1, rxjava, org.json) are fetched by
 * running each exercise's build once online (`warmGradle`). Daemons are disabled (no background
 * processes survive a build).
 */
export function wrapperDistDir(url = LANG_PINS.gradle.url) {
  const md5 = crypto.createHash('md5').update(url).digest('hex');
  return path.join(T.gradleHome, 'wrapper', 'dists', path.basename(url, '.zip'), BigInt(`0x${md5}`).toString(36));
}

export async function ensureGradleHome() {
  await ensureDir(T.gradleHome);
  await writeText(path.join(T.gradleHome, 'gradle.properties'), 'org.gradle.daemon=false\norg.gradle.console=plain\n');
  const rec = await fetchPinned({ ...LANG_PINS.gradle, dest: path.join(DL, path.basename(LANG_PINS.gradle.url)), name: `Gradle ${LANG_PINS.gradle.version} distribution`, revision: LANG_PINS.gradle.version });
  const dist = wrapperDistDir();
  if (!await exists(path.join(dist, `${path.basename(LANG_PINS.gradle.url)}.ok`))) {
    await rmrf(dist);
    await ensureDir(dist);
    await fsp.copyFile(rec.path, path.join(dist, path.basename(LANG_PINS.gradle.url)));
  }
  return rec;
}

/** The wrapper unpacked the pinned zip (marker present) rather than anything else. */
export async function verifyGradleDist() {
  const dist = wrapperDistDir();
  const ok = path.join(dist, `${path.basename(LANG_PINS.gradle.url)}.ok`);
  if (!await exists(ok) || !await exists(path.join(dist, `gradle-${LANG_PINS.gradle.version}`, 'bin', 'gradle'))) {
    throw new Error(`gradle distribution not unpacked in ${dist}`);
  }
  return { dir: dist };
}

/**
 * Run one exercise's Gradle test build online (with its example solution in place) so the wrapper
 * distribution and every dependency the test run resolves (JUnit platform, AssertJ, rxjava, org.json)
 * land in the cache. The outcome is irrelevant here; validation re-runs offline.
 */
export async function warmGradle(exerciseDir) {
  const env = { ...process.env, JAVA_HOME, GRADLE_USER_HOME: T.gradleHome, PATH: `${path.join(JAVA_HOME, 'bin')}:${process.env.PATH}` };
  const res = await run('./gradlew', ['--no-daemon', 'test'], { cwd: exerciseDir, env, timeoutMs: 1800000 });
  return res.code;
}

/** The agent's own writable Gradle home (a clone of the pristine one, never used for scoring). */
export async function ensureAgentGradleHome() {
  await cloneTree(T.gradleHome, T.gradleAgentHome);
}

/** Boost headers + Boost.DateTime built with b2 into eval/cache/toolchains/boost. */
export async function ensureBoost() {
  const rec = await fetchPinned({ ...LANG_PINS.boost, dest: path.join(DL, path.basename(LANG_PINS.boost.url)), name: `Boost ${LANG_PINS.boost.version}`, revision: LANG_PINS.boost.version });
  if (await exists(path.join(T.boost, 'lib', 'cmake', `Boost-${LANG_PINS.boost.version}`, 'BoostConfig.cmake'))) return rec;
  log('polyglot: building Boost.DateTime into the eval cache');
  const build = path.join(tmpRoot(), 'cceval-boost-build');
  await rmrf(build);
  await ensureDir(build);
  await runOk('/usr/bin/tar', ['-x', '-f', rec.path, '-C', build], { timeoutMs: 3600000 });
  const src = path.join(build, `boost_${LANG_PINS.boost.version.replace(/\./g, '_')}`);
  await runOk('./bootstrap.sh', ['--with-libraries=date_time', `--prefix=${T.boost}`], { cwd: src, timeoutMs: 3600000 });
  await runOk('./b2', ['-q', '-j8', 'link=shared,static', 'threading=multi', 'variant=release', 'install'], { cwd: src, timeoutMs: 3600000 });
  await rmrf(build);
  return rec;
}
