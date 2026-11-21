/**
 * workbench.band@1 — Pause / Resume all the app's work (agents.pause): every run, local or
 * frontier, conversation or agent, stops at its next step (what is under way finishes first) and
 * nothing new starts; Resume goes on from where each stopped. The state is a setting, so it
 * survives restarts and updates. While runs still finish their step the button says so.
 */

import { useState } from 'react';
import { definePlugin, errorMessage, useClient, useQuery } from '@cortex/sdk';

interface PauseState {
  paused: boolean;
  since_ms?: number;
  /** Agents still finishing their current step. */
  finishing?: string[];
}

function Pause() {
  const client = useClient();
  const state = useQuery<PauseState>('agents.pause.state', (c) => c.call<PauseState>('agents.pause.state', {}), { topics: ['agents.paused'] });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!state.data) return null;
  const paused = state.data.paused;
  const finishing = state.data.finishing ?? [];
  const turn = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.call<PauseState>('agents.pause', { paused: !paused });
      await state.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const label = !paused ? 'Pause' : finishing.length > 0 ? `Pausing · ${finishing.length} finishing` : 'Resume';
  const since = state.data.since_ms ? new Date(state.data.since_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : null;
  const title = error
    ? `It did not ${paused ? 'resume' : 'pause'}: ${error}`
    : !paused
      ? 'Pause all work: every conversation and agent stops after the step it is on, and nothing new starts. Resume goes on from there.'
      : finishing.length > 0
        ? `Pausing: ${finishing.join(', ')} still finishing the step under way. Press to resume instead.`
        : `Paused${since ? ` since ${since}` : ''}: press to resume every conversation and agent from where it stopped.`;
  return (
    <button
      type="button"
      className={`detent cx-status${paused ? ' cx-attn' : ''}`}
      aria-pressed={paused}
      data-paused={paused ? 'true' : 'false'}
      data-finishing={finishing.length}
      data-error={error ? 'true' : undefined}
      disabled={busy}
      title={title}
      onClick={() => void turn()}
    >
      {label}
    </button>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Pause });
