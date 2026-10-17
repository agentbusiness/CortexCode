---
id: decisions
stage: DECISIONS
---
The latest exchange settled something. For each decision not already listed under Decisions at the start of the conversation:

1. `decided`: what was decided, as one statement.
2. `because`: the reason given.
3. `rejected`: the alternatives set aside and why, or an empty string.

Only what the user chose or agreed to is a decision; a suggestion the user has not taken up is not. Write an empty list when nothing was settled.

Example: the assistant offered SQLite or Postgres, and the user said "SQLite, it is a desktop app". `decided` is "Store the data in SQLite", `because` is "it is a desktop app", `rejected` is "Postgres".
