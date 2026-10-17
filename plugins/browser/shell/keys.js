// @ts-check
/**
 * Key names (`Enter`, `Control+A`, `Shift+Tab`, `a`) to CDP `Input.dispatchKeyEvent` parameters.
 * The table follows the DOM `KeyboardEvent.key` / `code` / Windows virtual key code standards.
 */

/** @type {Record<string, { code: string, keyCode: number, text?: string }>} */
const NAMED = {
  Enter: { code: 'Enter', keyCode: 13, text: '\r' },
  Tab: { code: 'Tab', keyCode: 9 },
  Escape: { code: 'Escape', keyCode: 27 },
  Backspace: { code: 'Backspace', keyCode: 8 },
  Delete: { code: 'Delete', keyCode: 46 },
  Insert: { code: 'Insert', keyCode: 45 },
  Space: { code: 'Space', keyCode: 32, text: ' ' },
  ' ': { code: 'Space', keyCode: 32, text: ' ' },
  ArrowUp: { code: 'ArrowUp', keyCode: 38 },
  ArrowDown: { code: 'ArrowDown', keyCode: 40 },
  ArrowLeft: { code: 'ArrowLeft', keyCode: 37 },
  ArrowRight: { code: 'ArrowRight', keyCode: 39 },
  Home: { code: 'Home', keyCode: 36 },
  End: { code: 'End', keyCode: 35 },
  PageUp: { code: 'PageUp', keyCode: 33 },
  PageDown: { code: 'PageDown', keyCode: 34 },
  F1: { code: 'F1', keyCode: 112 }, F2: { code: 'F2', keyCode: 113 }, F3: { code: 'F3', keyCode: 114 },
  F4: { code: 'F4', keyCode: 115 }, F5: { code: 'F5', keyCode: 116 }, F6: { code: 'F6', keyCode: 117 },
  F7: { code: 'F7', keyCode: 118 }, F8: { code: 'F8', keyCode: 119 }, F9: { code: 'F9', keyCode: 120 },
  F10: { code: 'F10', keyCode: 121 }, F11: { code: 'F11', keyCode: 122 }, F12: { code: 'F12', keyCode: 123 },
};

/** CDP modifier bits. */
const MODIFIERS = { Alt: 1, Control: 2, Ctrl: 2, Meta: 4, Cmd: 4, Command: 4, Shift: 8 };

/**
 * @param {string} combo e.g. `Enter`, `Shift+Tab`, `Control+a`
 * @returns {{ down: Record<string, unknown>, up: Record<string, unknown> }}
 */
export function keyEvents(combo) {
  const parts = combo === '+' ? ['+'] : combo.split('+').map((part) => part.trim()).filter(Boolean);
  if (combo.endsWith('++')) parts.push('+');
  const keyName = parts.pop();
  if (!keyName) throw new Error(`empty key "${combo}"`);
  let modifiers = 0;
  for (const part of parts) {
    const bit = /** @type {Record<string, number>} */ (MODIFIERS)[part];
    if (bit === undefined) throw new Error(`unknown modifier "${part}" in "${combo}"`);
    modifiers |= bit;
  }
  const named = NAMED[keyName];
  /** @type {Record<string, unknown>} */
  let base;
  if (named) {
    base = { key: keyName === 'Space' ? ' ' : keyName, code: named.code, windowsVirtualKeyCode: named.keyCode, nativeVirtualKeyCode: named.keyCode };
    if (named.text && (modifiers & ~8) === 0) base.text = named.text;
  } else if ([...keyName].length === 1) {
    const char = keyName;
    const upper = char.toUpperCase();
    const isLetter = /^[a-z]$/i.test(char);
    const isDigit = /^[0-9]$/.test(char);
    const keyCode = isLetter || isDigit ? upper.charCodeAt(0) : 0;
    const code = isLetter ? `Key${upper}` : isDigit ? `Digit${char}` : '';
    base = { key: char, code, windowsVirtualKeyCode: keyCode, nativeVirtualKeyCode: keyCode };
    // A character with Control/Alt/Meta is a command, not text.
    if ((modifiers & ~8) === 0) base.text = char;
  } else {
    throw new Error(`unknown key "${keyName}"`);
  }
  const down = { type: base.text ? 'keyDown' : 'rawKeyDown', modifiers, ...base };
  const up = { type: 'keyUp', modifiers, key: base.key, code: base.code, windowsVirtualKeyCode: base.windowsVirtualKeyCode, nativeVirtualKeyCode: base.nativeVirtualKeyCode };
  return { down, up };
}
