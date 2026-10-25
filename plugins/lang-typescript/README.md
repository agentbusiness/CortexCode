# TypeScript (`lang-typescript`)

Everything CortexCode knows about TypeScript and JavaScript, as one `lang@1` plugin (the `lang`
family, see [docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of
`.ts` / `.tsx` / `.js` files and `package.json` manifests, typescript-language-server for
diagnostics, definition, references and hover, the tsc / npm test / eslint verifiers, and the
TypeScript skills. A `LanguagePack` served by `cc-lang-kit`, built like the reference plugin
`lang-rust`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-typescript`: category `lang-typescript`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

Methods are called on the category: `lang-typescript.describe`, `lang-typescript.extract`, ...

- **Languages** (names and symbol-id prefixes are the code graph's, so ids do not change):
  `typescript` (`.ts`, `.mts`, `.cts`), `tsx` (`.tsx`) and `javascript` (`.js`, `.jsx`, `.mjs`,
  `.cjs`, and files whose `#!` line runs `node`), all with prefix `js`; `package.json` (prefix
  `npm`, file name `package.json`).
- **extract**: tree-sitter-typescript / tree-sitter-javascript facts (functions, classes,
  interfaces, type aliases, enums and their members, namespaces, module-level variables and object
  literal methods with qualified names `Class.method`, signatures, JSDoc, decorators, parameters
  and types; React components and their props, also from a named props type; imports, `require`,
  exports and `module.exports`; calls, instantiations, renders, hook uses, `this` field reads and
  writes, thrown errors, `process.env` / `import.meta.env` reads and receiver-typed locals;
  jest / vitest / mocha / node:test tests named by their `describe` chain, Express routes, Electron
  `ipcMain` channels and `ipcRenderer` calls, Tauri `invoke` (when imported from
  `@tauri-apps/api`), `fetch` / axios requests with literal URLs and child processes run) and
  package.json facts (name, scripts with the repository files and sibling scripts they run, `bin`
  entries, dependencies by section). The extractors are the memory plugin's, ported to `lang@1`
  facts. With `extract`'s `root`, dependencies get their installed version and source directory
  from `node_modules` (the package directory, then its ancestors up to the root, as Node resolves),
  and scripts and bins the repository files they name; without it only what the file says is
  reported.
- **Server**: typescript-language-server 6.0.2 from `$CORTEX_HOME/toolchains/typescript-language-server/6.0.2`
  (installed with its TypeScript), else one on PATH; accepted only when `--version` runs (it needs
  `node` on PATH). The TypeScript it runs is its own choice, in this order: the workspace's
  `node_modules/typescript` (from the root up, also Yarn / pnpm SDKs) when it ships `tsserver.js`
  (TypeScript 6 and older), then CortexCode's TypeScript 6.0.3 (`tsserver.fallbackPath`), then the
  one beside the server. `tsserver.path` is not set, since it would override the workspace's own
  TypeScript. The server's description (and its `started` event) says which TypeScript serves a
  root. Projects on TypeScript 7 (the native compiler, no `tsserver.js`) are served by the
  TypeScript 6 fallback; TypeScript 7's own server (`tsc --lsp --stdio`) is not used (see
  `src/server.rs`: the kit now supports what it needs, per-root programs, pulled diagnostics and
  lower-cased URIs, but the server's behaviour against the kit could not be checked on this machine).
  JavaScript outside any tsconfig/jsconfig is the server's inferred project
  with `checkJs` off (navigation and syntax errors, no type errors; a jsconfig.json with `checkJs`
  turns them on). Automatic type acquisition is off (offline). `[plugin.config] server_path` names
  another executable.
- **install**: `npm install --prefix` of `typescript-language-server@6.0.2` into
  `toolchains/typescript-language-server/6.0.2/` and `typescript@6.0.3` into
  `toolchains/typescript/6.0.3/` (needs npm and the network; never global, nothing else is
  touched).
- **Verifiers**: `npx --no --offline -- tsc --noEmit -p .` (build, with `tsconfig.json`),
  `npm test` (test, with `package.json`), `npx --no --offline -- eslint .` (lint, with an ESLint
  config: `eslint.config.{js,mjs,cjs,ts,mts,cts}` or `.eslintrc*`); they require `npx` / `npm` on
  PATH. The tools come from the
  project's own `node_modules/.bin`: `--no` never installs, `--offline` never asks the registry.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-typescript     # -> plugins/lang-typescript/target/lang-typescript-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-typescript --install`). The probe extracts a
sample through the real `extract`, reads every skill and reports whether typescript-language-server
can run (an absent server does not fail the probe: extraction and skills work without it).
