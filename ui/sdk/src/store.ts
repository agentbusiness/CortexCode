/**
 * Durable slot state: what a region of the workbench must not forget when its plugin is swapped
 * (selected project and session, open tabs, scroll positions, drafts).
 *
 * The store belongs to the workbench host, not to any plugin: values live in namespaces keyed by
 * slot (`workbench.sidebar@1`) plus the shared `workbench` namespace, and a plugin version only
 * reads and writes them. The host persists the store through the shell, so it also survives an
 * app restart.
 */

export type StateValue = unknown;
type Namespace = Record<string, StateValue>;
export type StateDocument = Record<string, Namespace>;

export interface StatePersistence {
  load(): Promise<StateDocument>;
  save(document: StateDocument): void;
}

type Listener = () => void;

export class SlotStateStore {
  private document: StateDocument = {};
  private readonly listeners = new Map<string, Set<Listener>>();
  private persistence: StatePersistence | null = null;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;

  /** Load the persisted document; values already set in this session win over stale ones. */
  async attach(persistence: StatePersistence): Promise<void> {
    this.persistence = persistence;
    try {
      const loaded = await persistence.load();
      if (loaded && typeof loaded === 'object') {
        for (const [ns, values] of Object.entries(loaded)) {
          if (!values || typeof values !== 'object') continue;
          const current = this.document[ns] ?? {};
          this.document[ns] = { ...(values as Namespace), ...current };
          for (const key of Object.keys(values as Namespace)) this.emit(ns, key);
        }
      }
    } catch (error) {
      console.error('slot state could not be loaded', error);
    }
  }

  get<T>(ns: string, key: string): T | undefined {
    return this.document[ns]?.[key] as T | undefined;
  }

  set(ns: string, key: string, value: StateValue): void {
    const space = this.document[ns] ?? {};
    if (Object.is(space[key], value)) return;
    this.document[ns] = { ...space, [key]: value };
    this.emit(ns, key);
    this.scheduleSave();
  }

  subscribe(ns: string, key: string, listener: Listener): () => void {
    const id = `${ns}\u0000${key}`;
    let set = this.listeners.get(id);
    if (!set) {
      set = new Set();
      this.listeners.set(id, set);
    }
    set.add(listener);
    return () => {
      set.delete(listener);
    };
  }

  snapshot(): StateDocument {
    return this.document;
  }

  private emit(ns: string, key: string): void {
    const set = this.listeners.get(`${ns}\u0000${key}`);
    if (!set) return;
    for (const listener of [...set]) listener();
  }

  private scheduleSave(): void {
    if (!this.persistence || this.saveTimer) return;
    this.saveTimer = setTimeout(() => {
      this.saveTimer = null;
      this.persistence?.save(this.document);
    }, 250);
  }
}
