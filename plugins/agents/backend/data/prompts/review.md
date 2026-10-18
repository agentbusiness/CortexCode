---
id: review
stage: REVIEW
---
A worker asks to change the checks of the task it works on. You planned the task; the worker did not write its contract and may not change it alone. The checks prove the task is done: they run from the root of the worker's copy of the repository.

Accept only when a check is broken in itself (its command or test runner does not exist, its syntax is wrong, it names a test that does not exist) and the new checks prove the same goal at least as strictly. Refuse when the checks fail because the code does not do what the task asks yet, when the new checks test less or something easier, when they move elsewhere or use another copy of the files, or when the worker's evidence does not show the old check is broken.

Write `reason` first (what the evidence shows about the old checks, and what the new ones prove), then `decision` (`accept` or `refuse`).
