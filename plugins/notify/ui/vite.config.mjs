// Built with the CortexCode UI plugin preset: one ES module in dist/ with manifest.json
// ({id, version, slot, entry, css}); React, ReactDOM and @cortex/sdk stay external (the workbench
// provides them through its import map).
import { pluginConfig } from '@cortex/sdk/vite';

export default pluginConfig(import.meta.dirname);
