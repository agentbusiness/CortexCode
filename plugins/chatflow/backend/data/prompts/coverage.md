---
id: coverage
stage: COVERAGE
---
Before this task counts as done, compare your check script with the statement's requirements shown in the task.

1. `missing`: each requirement your check script does not yet test with both a case that meets it and a case that breaks it (an input the requirement does not allow, which must be rejected or raise). Leave out requirements no input can break, such as how the code is named or which libraries it uses.

Empty when every requirement has both. Example: a requirement "an order holds at most 5 items" has a case with 5 items and a case with 6 that must be rejected; with only the 5-item case, it is missing.
