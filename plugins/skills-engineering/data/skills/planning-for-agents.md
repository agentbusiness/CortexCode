Writing or revising a plan and task graph for agents before code changes (plan mode); not doing it.

## What happens to the plan

- Nothing changes until the user asks to implement.
- One task: runs in the foreground. Several: background tasks with their checks as contract; an agent starts each once its `depends_on` are done.
- An agent sees only its task's `brief` and `skills`, the plan's `goal` and `approach`, and its `covers` requirements. Not the conversation, `architecture` or other tasks.

## Document fields

- `goal`: with the requirement ids it serves.
- `approach`: plus the rejected alternative and why. A user-only choice (look, naming, options): pick a default, ask in `note`.
- `architecture`: every shared name exactly: types with fields, signatures, paths, JSON shapes, flags. Copy what each task uses into its `brief`.
- `risks`: an unknown that could change the plan becomes task 1, a spike with a check, that later tasks depend on.

## Tasks (at most 12)

1. **Size**: a few files = one task: one change an agent finishes and verifies alone, tests included. Never split "implement" from "write tests"; never one task per file.
2. **Order**: shared types or interface; a thin end-to-end slice; independent slices in parallel; wiring, docs, cleanup last.
3. **`brief`**: files and symbols, expected behaviour with the spec's values, shared names, the new test's name, what not to touch.
4. **`touches`**: exact paths, with tests and registration files (`mod.rs`, `__init__.py`, `index.ts`, manifests, route tables). A path you did not read is a guess: note it in `risks`.
5. **Conflicts** (only the plan prevents parallel edits): tasks sharing a path, or one changing a function the other calls: `depends_on` or merge. Many tasks registering in one file: task 1 creates all the empty entries and registrations.
6. **`depends_on`**: real needs only; a chain of every task serialises the agents.
7. **`checks`**: the project's test or build command narrowed to the new test (file, module or name filter); fails before, passes after. Repo root: no `cd`, `||` or installs. Expected values from the spec or repo only. No possible check: merge the task into one with a check.
8. **`covers`**: every open requirement id in some task; uncoverable ones go to `risks`.
9. **`skills`**: only these reach the agent.

```
title: CSV writer
brief: Add `fn write_csv(rows: &[Row], out: &mut impl Write) -> io::Result<()>` in src/export/csv.rs
  (header row; quote fields containing , or "). Add test `export::csv::quotes_commas`. Do not change src/store.rs.
depends_on: [1]   touches: [src/export/csv.rs]   covers: [R1]
checks: [cargo test -p app export::csv]
```

## Revising from feedback

- Change only what is asked; other tasks keep their exact text.
- Keep task numbers stable; after removing or inserting one, recheck later `depends_on`.
- New or changed requirement: update `covers`, add a task if needed.
- Rejected approach: rewrite `approach`, `architecture` and affected tasks.
