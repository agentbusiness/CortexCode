// @ts-check
/**
 * Accessibility snapshot of a page as indented text, from CDP `Accessibility.getFullAXTree`.
 *
 * Structural rules only: ignored nodes and unnamed generic containers are skipped (their children
 * are lifted to the parent), inline text boxes are dropped (their static text parent carries the
 * text), and every node whose role is interactive gets a `[ref=N]` that resolves to its backend
 * DOM node for `browser.click` / `browser.type`.
 */

/** Roles a person can act on: these get refs. */
export const INTERACTIVE_ROLES = new Set([
  'button', 'link', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox', 'listbox', 'option',
  'menuitem', 'menuitemcheckbox', 'menuitemradio', 'tab', 'slider', 'spinbutton', 'switch',
  'treeitem', 'textarea', 'select', 'MenuListPopup', 'PopUpButton', 'ToggleButton', 'gridcell',
  'columnheader', 'rowheader', 'disclosuretriangle',
]);

/** Containers that say nothing on their own. */
const TRANSPARENT_ROLES = new Set(['generic', 'none', 'presentation', 'GenericContainer', 'LayoutTable', 'LayoutTableRow', 'LayoutTableCell']);

/** Properties worth printing, in this order. */
const SHOWN_PROPERTIES = ['checked', 'pressed', 'selected', 'expanded', 'disabled', 'focused', 'required', 'level'];

/**
 * @typedef {{ type?: string, value?: unknown }} AXValue
 * @typedef {{ name: string, value: AXValue }} AXProperty
 * @typedef {{ nodeId: string, ignored?: boolean, role?: AXValue, name?: AXValue, value?: AXValue,
 *   description?: AXValue, properties?: AXProperty[], childIds?: string[], parentId?: string,
 *   backendDOMNodeId?: number }} AXNode
 */

/**
 * @param {AXNode[]} nodes
 * @returns {{ tree: string, refs: Map<number, number> }}
 */
export function formatAxTree(nodes) {
  /** @type {Map<string, AXNode>} */
  const byId = new Map();
  for (const node of nodes) byId.set(node.nodeId, node);
  const root = nodes.find((node) => !node.parentId) ?? nodes[0];
  /** @type {string[]} */
  const lines = [];
  /** @type {Map<number, number>} */
  const refs = new Map();
  let nextRef = 1;
  if (!root) return { tree: '', refs };

  /**
   * @param {AXNode} node
   * @param {number} depth
   * @param {string} parentName
   */
  const visit = (node, depth, parentName) => {
    const role = str(node.role?.value);
    const name = str(node.name?.value).trim();
    const children = (node.childIds ?? []).map((id) => byId.get(id)).filter(/** @returns {n is AXNode} */ (n) => Boolean(n));
    if (role === 'InlineTextBox') return;
    const skip = node.ignored || (TRANSPARENT_ROLES.has(role) && !name);
    if (skip) {
      for (const child of children) visit(child, depth, parentName);
      return;
    }
    if (role === 'StaticText' || role === 'text') {
      // Text that only repeats its parent's accessible name adds nothing.
      if (name && name !== parentName) lines.push(`${indent(depth)}- text: ${quote(name)}`);
      return;
    }
    let line = `${indent(depth)}- ${role || 'node'}`;
    if (name) line += ` ${quote(name)}`;
    const value = str(node.value?.value);
    if (value && value !== name) line += ` value=${quote(value)}`;
    for (const property of SHOWN_PROPERTIES) {
      const found = (node.properties ?? []).find((entry) => entry.name === property);
      if (!found) continue;
      const shown = found.value?.value;
      if (shown === false || shown === 'false' || shown === undefined || shown === null) continue;
      line += shown === true || shown === 'true' ? ` [${property}]` : ` [${property}=${String(shown)}]`;
    }
    if (INTERACTIVE_ROLES.has(role) && typeof node.backendDOMNodeId === 'number') {
      const ref = nextRef++;
      refs.set(ref, node.backendDOMNodeId);
      line += ` [ref=${ref}]`;
    }
    lines.push(line);
    for (const child of children) visit(child, depth + 1, name);
  };
  visit(root, 0, '');
  return { tree: lines.join('\n'), refs };
}

/** @param {number} depth */
function indent(depth) {
  return '  '.repeat(depth);
}

/** @param {unknown} value */
function str(value) {
  return typeof value === 'string' ? value : value === undefined || value === null ? '' : String(value);
}

/** @param {string} text */
function quote(text) {
  return JSON.stringify(text.replace(/\s+/g, ' '));
}

/**
 * Parse a target: `ref=N` (or `[ref=N]`) from a snapshot, otherwise a CSS selector.
 * @param {string} target
 * @returns {{ ref: number } | { selector: string }}
 */
export function parseTarget(target) {
  const match = /^\[?ref=(\d+)\]?$/.exec(target.trim());
  if (match) return { ref: Number(match[1]) };
  return { selector: target };
}
