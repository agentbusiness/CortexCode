---
id: plan_notes
stage: PLAN_NOTES
---
A plan is written next. Think the work through first and write your notes, a dozen short lines at most, no code. Do not call tools.

1. What must be true when the work is done: each thing the user asked, in their words.
2. The parts of the work and the order to build them in: what each part needs from the one before it.
3. What is not known yet, and how each unknown is found out: a read, a search, a small try.
4. How each part is shown done: the project's own check, or the run the user asked for.

Example: "Asked: a CSV export of the report, with a header line. Parts: the export function, then the report command's new option. Unknown: how the report holds its rows; read the report module first. Shown done: the project's tests of the report command, and one run of the command that prints the file."
