/**
 * card.agents@1 — the agents at work, one line each: its state, @name, task, the stage it is in
 * (route, read, act, verify…) and how long it has run; speed, contract checks and its +/− are in
 * the line's tooltip. The line opens the agent's session (steer it there; stop it from the board's
 * Agents view). No agent at work: nothing.
 */

import {
  definePlugin,
  setShared,
  useJournal,
  useNow,
  useQuery,
  useSelection,
  type AgentRecord,
  type Json,
  type JournalEntry,
  type Task,
  ServiceProblem,
} from '@cortex/sdk';
import './agents.css';

const ACTIVE = ['working', 'running', 'blocked', 'verifying'];

/** What an agent's journal says about its run: since when, which stage, how fast, which checks passed. */
export function runFacts(entries: readonly JournalEntry[]): { since: number | null; stage: string; tokensPerSecond: number | null; passed: number | null; checked: number | null } {
  let since: number | null = null;
  let stage = '';
  let tokensPerSecond: number | null = null;
  let passed: number | null = null;
  let checked: number | null = null;
  for (const entry of entries) {
    if (entry.kind === 'status') {
      if (entry.data.state === 'running') since = since ?? entry.ts_ms;
      else since = null;
      if (typeof entry.data.stage === 'string') stage = entry.data.stage.toLowerCase();
    }
    if (entry.kind === 'usage' && typeof entry.data.completion_tokens === 'number' && typeof entry.data.decode_ms === 'number' && entry.data.decode_ms > 0) {
      tokensPerSecond = entry.data.completion_tokens / (entry.data.decode_ms / 1000);
      if (typeof entry.data.stage === 'string') stage = entry.data.stage.toLowerCase();
    }
    if (entry.kind === 'verification') {
      const checks = ((entry.data.report as { checks?: Json[] } | undefined)?.checks ?? []) as Array<{ ok?: boolean }>;
      checked = checks.length;
      passed = checks.filter((check) => check.ok === true).length;
    }
  }
  return { since, stage, tokensPerSecond, passed, checked };
}

function AgentsSection() {
  const { projectId } = useSelection();
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });
  const tasks = useQuery<Task[]>(projectId ? `memory.task.list:${projectId}` : null, async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [], { topics: ['memory.task.changed'] });
  const active = (agents.data ?? []).filter((agent) => ACTIVE.includes(agent.status));
  const problem = agents.error && !agents.data;
  if (!problem && active.length === 0) return null;
  return (
    <section className="cx-cs" data-section="agents" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Agents</span></div>
      <div className="cx-cs-b">
        {problem && <ServiceProblem service="agents" state={agents} className="hint" />}
        {active.map((agent) => <AgentRow key={agent.name} agent={agent} task={(tasks.data ?? []).find((task) => task.id === agent.task_id)} />)}
      </div>
    </section>
  );
}

function AgentRow({ agent, task }: { agent: AgentRecord; task: Task | undefined }) {
  const journal = useJournal(agent.session_id ?? null);
  const facts = runFacts(journal.entries);
  const now = useNow(1000, true);
  const since = facts.since ?? agent.updated_ms;
  const elapsed = Math.max(0, Math.floor((now - since) / 1000));
  const total = task?.contract?.checks.length ?? facts.checked ?? 0;
  const detail = [
    `@${agent.name} · ${agent.status}${facts.stage ? ` · ${facts.stage}` : ''}`,
    facts.tokensPerSecond !== null ? `${facts.tokensPerSecond.toFixed(1)} tok/s` : '',
    total > 0 ? `${facts.passed ?? 0} of ${total} contract checks passed` : '',
    agent.session_id ? 'Open its session' : '',
  ].filter(Boolean).join('\n');
  return (
    <button type="button" className="cx-cs-row" data-agent={agent.name} data-status={agent.status} disabled={!agent.session_id} title={detail}
      onClick={() => { if (agent.session_id) { setShared('projectId', agent.project_id); setShared('sessionId', agent.session_id); } }}>
      <span className={`cx-pip cx-pip-${agent.status}`} />
      <span className="cx-at">@{agent.name}</span>
      <span className="t">{task?.title ?? agent.task_id ?? agent.role}</span>
      <span className="grow" />
      <span className={`ap ${agent.status === 'blocked' ? 'hot' : 'faint'}`}>{agent.status === 'blocked' ? 'blocked' : facts.stage || agent.status}</span>
      <span className="ap faint cx-agent-time">{Math.floor(elapsed / 60)}:{String(elapsed % 60).padStart(2, '0')}</span>
    </button>
  );
}

export default definePlugin({ slot: 'card.agents@1', component: AgentsSection });
