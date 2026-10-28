Security audit or pen-test of this codebase: find, verify, record and report vulnerabilities.

Read-only work: project files never change. The secaudit tool keeps the audit's state and report under the app's home (it survives the turn) and writes only record drafts into the project's `tmp/secaudit/` (git ignores it). The tool drives the audit: it maps the project, decides which attack classes apply, finds the lines worth judging, keeps the records and writes the report. You judge each line it shows: where its value comes from, who controls it, which check stops it.

## The loop

```bash
export SA="$CORTEX_HOME/live/plugins/skills-security/data/scripts/secaudit.py"
python3 "$SA" next
```

The first call maps the project (stack, entry points, the dependencies' known vulnerabilities looked up live, the units: one per attack class that applies here) and shows the first lines to judge. Every later call shows the next open lines, with ids like U3.4, the class's invariant, what counts as a finding, and the current upstream guidance. The audit runs unit by unit: when a unit's last line is settled, the tool prints the unit's one-line result and you reply with it; the audit goes on with the next unit after that reply. When nothing is open it writes the report, prints it whole with its path, and says the audit is complete: the audit ends there and nowhere else. A scoped request ("audit the API", "only src/"): run `python3 "$SA" recon --scope src` once before `next`.

For each line shown:

1. Backward: `read <path>:<from>-<to>` around it; `read graph://used-by/<function>` and `read references://<path:line:col>` for who calls it. Walk back until you reach a lower-trust party (a request field, an upload, an argument, a message, a model output, another app) or a check that stops the value (validation, allowlist, authorization, escaping, parameter binding).
2. Forward: what does the value do at the sink, and to whom?
3. Settle it with the fact that decided it:

```bash
python3 "$SA" settle U3.4 safe "bound as a parameter at app/db.py:40"
python3 "$SA" settle U3.1 U3.2 U3.5 safe "fixtures built by tests/conftest.py:12, never shipped"
python3 "$SA" settle U2.3 hardening "upgrade rustls to 0.23.46; the vulnerable path is not reached"
python3 "$SA" settle U9.2 blocked "the proxy configuration is not in the repository"
python3 "$SA" settle U3.7 finding
python3 "$SA" settle U8.1 lead
```

One unit only, when the task names it (other agents do the other units at the same time): `python3 "$SA" next --unit U3` shows that unit's lines and, once they are all settled, its one-line result; nothing else is yours.

When the project was audited before, the first unit is U0, the previous audit's issues, each with what the code shows now: settle each as `fixed "<what changed>"` or `still "<re-verified how>"`. Several ids share one reason when one control covers them, and that reason names the control as path:line in the project (the tool checks the place exists): `settle U4 safe "every handler sits behind the loopback middleware, startup.rs:137"` settles a whole unit. Then `python3 "$SA" next` again. A unit the tool itself could not show stays open; it comes back after the other units, and if it fails again the tool prints the settle that records the failure.

## Finding or lead

A finding has all four parts: who (a lower-trust party), what (an input or action they control), where (the control that should stop it and does not, as path:line), so what (a concrete result for someone else or for the system). Missing one, it is `safe` or `hardening`.

- `finding`: you read the whole path and saw the result. Prefer a run: a small script in `tmp/` calling the function or the local server with a hostile value and dummy data. Static is enough only when the code leaves no doubt. Never a deployed service, a real account, or anything that deletes, sends or pays.
- `lead`: the path is real but one fact outside the code decides it (a proxy, a deployment flag, a library version, a provider setting). Name that exact fact and how the owner checks it.

`settle ... finding` or `lead` writes `tmp/secaudit/finding-<n>.json` with the sink prefilled. Replace every value that starts with `? ` using `edit`, keeping the field names: the trace starts at the entrypoint and ends at the sink, each step a real path and line you read; the fingerprint is `<class>:<file>:<function>` in lowercase. Then:

```bash
python3 "$SA" finding tmp/secaudit/finding-1.json
```

It names each field to fix, or records it; `next` does not move on while a record is unfilled.

Severity, for findings only, from the demonstrated result, never from the class name: critical, an anonymous party gets code execution, the whole data store or any account; high, an explicit control fully defeated with real consequence (auth bypass, another tenant's data, stored script for other users); medium, a boundary crossed with a small blast radius or rare preconditions; low, non-secret internals or much effort for little. Overall never exceeds impact. The fix is the smallest change at the sink, its safe form (`skill://skills-engineering/security-pitfalls`), plus the test that would have caught it.

## The replies

Two kinds. After a unit is settled: the one line the tool printed for it, nothing more. After `next` prints the report: the report's summary, every confirmed finding (severity, title, path:line, the result in one line, the fix), every lead with its missing fact, what was blocked, and the report's path. Zero findings is a valid result: say what was covered, unit by unit. Fixing is separate work the user asks for.

## Traps

- Replying while the unit shown still has open lines: the next step is always the one the tool printed last.
- A line judged in your head but not settled: it stays open and the audit does not end.
- A shown line taken as a finding without the trace (seen: a cookie flag with no sensitive cookie behind it).
- Guessing a deployment fact either way: that is a `lead`.
