Work must happen later or on a clock: check back in N minutes, a weekly search, a nightly command.

A routine runs on its own clock after your turn ends: you keep being woken, an agent keeps doing
the brief, the command keeps running, until the routine ends or the user removes it. The user sees
every routine on the card and in the Schedule pane.

## Pick the kind

1. `wake` (the default): wakes this conversation with the brief. For checking back on something
   you are waiting for in this conversation.
2. `handoff`: an agent does the brief in a session of its own each time, and its result is kept
   there. For work that stands alone and repeats: a search, a review, a report.
3. `command`: runs a command in the project; you are woken only when it fails. For checks a
   command answers by its exit code.

## Make one

```bash
cx schedule add "<the step to take when woken>" --when "every 10m" --until done
cx schedule add "<the whole brief for the agent>" --kind handoff --when "every mon at 09:00"
cx schedule add --kind command --command "<check command>" --when "every day at 02:00"
```

1. Write the brief as the full step the run will take: it is all the run reads.
2. `--when`: `every 5m`, `every 2h`, `every day at 09:00`, `every mon,thu at 09:00`, `weekdays at
   8am`, `once at 2026-11-02 09:00`, `in 20m`, `cron 0 9 * * 1`. The answer lists the next times;
   check they are what you meant.
3. `--until done` ends it when the agents this conversation started are finished; `--until
   2026-12-01` on that date; `--max-runs 3` after three runs.
4. Work for every project (not this one): `--global`.
5. Scheduling the same thing again returns the routine that has it; edit that one instead.

The host already checks on the agents you start; you need no routine for that.

## Look and change

```bash
cx schedule list            # this project's routines and the global ones, with their next times
read schedule://r-3f2a9c    # one routine: what it does and its last runs
cx schedule edit r-3f2a9c --when "every 1h"
cx schedule pause r-3f2a9c  # resume, run (once now), remove
```

When a routine wakes you, its brief is the step: do it, and end your turn. When what it waited for
is done, remove it with `cx schedule remove <id>`.
