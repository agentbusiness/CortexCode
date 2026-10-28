/**
 * `@cortex/sdk`: what every CortexCode UI plugin builds on. Served once as a shared ES module
 * (`/ui/vendor/cortex-sdk.js`) through the workbench's import map, so the host and every plugin
 * version share one connection, one journal cache and one durable state store.
 */

export { Client, RpcError } from './rpc';
export type { CallOptions, ConnectionState, Envelope, SocketLike, WireError } from './rpc';
export { JournalView, JournalRegistry, applyPatch, mergeBuffered, findIndex } from './journal';
export type { JournalSnapshot, JournalStatus } from './journal';
export { QueryCache } from './query';
export type { QueryState } from './query';
export { SlotStateStore } from './store';
export type { StateDocument, StatePersistence } from './store';
export {
  createRuntime,
  installRuntime,
  runtime,
  ownRuntime,
  runtimeFor,
  activeComputer,
  ComputerScope,
  useRuntime,
  useComputer,
  HostContext,
  SlotContext,
  SlotProvider,
  definePlugin,
  useHost,
  useSlotInfo,
  useClient,
  useConnection,
  useEvent,
  useQuery,
  useQueries,
  useJournal,
  useSlotState,
  useShared,
  setShared,
  getShared,
  useNow,
  useUiPlugins,
} from './runtime';
export type {
  BrowserBounds,
  Host,
  QueryOptions,
  QueryResult,
  Runtime,
  ShellBridge,
  SlotInfo,
  SlotProps,
  SlotRenderProps,
  SlotListProps,
  UiPluginDefinition,
} from './runtime';
export { parseUnifiedDiff, reverseApply, hunkCounts, firstChangedLine, hunkAtLine } from './diff';
export { errorMessage, duration, clock, ago, bytes, basename, dirname, joinPath, relativePath, serviceProblem } from './format';
export { Cross, Caret, Divider, Menu, ServiceProblem } from './parts';
export type { MenuItem } from './parts';
export * from './types';
export { useSelection, useProjects, useSessions, listProjects, listSessions, openTarget } from './selection';
export type { ChatFocus, Selection, Target } from './selection';
export { DiffView } from './DiffView';
export { tag as hashlineTag, parseAnchored, decodeRead, countLines } from './hashline';
export type { FileText } from './hashline';
export * from './globals';
export { FakeClient, installTestRuntime, TestHost } from './testing';
export type { TestRuntime } from './testing';
export * from './code';
export * from './slots';
export { publishTheme, activeTheme, subscribeTheme, useThemeData, currentGround, useGround } from './theme';
export type { ThemeData, Ground, XtermPalette, HighlightRule } from './theme';
export { byOrder, contributions, pluginLabel, slotName } from './contrib';
export type { Contribution } from './contrib';
export { DEFAULT_MODEL_VARIANT, installLine, plural, useModelState } from './model';
export type { InstallProgress, ModelInfoData, ModelPhase, ModelSlotsData, ModelState } from './model';
