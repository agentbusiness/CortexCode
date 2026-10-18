TypeScript and JavaScript here: get it right, run and test it, code that runs, errors and fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the request, then what the project already says about it: the code it touches (signatures, JSDoc, types), its tests, docs and callers.
   - Per function or class: what it takes, returns (a value or a `Promise`) and throws.
   - The request and the project decide, never a name alone. "whether" returns a boolean. "find" may return `undefined` or `null`, as the project does. "count" returns a number.
   - When there may be no answer and nothing names a value for it, the declared return type decides (`T | undefined` or `T | null`); without one, `undefined` is JavaScript's "no value" (`find`, `Map.get`). Not `false`, `-1` or `[]`.
   - Keep existing names, signatures and export style (default or named): other modules import them exactly.
   - The request and the project disagree: say so and ask, instead of silently picking one.
2. **Before coding, note the request's examples: they are the check, their values copied, never computed.**
   - Each behaviour, value, error or limit the request names is one case. A shown requirement list is that list.
   - Unsure of a domain, format, protocol or package? Look it up: the research plugin, the package's docs, how others solved it:
     `read research://search/<topic>?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that explains it.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the tests.
   - Add the next behaviour. Run again after every change.
   - A new file is written whole with `edit` `content`; an existing file is changed with anchored `edits` only, every change to it in one call.
   - Change a function: replace it from its first line to its closing `}`. Never leave part of the old body or a stray brace.
   - Fix the errors the edit result shows before moving on.
4. **Quick runs** of one example:

```bash
node -e 'const { parseOrder } = require("./src/order.js"); console.log(parseOrder("2 pens"))'
node --input-type=module -e 'import { parseOrder } from "./src/order.ts"; console.log(parseOrder("2 pens"))'
```

5. **Never rewrite, reformat or clean up code that works, and never write a working file whole again: change only the lines the task needs.** Code that works and that the task does not touch stays exactly as it is, unless the user asks to change it. Run the project's tests (section 2); fix the first failure's cause, run again.
6. **Before you finish**, check each requirement against the code, including the errors the request or the project names (class and message).

**How the data is held.** Decide it before writing functions, from what each operation returns and changes (`skill://skills-engineering/designing-the-code`):
- Objects and arrays are shared by reference: return a copy (`[...xs]`, `{ ...o }`, `structuredClone(o)`) when the caller must not change the stored one; `readonly` and `Readonly<T>` say it in the type.
- A state with several values: a discriminated union on one `kind` field, not optional fields that can conflict.
- Containers: `Map<K, V>` for lookup (any key type, insertion order), `Set` for uniqueness, an array for order; plain objects only for fixed string keys.
- Integers beyond 2^53 lose precision as `number`: use `bigint`.
- Standard building blocks first: array methods (`map`, `filter`, `reduce`, `flatMap`), `Map` and `Set`, `Intl`, `structuredClone`, and the project's dependencies, before writing your own.

## 2. Run, test, check

Run tools only from the project's `node_modules`: `npx --no -- <tool>`. `--no` blocks downloads; a plain `npx tsc` can fetch an unrelated package.

```bash
npx --no -- tsc --noEmit            # type-check the whole project (uses tsconfig.json)
npm test                            # the package's test script
npm test -- -t "adds"               # jest or vitest: tests whose name matches
npm test -- src/sum.test.ts         # one file
node --test --test-name-pattern="adds" test/sum.test.js   # node:test
npx --no -- mocha --grep "adds" test/sum.test.js          # mocha
node --check src/a.js               # syntax check of plain JavaScript
```

- **Which runner:** see `scripts.test` and `devDependencies` in `package.json`.
- `vitest` alone watches; use `vitest run`. pnpm and yarn take arguments without `--`.
- Never `tsc file.ts`: it ignores `tsconfig.json`.
- **Running `.ts`:** Node 22.18+ runs `.ts` directly when relative imports carry extensions. Else (enum, namespace, parameter properties, extensionless imports): `npx --no -- tsx file.ts` if installed.
- No `node_modules`: `npm ci` (or `pnpm install`, `yarn install`, per the lockfile) installs what the project already lists.
- A new package (`npm install <pkg>`) is a change to the user's project: ask before adding one.

