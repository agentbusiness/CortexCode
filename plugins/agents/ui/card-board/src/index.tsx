/**
 * card.board@1 — the newest three board messages, one line each: who, what, when (the kind and
 * whole text in the tooltip). A line opens the whole board. Nothing posted: nothing.
 */

import { clock, definePlugin, setShared, showPane, useQuery, useSelection, type BoardMessage, ServiceProblem } from '@cortex/sdk';
import './board.css';

function BoardSection() {
  const { projectId } = useSelection();
  const board = useQuery<BoardMessage[]>(projectId ? `memory.board.list:${projectId}` : null, async (c) => (await c.call<{ messages: BoardMessage[] }>('memory.board.list', { project_id: projectId }))?.messages ?? [], { topics: ['memory.board.posted'] });
  const all = board.data ?? [];
  const problem = board.error && !board.data;
  if (!problem && all.length === 0) return null;
  const latest = [...all].sort((a, b) => b.ts_ms - a.ts_ms).slice(0, 3);
  const viewAll = () => {
    setShared('board.view', 'board');
    showPane('board');
  };
  return (
    <section className="cx-cs" data-section="board" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l">
        <span className="ap">Board</span>
        {all.length > latest.length && <span className="ap faint sub">{all.length}</span>}
      </div>
      <div className="cx-cs-b">
        {problem && <ServiceProblem service="memory" state={board} className="hint" />}
        {latest.map((message) => {
          const who = message.author === 'user' ? 'You' : message.author === 'chat' ? 'Chat' : `@${message.author}`;
          return (
            <button key={message.id} type="button" className="cx-cs-row" data-message={message.id} onClick={viewAll}
              title={`${who} · ${message.kind.replace(/_/g, ' ')} · ${clock(message.ts_ms)}\n${message.text}\nOpen the board`}>
              <span className={message.author === 'user' || message.author === 'chat' ? 'ap' : 'cx-at'}>{who}</span>
              <span className="t">{message.text}</span>
              <span className="grow" />
              <span className="ap faint">{clock(message.ts_ms)}</span>
            </button>
          );
        })}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.board@1', component: BoardSection });
