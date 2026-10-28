/**
 * The start-up facts the splash waits for that live outside React: whether the slot state (the
 * saved layout, the open project and session) came from the frame. It comes once the memory plugin
 * has loaded the store, which can take seconds after the window opens.
 */

type Listener = () => void;

let stateSettled = false;
const listeners = new Set<Listener>();

/** The slot state is attached (loaded, or found missing): the layout is the person's own. */
export function markStateSettled(): void {
  stateSettled = true;
  for (const listener of [...listeners]) listener();
}

export function isStateSettled(): boolean {
  return stateSettled;
}

export function subscribeBoot(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
