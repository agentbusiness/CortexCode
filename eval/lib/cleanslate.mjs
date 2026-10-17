// A test starts from a clean slate (owner, 2026-10-15): the app it drives serves the skills of
// this repository exactly as shipped and carries no learned state. Learning (skills it fixes,
// rules and style it learns) is a product feature; resetting it is the test's business only, so
// this module only reads the product through its own APIs and refuses to start a task on a slate
// that is not clean. Nothing here deletes product state.
//
// What it checks before every task:
// - every active plugin that serves skills (a `data/skills` folder beside its executable, or the
//   live one in the app's code folder, `<home>/code/plugins/<plugin>/data/skills`, which wins when
//   it exists) serves the files of `plugins/<plugin>/data/skills` byte for byte: a fallback to an
//   older build (the frame's last known good), a stale install or a learned edit would test other
//   skills;
// - no learned global rule (`memory.rule.list`, source `learned`) and no style entry
//   (`memory.style.list`): both go into every prompt.
// Model states hibernated by earlier conversations are a cache keyed by conversation, and new
// test projects never reuse them, so they are not state a test depends on.
import fs from 'node:fs';
import path from 'node:path';
import { REPO_ROOT } from './paths.mjs';

/** The `data/skills` folder a plugin executable serves: beside it or up to four levels above. */
export function skillsDirOf(exec) {
  let dir = path.dirname(exec);
  for (let i = 0; i < 5 && dir !== path.dirname(dir); i++) {
    const candidate = path.join(dir, 'data', 'skills');
    if (fs.existsSync(candidate) && fs.statSync(candidate).isDirectory()) return candidate;
    dir = path.dirname(dir);
  }
  return null;
}

/** Files that differ between two folders (relative paths; missing on either side counts). */
export function differingFiles(a, b) {
  const list = (dir) => (fs.existsSync(dir) ? fs.readdirSync(dir).filter((name) => fs.statSync(path.join(dir, name)).isFile()) : []);
  const names = [...new Set([...list(a), ...list(b)])].sort();
  return names.filter((name) => {
    const left = path.join(a, name);
    const right = path.join(b, name);
    if (!fs.existsSync(left) || !fs.existsSync(right)) return true;
    return !fs.readFileSync(left).equals(fs.readFileSync(right));
  });
}

/**
 * Check the slate through `call(method, params)` (a frame client's request); `codeDir` is the
 * app's code folder (`<home>/code`). Returns `{ ok, problems: [text], skills: [{plugin, dir}], rules: n }`.
 */
export async function checkCleanSlate(call, { repoRoot = REPO_ROOT, codeDir = null } = {}) {
  const problems = [];
  const listed = await call('frame.plugins.list', {});
  const rows = listed?.plugins ?? listed ?? [];
  const skills = [];
  for (const row of rows) {
    if (row.state !== 'active' || !row.exec || row.transport === 'websocket') continue;
    const packaged = skillsDirOf(row.exec);
    if (!packaged) continue;
    const live = codeDir && path.join(codeDir, 'plugins', row.plugin || row.category, 'data', 'skills');
    const served = live && fs.existsSync(live) && fs.statSync(live).isDirectory() ? live : packaged;
    const shipped = path.join(repoRoot, 'plugins', row.plugin || row.category, 'data', 'skills');
    skills.push({ plugin: row.plugin || row.category, dir: served });
    if (!fs.existsSync(shipped)) {
      problems.push(`${row.plugin || row.category} serves skills from ${served}, but the repository has no ${path.relative(repoRoot, shipped)}`);
      continue;
    }
    const differ = differingFiles(shipped, served);
    if (differ.length) {
      problems.push(`${row.plugin || row.category} serves skills that differ from the repository (${differ.join(', ')}) from ${served}`);
    }
  }
  let rules = 0;
  const ruleList = await call('memory.rule.list', { scope: 'global' }).catch(() => null);
  for (const rule of ruleList?.rules ?? []) {
    rules += 1;
    if (rule.source === 'learned') problems.push(`a learned global rule is in every prompt: ${rule.id}: ${String(rule.text).slice(0, 120)}`);
  }
  const styleList = await call('memory.style.list', { scope: 'global' }).catch(() => null);
  for (const entry of styleList?.entries ?? []) {
    problems.push(`a learned style entry is in every prompt: ${entry.id} (${entry.area}): ${String(entry.text).slice(0, 120)}`);
  }
  return { ok: problems.length === 0, problems, skills, rules };
}

/** checkCleanSlate, throwing with every problem named when the slate is not clean. */
export async function requireCleanSlate(call, options) {
  const slate = await checkCleanSlate(call, options);
  if (!slate.ok) throw new Error(`the app is not on a clean slate for a test:\n- ${slate.problems.join('\n- ')}`);
  return slate;
}
