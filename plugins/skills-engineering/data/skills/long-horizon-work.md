Doing a multi-step feature or migration now, alone or via cx tasks for agents; not writing a plan.

## Before the first edit

1. Turn the goal into outcomes, each proved by a command (a test, a build, a CLI run with expected output). None yet: writing that test is step 1.
2. Map what it touches: `read graph://search/<words>`, `symbol://<name>`, `graph://impact/<name>`. Run those tests once: what already fails?
3. Keep the plan in a file; your memory fades in a long conversation. Write `$TMPDIR/plan-<topic>.md` with `edit`:

```markdown
Goal: <one line>. Done when: <check commands>
1. [ ] <step> | check: <command> | cp: <checkpoint sha>
2. [ ] <step> | check: <command>
Facts: <path:line findings needed later>
Dead ends: <approach, why it failed>
```

Reread and update it after each step.

## Steps

- One step = one change that keeps build and tests green; its check fails before, passes after.
- Order: shared types, a thin end-to-end slice, other slices, docs last.
- Over about 20 tool calls: split the step.
- Verified step: save a checkpoint, no commit ("Checkpoint and roll back" in `skill://skills-engineering/git-and-github`); note its sha, mark done.

## Dead ends

Signals: the same failure three times, a fix outgrowing the plan, editing unread code.

1. Stop. Note the attempt and why it failed under Dead ends.
2. Shelve it; return to the last checkpoint (same git section).
3. Read more or try another approach.

Never stack fixes on a broken attempt or edit a check to pass.

## Parallel work with agents

Split only if tasks change disjoint files, none changes code another calls, each has its own check. Else one depends on the other.

Before DECIDE: the DELEGATE route with the goal as `brief`; the host plans and starts agents. Inside a run, by hand:

```bash
cx task create "Goal: CSV export" --brief @/tmp/goal.md          # first word printed: <goal-id>
cx task create "Exporter core" --parent-id <goal-id> --brief @/tmp/core.md --plan @/tmp/core-plan.md \
  --touches src/export/csv.rs \
  --contract '{"checks":[{"name":"unit","kind":"command","spec":"cargo test -p app export::"}]}'
cx call agents.spawn "{\"project_id\":\"$CORTEX_PROJECT_ID\",\"task_id\":\"<task-id>\",\"from_session\":\"$CORTEX_SESSION_ID\"}"
```

- Spawn only tasks whose dependencies are done. With `--depends-on <id>`, same parent, no `--assignee`, a task starts by itself.
- An agent sees only its task: the brief names files, symbols, expected behaviour, what not to touch.
- Follow: `read task://`, `read board://`, `read agent://<name>`. Steer: `cx agent send @<name> "<text>"`. Record decisions: `cx board post "<text>" --kind decision`.
- As an agent blocked by another task: `cx board post "<what you need>" --kind blocker --refs <task-id>`, then continue.

## Done

- Every outcome's check and touched module's tests pass, run fresh.
- Every plan step done, or reported as not done.
- `git status --short` shows only intended files; the diff passed the review in `skill://skills-engineering/git-and-github`.
- Report what changed, how it was verified, what remains.
