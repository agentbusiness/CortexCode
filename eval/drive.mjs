// Drive the open CortexCode app with one message, watching the conversation in its window:
//   node eval/drive.mjs <repository root> <message...>
// Attaches to the running app (run/frame.json), opens the repository as a project, starts a chat
// session, sends the message in auto mode and prints each journal entry as the run makes it, until
// the run ends. Approvals are left to the person at the window.
import { runningApp } from './lib/driver.mjs';
import { FrameClient } from './lib/frame.mjs';

const [, , root, ...words] = process.argv;
const text = words.join(' ');
if (!root || !text) { console.error('usage: node eval/drive.mjs <repository root> <message...>'); process.exit(2); }
const info = runningApp();
if (!info) { console.error('the app is not running (no run/frame.json)'); process.exit(2); }
const client = await new FrameClient(`ws://127.0.0.1:${info.port}/ws?token=${encodeURIComponent(info.token)}`).connect();
const stamp = () => new Date().toISOString().slice(11, 19);
// A plugin still starting after a launch answers `unavailable` (retryable): wait for it, up to five minutes.
async function call(method, params, timeoutMs = 120000) {
  const until = Date.now() + 5 * 60 * 1000;
  for (;;) {
    try { return await client.request(method, params, { timeoutMs }); }
    catch (error) {
      if (error?.code !== 'unavailable' || Date.now() > until) throw error;
      console.log(`${stamp()} waiting: ${error.message}`);
      await new Promise((f) => setTimeout(f, 3000));
    }
  }
}
console.log(`${stamp()} opening ${root} (indexing a large repository can take minutes)`);
const project = await call('frame.project.open', { root }, 3 * 3600 * 1000);
console.log(`${stamp()} project ${project.id}`);
const session = await call('frame.session.create', { project_id: project.id, kind: 'chat', title: text.slice(0, 60) });
console.log(`${stamp()} session ${session.id}`);
const turn = await call('chatflow.turn', { session_id: session.id, text, mode: 'auto' });
console.log(`${stamp()} run ${turn.run_id}`);

const TERMINAL = new Set(['done', 'failed', 'cancelled', 'stopped', 'blocked']);
const cut = (s, n = 220) => String(s ?? '').replace(/\s+/g, ' ').slice(0, n);
function line(e) {
  const d = e.data ?? {};
  switch (e.kind) {
    case 'user_message': return `USER ${cut(d.text)}`;
    case 'route': { const det = d.detail ?? {}; return `ROUTE ${d.route ?? det.route} skills=${JSON.stringify(det.skills ?? d.skills ?? [])} effort=${det.effort ?? ''} work=${cut(det.work, 120)}`; }
    case 'tool_call': { const a = d.args ?? {}; return `CALL ${d.tool} ${cut(a.path ?? a.command ?? JSON.stringify(a), 200)}${d.host ? ' [host]' : ''}`; }
    case 'tool_result': return `  -> ${cut(d.output, 200)}`;
    case 'assistant_message': return d.streaming ? null : `ASSISTANT ${cut(d.text, 400)}`;
    case 'reasoning': return d.streaming ? null : `  (thought ${String(d.text ?? '').length} chars)`;
    case 'note': return `NOTE[${d.purpose ?? ''}] ${cut(d.text, 160)}`;
    case 'status': return `STATUS ${d.state}${d.detail ? ' ' + cut(d.detail, 120) : ''}`;
    case 'approval': return `APPROVAL ${d.state} ${cut(d.command ?? d.text ?? JSON.stringify(d), 160)}  <- answer it in the window`;
    default: return `${e.kind} ${cut(JSON.stringify(d), 160)}`;
  }
}
let last = 0; let terminal = null; let seen = new Map();
const deadline = Date.now() + 3 * 3600 * 1000;
while (Date.now() < deadline) {
  const r = await client.request('frame.journal.read', { session_id: session.id, from: last + 1 }).catch(() => null);
  for (const e of r?.entries ?? []) {
    last = e.seq;
    const out = line(e);
    if (out) console.log(`${stamp()} ${String(e.seq).padStart(4)} ${out}`);
    if (e.kind === 'status' && TERMINAL.has(e.data?.state) && (!e.data?.run_id || e.data.run_id === turn.run_id)) terminal = e;
  }
  if (terminal) {
    const st = await client.request('chatflow.state', { session_id: session.id }).catch(() => null);
    if (!st || (!st.running && !st.pending_followups)) break;
  }
  await new Promise((f) => setTimeout(f, terminal ? 2000 : 1500));
}
console.log(`${stamp()} END ${terminal?.data?.state ?? 'deadline'} session=${session.id}`);
process.exit(0);
