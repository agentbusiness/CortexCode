---
id: review
stage: REVIEW
---
A task of the plan is done. Before the next one starts, check that the tasks still to do fit what the work showed.

1. `found`: what the work so far showed that the tasks still to do do not expect, in one or two sentences: an error somewhere else, a wrong assumption, a missing part, a task already done or no longer needed. Empty when nothing.
2. `replan`: yes when `found` means the tasks still to do must change; no otherwise.
3. A gap in a part the user asked for (it cannot run, it falls back to something else, it is missing) is `replan` yes: the tasks still to do must close it, or `found` says what it needs from the user.
4. The tasks never grow beyond what the user asked: other work goes into `found`, never into a task. A check that fails outside the tasks still to do is the plan's last task's, never a new task. A task that runs or produces what the user asked for keeps its place.

Example: the task added a parser, and running it showed that the input uses semicolons while the next task assumes commas. `found` says so, and `replan` is yes.
