// Shared adapter shape for the language-specific suites (LiveCodeBench, BigCodeBench-Hard, RustEvo²,
// Web-Bench). Each suite materializes tasks at prepare time into eval/cache/hidden/<suite>/<id>/ with
// a task.json, an agent-visible `template/` and hidden files (tests, reference solutions); this
// module turns such a store into the interface lib/runner.mjs drives (see lib/suites.mjs).
import path from 'node:path';
import { HIDDEN } from './paths.mjs';
import { materializeWorkspace, snapshotSha } from './workspace.mjs';
import { readJson } from './util.mjs';

/** Deterministic subset: round-robin over languages in manifest order, so --limit stays balanced. */
export function balancedLimit(tasks, limit) {
  if (!limit || limit >= tasks.length) return tasks;
  const byLang = new Map();
  for (const t of tasks) {
    if (!byLang.has(t.lang)) byLang.set(t.lang, []);
    byLang.get(t.lang).push(t);
  }
  const out = [];
  const lists = [...byLang.values()];
  for (let i = 0; out.length < limit; i++) {
    let any = false;
    for (const l of lists) {
      if (i < l.length && out.length < limit) { out.push(l[i]); any = true; }
    }
    if (!any) break;
  }
  return out;
}

/** Filter manifest entries by ids, language and group (comma lists allowed for lang and group). */
export function filterTasks(list, { ids, lang, group } = {}) {
  let out = list;
  if (ids?.length) out = out.filter((t) => ids.includes(t.id));
  if (lang) {
    const langs = String(lang).split(',');
    out = out.filter((t) => langs.includes(t.lang));
  }
  if (group) {
    const groups = String(group).split(',');
    out = out.filter((t) => groups.includes(t.group));
  }
  return out;
}

/**
 * Build a runner adapter over a materialized task store.
 *   o.name, o.title           suite id and report title
 *   o.tries                   attempts per task (1 = pass@1)
 *   o.defaultTimeoutMs        wall-clock budget per task
 *   o.select(list, opts)      optional extra selection over manifest entries (e.g. a date window);
 *                             excluded tasks (manifest `excluded`) are never listed
 *   o.hiddenFiles(task)       [{rel, abs}] files that must never appear in a workspace
 *   o.score(task, workspace, scoreDir, outDir, tag, state) -> {passed, ..., output}
 *   o.links(task)             dependency links for the workspace (excluded from git)
 *   o.exclude(task), o.provision(task, dest)  git-excluded paths and a step that fills them
 *   o.agentPath(), o.agentEnvExtra()  PATH entries and env for agents
 *   o.aiderFiles(task)        files aider gets in chat (its benchmark protocol)
 *   o.sequential              {initialState, advance, maxTries}: multi-step tasks (Web-Bench)
 */
export function makeAdapter(o) {
  const dir = path.join(HIDDEN, o.name);
  const adapter = {
    name: o.name,
    title: o.title,
    tries: o.tries ?? 1,
    defaultTimeoutMs: o.defaultTimeoutMs ?? 30 * 60000,
    dir,
    taskDir: (id) => path.join(dir, id),
    loadTask: (id) => readJson(path.join(dir, id, 'task.json')),
    async listTasks(opts = {}) {
      const m = await readJson(path.join(dir, 'tasks.json'));
      let list = m.tasks.filter((t) => !t.excluded);
      list = filterTasks(list, opts);
      if (o.select) list = o.select(list, opts, m);
      const picked = balancedLimit(list, opts.limit);
      return Promise.all(picked.map((t) => adapter.loadTask(t.id)));
    },
    prompt: (task) => task.prompt,
    agentPath: () => (o.agentPath ? o.agentPath() : []),
    agentEnvExtra: () => (o.agentEnvExtra ? o.agentEnvExtra() : {}),
    async makeWorkspace(task, dest) {
      await materializeWorkspace(path.join(dir, task.id, 'template'), dest, { links: o.links ? o.links(task) : [], exclude: o.exclude ? o.exclude(task) : [] });
      const snapshot = await snapshotSha(dest);
      // Dependency provisioning after the snapshot (excluded from git through o.exclude).
      if (o.provision) await o.provision(task, dest);
      return { snapshot };
    },
    hiddenFiles: (task) => o.hiddenFiles(task),
    async score(task, workspace, outDir, tag, scratch, state) {
      const res = await o.score(task, workspace, path.join(scratch, 'score'), outDir, tag, state);
      return { ...res, feedback: res.feedback ?? null };
    },
    followup: o.followup ?? (() => null),
    aiderFiles: o.aiderFiles,
    sequential: o.sequential ?? null,
    reportNote: o.reportNote ?? null,
  };
  return adapter;
}
