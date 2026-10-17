// Docker access for the container suites (SWE-bench, Terminal-Bench). The official harnesses talk to
// the Docker Engine through the Docker SDK for Python (docker.from_env) or the docker CLI; both find
// the engine through DOCKER_HOST, which Docker Desktop on macOS does not set (its socket lives in
// ~/.docker/run). The active docker context's endpoint is passed explicitly, so the harnesses reach
// the same engine the docker CLI (and CortexCode's container plugin) uses.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { run } from './util.mjs';

let cached = null;

/**
 * {DOCKER_HOST, DOCKER_CONFIG} for harness processes: the active docker context's endpoint, and the
 * user's docker config directory (CLI plugins such as compose and buildx are found through it), so
 * a baseline process with its own HOME still drives the same engine with the same CLI.
 */
export async function dockerEnv() {
  if (cached) return cached;
  const out = {};
  const config = process.env.DOCKER_CONFIG ?? path.join(os.homedir(), '.docker');
  if (fs.existsSync(config)) out.DOCKER_CONFIG = config;
  if (process.env.DOCKER_HOST) {
    out.DOCKER_HOST = process.env.DOCKER_HOST;
  } else {
    const res = await run('docker', ['context', 'inspect', '--format', '{{.Endpoints.docker.Host}}'], { timeoutMs: 30000 });
    const host = res.code === 0 ? res.stdout.trim() : '';
    if (host) out.DOCKER_HOST = host;
  }
  cached = out;
  return cached;
}

/** `docker image inspect` of one image, or null when it is not present locally. */
export async function inspectImage(ref) {
  const res = await run('docker', ['image', 'inspect', ref], { timeoutMs: 60000 });
  if (res.code !== 0) return null;
  try { return JSON.parse(res.stdout)[0] ?? null; } catch { return null; }
}

/** Docker platform string for an image architecture as Docker reports it (amd64, arm64, x86_64). */
export function platformOf(arch) {
  if (arch === 'arm64' || arch === 'aarch64') return 'linux/arm64';
  if (arch === 'amd64' || arch === 'x86_64') return 'linux/amd64';
  throw new Error(`unknown image architecture ${arch}`);
}

/**
 * The frame must have a container plugin (container@1, protocol/src/contracts/container.rs) for
 * CortexCode to reach a task container; fail with that reason instead of a later unknown_method.
 */
export async function requireContainerPlugin(client) {
  try {
    await client.request('container.list', {}, { timeoutMs: 60000 });
  } catch (error) {
    throw new Error(`the frame serves no container@1 plugin (container.list: ${error?.message ?? error}); the container suites need CortexCode's container plugin in cortex.dev.toml`);
  }
}
