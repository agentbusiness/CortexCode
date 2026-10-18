A question the repo can't answer: a domain's rules, a library choice, an outside error, prior art.

## 0. When to go online

- **Before coding something you know only roughly** (a domain's rules, a file format, a protocol, an algorithm's edge cases):
  1. `read research://search/<topic> rules?sources=web,github&limit=5`
  2. `read research://fetch/<url>` for the one or two pages that state the rules.
  3. Copy each rule into your check cases, then code. Reading beats guessing edge rules.
- **Three failed fixes on one problem**: reduce it to the core: the smallest failing input, one sentence "happens X, should Y". Search that sentence (`sources=github_issues,web`), read two sources, return to the code.
- **Not available** (plugin off, offline): say so once; continue from the request and the code.

## 1. Use what you already have

- A dependency's API: read its installed source (`~/.cargo/registry/src`, the venv's `site-packages`, `node_modules`, `go env GOMODCACHE`). Online only for alternatives, known bugs, newer versions.
- Earlier findings first: `read research://notes/<topic>?q=<words>` (all topics: `research://notes/?q=<words>`).

## 2. Pick sources for the question

| Question | `sources=` | `sort=` / `since=` |
|---|---|---|
| Which library or tool | `github,web` | `popular`, then `recent` with `since` a year ago |
| An error from a dependency | `github_issues,web` | relevance (default) |
| How others call an API | `github_code` (needs `gh` signed in) | relevance |
| Method, algorithm, state of the art | `arxiv,semantic_scholar` | `recent` with `since`; `popular` for foundations |
| Experience, pitfalls, comparisons | `hackernews,reddit` | `popular` |

```
read research://search/"speculative decoding" draft?sources=arxiv,semantic_scholar&sort=recent&since=2025-01-01&limit=8
```

## 3. Write the query

- 2 to 4 content words (arXiv needs every word to match). Quote fixed phrases: `"tree-sitter" incremental`.
- An error: quote its constant part; drop paths, line numbers, addresses, your names.
- GitHub qualifiers: `language:rust stars:>200`, `repo:owner/name`, `is:closed` for issues. Web: `site:docs.rs`.
- `?` starts the options: keep it out of the words.
- Start broad, narrow with the best hits' words. Zero results: synonyms, fewer words.
- A source `FAILED` on the `sources:` line: skip it for now.

## 4. Read before you cite

- A snippet is not evidence: `read research://fetch/<url>`.
- A long document ends with the URI that continues it (`#from=<offset>`).
- A paper: abstract page first, then only the needed PDF part (the hit's `pdf:` line).
- Record each finding at once:

```bash
cx research note <topic> "<claim in your words>" --quote "<sentence copied from the fetched text>" --url <url> --title "<title>" --date 2026-03-02
```

One kebab-case topic per question, reused. Quote copied, never paraphrased. Date is the source's, not today's.

## 5. Weigh

- Strongest first: project docs, source, maintainers; a paper with method and data; an issue with a reproduction; a blog or forum.
- Software: last release date and recent maintainer answers outweigh stars.
- A deciding claim needs two independent sources or one primary. Conflict: prefer the newer primary source, report the conflict.

## 6. Stop

Stop when two sources agree, two searches add nothing, or after about 10 searches and 8 fetches.

Bigger, or three or more sub-questions: a background deep job writing `docs/research/<topic-slug>.md` (later jobs skip sources already cited).

```bash
cx research deep <topic> --questions "<q1>" --questions "<q2>" --sources arxiv,github,web --since 2025-01-01
cx research job <job-id>          # state and progress; keep working meanwhile
```

## Offline

A read answers `offline: ...`: use notes, cached fetches (`cached copy (offline)`) and installed sources; say it is unverified online. Never write a URL, title, date or quote you did not read.

## The answer

Each claim with its source (title, date, URL); single-source claims marked; what was not found said plainly.
