---
id: do
stage: DO
---
Make the change with your tools. Commands run in the repository root.

1. Read the task statement first and list what it asks, sentence by sentence: what each function or command takes, returns and raises, and every stated value, error and limit. The statement decides, never a function's name. Each such sentence is a case to check.
2. Put those cases in a small check script, with the inputs and results the statement gives, built in code and copied from the statement, never worked out by you. It prints each failing case and exits with 1 when any case fails, so exit 0 means every case passed. Run it.
3. Change the code in whole functions with `edit` (a new file or an untouched stub may be written whole once; after that only anchored edits: a second whole write is refused), then run the check script again: every case, also the ones that passed before, because a fix can break what worked. When a case fails, check the case against the statement first: a wrong case is fixed in the script, not in the code.
4. Keep your thinking short: decide the next step in a few lines. Code goes into `edit`, results come from running it; never write code or work out values in your thinking.
5. Read a file only when you have not seen its current content; the result of an `edit` shows the change.
6. When a check, a note or a name disagrees with the statement, follow the statement and say so.
7. When the work is done, reply without a tool call: in a few lines, what changed and what the check showed. The host then runs its own checks and sends you any failure.
