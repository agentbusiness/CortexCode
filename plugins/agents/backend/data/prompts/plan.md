---
id: plan
stage: PLAN
---
Split the goal into tasks for background agents that work at the same time, each alone in its own copy of the repository. Fill each task's fields in this order:

1. `title`: a few words, its name on the board.
2. `brief`: what must be true when the task is done, and what the agent needs to start: the files, the symbols and the expected behaviour, by name, and the task's own test that shows it is done (never "no new warnings", never the whole suite unless that is the task). Never code.
3. `plan`: the steps of the work, one short sentence each.
4. `checks`: from the listed check commands, the ones that test this task; none when none does.
5. `touches`: the files and symbols it changes.
6. `depends_on`: the titles of the tasks that must be finished first.
7. `effort`: how hard the task is to think through: none, low, medium, high or xhigh.

Split rules:
- Only what the goal asks: never a task for work it does not ask for.
- One task is one small part an agent can make and check alone: a new file or module, a function group, a command. A goal that is one such part is one task; a goal with more parts is one task per part.
- Tasks without a dependency between them run at the same time, so they must not change the same files or symbols, nor code that calls what another changes. When two parts touch the same code, one depends on the other.
- When the parts add new files, the first task, "Lay out …", adds each new file with nothing in it yet and registers it (a `mod` line, an export, an import, a route entry); the part tasks depend on it and change only their own files, so they run at the same time.

Example: "add a CSV export and a JSON export to the report command": 1 "CSV export" and 2 "JSON export" touch separate new files and run at the same time; 3 "Export option on the command" changes the command and depends on both. "add a parser module and a cache module": 1 "Lay out parser and cache" adds src/parser.rs and src/cache.rs, empty, and registers both in src/lib.rs; 2 "Parse the input" and 3 "Cache parsed results" depend on 1 and run at the same time.
