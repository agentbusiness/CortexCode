// The few vitest-style helpers the tests use, on node:test (this package has no dependencies).
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

export { describe, it };

export function expect(actual) {
  return {
    toBe: (expected) => assert.strictEqual(actual, expected),
    toEqual: (expected) => assert.deepStrictEqual(actual, expected),
    toBeUndefined: () => assert.strictEqual(actual, undefined),
    toThrow: () => assert.throws(actual),
    toMatchObject: (expected) => {
      for (const [key, value] of Object.entries(expected)) assert.deepStrictEqual(actual[key], value, key);
    },
  };
}
