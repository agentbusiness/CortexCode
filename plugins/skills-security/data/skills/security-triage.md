A reported vulnerability, CVE, scanner alert or suspicious code: is it real here, how bad, what fix

Nothing changes in the project: the result is a verdict with evidence. Fixing is separate work the user asks for.

## 1. Pin the claim

Write the claim as one sentence with four parts: who (the lower-trust party), what they control, where the control fails (a function, a dependency, a configuration), so what (the result). A report that lacks a part gets that part from your reading, or the verdict says it is missing.

Find the code:

- A function or component: `read symbol://<name>`, `read graph://search/<words>`.
- A CVE or GHSA id: its record, live from the OSV database (or the NVD), with severity, affected versions, fixed versions and the weakness's mitigations:

```bash
export SA="$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py"
python3 "$SA" cve CVE-2021-23337
```

- A dependency and version: the lockfile decides what runs, not the manifest:

```bash
grep -n -i "<package>" package-lock.json yarn.lock pnpm-lock.yaml Cargo.lock poetry.lock requirements.txt go.mod Gemfile.lock composer.lock 2>/dev/null
```

- Every dependency at once, against OSV: `python3 "$SA" deps`.

- A scanner alert: its path:line, then the shapes around it: `python3 "$SA" scan <path>`.

## 2. Reachability

- Dependency: is the affected version installed (lockfile), is the affected function or feature used (`grep -rn "<function>"`, `read graph://used-by/<name>`), and does outside data reach it? A vulnerable function that is never called with outside data is not reachable: say so, still recommend the upgrade as hardening.
- Code: trace backward from the line to a lower-trust party or to a check that stops the value (`read graph://used-by/<function>`, `read references://<path:line:col>`), then forward to the effect.
- Configuration: find which environment uses it (the file that selects it, the deployment that mounts it); a fact you cannot read is a lead.

Confirm with a run when you can: a small script in `tmp/` calling the function or the local server with the hostile value from the report and dummy data; never a deployed service, a real account, or anything that deletes, sends or pays.

## 3. Verdict

- Real here: the whole path read and the result seen or beyond doubt. Severity from the demonstrated result: critical, an anonymous party gets code execution, the data store or any account; high, an explicit control fully defeated with real consequence; medium, a boundary crossed with a small blast radius; low, non-secret internals or much effort for little. A CVE's published score describes the library's worst case, not this project: rate what the trace shows.
- Not reachable here: name the check or the missing call that stops it, as path:line.
- Cannot tell: name the one missing fact (a proxy header, a deployment flag, a runtime version, a provider setting) and how the owner checks it.

## 4. Reply

1. The verdict in one line, first.
2. The evidence: the trace as path:line steps, the lockfile line for a dependency, what the run showed.
3. The severity and why, or why none.
4. The fix: the version to pin, or the smallest change at the sink (the safe form, `skill://skills-engineering/security-pitfalls`), and the test that would catch a regression.
5. What you could not check.

When the user wants the verdict kept with other findings, record it with `python3 "$SA" new <verdict>` and `python3 "$SA" finding <file>` as the audit skill does.

## Traps

- Confirming from the report's wording: the report may be wrong about this code; read it.
- Rating by the CVE score or the class name instead of the demonstrated result.
- Calling a manifest range the installed version: read the lockfile.
- A fix that filters characters at the entry instead of using the sink's safe form.
