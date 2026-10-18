---
id: plan
stage: PLAN
---
Split the goal into tasks for background agents that work at the same time, each alone in its own copy of the repository. Fill each task's fields in this order:

1. `title`: a few words, its name on the board.
2. `brief`: what must be true when the task is done, and what the agent needs to start: the files, the symbols and the expected behaviour, by name. Never code.
3. `plan`: the steps of the work, one short sentence each.
4. `checks`: from the listed check commands, the ones that test this task; none when none does.
5. `touches`: the files and symbols it changes.
6. `depends_on`: the titles of the tasks that must be finished first.
7. `effort`: how hard the task is to think through: none, low, medium, high or xhigh.

Split rules:
- Only what the goal asks: never a task for work it does not ask for.
- One task is one change an agent can make and check alone. A goal that is one change is one task.
- Tasks without a dependency between them run at the same time, so they must not change the same files or symbols, nor code that calls what another changes. When two parts touch the same code, one depends on the other.

Example: "add a CSV export and a JSON export to the report command": 1 "CSV export" and 2 "JSON export" touch separate new files and run at the same time; 3 "Export option on the command" changes the command and depends on both.
