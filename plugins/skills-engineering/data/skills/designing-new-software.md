New program, service or library from scratch: data model, boundaries, build order; not ideation.

## Order of work

1. **Runnable example first**: the task's exact command or call, input and expected output. Its test is the first check.
2. **Scaffold with the ecosystem's tool**: `cargo new`, `go mod init`, `npm init` plus `tsconfig.json`, `pyproject.toml`.
3. **Data model before logic**: entities, fields, identity, invariants, who changes each. Illegal states impossible: an enum, not flag combinations; required fields non-null; units in names (`timeout_ms`). How each type holds its data (ownership, references, containers): `skill://skills-engineering/designing-the-code`.
4. **Walking skeleton**: the thinnest path through every layer (entry point -> core -> storage or IO -> output), trivial logic, tested end to end.
5. **One slice at a time**, each with its test, each running before the next.
6. Then error messages, configuration, README.

More than about five slices: use `skill://skills-engineering/long-horizon-work`.

## Boundaries

- The core (domain rules) does no IO and imports no adapter (CLI, HTTP, database, files, clock, randomness, network).
- Pass effects in: data or a small interface (`Clock`, `Store`) from the caller, never a global.
- Validate input at the edge into typed values; inside, trust the types.
- Single-implementation interfaces only at IO boundaries. No frameworks, plugin systems, config layers or abstract base classes before a second use.
- Library code returns errors with context; never prints and continues, never exits.
- Files and formats the program writes carry a version field from day one.

## Layout

Unless the ecosystem has a stronger convention:

```
src/<domain>/     types and rules, no IO
src/<adapter>/    cli, http, db
tests/            end-to-end tests driving the entry point
README.md         build, run, test commands
```

## Dependencies

Check what exists first:

```bash
pip list                                  # the session venv
ls ~/.cargo/registry/src/*/ 2>/dev/null   # crates downloaded before
ls "$(go env GOMODCACHE)" 2>/dev/null     # Go modules downloaded before
ls node_modules 2>/dev/null               # packages already installed here
```

Prefer the standard library. A new dependency must save much code, and it is a change to the user's project: ask before adding one.

## Before finishing

- The first example passes, run fresh.
- Each core rule has a unit test; the end-to-end test drives the real entry point.
- A clean checkout builds and runs with exactly the README's commands.
