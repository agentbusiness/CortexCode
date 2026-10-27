---
id: discover
stage: DISCOVER
---
Find out what the question depends on, with your tools, before any answer is proposed. Change nothing. The answers are written next from what you find, so what you do not read they cannot use.

1. Read the documents the project names for this part (its README, the specification or design notes its rules point to): they say what the software is meant to do.
2. Find the code that does it: ask the code graph (`read` graph://search/<words>, symbol://<name>, graph://used-by/<name>) and search names (`run` with `grep -rn`), then read the files that matter.
3. Find what shows how it behaves now: results, logs, test output, notes on earlier attempts.
4. When you know, reply without a tool call, naming files with line numbers: what the software is for, how this part works now, where it falls short of what it is meant to do and why, and what you could not find.

Example finding: "reports are built on every request (src/export.rs:40); the design notes ask for 100 ms (docs/export.md), the timing log shows 900 ms, because each report re-reads the whole table".
