Security audit or pen-test of this codebase: find, verify, record and report vulnerabilities.

Read-only work: project files never change; everything the audit writes goes to `tmp/secaudit/` (git ignores it). The secaudit tool does the mechanical part (the stack, the checklist, the suspicious lines, the record checks, the report); you trace and judge.

## 1. Map the project

```bash
export SA="$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py"
python3 "$SA" recon
```

A scoped request ("audit the API", "only src/"): `python3 "$SA" recon --scope src`.

The result names the stack, the entry points and handlers, the dependency files with every known vulnerability of the pinned versions (looked up live in the OSV database, with the fixed versions), and the units U1, U2, ...: one per attack class that applies here, with the structural fact that makes it apply and how many suspicious lines it found. The units are the audit's checklist; nothing else counts as coverage.

## 2. Hunt one unit at a time

```bash
python3 "$SA" hunt U1
```

It prints the unit's invariant, where to look, what counts as a finding, the current upstream guidance for the class (fetched live), and the suspicious lines as path:line grouped by shape. The dependencies unit prints the vulnerable packages instead of lines: for each, find whether the project calls the affected code with outside data. For each line:

1. Backward: where does the value come from? `read graph://used-by/<function>`, `read references://<path:line:col>`, `read <path>:<from>-<to>` around it. Walk back until you reach a lower-trust party (a request field, an uploaded file, an argument, a message, a model output, another app) or a check that stops the value (validation, allowlist, authorization, escaping, parameter binding).
2. Forward: what does the value do at the sink (which command, query, path, page, record), and to whom?
3. Judge (section 3), record when it is a finding (section 4), then the next line. A value that never comes from outside, or that a check you read stops, is settled.

When a unit's shapes found no lines, read its "look for" items against the entry points and handlers from the map. When every line is settled:

```bash
python3 "$SA" cover U1 covered "input bound at app/db.py:40; uploads renamed"
```

`candidate` when you recorded findings for the unit, `blocked "<why>"` when something could not be read or run. The result names the next unit.

## 3. What is a finding

A finding has all four parts: who (a lower-trust party), what (an input or action they control), where (the control that should stop it and does not, as path:line), so what (a concrete result for someone else or for the system). Missing one, it is not a finding.

- `confirmed`: you read the whole path and saw the result. Prefer a run: a small script in `tmp/` calling the function or the local server with a hostile value (`' OR '1'='1`, `../../etc/passwd`, `$(id)`, `<img src=x onerror=alert(1)>`, another user's id) and dummy data. Static is enough only when the code leaves no doubt (a shell string built from a request field and run as is). Never a deployed service, a real account, or anything that deletes, sends or pays.
- `needs_validation`: the path is real but one fact outside the code decides it (a proxy strips the header, the deployment sets the flag, the library version, a provider setting). Name that exact fact and how the owner checks it. No severity.
- `rejected`: a check you read stops it, or there is no victim. Record it so the next audit does not repeat the work.

A missing best practice with no reachable effect (a cookie flag, a header, an unpinned action in a job without secrets) is a hardening note: put it in the unit's `cover` text, not in a record.

Severity, for confirmed records only, comes from the demonstrated result and never from the class name:

| overall | means |
|---|---|
| critical | an anonymous party gets code execution, the whole data store, or any account |
| high | an explicit control fully defeated with real consequence: auth bypass, another tenant's data read or written, stored script for other users, authenticated code execution |
| medium | a real boundary crossed with a small blast radius or rare preconditions |
| low | non-secret internals disclosed, or much effort for little |
| informational | confirmed but minimal; a step in something bigger |

Overall never exceeds impact. If you cannot say the damage in one sentence, it is lower than it feels.

## 4. Record each decision

```bash
python3 "$SA" new confirmed
```

(or `needs_validation`, `rejected`). It writes `tmp/secaudit/finding-<n>.json`. Replace every value that starts with `? ` using `edit`, keeping the field names: the trace starts at the entrypoint and ends at the sink, each step a real path and line you read; the fingerprint is `<class>:<file>:<function>` in lowercase with no line number, and the same for the same root cause. The fix is the smallest change at the last trusted point, the sink's safe form (`skill://skills-engineering/security-pitfalls`), plus the test that would have caught it. Then:

```bash
python3 "$SA" finding tmp/secaudit/finding-1.json
```

It names each field to fix, or records the finding.

## 5. Report and reply

```bash
python3 "$SA" status
python3 "$SA" report
```

`status` says what is left: hunt it, or mark it blocked with why. `report` writes `tmp/secaudit/REPORT.md` from the records and the coverage and prints the summary. Reply with: each confirmed finding (severity, title, path:line, the result in one line, the fix), each lead with its missing fact, what was not covered (blocked units, scope), and the report's path. Zero confirmed findings is a valid result: say so and say what was covered; never add a low finding to fill the list. Fixing is separate work the user asks for.

## Traps

- A suspicious line is a question, not a finding: trace it before recording (seen: a cookie flag reported with no sensitive cookie behind it).
- Guessing a deployment fact either way (a proxy, a WAF, a provider setting): that is `needs_validation`.
- Stopping at a unit's first finding: the same root cause usually repeats; finish the list.
- A crash reported as code execution, or a party harming only itself reported as an attack.
- Writing the reply before the records: the report is derived from them, and the two must agree.
