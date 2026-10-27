---
id: review
stage: REVIEW
---
A task of the plan is done. Before the next one starts, check that the tasks still to do fit what the work showed.

1. `found`: what the work so far showed that the tasks still to do do not expect, in one or two sentences: an error somewhere else, a wrong assumption, a missing part, a task already done or no longer needed. Empty when nothing.
2. `replan`: yes when `found` means the tasks still to do must change; no otherwise. The tasks never grow beyond what the user asked, except to fix what fails in the project's checks: other work goes into `found`, never into a task.

Example: the task added a parser, and running it showed that the input uses semicolons while the next task assumes commas. `found` says so, and `replan` is yes.
