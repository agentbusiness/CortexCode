import { createRoot } from 'react-dom/client';
import { createRuntime, installRuntime, openTarget, type ShellBridge, type StateDocument, type Target } from '@cortex/sdk';
import { Workbench, applyTheme, type ThemePreference } from './workbench/Workbench';
import { PluginRegistry } from './workbench/registry';
import './frame.css';

/** What the Electron preload exposes (`ui/shell/preload.cjs`). `loadState` reads the slot state an
 * earlier shell kept itself (before the frame kept it), for the one-time move into the frame. */
interface ShellApi extends ShellBridge {
  loadState(): Promise<StateDocument>;
  onNavigate(listener: (target: Target) => void): void;
}


declare global {
  interface Window {
    cortexShell?: ShellApi;
    __cortexBoot?: { id: string; at: number };
  }
}

// The plate is the default ground: the document says so before the theme plugin's styles arrive.
applyTheme('dark');

const params = new URLSearchParams(location.search);
const token = params.get('token') ?? '';
const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?token=${encodeURIComponent(token)}`;

const runtime = createRuntime(wsUrl);
installRuntime(runtime);

const shellApi = window.cortexShell ?? null;
// The durable slot state lives in the frame (frame.ui_state), so it is the same in every window
// and survives app restarts and swaps of the shell. A document an earlier shell kept on its own is
// moved into the frame once.
void runtime.state.attach({
  load: async () => {
    const stored = await runtime.client.call<{ document: StateDocument | null; rev: number }>('frame.ui_state.get');
    if (stored.rev === 0 && shellApi) {
      const kept = await shellApi.loadState().catch(() => ({}) as StateDocument);
      if (kept && Object.keys(kept).length) {
        await runtime.client.call('frame.ui_state.put', { document: kept });
        return kept;
      }
    }
    return stored.document ?? {};
  },
  save: (document) => {
    runtime.client.call('frame.ui_state.put', { document }).catch((error: unknown) => console.error('slot state not saved', error));
  },
}).then(() => {
  const view = runtime.state.get<ThemePreference>('workbench', 'view');
  if (view) applyTheme(view);
  // The app opens on the chat: the right pane starts collapsed (the card opens with it), and the
  // settings window and the overlay never come back from the saved state.
  runtime.state.set('workbench', 'settings.open', null);
  runtime.state.set('workbench', 'overlay.open', false);
  runtime.state.set('workbench', 'layout.paneCollapsed', true);
});

const registry = new PluginRegistry(runtime.client);
registry.start();

// Targets to show: from a shell module (a notification clicked) or from the frame's event.
shellApi?.onNavigate((target) => void openTarget(target ?? {}));
runtime.client.on('notify.activate', (_topic, data) => {
  const record = (data ?? {}) as { target?: Target };
  void openTarget(record.target ?? (record as Target));
});

// What the person is looking at, published so notifications can stay quiet about it: on every
// change and, as a heartbeat (a listener treats a report older than 30 s as stale), every 10 s.
// `ui.focus` is the workbench's topic; `notify.focus` carries the same record in the notify
// plugin's contract.
const FOCUS_HEARTBEAT_MS = 10_000;
let lastFocus = '';
let lastFocusAt = 0;
const publishFocus = (heartbeat = false) => {
  const focus = {
    project_id: runtime.state.get<string>('workbench', 'projectId') ?? null,
    session_id: runtime.state.get<string>('workbench', 'sessionId') ?? null,
    window_focused: document.hasFocus(),
    visible: document.visibilityState === 'visible',
  };
  const key = JSON.stringify(focus);
  if (key === lastFocus && !(heartbeat && Date.now() - lastFocusAt >= FOCUS_HEARTBEAT_MS - 50)) return;
  lastFocus = key;
  lastFocusAt = Date.now();
  runtime.client.publish('ui.focus', focus);
  runtime.client.publish('notify.focus', { ...focus, source: 'workbench', at: lastFocusAt });
};
const onFocusChange = () => publishFocus();
runtime.state.subscribe('workbench', 'projectId', onFocusChange);
runtime.state.subscribe('workbench', 'sessionId', onFocusChange);
window.addEventListener('focus', onFocusChange);
window.addEventListener('blur', onFocusChange);
document.addEventListener('visibilitychange', onFocusChange);
setInterval(() => publishFocus(true), FOCUS_HEARTBEAT_MS);
runtime.client.onState((state) => {
  if (state === 'open') {
    lastFocus = '';
    publishFocus();
  }
});

// Identifies this page load (diagnostics: a plugin hot-swap must never reload the page).
window.__cortexBoot = { id: crypto.randomUUID(), at: Date.now() };
// A rebuilt renderer is the one change a hot-swap cannot apply: reload, so the window never shows
// the old layout with new plugin packages (shared state lives in the frame and survives).
runtime.client.on('ui.renderer.', () => window.location.reload());

const shell: ShellBridge | null = shellApi
  ? {
      pickFolder: (title) => shellApi.pickFolder(title),
      pickFile: (title) => shellApi.pickFile(title),
      reveal: (file) => shellApi.reveal(file),
      setBrowserBounds: (bounds) => shellApi.setBrowserBounds(bounds),
      pathForFile: (file) => shellApi.pathForFile?.(file) ?? '',
      saveAttachment: (bytes, name) => shellApi.saveAttachment?.(bytes, name) ?? Promise.resolve({ error: 'this shell cannot save attachments' }),
    }
  : null;

const root = document.getElementById('root');
if (!root) throw new Error('the workbench root element is missing');
createRoot(root).render(<Workbench registry={registry} shell={shell} />);
