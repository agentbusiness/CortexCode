Fix one recorded security finding: trace it, change the sink, prove it with a test, record the fix

The finding came from a security audit and sits in the project's graph with its trace, its fix and the test that proves it. The Security tab started this session for it. Your work ends when the fix is recorded, not when the code is changed.

## 1. Read the finding

```bash
export SA="$CORTEX_HOME/live/plugins/skills-security/data/scripts/secaudit.py"
python3 "$SA" show <id>
```

The id is in the user's message (`<class>:<file>:<function>`, or `hardening:...`). The output gives who the attacker is, what they can do, the trace from the entry point to the sink as `path:line`, the fix the audit proposed, the test that proves it, and what the code says now. For a dependency finding it lists the exact upgrade commands (`python3 "$SA" fix-plan <id>` prints them alone).

## 2. Make the fix at the sink

1. `read` the sink and the step before it; `read graph://used-by/<function>` for who else reaches it.
2. Change the last trusted point: the safe form of the sink (`skill://skills-engineering/security-pitfalls`), the check that was missing, or the version bump. The smallest change that closes the path; nothing beyond the finding.
3. Run the test the finding names; when there is none, write the regression test the finding describes and run it, then the project's test command.

## 3. Record the fix

```bash
python3 "$SA" resolve <id> "ran <test command>: <what it showed>"
```

`resolve` checks the code first: the flagged line must have changed or gone, or the package must pin a newer version. When nothing changed it says so and the finding stays open; fix, then resolve again. When it records, reply with one line: the change as `path:line`, how it was verified, and that the finding is marked fixed.

## Traps

- Replying before `resolve` recorded the fix: the Security tab still shows the finding as being fixed, and the turn does not end.
- Fixing more than the finding: other suspicious lines are the audit's work, not this session's.
- A version bump without the test run: the lockfile changes, the build may not.