**Checking your work.**
1. When the project has tests, run them (section 2) before and after the change, and add the request's examples there. A project without tests gets them: `tests/<name>.test.mjs` with `node:test` and `node:assert`, importing the project's file by its path, run with `node --test tests/`; they stay in the project.
2. One check per example of the request, input and expected value copied, never computed or changed; no case the request does not give. When the request gives no examples, run every function you wrote at least once, with checks that follow from the inputs (what goes in comes out, a count after two adds is 2).
3. Build a long input in code from its parts, never typed out: `Array(18).fill(0)`. When an example gives only part of an input, build the whole input and check its size before you trust the result.
4. Each check shows what the code returned when it fails: `assert.deepStrictEqual(got, want)` from `node:assert`.
5. Run the tests whole, without `| tail`, `| head` or `| grep`: the result shows each failure and the exit status.
6. A check fails: find out whether the code, the input you built or the expected value is wrong. When a failure shows how the tests use your code (a type, an exception, a name), apply that everywhere it fits, not only on the failing line.
7. After any fix, run all the tests again, the ones that passed too: a fix can break what worked.
8. A one-off try (call a function, print a value) goes in `tmp/probe.mjs`, run with `node tmp/probe.mjs`; git ignores `tmp/`.

## 3. Code that runs

Compare with `===`; `==` converts types:

```ts
if (input == 0)            // bad: true for "", "0", []
if (input === 0)           // good; `x == null` only to catch null and undefined together
```

Await every promise; `forEach` does not wait:

```ts
const user = getUser(id); user.name;                 // bad: a Promise, name is undefined
const user = await getUser(id);                      // good
items.forEach(async (i) => { await save(i); });      // bad: returns before saving
for (const i of items) await save(i);                // good, or await Promise.all(items.map(save))
try { return load(); } catch { ... }                 // bad: the rejection escapes
try { return await load(); } catch { ... }           // good
```

A method passed as a value loses `this`:

```ts
setTimeout(this.tick, 1000);              // bad: this is undefined inside tick
setTimeout(() => this.tick(), 1000);      // good
```

Do not mutate arrays you were given:

```ts
function sorted(xs) { return xs.sort(); }               // bad: mutates the caller's array, sorts as strings
function sorted(xs) { return [...xs].sort((a, b) => a - b); }
```

Imports must match the export and the module system:

```ts
export default function parse() {}   import { parse } from "./parse.js";   // bad: no named export
export function parse() {}           import { parse } from "./parse.js";   // good
const x = require("./x");            // bad in an ESM file: require is not defined
import x from "./x.js";              // good: ESM needs the full file name at run time
```

Throw `Error` objects with the class and message the request or the project uses:

```ts
throw "invalid width";                        // bad: not an Error, no stack, instanceof fails
throw new RangeError("width must be positive");
```

- Return values, never print them.
- Integer division: `Math.trunc(a / b)`. `-7 % 3` is `-1`. Floats: compare with a tolerance (`toBeCloseTo`).
- `-0 === 0` is true, but deep equality (`toEqual`, `Object.is`) tells them apart.
- `as T` and `!` check nothing at run time.

## 4. Errors

| Error | Usual cause and fix |
|---|---|
| TS2322 / TS2345 not assignable | fix the innermost mismatch (last indented line); `Promise<T>` vs `T` is a missing `await` |
| TS2339 / TS2551 property does not exist | a typo, a union not narrowed, or a `Promise` not awaited |
| TS2304 / TS2552 Cannot find name | missing import or declaration |
| TS2307 Cannot find module | wrong relative path or case, or package missing from `node_modules` |
| TS2305 / TS2614 no exported member; TS1192 no default export | default vs named import; check the exporting file |
| TS2554 Expected N arguments | the call does not match the signature |
| TS18048 / TS18047 / TS2532 possibly undefined or null | narrow: `if (x === undefined) throw ...`, `x?.y`, `x ?? fallback` |
| TS2366 / TS2355 / TS7030 not all paths return | return on every path, or throw at the end |
| TS2835 relative imports need explicit file extensions | `from "./util.js"` (the `.js` name of `util.ts`) |
| `ERR_MODULE_NOT_FOUND` | ESM needs full file names: `./b.js`, `./dir/index.js` |
| `Cannot use import statement outside a module` | Node sees CommonJS: use `require` like the other files, or `.mjs` |
| `Named export 'x' not found ... CommonJS module` | `import pkg from "pkg"; const { x } = pkg;` |
| `ERR_REQUIRE_ESM` | `await import("pkg")`, or make the file ESM |
| `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX` | run with `npx --no -- tsx` |
| `Cannot read properties of undefined (reading 'x')`, `x is not a function` | missing `await`, a failed `find`, default vs named import, lost `this` |

Never silence a type error with `any`, `as unknown as T`, `// @ts-ignore` or a looser tsconfig.
