import { createPortal } from 'react-dom';
import { Activity, Component, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { SlotProvider, type UiPlugin } from '@cortex/sdk';
import { SlotEngine, type Instance } from './engine';
import { WorkbenchContext } from './context';

/**
 * One slot of the workbench. It asks the registry which plugin fills it, and lets its engine
 * stage, flip and roll back versions. The staged version renders inside `<Activity mode="hidden">`
 * (its DOM exists but is not displayed, its effects wait); readiness is read off that DOM: a
 * version is ready when its subtree committed with the ready mark, and failed when its boundary
 * drew the failure mark instead.
 */
export function SlotHost({ name, props, className, pluginId }: { name: string; props?: Record<string, unknown>; className?: string; pluginId?: string }) {
  const workbench = useContext(WorkbenchContext);
  if (!workbench) throw new Error('SlotHost outside the workbench');
  const { registry, loadModule, loadStyle, missingSdk, onSwap } = workbench;

  const engine = useMemo(() => new SlotEngine(name, loadModule, loadStyle, missingSdk), [name, loadModule, loadStyle, missingSdk]);
  useEffect(() => {
    engine.onRecord = onSwap;
    return () => engine.dispose();
  }, [engine, onSwap]);

  const version = useSyncExternalStore(
    (listener) => engine.subscribe(listener),
    () => engineVersion(engine),
  );
  void version;

  // A host's list binds each of its slot hosts to one contributed plugin; a plain slot takes the
  // plugin that fills it.
  const desired = useSyncExternalStore(
    (listener) => registry.subscribe(listener),
    () => (pluginId ? registry.byId(pluginId) : registry.forSlot(name)),
  );
  const registryLoaded = useSyncExternalStore((listener) => registry.subscribe(listener), () => registry.isLoaded());
  useEffect(() => {
    if (desired) engine.desire(desired);
    else if (registryLoaded) engine.release();
  }, [engine, desired, registryLoaded]);

  const container = useRef<HTMLDivElement>(null);
  const stagingKey = engine.staging?.key ?? null;
  useLayoutEffect(() => {
    const root = container.current;
    if (!root || !stagingKey) return;
    const check = () => {
      if (root.querySelector(`[data-slot-failed="${stagingKey}"]`)) {
        const message = root.querySelector(`[data-slot-failed="${stagingKey}"]`)?.getAttribute('data-reason') ?? 'render failed';
        engine.stagedFailed(stagingKey, message);
        return true;
      }
      if (root.querySelector(`[data-slot-ready="${stagingKey}"]`)) {
        engine.ready(stagingKey);
        return true;
      }
      return false;
    };
    if (check()) return;
    const observer = new MutationObserver(() => {
      if (check()) observer.disconnect();
    });
    observer.observe(root, { childList: true, subtree: true, attributes: true });
    return () => observer.disconnect();
  }, [engine, stagingKey]);

  const instances = engine.instances();
  const active = engine.active;
  const rejected = [...engine.rejected].join(' ');
  // Settled: a plugin is mounted, it failed, it waits for a newer window, or none will come (the
  // start-up splash waits for it).
  const settled = Boolean(active) || Boolean(engine.broken) || Boolean(engine.waiting) || (registryLoaded && (!desired || rejected !== ''));
  return (
    <div
      ref={container}
      className={`cx-slot${className ? ` ${className}` : ''}`}
      data-slot={name}
      data-settled={settled ? 'true' : 'false'}
      data-plugin-id={active?.plugin.id ?? ''}
      data-plugin-version={active?.plugin.version ?? ''}
      data-plugin-hash={active?.plugin.hash ?? ''}
      data-staging-hash={engine.staging?.plugin.hash ?? ''}
      data-rejected={rejected}
    >
      {instances.map((instance) => (
        <Activity key={instance.key} mode={instance === active ? 'visible' : 'hidden'}>
          <div className="cx-instance" data-instance={instance.key} data-hash={instance.plugin.hash}>
            <SlotBoundary
              instanceKey={instance.key}
              onError={(reason) => {
                if (engine.staging?.key === instance.key) engine.stagedFailed(instance.key, reason);
                else engine.activeFailed(instance.key, reason);
              }}
            >
              <SlotProvider info={{ slot: name, pluginId: instance.plugin.id, version: instance.plugin.version, hash: instance.plugin.hash }}>
                <PluginComponent instance={instance} name={name} props={props} />
              </SlotProvider>
              <span data-slot-ready={instance.key} hidden />
            </SlotBoundary>
          </div>
        </Activity>
      ))}
      {engine.broken && (
        <SlotFailure
          title={`${engine.broken.instance.plugin.id} stopped`}
          reason={engine.broken.reason}
          onRetry={() => engine.retry()}
        />
      )}
      {!engine.broken && !active && !engine.staging && registryLoaded && desired && rejected !== '' && (
        <SlotFailure
          title={`${desired.id} ${desired.version} could not be mounted`}
          reason={engine.reasons.get(desired.hash) ?? 'no reason given'}
          onRetry={() => engine.retryRejected()}
        />
      )}
      {!active && !engine.staging && !engine.broken && !engine.waiting && rejected === '' && (
        <SlotVacancy name={name} desired={desired} registryLoaded={registry.isLoaded()} registryError={registry.error} />
      )}
    </div>
  );
}

function engineVersion(engine: SlotEngine): string {
  return `${engine.active?.key ?? ''}|${engine.staging?.key ?? ''}|${engine.broken ? 'x' : ''}|${engine.rejected.size}|${engine.waiting?.plugin.hash ?? ''}`;
}

function PluginComponent({ instance, name, props }: { instance: Instance; name: string; props?: Record<string, unknown> }) {
  const Plugin = instance.component;
  return <Plugin slot={name} {...props} />;
}

/**
 * A slot that failed draws nothing in its place (a band or header keeps its layout); the failure is
 * a card floating over the window, with its reason and a way to mount it again.
 */
function SlotFailure({ title, reason, onRetry }: { title: string; reason: string; onRetry: () => void }) {
  const [closed, setClosed] = useState(false);
  const host = document.getElementById('cc') ?? document.body;
  if (closed) return null;
  return createPortal(
    <div className="cx-slot-alert notice contradiction" role="alert">
      <span className="kindmark" />
      <span className="ap lbl">{title}</span>
      <p>{reason}</p>
      <div className="acts">
        <button className="detent" type="button" onClick={onRetry}>Mount again</button>
        <button className="detent" type="button" onClick={() => setClosed(true)}>Close</button>
      </div>
    </div>,
    host,
  );
}

function SlotVacancy({ name, desired, registryLoaded, registryError }: {
  name: string;
  desired: UiPlugin | undefined;
  registryLoaded: boolean;
  registryError: string | null;
}) {
  let text: string;
  if (registryError && !registryLoaded) text = `The plugin list is unavailable: ${registryError}`;
  else if (!registryLoaded) text = 'Loading…';
  else if (!desired) text = `No UI plugin fills ${name}.`;
  else text = 'Loading…';
  return (
    <div className="cx-slot-empty">
      <p className="hint">{text}</p>
    </div>
  );
}

interface BoundaryProps {
  instanceKey: string;
  onError: (reason: string) => void;
  children: ReactNode;
}

class SlotBoundary extends Component<BoundaryProps, { error: string | null }> {
  override state = { error: null as string | null };
  private reported = false;

  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? `${error.name}: ${error.message}` : String(error) };
  }

  override componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error(`UI plugin instance ${this.props.instanceKey} failed`, error, info.componentStack);
    if (this.reported) return;
    this.reported = true;
    this.props.onError(error instanceof Error ? `${error.name}: ${error.message}` : String(error));
  }

  override render() {
    if (this.state.error !== null) {
      return (
        <div className="cx-slot-failed" data-slot-failed={this.props.instanceKey} data-reason={this.state.error}>
          <p className="hint">This region failed: {this.state.error}</p>
        </div>
      );
    }
    return this.props.children;
  }
}
