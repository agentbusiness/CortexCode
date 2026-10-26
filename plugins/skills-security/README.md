# Security skills (`skills-security`)

Security know-how for CortexCode's model as one `skills@1` plugin (the `skills` family), in the
shape of the Cloudflare security-audit skill but cut for a small local model working alone: three
procedural skills and one bundled tool, `secaudit`, that does the mechanical half of an audit so
the model's steps stay small (map the project, keep the checklist, find the suspicious lines, check
each record, write the report). The model sees one line per skill in its system prompt and reads a
skill with `read skill://skills-security/<name>`.

| Skill | When |
|---|---|
| `security-audit` | the user asks for a security audit or pen-test of the codebase: the whole flow, recon to report |
| `security-review-change` | a diff, pull request, file or function to review for security |
| `security-triage` | a reported vulnerability, CVE, scanner alert or suspicious code to judge |

One self-contained plugin folder, separable into its own repository (only `cc-protocol`,
`cc-lang-kit` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, `data/` as a backend asset, the UI package, source location |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-skills-security`: category `skills-security`, contract `skills@1`, served by `cc_lang_kit::run_skills_plugin` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary of when to read it, a blank line, the Markdown body) |
| `data/scripts/secaudit.py` | the audit tool (Python 3.8+, standard library only), run by the model through `run` from the code folder: `$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py` |
| `data/scripts/classes-*.json` | the attack classes and the code shapes the tool searches for |
| `ui/` | npm workspace with one package, `ui/security` (`skills-security.pane` on `pane.security@1`): the Security tab of the right pane |

## The tool

The script drives the audit; the model judges what it shows.

```text
secaudit.py next                             the driving command: maps the project on the first call (stack, entry points, the
                                             dependencies' known vulnerabilities, the units); then the next open lines to judge, with
                                             ids (U3.4), the class's invariant and the live upstream guidance; then, when nothing is
                                             open, writes the report
secaudit.py settle <id>... <verdict> ["note"] safe | hardening | blocked, each with the fact that settles it (a judgment of several
                                             lines names the place it rests on, path:line in the project, or the package); finding |
                                             lead write a prefilled record file, and next refuses to move on until it is recorded.
                                             When a unit's last line is settled it prints the unit's one-line result for the user;
                                             a unit whose step fails stays open, is retried after the others, and a second failure
                                             prints the settle that records it
secaudit.py finding <file.json>              checks the record (fields by verdict, real paths and lines, severity bounds) and keeps it
secaudit.py scan <path>... | --changed [ref]  the suspicious lines of named files or of a change, by class (reviews, triage)
secaudit.py deps                             the project's pinned dependencies against OSV, live
secaudit.py cve <id>                         one advisory (GHSA or CVE), live: severity, affected and fixed versions, mitigations
secaudit.py recon [--scope <path>...]        the map alone (a scoped audit runs it before next)
secaudit.py status | report | hunt | cover | new   the pieces next drives, callable on their own
secaudit.py show <id>                        one finding from the project's graph: who, what, trace, fix, test, what the code says now
secaudit.py fix-plan <id>                    the exact upgrade commands of a dependency finding (its quick fix)
secaudit.py recheck <id>                     what the code says now; a sink that is gone or a package that moved on marks it fixed
secaudit.py resolve <id> "<how>"             record a fix: refused while the sink is unchanged; else fixed in the graph with the session and commit
secaudit.py status --fixing                  the fix skill's done guard: 0 once this session's finding is recorded fixed
```

## The Security tab and the findings in the graph

Findings do not stay in files: `finding` and the report push every confirmed finding, lead and
hardening note into the project's graph through the memory plugin's `memory.security.*` methods
(the script calls the app with `cx`; without the app it notes that and the next push carries
everything). Each finding carries a computed priority (severity band, verdict, who the attacker
is, a quick fix at hand, age) and its reason in words. The plugin's UI package `ui/security`
(slot `pane.security@1`) is the Security tab in the right pane: the last audit, the active findings
by priority, each one's trace linked into the code view, and the actions: fix in a session, quick
fix, accept risk, not an issue, reopen, forget. Fixing starts a session whose opening message
names the finding, the sink and the fix; the `security-fix` skill drives it and its `done` guard
(`status --fixing`) keeps the turn open until `resolve` has seen the change in the code and marked
the finding fixed. Lifecycle: open, fixing, fixed, verified (the next audit's re-check), accepted,
gone; fixed and verified leave the active list and stay as history.

Everything it writes goes under `tmp/secaudit/` in the audited project (git ignores `tmp/`).
Which classes apply is decided from structural facts, never from the model's judgment: languages,
imports (from source files and manifests), file names, code shapes. Each class in the catalog
carries its invariant, where to look, what counts as a finding versus a hardening note, and the
regular expressions for its code shapes, per language; a hit is shown to the model as a question to
trace, never as a finding. A finding record needs who, what, where and so what; `confirmed` records
carry evidence, severity (overall never above impact) and the smallest fix; `needs_validation`
records carry the one missing fact and how the owner checks it; `rejected` records carry the
reason, so the next audit does not repeat the work.

## What a run leaves behind

The audit's state and report live under the app's home, one folder per project
(`$CORTEX_HOME/audits/<project>-<id>/`): `REPORT.md`, one `units/<unit>.md` per attack class with
every reviewed line, its verdict and the reviewer's reasoning, the records (`findings.json`), the
judgments (`hits-state.json`) and the dependency check (`deps.json`). Nothing is written into the
project except record drafts in its git-ignored `tmp/secaudit/`, which the host removes when the
turn ends. A new audit of a project that already has a finished one archives it under `runs/<stamp>/`
and starts with unit U0: every previous finding, lead and hardening note, each with a mechanical
pre-check (the flagged line unchanged, moved or gone; the package still pinned at the same version),
for the model to settle as `fixed` or `still`; a still-open finding is carried into the new report
as it was, and the report's "Previous audit" section shows the comparison.

## Live sources

Nothing is mirrored or cached. At call time the tool asks:

| Source | What for | Where |
|---|---|---|
| OSV, the open vulnerability database (carries the GitHub Advisory Database, the npm, PyPI, crates.io, Go, RubyGems, Packagist, Maven and NuGet advisories and the CVE list) | every known vulnerability of the exact dependency versions the project's lockfiles pin, with the fixed versions | `recon` (summary and `tmp/secaudit/deps.json`), the dependencies unit of `hunt`, `deps`, the report |
| NVD | a CVE's description, CVSS and weakness ids when OSV has no record | `cve` |
| MITRE's CWE API | the mitigations of the weakness behind an advisory | `cve` |
| the Cloudflare security-audit skill repository | the current hunting guidance for the attack class being hunted | `hunt` |

Lockfiles read: package-lock.json, yarn.lock, pnpm-lock.yaml, requirements*.txt, poetry.lock,
Pipfile.lock, uv.lock, pdm.lock, Cargo.lock, go.mod, Gemfile.lock, composer.lock,
packages.lock.json and pom.xml (literal versions). Every call is HTTPS with an 8-second timeout; a
project marked offline (`.cortex-offline`) or an unreachable source gets one line saying the live
check did not run, and the audit continues from the code. The `classes-*.json` beside the tool are
its own search heuristics (the code shapes to look at, written from the upstream guidance); they
ship with the plugin and change with it.

## The Security tab's controls

The package `ui/security` (`skills-security.pane`) reads `memory.security.summary`,
`memory.security.findings` and `memory.security.finding`, follows `memory.security.changed`, and
writes through `memory.security.finding.update` as `by: "user"`:

- The head bar: the last audit (when, units, lines judged) and "Audit now", which creates a chat
  session "Security audit" and sends `security audit of this codebase` as its first message with
  the person's default mode, as the composer does.
- The severity chips (critical, high, medium, low, lead, note) carry the counts of open and fixing
  findings and filter the list; "Show fixed" adds fixed, verified and gone findings.
- A row: severity, title, the sink's `path:line`, class, status, "quick fix" when the host spelled
  the fix out; accepted and closed findings are greyed. The row opens the finding under itself.
- "Fix in a session" and "Quick fix" (open findings) create a session `Fix: <title>` whose first
  message names the finding, the principal, result and control at the sink, the fix (or the quick
  fix's commands and the version the lockfile must pin) and the `resolve` command to record it;
  then the finding is `fixing` with that session, and the session is selected.
- "Accept risk" (open) and "Not an issue" (open, accepted) take a note inline, required, and mark
  it `accepted` or `gone`; "Reopen" (fixing, accepted, fixed, verified, gone) marks it `open`;
  "Forget" (`memory.security.forget`, fixed, verified and gone) deletes it after a second click.
  A refused move shows the store's answer under the actions.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
(cd ui && npm install && npm run build)     # -> ui/security/dist (index.js, style.css, manifest.json)
cortex-pack build plugins/skills-security   # -> plugins/skills-security/target/skills-security-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/skills-security --install`). The probe
runs `describe` (at least one skill), reads every skill back through `skill` (line 1 is the summary
`describe` gave, a body follows) and asks for an unknown skill (`not_found`).

Checking the tool by hand, from any project's root:

```sh
python3 plugins/skills-security/data/scripts/secaudit.py recon --out /tmp/secaudit-check
```
