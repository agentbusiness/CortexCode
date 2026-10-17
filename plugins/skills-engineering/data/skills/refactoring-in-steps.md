Splitting, extracting or moving code across files, behaviour unchanged; not a rename or bug fix.

One rename needs no steps: find uses with `references://<path:line:col>` and `git grep -n -w "<name>"`, edit all, build, test.

## Rules

- **Green to green**: build and tests pass before you start and after every step. Red over several edits: roll back, take a smaller step.
- **Thin coverage**: first add tests asserting what the code returns now, odd results included.
- **Structure only**: no behaviour change, rename or cleanup mixed in. Test assertions stay; only test imports change.

## 1. Blast radius

- `read outline://<file>`: the items.
- `read graph://impact/<name>` per item to move: call sites, covering tests.
- The name as text, for what the graph misses (reflection, registries, templates, config, docs):

```bash
git grep -n -w "<name>"
```

Keep working: public import paths, serialized names (JSON keys, DB columns, stored enums), CLI flags, env vars.

## 2. Split a file into modules

1. Group items by what they use (`read graph://uses/<name>`). Items both new modules need: a third, shared module (else an import cycle).
2. Create the new files; move items verbatim.
3. The old module re-exports the moved names. Build, test.
4. Migrate importers in batches; build after each.
5. Drop re-exports only if nothing outside the project imports the old path.

Prove only imports and visibility changed:

```bash
diff <(git show HEAD:<old-file> | sort) <(cat <old-file> <new-files> | sort)   # expect import, visibility, module and blank lines
```

Moving one item: create, re-export or delegate, migrate, delete the old copy when `git grep` finds no use.

## 3. Extract a function

Check what crosses the cut: early `return`, `break`, `continue`, exceptions, variables assigned inside and read after (return them).

## What restructuring silently changes

- Module-level side effects (registrations, decorators, static initialisers) run at import: never, if nothing imports the module.
- Evaluation order; laziness (a generator becoming a list, a lazy import becoming eager).
- Which handler catches an exception.
- Test patches on the old module path stop applying: patch where the name is looked up.
- Visibility (`pub`, exports) from the new place.

## Done

Build, lint and touched modules' tests pass; `git diff` is structural only; newly dead code is deleted.
