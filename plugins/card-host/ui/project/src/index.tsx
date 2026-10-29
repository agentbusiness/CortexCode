/**
 * card.header@1 — the card's header: the project's name and folder. Opening another folder and
 * switching projects are the sidebar's; indexing again is the Index row's.
 */

import './style.css';
import { definePlugin, useSelection } from '@cortex/sdk';

function ProjectHeader() {
  const { project } = useSelection();
  return (
    <div className="cx-card-head" data-build={__CORTEX_BUILD__}>
      <span className="cx-card-name" title={project?.name}>{project?.name ?? 'No project'}</span>
      {/* Right to left so a long folder keeps its end; the path itself stays left to right. */}
      <span className="ap faint cx-card-root" title={project?.root}><span dir="ltr">{project ? project.root.replace(/^\/Users\/[^/]+/, '~') : ''}</span></span>
    </div>
  );
}

export default definePlugin({ slot: 'card.header@1', component: ProjectHeader });
