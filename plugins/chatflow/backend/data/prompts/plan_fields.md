---
id: plan_fields
---
Fill the plan's fields in this order:

1. `analysis`: your thinking, a dozen short lines at most, no code: what must be true when the work is done (every behaviour, value, error and limit the statement states, in its words; what each function takes, returns and raises, from the statement, never from a name); the order to build it in; how each part is checked (the statement's own examples); what is unknown and how to find out.
2. `goal`: what the finished work achieves, in one or two sentences.
3. `approach`: how, and why this way.
4. `architecture`: the modules, data and interfaces it adds or changes, by their names in the repository.
5. `risks`: what could go wrong or is unknown, each with how to find out.
6. `tasks`: one task for the whole work, unless the request names several parts (several features, commands, functions or files it asks for): then one task per named part. A task changes code: never a task only to verify, test or review. Reading the files, searching and running the checks are steps inside every task, never tasks of their own: each task starts with its files already read and ends by running its own cases. For each task:
- `title`: a few words, its name on the board.
- `brief`: what must be true when this task is done, which part it builds (files and symbols by name), and the statement's cases that show it works. Only this task's own part: what a later task builds is not in it. Never code.
- `depends_on`: the numbers of the earlier tasks it needs (tasks are numbered from 1 in the order written).
- `touches`: the files or symbols it changes.
- `checks`: the listed check commands that test this task; none when none does.
- `covers`: the spec's requirement ids it serves.
- `skills`: skills from the Skills list whose summary names this task's work; usually none.
With several tasks, order them so each builds on the ones before it.
7. `note`: one short sentence for the user.

Examples: "add a CSV export to the report command" is one task, "CSV export", even with several rules to meet. "add a CSV export and a JSON export, and an --out option for both" names three parts: 1 "CSV export", 2 "JSON export", 3 "--out option" (after 1 and 2).
