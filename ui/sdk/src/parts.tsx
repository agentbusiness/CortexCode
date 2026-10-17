/**
 * The design's shared marks: the register cross, the drawn
 * caret, and the picker's menu. Plugins draw with these rather than with an icon set; the design has
 * none — apparatus is set in type, and marks are rules and squares.
 */

import { useContext, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { SlotContext, setShared } from './runtime';
import { serviceProblem } from './format';

export function Cross({ className = 'cross' }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 9 9" aria-hidden>
      <path d="M4.5 0v9M0 4.5h9" stroke="currentColor" strokeWidth="1.1" />
    </svg>
  );
}

const CARET_TURN = { down: 0, up: 180, right: -90, left: 90 } as const;

/** The design's one drawn glyph: the chevron on a `.pick`, rotated for the other directions. */
export function Caret({ dir = 'down', className }: { dir?: keyof typeof CARET_TURN; className?: string }) {
  return (
    <svg viewBox="0 0 8 8" width="8" height="8" aria-hidden className={className} style={{ transform: `rotate(${CARET_TURN[dir]}deg)`, flex: "none" }}>
      <path d="M1 3l3 3 3-3" stroke="currentColor" fill="none" strokeWidth="1.1" />
    </svg>
  );
}

export interface MenuItem {
  key: string;
  label: string;
  note?: string;
  selected?: boolean;
  disabled?: boolean;
  onSelect: () => void;
}

/**
 * A control that opens a list (`.pick` stating a value, or a `.detent`) and the design's `.menu`
 * under it. Closes on a choice or a click outside.
 */
export function Menu({
  label,
  value,
  items,
  variant = 'pick',
  disabled,
  title,
  className,
}: {
  label: ReactNode;
  value?: ReactNode;
  items: MenuItem[];
  variant?: 'pick' | 'detent';
  disabled?: boolean;
  title?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  // The list is drawn over everything (portaled to the workbench root) but still belongs to the
  // plugin that opened it: it carries that plugin's slot marks.
  const owner = useContext(SlotContext);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const [place, setPlace] = useState<{ left: number; top: number }>({ left: 0, top: 0 });

  useLayoutEffect(() => {
    if (!open || !trigger.current) return;
    const rect = trigger.current.getBoundingClientRect();
    const height = menu.current?.offsetHeight ?? 0;
    const width = menu.current?.offsetWidth ?? 0;
    const below = rect.bottom + 4;
    const top = below + height > window.innerHeight - 8 ? Math.max(8, rect.top - 4 - height) : below;
    const left = Math.min(rect.left, Math.max(8, window.innerWidth - width - 8));
    setPlace({ left, top });
  }, [open, items.length]);

  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (menu.current?.contains(target) || trigger.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const root = typeof document !== 'undefined' ? document.getElementById('cc') ?? document.body : null;
  return (
    <>
      <button
        ref={trigger}
        type="button"
        className={`${variant}${className ? ` ${className}` : ''}`}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        title={title}
        onClick={() => setOpen((current) => !current)}
      >
        {label}
        {value !== undefined && <span className="v">{value}</span>}
        <Caret />
      </button>
      {open && root && createPortal(
        <div ref={menu} className="menu open" role="menu" style={{ left: place.left, top: place.top }} data-slot={owner?.slot} data-plugin-id={owner?.pluginId} data-portal="menu">
          {items.map((item) => (
            <button
              key={item.key}
              type="button"
              role="menuitemradio"
              aria-selected={item.selected ? 'true' : 'false'}
              aria-checked={item.selected ? 'true' : 'false'}
              disabled={item.disabled}
              onClick={() => {
                setOpen(false);
                item.onSelect();
              }}
            >
              <span className="mk" />
              <span className="ml">{item.label}</span>
              {item.note && <span className="mh">{item.note}</span>}
            </button>
          ))}
        </div>,
        root,
      )}
    </>
  );
}

/**
 * A section's line for a service that failed or is not running; when it is not running, the line
 * offers the plugins panel (where a plugin is replaced or restarted).
 */
export function ServiceProblem({ service, state, className = 'hint' }: {
  service: string;
  state: { error?: string | undefined; errorCode?: string | undefined };
  className?: string;
}) {
  const missing = state.errorCode === 'unavailable';
  return (
    <p className={className} data-service-problem={service} data-missing={missing ? 'true' : 'false'}>
      {serviceProblem(service, state)}
      {missing && (
        <>
          {' '}
          <button type="button" className="cx-file-link" onClick={() => setShared('settings.open', 'plugins')}>Plugins</button>
        </>
      )}
    </p>
  );
}

/**
 * A draggable vertical divider between two regions: the design's 1px hairline with a wider hit
 * area. Dragging reports the pointer's x (`onMove`, every move; `onEnd` once released); a double
 * click asks for the default (`onReset`). It has no keyboard binding (the product has none).
 */
export function Divider({ name, label, onMove, onEnd, onReset, className }: {
  name: string;
  label: string;
  onMove: (clientX: number) => void;
  onEnd: () => void;
  onReset: () => void;
  className?: string;
}) {
  const dragging = useRef(false);
  const finish = (element: HTMLElement, pointerId: number) => {
    if (!dragging.current) return;
    dragging.current = false;
    delete element.dataset.dragging;
    const root = document.getElementById('cc');
    if (root) delete root.dataset.resizing;
    if (element.hasPointerCapture(pointerId)) element.releasePointerCapture(pointerId);
    onEnd();
  };
  return (
    <div
      className={`cx-divider${className ? ` ${className}` : ''}`}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      title={`${label}: drag to resize, double-click for the default width`}
      data-divider={name}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        dragging.current = true;
        event.currentTarget.dataset.dragging = 'true';
        const root = document.getElementById('cc');
        if (root) root.dataset.resizing = 'true';
      }}
      onPointerMove={(event) => {
        if (dragging.current) onMove(event.clientX);
      }}
      onPointerUp={(event) => finish(event.currentTarget, event.pointerId)}
      onPointerCancel={(event) => finish(event.currentTarget, event.pointerId)}
      onLostPointerCapture={(event) => finish(event.currentTarget, event.pointerId)}
      onDoubleClick={onReset}
    />
  );
}
