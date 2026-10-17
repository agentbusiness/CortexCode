/**
 * settings.notify@1 — notifications: whether macOS shows them, which kinds are wanted, and the
 * "only when I am not looking at that session" rule. Preferences are written to the main settings
 * database through notify.prefs.set (memory.settings, section `notify`). While CortexCode is in
 * front, notifications mark their session in the session list instead of going to macOS.
 */

import { useEffect, useState } from 'react';
import { definePlugin, errorMessage, useClient, useQuery } from '@cortex/sdk';
import { nativeText } from './api';
import type { PrefsChange, PrefsResult, StatusResult } from './api';
import './notify.css';

function Switch({ on, label, busy, onToggle }: { on: boolean; label: string; busy: boolean; onToggle(): void }) {
  return <button type="button" className="sw" aria-pressed={on} aria-label={label} disabled={busy} onClick={onToggle} />;
}

function NotifySettings() {
  const client = useClient();
  const status = useQuery<StatusResult>('notify.status', (c) => c.call<StatusResult>('notify.status', {}), { topics: ['notify.changed'] });
  const prefs = useQuery<PrefsResult>('notify.prefs', (c) => c.call<PrefsResult>('notify.prefs.get', {}), {
    topics: ['memory.settings.changed'],
    when: (_topic, data) => typeof (data as { path?: unknown })?.path === 'string' && String((data as { path: string }).path).startsWith('notify'),
  });
  const [saving, setSaving] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  // The native permission changes outside this panel: look again now and then.
  useEffect(() => {
    const timer = setInterval(() => void status.refresh(), 5_000);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.refresh]);

  const change = async (what: string, params: PrefsChange) => {
    setSaving(what);
    setSaveError(null);
    try {
      await client.call<PrefsResult>('notify.prefs.set', params);
      await prefs.refresh();
    } catch (cause) {
      setSaveError(errorMessage(cause));
    } finally {
      setSaving(null);
    }
  };

  const p = prefs.data;
  const s = status.data;
  return (
    <div className="cx-notify" data-panel="notify" data-build={__CORTEX_BUILD__}>
      <h3 className="cx-notify-title">Notifications</h3>
      <p className="cx-notify-line">
        CortexCode tells macOS when something is done, fails, or needs you — so you can look away while agents work.
        While CortexCode is in front, it marks the session in the list instead.
      </p>

      <section className="cx-notify-block" aria-label="Delivery">
        <div className="ap">Delivery</div>
        {status.error && !s && <p className="inline-error">The notify plugin did not answer: {status.error}</p>}
        {s && (
          <table className="tbl cx-notify-facts">
            <tbody>
              <tr>
                <td>Notifications</td>
                <td data-fact="native">{nativeText(s.native)}</td>
              </tr>
              {s.prefs_source !== 'settings' && (
                <tr>
                  <td>Preferences</td>
                  <td data-fact="prefs">{`Built-in defaults: ${s.prefs_detail || 'the settings database is not available'}.`}</td>
                </tr>
              )}
            </tbody>
          </table>
        )}
      </section>

      <section className="cx-notify-block" aria-label="When to notify">
        <div className="ap">When to notify</div>
        {prefs.error && !p && <p className="inline-error">Preferences could not be read: {prefs.error}</p>}
        {p && (
          <>
            <div className="row" data-pref="only_when_away">
              <div className="grow">
                <div className="nm">Only when I am not looking at that session</div>
                <div className="cx-notify-note">Stay quiet about a conversation or agent while its session is on screen in the focused window.</div>
              </div>
              <Switch
                on={p.only_when_away}
                label="Only when I am not looking at that session"
                busy={saving !== null}
                onToggle={() => void change('only_when_away', { only_when_away: !p.only_when_away })}
              />
            </div>
            {p.kinds.map((kind) => (
              <div className="row" key={kind.kind} data-kind={kind.kind}>
                <div className="grow">
                  <div className="nm">{kind.label}</div>
                  <div className="cx-notify-note">{kind.description}</div>
                </div>
                <Switch on={kind.enabled} label={kind.label} busy={saving !== null} onToggle={() => void change(kind.kind, { kind: kind.kind, enabled: !kind.enabled })} />
              </div>
            ))}
          </>
        )}
        {saveError && <p className="inline-error">Not saved: {saveError}</p>}
      </section>

    </div>
  );
}

export default definePlugin({ slot: 'settings.notify@1', component: NotifySettings });
