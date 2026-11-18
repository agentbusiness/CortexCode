---
id: plan_fields
---
Fill the plan's fields in this order:

1. `analysis`: your thinking, a dozen short lines at most, no code: what must be true when the work is done (every behaviour, value, error and limit the request states, in its words); the order to build it in; how each part is shown done; what is unknown and how to find out.
2. `goal`: what the finished work achieves, in one or two sentences.
3. `approach`: how, and why this way.
4. `architecture`: the modules, data and interfaces it adds or changes. Name a file, function or type only when the conversation or the reads above show it; a new one is named as new.
5. `risks`: what could go wrong or is unknown, each with how to find out.
6. `tasks`, by these rules:
   1. One task per part that can be built and checked on its own (a module, a small group of functions, a command, an option): a small task is finished and checked before the next builds on it.
   2. A request with one such part is one task: a split nobody needs costs a hand-over.
   3. A task changes code. Reading, searching and running the checks are steps inside every task, never tasks of their own: each task finds its own code when it runs.
   4. The one exception: running or producing what the user asked for (a measurement, an export, a report) is a task of its own, after the tasks it needs.
   5. Only what the user asked: other work is named in `note`, never made a task.
   6. Existing names stay: a task builds on the functions, types and files that are there. Removing or renaming something other files use is one task that changes every use too.
   7. Order the tasks so each builds on the ones before it.
   8. When the request reports failures (test output, an error): every failure gets a task that fixes its cause. Code that works and that no failure runs through is not changed.
7. For each task:
- `title`: a few words naming what becomes true ("Retry failed uploads"), never only a function's name and never the method.
- `brief`: what is true when the task is done, in the request's terms: what the part does, takes and gives back, and how it fails. Name a file, function or type only when the conversation or the reads above show it, or as new when the task creates it: never a name you only expect to be there. End with what shows it done: the project's own check that covers it, or the run the user asked for; a new test only when the request asks for one or the project already tests this code. Only this task's own part. No function bodies.
- `known`: what the conversation above already established that this task needs, one line each, copied from the reads and answers above: a file and line with what is there ("src/export.rs:40 writes the header before the rows"), a conclusion the user agreed to. Empty when nothing is.
- `rules`: each thing the request says must hold that this task makes true, one line each in the request's own words ("an empty list gives only the header"). Every rule the request states belongs to one task, at most four to a task: a part with more rules is two tasks. A rule says what must hold, never how.
- `depends_on`: the numbers of the earlier tasks it needs (tasks are numbered from 1 in the order written).
- `touches`: the files or symbols it changes, as far as the conversation shows them; empty for a task that only runs something.
- `checks`: the listed check commands that test this task; none when none does.
- `skills`: skills from the Skills list whose summary names this task's work; usually none.
8. `replaces`: the listed open background work this plan takes the place of. It stops when the plan starts; what it changed stays in the files, so a task can build on it. Leave out work that goes on next to this plan.
9. `note`: one short sentence for the user, naming what more should be done that the user did not ask for.

Examples: "add a CSV export to the report command" is one task, "CSV export". "add a CSV export and a JSON export, and an --out option for both" is three: 1 "CSV export", 2 "JSON export", 3 "--out option" (after 1 and 2). "add a result cache and measure how much faster the report gets" is two: 1 "Cache the results", 2 "Measure the report with and without the cache" (after 1).
