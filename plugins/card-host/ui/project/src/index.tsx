/**
 * card.header@1 — the card's header: the project (name and folder), and its menu: open another
 * folder, index the project again, or switch to another open project.
 */

import './style.css';
import { Menu, definePlugin, errorMessage, setShared, useClient, useHost, useProjects, useSelection, type Project } from '@cortex/sdk';

function ProjectHeader() {
  const client = useClient();
  const host = useHost();
  const { project, projectId } = useSelection();
  const projects = useProjects();
  const switchTo = (id: string) => {
    setShared('projectId', id);
    setShared('sessionId', null);
    setShared('code.target', null);
  };
  const openFolder = async () => {
    if (!host.shell) return host.notify('Opening a folder needs the CortexCode app.');
    const root = await host.shell.pickFolder();
    if (!root) return;
    try {
      const opened = await client.call<Project>('frame.project.open', { root });
      await projects.refresh();
      switchTo(opened.id);
    } catch (cause) {
      host.notify(`Could not open ${root}: ${errorMessage(cause)}`);
    }
  };
  const refresh = async () => {
    if (!projectId) return;
    try {
      await client.call('memory.refresh', { project_id: projectId });
      host.notify(`Indexed ${project?.name ?? 'the project'} again`);
    } catch (cause) {
      host.notify(`The index was not refreshed: ${errorMessage(cause)}`);
    }
  };
  const others = (projects.data ?? []).filter((entry) => entry.id !== projectId);
  return (
    <div className="cx-card-head" data-build={__CORTEX_BUILD__}>
      <span className="cx-card-name" title={project?.name}>{project?.name ?? 'No project'}</span>
      {/* Right to left so a long folder keeps its end; the path itself stays left to right. */}
      <span className="ap faint cx-card-root" title={project?.root}><span dir="ltr">{project ? project.root.replace(/^\/Users\/[^/]+/, '~') : ''}</span></span>
      <Menu
        variant="detent"
        label="…"
        title="Project actions"
        className="tiny cx-card-menu"
        items={[
          { key: 'open', label: 'Open folder…', note: 'Another project from this Mac', onSelect: () => void openFolder() },
          { key: 'refresh', label: 'Refresh index', note: 'Re-read changed files into the code graph', disabled: !projectId, onSelect: () => void refresh() },
          ...others.map((entry) => ({ key: entry.id, label: `Switch to ${entry.name}`, note: entry.root, onSelect: () => switchTo(entry.id) })),
        ]}
      />
    </div>
  );
}

export default definePlugin({ slot: 'card.header@1', component: ProjectHeader });
