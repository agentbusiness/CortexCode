# Engineering skills (`skills-engineering`)

Engineering know-how for CortexCode's model, independent of any language, as one `skills@1` plugin
(the `skills` family): how to research, generate design options, write a spec, plan for agents, carry out long work, design new software, find one's way in
an unfamiliar codebase, debug and pin bugs with tests, refactor, work with git and GitHub (including
reviewing a diff), and make code fast and secure. The model sees one line per skill (name and summary) in its system prompt and
reads a skill with `read skill://skills-engineering/<name>`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-skills-engineering`: category `skills-engineering`, contract `skills@1`, served by `cc_lang_kit::run_skills_plugin` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary of when to read it, a blank line, the Markdown body) |
| `verify/` | gate script, run against the real frame |

## What it serves

Methods are called on the category: `skills-engineering.describe` (id, name and every skill's name
and summary) and `skills-engineering.skill` (`{name}` -> `{name, text}`, the whole file). Skills are
written to [docs/SKILLS-BRIEF.md](../../docs/SKILLS-BRIEF.md): up to about 3,000 words, only what improves the
model's coding, CortexCode's tools and URIs by their real names.

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/skills-engineering   # -> plugins/skills-engineering/target/skills-engineering-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/skills-engineering --install`). The probe runs
`describe` (at least one skill), reads every skill back through `skill` (line 1 is the summary
`describe` gave, a body follows) and asks for an unknown skill (`not_found`). The unit tests also
hold the shipped skills to the brief's format (1-16 skills, a summary of at most 100 characters
naming the situation, blank line 2, 250-3200 words, no links in prose).

## Verification

`verify/skills_engineering.mjs` loads the folder in a real frame and checks both methods against the
files on disk read independently: the skills described are exactly the files, each with its first
line as summary, each read back byte for byte; the brief's format; refusals (an unknown skill, a
path-like name, a missing name).
