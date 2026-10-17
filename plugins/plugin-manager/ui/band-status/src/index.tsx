/**
 * workbench.band@1 — a service problem, and nothing else: while every service the product expects
 * runs (one per category; a process the frame supervises, or the browser the UI shell serves) it
 * draws nothing. When one is not running (crashed, restarting, missing) and its plugin is not
 * disabled on purpose, it says which in rubric; pressing it opens Settings > Plugins, where every
 * service has Restart and Back to known good.
 */

import { definePlugin, useQuery, useShared, type PluginInfo } from '@cortex/sdk';
import { downServices, type PluginUnit } from './services';

function Status() {
  const listed = useQuery<{ plugins: PluginInfo[]; units: PluginUnit[] }>('frame.plugins.list:band', async (client) => {
    const result = await client.call<PluginInfo[] | { plugins: PluginInfo[]; units?: PluginUnit[] }>('frame.plugins.list', {});
    return Array.isArray(result) ? { plugins: result, units: [] } : { plugins: result?.plugins ?? [], units: result?.units ?? [] };
  }, { topics: ['plugin.state', 'plugin.swapped'] });
  const [, setSettings] = useShared<string | null>('settings.open', null);
  const [, setOverlay] = useShared<boolean>('overlay.open', false);
  if (!listed.data) return null;
  const down = downServices(listed.data.plugins, listed.data.units);
  if (down.length === 0) return null;
  const label = down.length === 1 ? `${down[0]!.category} not running` : `${down.length} services not running`;
  return (
    <button className="detent cx-status cx-attn" type="button" data-down={down.map((row) => row.category).join(' ')}
      onClick={() => { setSettings('plugins'); setOverlay(true); }}
      title={`Not running: ${down.map((row) => `${row.category} (${row.plugin?.state ?? 'not started'})`).join(', ')}. Open Settings > Plugins to restart.`}
      data-build={__CORTEX_BUILD__}>
      {label}
    </button>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Status });
