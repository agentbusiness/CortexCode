TypeScript and JavaScript here: get it right, run and test it, code that runs, errors and fixes.

## 1. Get it right

1. **Pin down what the code must do.** Read the statement and the code (signatures, stubs, tests).
   - Per function or class: what it takes, returns (a value or a `Promise`) and throws.
   - The statement decides, never the name. "whether" returns a boolean. "find" may return `undefined` or `null`, as stated. "count" returns a number.
   - When there may be no answer and the statement does not name a value for it, the declared return type decides (`T | undefined` or `T | null`); without one, `undefined` is JavaScript's "no value" (`find`, `Map.get`). Not `false`, `-1` or `[]`, and not a value remembered from elsewhere.
   - Keep existing names, signatures and export style (default or named): tests import them exactly.
2. **Before coding, turn the statement into cases** for the check script (section 2).
   - Each sentence that states a behaviour, value, error or limit is one case. A shown requirement list is that list.
   - A sentence with no case is a requirement you will miss.
   - Unsure of a domain, format or protocol? Learn its rules with the research plugin; copy them into cases:
     `read research://search/<topic> rules?sources=web,github&limit=5`, then `read research://fetch/<url>` for the page that states them.
3. **Write whole functions, in slices.**
   - First the simplest complete path (one case, input to result). Run the check script.
   - Add the next stated behaviour. Run again after every change.
   - New file, or a stub you have not changed yet: send it whole once with `edit` `content`. After that: anchored `edits` only, every change in one call (a second whole write is refused).
   - Rewrite a function: replace it from its first line to its closing `}`. Never leave part of the old body or a stray brace.
   - Fix the errors the edit result shows before moving on.
4. **Quick runs** of one example:

```bash
node -e 'const { solve } = require("./src/solve.js"); console.log(solve([3, 1, 2]))'
node --input-type=module -e 'import { solve } from "./src/solve.ts"; console.log(solve([3, 1, 2]))'
npx --no -- tsx try.ts      # a scratch file in the project root; delete it before finishing
```

5. **Run the project's tests** (section 2). Fix the first failure's cause, run again. Change only the code that failure runs through: a function whose cases pass stays as it is unless the task asks to change it.
6. **Before you finish**, check each requirement against the code, including named errors (class and message).

A check disagrees with the statement: the statement wins. Fix the code, name the wrong check.

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
- **Offline:** no `node_modules`: `npm ci --offline` (or `pnpm install --offline`, `yarn install --offline`, per the lockfile).
- Never `npm install <pkg>`: a missing package cannot be fetched. Say so.

**Checking your work.** One scratch file, `check.mjs` (`check.ts` with tsx). Edit it; never re-type it.
- Build inputs in code.
- Copy expected values from the statement, never work them out yourself.
- Print one line per case, no `assert`: one run shows every case.

```js
import { solve } from './src/solve.js';

const cases = [[[3, 1, 2], [1, 2, 3]], [Array.from({ length: 10 }, (_, i) => i % 5), Array.from({ length: 10 }, (_, i) => i % 5)]];
for (const [given, want] of cases) {
  let got;
  try { got = solve(given); } catch (error) { got = `${error.name}: ${error.message}`; }
  console.log(JSON.stringify(got) === JSON.stringify(want) ? 'ok ' : 'BAD', given, '->', got, 'want', want);
}
```

Run `node check.mjs` (`npx --no -- tsx check.ts` for TypeScript sources).
- After any fix, run every case again, passing ones too. One fix can break another case.
- A case fails: check the case before the code. Re-derive it from the statement. A case that breaks a stated rule or holds a value you computed is bad: fix it first.
- Keep the file while the work can change. Delete it when the user is done.

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

Throw `Error` objects with the stated class and message:

```ts
throw "invalid width";                        // bad: not an Error, no stack, instanceof fails
throw new RangeError("width must be positive");
```

- Return values, never print them.
- Integer division: `Math.trunc(a / b)`. `-7 % 3` is `-1`. Floats: compare with a tolerance (`toBeCloseTo`).
- `-0 === 0` is true, but deep equality (`toEqual`, `Object.is`) tells them apart.
  Where a result can be zero, return `x || 0`. In check scripts compare with `Object.is`.
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
