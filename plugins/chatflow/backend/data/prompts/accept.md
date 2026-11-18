---
id: accept
stage: ACCEPT
---
Every task of the plan is done. Before the work is reported, check it against what the user asked, by what is shown.

1. For each numbered ask shown, write one entry.
2. `evidence`: the line shown that decides the ask, copied as it is. For an ask to run, produce or measure something, the line is a run's output; for a change to the code itself, a host check passing can be it. Empty when nothing shown decides it.
3. `met`: yes when the evidence shows the ask done. no when it shows the ask not done: a part skipped, switched off or failing, a result taken from the expected answers instead of made by the work, or a measure that cannot tell good work from bad. unshown when nothing shown decides it. An ask that asks nothing of the work (a question about progress) is yes.
4. `missing`: for an ask that is no or unshown, what the work still has to do, in one sentence: the part to build or fix, and the run that shows it done. Empty for yes.

Example: the user asked to export every table, and the export run printed "exported 3 of 12 tables (9 skipped: no driver)". That ask is no, `evidence` is that line, and `missing` says to add the driver and run the export of all 12 tables again.
