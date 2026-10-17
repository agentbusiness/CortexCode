/**
 * The few drawn marks the code view's buttons need. The design has no icon set: like its caret,
 * each mark is a handful of 1.1px strokes on an 12×12 field, in the button's own ink. Every
 * button that shows one also carries a title and an accessible name.
 */

const stroke = { stroke: 'currentColor', fill: 'none', strokeWidth: 1.1 } as const;

export type MarkName = 'more' | 'list' | 'refresh' | 'collapse' | 'expand' | 'stage' | 'unstage' | 'color' | 'split' | 'unified' | 'open' | 'copy';

export function Mark({ name }: { name: MarkName }) {
  return (
    <svg className="cx-mark" viewBox="0 0 12 12" aria-hidden>
      {name === 'more' && (<><rect x="1.5" y="5.5" width="1.5" height="1.5" fill="currentColor" /><rect x="5.25" y="5.5" width="1.5" height="1.5" fill="currentColor" /><rect x="9" y="5.5" width="1.5" height="1.5" fill="currentColor" /></>)}
      {name === 'list' && (<path d="M1 2.5h10M1 6h10M1 9.5h10" {...stroke} />)}
      {name === 'refresh' && (<><path d="M10 6a4 4 0 1 1-1.2-2.85" {...stroke} /><path d="M9.2 1.2v2.3H6.9" {...stroke} /></>)}
      {name === 'collapse' && (<path d="M3 1.5l3 3 3-3M3 10.5l3-3 3 3" {...stroke} />)}
      {name === 'expand' && (<path d="M3 4.5l3-3 3 3M3 7.5l3 3 3-3" {...stroke} />)}
      {name === 'stage' && (<><rect x="1.5" y="1.5" width="9" height="9" {...stroke} /><path d="M6 3.5v5M3.5 6h5" {...stroke} /></>)}
      {name === 'unstage' && (<><rect x="1.5" y="1.5" width="9" height="9" {...stroke} /><path d="M3.5 6h5" {...stroke} /></>)}
      {name === 'color' && (<><rect x="1.5" y="1.5" width="9" height="9" {...stroke} /><path d="M1.5 1.5h4.5v9H1.5z" fill="currentColor" /></>)}
      {name === 'split' && (<><rect x="1.5" y="1.5" width="9" height="9" {...stroke} /><path d="M6 1.5v9" {...stroke} /></>)}
      {name === 'unified' && (<><rect x="1.5" y="1.5" width="9" height="9" {...stroke} /><path d="M1.5 6h9" {...stroke} /></>)}
      {name === 'open' && (<><path d="M5 2H2v8h8V7" {...stroke} /><path d="M7 1.5h3.5V5M10.5 1.5 5.5 6.5" {...stroke} /></>)}
      {name === 'copy' && (<><rect x="3.5" y="3.5" width="7" height="7" {...stroke} /><path d="M1.5 8V1.5H8" {...stroke} /></>)}
    </svg>
  );
}

/** A small square button holding a mark (a `.detent` without its label). */
export function MarkButton({ name, title, onClick, pressed, disabled }: {
  name: MarkName;
  title: string;
  onClick: () => void;
  pressed?: boolean;
  disabled?: boolean;
}) {
  return (
    <button type="button" className="detent cx-markbtn" title={title} aria-label={title} aria-pressed={pressed} disabled={disabled} onClick={onClick}>
      <Mark name={name} />
    </button>
  );
}
