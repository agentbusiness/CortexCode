/**
 * sidebar.actions@1 — New chat: a conversation in the selected project, selected at once, on the
 * computer the project lives on.
 */

import { useState } from 'react';
import { ComputerScope, Cross, definePlugin, errorMessage, useClient, useHost, useSelection, useShared, type Session } from '@cortex/sdk';

/** Props: `{ compact }` — in the sidebar's strip, a square with the register cross. */
function NewChat({ compact = false }: { compact?: boolean }) {
  const [node] = useShared<string | null>('projectNode', null);
  return (
    <ComputerScope node={node}>
      <NewChatButton compact={compact} />
    </ComputerScope>
  );
}

function NewChatButton({ compact }: { compact: boolean }) {
  const client = useClient();
  const host = useHost();
  const selection = useSelection();
  const [busy, setBusy] = useState(false);
  const start = async () => {
    if (!selection.projectId) return;
    setBusy(true);
    try {
      const session = await client.call<Session>('frame.session.create', { project_id: selection.projectId, kind: 'chat', title: '' });
      selection.select(selection.projectId, session.id);
    } catch (cause) {
      host.notify(`No new conversation: ${errorMessage(cause)}`);
    } finally {
      setBusy(false);
    }
  };
  const title = selection.project ? `Start a conversation in ${selection.project.name}` : 'Open or select a project first';
  if (compact) {
    return (
      <button className="cx-strip-mark cx-strip-new" type="button" disabled={!selection.projectId || busy} onClick={() => void start()} data-action="new-chat"
        aria-label="New chat" title={`New chat: ${title.charAt(0).toLowerCase()}${title.slice(1)}`} data-build={__CORTEX_BUILD__}>
        <Cross className="cx-strip-cross" />
      </button>
    );
  }
  return (
    <button className="detent" type="button" disabled={!selection.projectId || busy} onClick={() => void start()} data-action="new-chat"
      title={title} data-build={__CORTEX_BUILD__}>
      New chat
    </button>
  );
}

export default definePlugin({ slot: 'sidebar.actions@1', component: NewChat });
