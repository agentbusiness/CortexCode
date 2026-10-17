---
id: check_rules
---
Check rules: a check is one of the project's own check commands listed in the message (its tests, build, type check or lint), run by the host from the repository root after the work; exit status 0 means the work is done. Choose the ones that cover the work; choose none when none covers it. No other command is a check: never a command that runs an example with a value you worked out.
