Can't find where a behaviour is implemented, what calls it or which tests cover it in a new repo.

## 1. Rules and real commands

Read `rule://`, then `AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`, the README's build section. CI files hold the real commands:

```bash
git log --oneline -15
grep -hE -A3 '^[[:space:]]*(- )?run:' .github/workflows/*.yml 2>/dev/null | head -60
ls Makefile justfile Taskfile.yml package.json pyproject.toml Cargo.toml go.mod 2>/dev/null
```

Entry points: `[[bin]]` or `src/main.rs`, `"bin"` and `"scripts"` in package.json, `[project.scripts]` in pyproject.toml, `cmd/*/main.go`, Makefile targets, route tables, CLI argument definitions.

Run the area's tests once before changing anything, to see existing failures.

## 2. Find where it happens

| You have | Read |
|---|---|
| a name | `symbol://<name>`: its code, what it takes and returns, where it is used, its tests |
| a behaviour in words | `graph://search/<words>` |
| a file | `outline://<path>` before bodies; then ranges `path:120-180` |
| a function or class | `graph://used-by/<name>` (where it is used), `graph://uses/<name>` (what it uses), `graph://path/<name>/<name>` (how two relate) |
| a position | `definition://`, `references://`, `hover://` with `<path:line:col>` |
| a planned change | `graph://impact/<name>`: call sites, covering tests, tasks, rules |

Best search keys: user-visible strings (error messages, log lines, CLI flags, route paths, config keys, UI labels):

```bash
git grep -n "exact text of the message"
```

Trace inward from the entry point (handler -> service -> storage) until you can name the function to change and its callers.

## 3. Read its tests

They show intended behaviour, fixtures, how to call the code. Find them in `graph://impact/<name>`, or:

```bash
git grep -ln "<function_name>" -- '*test*' '*spec*'
```

Copy their setup for a new test.

## 4. Learn the house style

Read two or three similar neighbouring files; copy their error handling, logging, naming, dependency passing, test structure. A line looks wrong: find why before changing it:

```bash
git log -L 40,60:<file> --oneline | head -40
git log -S "<odd_identifier>" --oneline
```

## Traps

- Generated code (a "generated" or "DO NOT EDIT" header, `gen/`, `*_pb2.py`, `dist/`, `build/`): change the generator or its input, then regenerate.
- One name, several definitions (vendored copies, `examples/`, old versions): confirm via `references://` or imports that you edit the one that runs.
- Callers the graph misses (reflection, registries, decorators, string-keyed dispatch, templates, config): also `git grep` the name as a string.
- Monorepos: run commands from the package owning the file, with its manifest and test command.
- Feature flags and config switches: find which branch the case takes before editing.

## Enough reading

Ready when you can name the function to change, its callers, the test that proves the change, and its command. Read more only for a specific missing fact.
