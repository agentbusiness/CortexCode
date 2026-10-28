/**
 * sidebar.actions@1 — Open project: the native folder dialog (through the shell), then
 * frame.project.open, and the new project is selected. Its caret menu offers the same folder flow
 * and New project from GitHub (a clone into the dev folder; see FromGithub).
 */

import { useState } from 'react';
import { Menu, definePlugin, errorMessage, setShared, useClient, useHost, useProjects, type Project } from '@cortex/sdk';
import { FromGithub } from './FromGithub';
import './open-project.css';

/** Props: `{ compact }` — the sidebar's strip has no room for it; it stays in the open sidebar. */
function OpenProject({ compact = false }: { compact?: boolean }) {
  const client = useClient();
  const host = useHost();
  const projects = useProjects();
  const [busy, setBusy] = useState(false);
  const [fromGithub, setFromGithub] = useState(false);
  const select = async (project: Project) => {
    await projects.refresh();
    // A project opened here lives on this computer.
    setShared('projectNode', null);
    setShared('projectId', project.id);
    setShared('sessionId', null);
  };
  const open = async () => {
    if (!host.shell) {
      host.notify('Opening a folder needs the CortexCode app.');
      return;
    }
    const root = await host.shell.pickFolder();
    if (!root) return;
    setBusy(true);
    try {
      await select(await client.call<Project>('frame.project.open', { root }));
    } catch (cause) {
      host.notify(`Could not open ${root}: ${errorMessage(cause)}`);
    } finally {
      setBusy(false);
    }
  };
  if (compact) return null;
  return (
    <span className="cx-open-project">
      <button className="detent" type="button" onClick={() => void open()} disabled={busy} data-action="open-project"
        title="Choose a folder on this Mac; CortexCode indexes it and works inside it" data-build={__CORTEX_BUILD__}>
        {busy ? 'Opening…' : 'Open project'}
      </button>
      <Menu variant="detent" className="cx-open-more" label="" title="More ways to start a project" disabled={busy} items={[
        { key: 'folder', label: 'A folder on this Mac', note: 'The folder is the project', onSelect: () => void open() },
        { key: 'github', label: 'From GitHub', note: 'Clone a repository into your dev folder', onSelect: () => setFromGithub(true) },
      ]} />
      {fromGithub && <FromGithub onClose={() => setFromGithub(false)} onOpened={select} />}
    </span>
  );
}

export default definePlugin({ slot: 'sidebar.actions@1', component: OpenProject });
