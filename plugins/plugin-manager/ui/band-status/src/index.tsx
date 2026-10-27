/**
 * workbench.band@1 — a service problem, and nothing else: while every service the product expects
 * runs (one per category; a process the frame supervises, or the browser the UI shell serves) it
 * draws nothing. When one is not running (crashed, restarting, missing) and its plugin is not
 * disabled on purpose, it says which in rubric; pressing it opens Settings > Plugins, where every
 * service has Restart and Back to known good. Beside it, "Fix it in a chat".
 *
 * The fix: a conversation on CortexCode's own source (the code folder opened as a project) that
 * starts with the problem and where to look. It is started here for every caller: a notification's
 * "Fix it" (`notify.activate` with `open: fix_plugin`), Settings > Plugins and the updater's popup
 * (both set the shared `plugins.fix`), and the button beside the band notice.
 */

import { useEffect, useRef, useState } from 'react';
import { definePlugin, errorMessage, setShared, useClient, useEvent, useQuery, useSelection, useShared, type PluginInfo, type Session } from '@cortex/sdk';
import { downServices, type PluginUnit } from './services';

/** What to fix: the plugin, its service, the version, and what happened, in a sentence. */
export interface FixRequest { plugin?: string | null; category?: string | null; version?: string | null; reason: string; ts: number }

const short = (version: string) => version.split('+')[0] || version;

/** The first message of the fix conversation. */
function brief(req: FixRequest, plugin: string, category: string): string {
  const version = req.version ? ` version ${short(req.version)}` : '';
  return [
    `The ${plugin} plugin (the "${category}" service${version}) ${req.reason}.`,
    `Its code is in plugins/${plugin}.`,
    `Read the last lines about it in ~/.cortexcode/logs/${category}.log and ~/.cortexcode/logs/frame.log first, then find the cause in the code and fix it.`,
  ].join(' ');
}

function Status() {
  const client = useClient();
  const selection = useSelection();
  const [mode] = useShared<string>('agents.defaultMode', 'auto');
  const [request, setRequest] = useShared<FixRequest | null>('plugins.fix', null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const handled = useRef(0);

  /** Open the code folder as a project and start the conversation with the problem. */
  const fix = async (req: FixRequest) => {
    setBusy(true);
    setFailure(null);
    try {
      const list = await client.call<{ units?: PluginUnit[] }>('frame.plugins.list', {});
      const units = list?.units ?? [];
      const plugin = req.plugin || units.find((unit) => unit.category === req.category)?.id || req.category || 'unknown';
      const category = req.category || units.find((unit) => unit.id === req.plugin)?.category || plugin;
      const source = await client.call<{ code_dir: string }>('source.status', {});
      const project = await client.call<{ id: string }>('frame.project.open', { root: source.code_dir });
      const session = await client.call<Session>('frame.session.create', { project_id: project.id, kind: 'chat', title: `Fix ${plugin}` });
      setShared('chat.stick', Date.now());
      await client.call('chatflow.turn', { session_id: session.id, text: brief(req, plugin, category), mode });
      selection.select(project.id, session.id);
    } catch (cause) {
      setFailure(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  // Requests from Settings > Plugins and the updater's popup.
  useEffect(() => {
    if (!request || request.ts <= handled.current) return;
    handled.current = request.ts;
    setRequest(null);
    void fix(request);
  }, [request]);
  // A notification's "Fix it".
  useEvent('notify.activate', (_topic, data) => {
    const record = (data ?? {}) as { target?: { open?: string; plugin?: string; category?: string; version?: string; restarts?: number }; action?: string | null };
    const target = record.target;
    if (target?.open !== 'fix_plugin' || record.action === 'Open Plugins') return;
    const restarts = target.restarts ?? 0;
    void fix({ plugin: target.plugin, category: target.category, version: target.version, ts: Date.now(), reason: `stops right after it starts: ${restarts} exits in a row, and CortexCode keeps restarting it` });
  });

  const listed = useQuery<{ plugins: PluginInfo[]; units: PluginUnit[] }>('frame.plugins.list:band', async (client) => {
    const result = await client.call<PluginInfo[] | { plugins: PluginInfo[]; units?: PluginUnit[] }>('frame.plugins.list', {});
    return Array.isArray(result) ? { plugins: result, units: [] } : { plugins: result?.plugins ?? [], units: result?.units ?? [] };
  }, { topics: ['plugin.state', 'plugin.swapped'] });
  const [, setSettings] = useShared<string | null>('settings.open', null);
  const [, setOverlay] = useShared<boolean>('overlay.open', false);
  if (!listed.data) return busy ? <span className="cx-status">Starting the fix conversation…</span> : null;
  const down = downServices(listed.data.plugins, listed.data.units);
  if (down.length === 0 && !busy && !failure) return null;
  const label = down.length === 1 ? `${down[0]!.category} not running` : `${down.length} services not running`;
  const first = down[0];
  const unit = first ? listed.data.units.find((u) => u.category === first.category) : undefined;
  return (
    <>
      {down.length > 0 && (
        <button className="detent cx-status cx-attn" type="button" data-down={down.map((row) => row.category).join(' ')}
          onClick={() => { setSettings('plugins'); setOverlay(true); }}
          title={`Not running: ${down.map((row) => `${row.category} (${row.plugin?.state ?? 'not started'})`).join(', ')}. Open Settings > Plugins to restart.`}
          data-build={__CORTEX_BUILD__}>
          {label}
        </button>
      )}
      {first && (
        <button className="detent cx-status" type="button" data-action="fix-in-chat" disabled={busy}
          title={`Start a conversation on CortexCode's own source about the ${first.category} service`}
          onClick={() => void fix({ plugin: unit?.id, category: first.category, version: first.plugin?.version, ts: Date.now(), reason: first.plugin?.state === 'crashed' ? `stops right after it starts: ${first.plugin.restarts ?? 0} exits in a row, and CortexCode keeps restarting it` : `is ${first.plugin?.state ?? 'not started'} and does not serve` })}>
          {busy ? 'Starting the fix…' : 'Fix it in a chat'}
        </button>
      )}
      {failure && <span className="cx-status cx-attn" title={failure}>The fix conversation could not start: {failure}</span>}
    </>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Status });
