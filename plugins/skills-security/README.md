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

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-skills-security`: category `skills-security`, contract `skills@1`, served by `cc_lang_kit::run_skills_plugin` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary of when to read it, a blank line, the Markdown body) |
| `data/scripts/secaudit.py` | the audit tool (Python 3.8+, standard library only), run by the model through `run` from the code folder: `$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py` |
| `data/scripts/classes-*.json` | the attack classes and the code shapes the tool searches for |

## The tool

```text
secaudit.py recon [--scope <path>...]      the stack, entry points, dependency files; the units (attack classes that apply here)
secaudit.py hunt <unit>                     the unit's invariant, where to look, what counts, the suspicious lines as path:line
secaudit.py cover <unit> <status> ["note"]  covered | candidate | blocked, with what settled it
secaudit.py new <verdict>                   a record template: confirmed | needs_validation | rejected
secaudit.py finding <file.json>             checks the record (fields by verdict, real paths and lines, severity bounds) and keeps it
secaudit.py status                          what is left
secaudit.py report [--partial]              tmp/secaudit/REPORT.md from the records and the coverage
secaudit.py scan <path>... | --changed [ref] the suspicious lines of named files or of a change, by class (reviews, triage)
secaudit.py deps                            the project's pinned dependencies against OSV, live
secaudit.py cve <id>                        one advisory (GHSA or CVE), live: severity, affected and fixed versions, mitigations
```

Everything it writes goes under `tmp/secaudit/` in the audited project (git ignores `tmp/`).
Which classes apply is decided from structural facts, never from the model's judgment: languages,
imports (from source files and manifests), file names, code shapes. Each class in the catalog
carries its invariant, where to look, what counts as a finding versus a hardening note, and the
regular expressions for its code shapes, per language; a hit is shown to the model as a question to
trace, never as a finding. A finding record needs who, what, where and so what; `confirmed` records
carry evidence, severity (overall never above impact) and the smallest fix; `needs_validation`
records carry the one missing fact and how the owner checks it; `rejected` records carry the
reason, so the next audit does not repeat the work.

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

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/skills-security   # -> plugins/skills-security/target/skills-security-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/skills-security --install`). The probe
runs `describe` (at least one skill), reads every skill back through `skill` (line 1 is the summary
`describe` gave, a body follows) and asks for an unknown skill (`not_found`).

Checking the tool by hand, from any project's root:

```sh
python3 plugins/skills-security/data/scripts/secaudit.py recon --out /tmp/secaudit-check
```
