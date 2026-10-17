/**
 * Definitions the plugin build preset (`@cortex/sdk/vite`) puts into every plugin bundle.
 */
declare global {
  /** Build identity of the plugin bundle: a hash of its sources, or a stamp given at build time. */
  const __CORTEX_BUILD__: string;
  /** The plugin's version from its package.json. */
  const __CORTEX_PLUGIN_VERSION__: string;
}

export {};
