---
id: checks
stage: CHECKS
---
The host ran the proposed checks before any change; their output is above. Write `checks`: the commands the host will run when the work is done, picked from the listed commands. Go through each check:
- It fails because the work is not done yet (a test of the missing behaviour fails, the build breaks on the missing code): keep it.
- It fails because the command itself does not work (a missing program, module, file or argument, or no tests found): replace it with another listed command that covers the work, or leave it out.
- A check that passes now: keep it when it runs the code the work changes (it shows the change breaks nothing that worked); leave it out otherwise.
