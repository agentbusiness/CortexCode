/**
 * theme@1 — CortexCode's design. The stylesheet this bundle ships is the theme:
 * the tokens of both grounds (compiled from tokens/cortex.tokens.json), the bundled faces (files next
 * to the stylesheet, added to it after the bundle is built), the
 * design's own rules (reference.css and components.css, unchanged apart from the token section),
 * and CortexCode's shared primitives. The workbench mounts exactly one theme; when this version
 * becomes the active one it publishes its data (token values, CodeMirror and xterm themes)
 * through @cortex/sdk, so renderers that do not read CSS restyle too.
 */

import { useEffect } from 'react';
import { definePlugin, publishTheme } from '@cortex/sdk';
import { themeData } from './generated/theme-data';
import './generated/tokens.css';
import './design-rules.css';
import './components.css';
import './primitives.css';

function Theme() {
  useEffect(() => {
    publishTheme(themeData);
  }, []);
  return <span hidden data-theme-plugin={themeData.id} data-theme-build={__CORTEX_BUILD__} />;
}

export { themeData };

export default definePlugin({ slot: 'theme@1', component: Theme });
