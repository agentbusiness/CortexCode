// The skill catalog as shipped in this repository: every `data/skills/<name>.md` of the plugins that
// carry skills (protocol `skills::CARRIERS`: language plugins, skills plugins and the memory plugin),
// named the way the tools plugin reads them (`skill://<category>/<name>`, the category from the
// plugin's manifest).
//
// This is the set the bank is validated against at load time. It is not the catalog a session is
// offered: that one depends on the project's indexed languages and is read from the session's
// `context` entry by score.mjs.
import fs from 'node:fs';
import path from 'node:path';
import { REPO_ROOT } from '../../lib/paths.mjs';

export const PLUGINS_DIR = path.join(REPO_ROOT, 'plugins');
const FAMILIES = /^(lang|skills)-/;
/** The contracts whose plugins carry skills (protocol `skills::CARRIERS`). */
const SKILL_CONTRACTS = new Set(['lang@1', 'skills@1', 'memory@1']);
/** The chat flow's constrained-stage schemas (ROUTE: router.json, DECIDE: decide.json). Which of
 *  them chooses skills, and on which routes, is read from the files (`skills` and its `x-when`). */
export const CHATFLOW_SCHEMAS = path.join(PLUGINS_DIR, 'chatflow', 'backend', 'data', 'schemas');
/** Journal `route` entries name their stage; these are the stages whose decision is constrained by a
 *  schema file, and that file. */
const DECISION_SCHEMAS = { route: 'router.json', decide: 'decide.json' };

/** The tools plugin's rule for a skill name (lang-kit `skills::valid_name`). */
export function validSkillName(name) {
  return /^[a-z0-9_-]+$/.test(name);
}

/** A `[backend]` key (`category`, `contract`) of a plugin manifest, or null. */
function backendField(text, key) {
  let section = null;
  for (const raw of text.split('\n')) {
    const line = raw.replace(/#.*$/, '').trim();
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) { section = header[1]; continue; }
    const m = line.match(new RegExp(`^${key}\\s*=\\s*"([^"]+)"`));
    if (m && section === 'backend') return m[1];
  }
  return null;
}

/** `[backend] category` of a plugin manifest, or null. */
export function manifestCategory(text) {
  return backendField(text, 'category');
}

/**
 * Scan the plugins folder. Returns `{ uris: Set<string>, plugins: { <category>: { dir, manifest,
 * skills: [name] } } }`. A plugin folder without a manifest (still being written) is listed with
 * `manifest: false` and its folder name as category: its skills exist as files but no frame loads
 * them yet.
 */
export function scanCatalog(pluginsDir = PLUGINS_DIR) {
  const uris = new Set();
  const plugins = {};
  let dirs = [];
  try { dirs = fs.readdirSync(pluginsDir, { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name).sort(); } catch { /* no plugins folder */ }
  for (const dir of dirs) {
    const skillsDir = path.join(pluginsDir, dir, 'data', 'skills');
    if (!fs.existsSync(skillsDir)) continue;
    const manifestPath = path.join(pluginsDir, dir, 'plugin.toml');
    const manifest = fs.existsSync(manifestPath);
    const text = manifest ? fs.readFileSync(manifestPath, 'utf8') : '';
    // A manifest says whether the plugin carries skills; a folder without one (still being
    // written) counts when its name is a family's.
    if (manifest ? !SKILL_CONTRACTS.has(backendField(text, 'contract')) : !FAMILIES.test(dir)) continue;
    const category = (manifest && manifestCategory(text)) || dir;
    const skills = fs.readdirSync(skillsDir, { withFileTypes: true })
      .filter((e) => e.isFile() && e.name.endsWith('.md'))
      .map((e) => e.name.slice(0, -3))
      .filter(validSkillName)
      .sort();
    plugins[category] = { dir, manifest, skills };
    for (const name of skills) uris.add(`skill://${category}/${name}`);
  }
  return { uris, plugins };
}

/**
 * The deciding stages as the chat flow's schemas define them: per journal stage (`route`, `decide`)
 * the routes it may choose and the routes on which its `skills` field is live (the field's
 * `x-when.route`). Read from the shipped schema files, so a route added there (PLAN) is known here
 * without a list in code. Returns `{ <stage>: { routes: [..], skills_on: [..] } }`; a missing or
 * unreadable file leaves its stage out.
 */
/**
 * The always-loaded skills (`always = true` in a plugin's data/skills/applies.toml): a language
 * plugin's guide sits in every session's system message of a project in that language, so it is
 * never a choice the model makes. Returns a Set of `skill://<category>/<name>`.
 */
export function alwaysSkills(pluginsDir = PLUGINS_DIR) {
  const out = new Set();
  const { plugins } = scanCatalog(pluginsDir);
  for (const [category, info] of Object.entries(plugins)) {
    const file = path.join(pluginsDir, info.dir, 'data', 'skills', 'applies.toml');
    let text = '';
    try { text = fs.readFileSync(file, 'utf8'); } catch { continue; }
    let table = null;
    for (const line of text.split('\n')) {
      const header = line.match(/^\s*\[([a-z0-9-]+)\]\s*$/);
      if (header) { table = header[1]; continue; }
      if (table && /^\s*always\s*=\s*true\s*$/.test(line)) out.add(`skill://${category}/${table}`);
    }
  }
  return out;
}

export function decisionStages(dir = CHATFLOW_SCHEMAS) {
  const out = {};
  for (const [stage, file] of Object.entries(DECISION_SCHEMAS)) {
    let schema;
    try { schema = JSON.parse(fs.readFileSync(path.join(dir, file), 'utf8')); } catch { continue; }
    const props = schema?.properties ?? {};
    const routes = Array.isArray(props.route?.enum) ? props.route.enum : [];
    const when = props.skills?.['x-when']?.route;
    out[stage] = { routes, skills_on: Array.isArray(when) ? when : (props.skills ? routes : []) };
  }
  return out;
}

let stagesCache = null;
/** decisionStages() of the shipped schemas, read once per process. */
export function shippedDecisionStages() {
  stagesCache ??= decisionStages();
  return stagesCache;
}
