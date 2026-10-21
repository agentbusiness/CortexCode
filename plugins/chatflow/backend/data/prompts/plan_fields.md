---
id: plan_fields
---
Fill the plan's fields in this order:

1. `analysis`: your thinking, a dozen short lines at most, no code: what must be true when the work is done (every behaviour, value, error and limit the request states, in its words; what each function takes, returns and raises, from the request and the project's code and tests, never from a name alone or from memory of similar code); the order to build it in; how each part is checked (the project's tests, or the request's own examples); what is unknown and how to find out.
2. `goal`: what the finished work achieves, in one or two sentences.
3. `approach`: how, and why this way.
4. `architecture`: the modules, data and interfaces it adds or changes, by their names in the repository.
5. `risks`: what could go wrong or is unknown, each with how to find out.
6. `tasks`: one task for the whole work, unless the request names several parts (several features, commands, functions or files it asks for): then one task per named part. A task changes code: never a task only to verify, test or review. Only what the user asked: never a task for work the user did not ask for (name it in `note` instead), and each task the simplest way to do it. Reading the files, searching and running the checks are steps inside every task, never tasks of their own: each task starts with its files already read and ends by running its own cases. For each task:
- `title`: a few words naming what it makes happen ("Retry failed uploads"), never only a function's name ("Implement retry_upload") and never the method: what becomes true, not how it is done.
- `brief`: what must be true when this task is done, not the method to get there (unless the user named one): what each function it builds takes and returns ("returns the parsed rows, or an empty list for an empty file"), from the request and the project, never from a name alone; which part it builds (files and symbols by name); and the check that shows it is done: the project's tests, the request's examples, or for a property no test runs (a type's variance, thread safety) a check that compiles or passes only when it holds. When that check passes, the task is done. Only this task's own part: what a later task builds is not in it. Never code.
When the request reports failures (test output, an error): find what causes each one, and give every failure a task. Code that works and that no failure runs through is not changed.
- `rules`: each thing the request says must hold that this task makes true, one line each in the request's own words: a behaviour, a value, an error and its message, a limit ("a roll over 10 pins is refused", "an empty list returns 0"). Every rule the request states belongs to one task; a rule is a check, never a method.
- `depends_on`: the numbers of the earlier tasks it needs (tasks are numbered from 1 in the order written).
- `touches`: the files or symbols it changes.
- `checks`: the listed check commands that test this task; none when none does.
- `covers`: the spec's requirement ids it serves.
- `skills`: skills from the Skills list whose summary names this task's work; usually none.
With several tasks, order them so each builds on the ones before it.
7. `note`: one short sentence for the user, naming what more should be done that the user did not ask for.

Examples: "add a CSV export to the report command" is one task, "CSV export", even with several rules to meet. "add a CSV export and a JSON export, and an --out option for both" names three parts: 1 "CSV export", 2 "JSON export", 3 "--out option" (after 1 and 2).
