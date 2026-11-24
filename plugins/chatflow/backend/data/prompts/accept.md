---
id: accept
stage: ACCEPT
---
Every task of the plan is done. Before the work is reported, check it against what the user asked, by what is shown.

1. For each numbered ask shown, write one entry.
2. `evidence`: the line shown that decides the ask, copied as it is. Empty when nothing shown decides it.
   - An ask about what the software does (its results, its numbers, its speed, what it finds, which device or settings it uses): the line is from a run of the software itself on the user's data, under "What the work's runs printed".
   - An ask about the code itself (a name, a file, dead code, a build that passes): a host check passing can be it.
   - A host check passing, or a test's name with "ok", shows that the tests pass. It is never the evidence for what the software does.
   - A run marked "a script this work wrote itself", and a tally or verdict from a report the work wrote (an audit, a gap list, an inventory), is the work's own account. It never decides an ask about what the software does: that ask is unshown unless the software itself ran.
3. `met`: yes when the evidence shows the ask done. no when it shows the ask not done: a part skipped, switched off or failing, a fallback used in place of the asked part, a result taken from the expected answers instead of made by the work, or a measure that cannot tell good work from bad. unshown when nothing shown decides it. An ask that asks nothing of the work (a question about progress) is yes.
4. `missing`: for an ask that is no or unshown, what the work still has to do, in one sentence: the part to build or fix, and the run of the software that shows it done. Empty for yes.

Example: the user asked to export every table, and the export run printed "exported 3 of 12 tables (9 skipped: no driver)". That ask is no, `evidence` is that line, and `missing` says to add the driver and run the export of all 12 tables again.
