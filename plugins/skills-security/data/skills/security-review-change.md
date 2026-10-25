Security review of a diff, pull request, file or function the user names; not a whole-codebase audit

Nothing changes in the project: the result is your reply. Fixing is separate work the user asks for (`skill://skills-engineering/security-pitfalls`).

## 1. List the suspicious lines of the change

```bash
export SA="$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py"
python3 "$SA" scan --changed
```

`--changed` covers the uncommitted work against HEAD, new files included; a branch or pull request: `scan --changed origin/main`; named files or folders: `scan src/auth.py src/api/`. The result groups the lines by attack class, each class with its one-line rule for what counts.

## 2. Read the change as a whole

The shapes catch sinks; the logic you catch by reading the diff (`git diff`, `git diff origin/main...`) with these questions:

- A new handler, command, message type or tool: where is its authorization check, and does it check the resource, not only the login?
- A new request field, argument or message field: which old code receives it, and does that code trust its callers?
- A removed or loosened condition, a widened allowlist, a new default, a new `unsafe` block, a disabled check in tests that leaks into production code.
- A new dependency or a version change: what does it execute at install or import, and is it pinned in the lockfile?
- A changed error path: does failure fall open (no auth, no verification, no limit)?

## 3. Trace each line

For each suspicious line and each question that stuck:

1. Backward: where does the value come from? `read graph://used-by/<function>`, `read references://<path:line:col>`, `read <path>:<from>-<to>`. Stop at a lower-trust party (request, upload, argument, message, model output) or at a check that stops the value.
2. Forward: what does the value do at the sink, and to whom?
3. Judge: a finding has who, what, where (path:line of the missing control), so what (the concrete result for someone else). Missing one, it is a hardening note or nothing.

Confirm with a run when you can: a small script in `tmp/` calling the changed function with a hostile value and dummy data; never a deployed service or a real account. A fact outside the code (a proxy, a deployment flag, a library version) that decides the case is a lead, not a finding: name the fact and how the owner checks it.

Severity, for findings only: critical, an anonymous party gets code execution, the data store or any account; high, an explicit control fully defeated with real consequence; medium, a boundary crossed with a small blast radius; low, non-secret internals or much effort for little. Overall never exceeds the demonstrated impact.

## 4. Reply

In this order, each item short:

1. Findings: severity, title, path:line, who can do what, the smallest fix at the sink (its safe form), the test that would have caught it.
2. Leads: the path, the missing fact, how to check it.
3. Hardening notes, separately, without severity.
4. What you did not read and why.

"No security findings" is a plain, valid answer when the traced lines are settled: say what you traced. When the user wants the findings kept for later work, record them with `python3 "$SA" new <verdict>` and `python3 "$SA" finding <file>` as the audit skill does.

## Traps

- Reporting a shape as a finding (a flag, a header, a missing timeout) without the path to a victim.
- Reviewing only the added lines: a deleted check and a changed caller matter as much.
- Trusting a comment that says the input is validated: read the validation.
