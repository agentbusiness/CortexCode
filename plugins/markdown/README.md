# Markdown (`markdown`)

Markdown documents, two ways: the right pane's Markdown view (`pane.markdown@1`: the workspace's
Markdown documents, rendered with react-markdown and GFM), and the code graph's facts of a
repository's prose (`lang-markdown`, a `lang@1` language pack, docs/PROJECT-KNOWLEDGE-PLAN.md P1).

One self-contained plugin folder, separable into its own repository (only `cc-protocol`,
`cc-lang-kit` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, backend (category `lang-markdown`, contract `lang@1`), UI packages and their slots, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-markdown`: `MarkdownPack` served by the kit |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/pane` (pane.markdown@1); each package builds `dist/` with `manifest.json` |

## The language pack (`lang-markdown`)

Repository documents are indexed by default, beside the code, so what a project says about itself
(README, `docs/`, ADRs, changelogs) is searchable by meaning and bindable to knowledge items.

- **Languages:** `markdown` (symbol-id prefix `md`; extensions `md`, `mdx`, `markdown`; file names
  `README`, `CHANGELOG`, `CONTRIBUTING`, `AGENTS`, `CLAUDE`).
- **extract** (`backend/src/sections.rs`, a line parser, no grammar): every heading (ATX `#`..`######`
  and setext `===` / `---` underlines; nothing inside a fenced code block) is a `section` symbol
  named by its heading, `qname` and `signature` the heading path (`Bowling > Scoring > Handicap`;
  a repeated path gets ` (2)`), `doc` its own text squashed to 600 characters (what it is embedded
  by), `code` its own text to 4,000 characters, `parent` the enclosing heading, the span from the
  heading to the next heading of its level or above (so it encloses its subsections), `exported`
  for level-one headings. Every backtick identifier in a section's prose (`compute_hdcp`,
  `Score.total`, `frame::ten()`; nothing with spaces, slashes or dashes, nothing starting with a
  digit, not inside fenced code) is a `Mention` reference from the section (from the file before
  the first heading); the code graph links it to the one symbol of the project with that name or
  qualified name (`MENTIONS`), and links nothing when the name is ambiguous.
- No language server, verifiers or skills: `describe` reports none, `diagnostics` answers
  `unavailable`.
- Probe: the kit's sample document must yield `Bowling`, `Bowling > Scoring` and
  `Bowling > Scoring > Handicap`.

## Build and run

```sh
cd backend && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/markdown                  # -> plugins/markdown/target/markdown-<version>-<target>.ccplugin
```

The memory plugin's `tests/docs.rs` indexes a project through this pack (served in-process by its
test host) and checks sections, mentions, search and knowledge links on the real graph.

`cortex-pack build plugins/markdown --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI package together; Settings > Plugins disables and enables the whole plugin.
