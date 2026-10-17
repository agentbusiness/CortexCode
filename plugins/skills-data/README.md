# Data and query skills (`skills-data`)

Data and query know-how for CortexCode's model as one `skills@1` plugin (the `skills` family):
SQL (correctness, dialects, SQL in application code, performance, migrations), Cypher for Neo4j,
data formats, dataframes, jq and regular expressions (querying the repository's own code graph is
the memory plugin's skill, `skill://memory/irongraph-queries`). The model
sees one line per applicable skill (name and summary) and reads a skill with
`read skill://skills-data/<name>`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-skills-data`: category `skills-data`, contract `skills@1`, served by `cc_lang_kit::run_skills_plugin` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary of when to read it, a blank line, the Markdown body) |
| `data/skills/applies.toml` | the projects each skill applies to (languages, files, imports); the chat flow offers a skill only where one holds |
| `verify/` | gate script, run against the real frame |

## What it serves

Methods are called on the category: `skills-data.describe` (id, name and every skill's name
and summary) and `skills-data.skill` (`{name}` -> `{name, text}`, the whole file). Skills are
written to [docs/SKILLS-BRIEF.md](../../docs/SKILLS-BRIEF.md): up to about 3,000 words, only what improves the
model's coding, CortexCode's tools and URIs by their real names.

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/skills-data   # -> plugins/skills-data/target/skills-data-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/skills-data --install`). The probe runs
`describe` (at least one skill), reads every skill back through `skill` (line 1 is the summary
`describe` gave, a body follows) and asks for an unknown skill (`not_found`). The unit tests also
hold the shipped skills to the brief's format (1-16 skills, a summary of at most 100 characters
naming the situation, blank line 2, 250-3200 words, no links in prose).

## Verification

`verify/skills_data.mjs` loads the folder in a real frame and checks both methods against the
files on disk read independently: the skills described are exactly the files, each with its first
line as summary, each read back byte for byte; the brief's format; refusals (an unknown skill, a
path-like name, a missing name).
