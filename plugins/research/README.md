# Research (`research`)

Research that needs the world: search papers, code and discussions, read pages and PDFs as clean text, keep cited notes, and run deep research as a background job with the local model. Part 3 of `docs/IDEATION-WORKFLOW.md`. Contract: `research@1` (`protocol/src/contracts/research.rs`).

Rules it keeps: CortexCode works offline, so research is optional and online only when a network exists. Without one, `research.search` and `research.fetch` fail with `unavailable` and a message starting `offline` (fetch still serves a cached copy, marked `stale`); notes and the cache keep working. Notes are kept by the memory plugin: without it, the note methods (and a deep job, at its first note call) fail with `unavailable` and memory's own reason; there is no second store. Nothing is made up: a source that cannot answer says why. No cloud models, no paid APIs, no required keys. Keys are used only when the user already has them: `gh`'s sign-in, `GH_TOKEN`/`GITHUB_TOKEN`, `SEMANTIC_SCHOLAR_API_KEY`/`S2_API_KEY` in the environment.

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, provides / publishes / uses, config defaults, source location, verify commands |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-research`: category `research`, contract `research@1` |
| `backend/tests/fixtures/` | recorded answers of every source, an article, an arXiv abstract page, an arXiv PDF |
| `verify/` | `network.mjs`: the real frame against the real network |

## Methods

| Method | What it does |
|---|---|
| `research.search` | `{query, sources?, since?, limit?, sort?}` -> `{query, results: [{source, title, url, date, authors, snippet, score, extra}], sources: [{source, ok, count, via, error}]}`. Sources run concurrently; results are interleaved across sources (each keeps its own order) and deduplicated by URL; `since` (`YYYY-MM-DD`) goes into each source's query where it can and filters by date as well; `limit` is per source (default 5, at most 25); `sort` is `relevance` (default), `recent` or `popular`. |
| `research.fetch` | `{url, refresh?, max_chars?, offset?}` -> `{url, title, text, kind: html\|pdf\|other, published, bytes, truncated, final_url, authors, site_name, pages, total_chars, cached, stale, fetched_ms, content_sha256}`. |
| `research.note.add` | `{topic, claim, quote, url, title, date, source?, question?, by?, project_id?}` -> the note (`n-<16 hex>`, idempotent). Global unless `project_id` names a project. |
| `research.note.search` | `{text, topic?, limit?, project_id?}` -> `{notes, pending}`: notes by meaning, best first, with `score`; `pending` (left out when 0): notes IronGraph has not embedded yet. |
| `research.notes` | `{topic?, project_id?}` -> a topic's notes (all topics without it), oldest first. |
| `research.deep` | `{project_id, topic, questions?, depth?, sources?, since?, max_sources?, root?, fresh?, session_id?}` -> the `Job` at once; the job runs in the background. |
| `research.job.status` / `research.job.cancel` / `research.jobs` | a job; cancel one (its notes stay); the project's jobs, newest first. |
| `research.sources` | what each source would use now (gh signed in, keys, the browser), without network access. |

Events: `research.progress` (`{job_id, project_id, topic, phase, round, done, total, detail, session_id, ts_ms}`) and `research.job` (the `Job` whenever its state changes: running, done, failed, cancelled, interrupted).

## Sources

| Source | How |
|---|---|
| `arxiv` | The export API (Atom). The query's words are ANDed (`all:`), quoted phrases kept; `since` becomes `submittedDate:[<since> TO ...]`; `sortBy=submittedDate` for `recent`, else `relevance`. Requests are spaced 3 s apart (arXiv's API terms). Hits carry `pdf`, `arxiv_id`, `category`. |
| `semantic_scholar` | The Graph API's free tier: `/paper/search` (relevance), `/paper/search/bulk` sorted by `publicationDate:desc` (`recent`) or `citationCount:desc` (`popular`); `since` is `publicationDateOrYear=<since>:`. The unauthenticated pool is shared and often answers 429: the request is retried when the server asks for a short wait, then reported as `rate limited` (with the hint that a personal key raises it). |
| `github`, `github_issues`, `github_code` | `gh api search/repositories|issues|code` when `gh` is installed and signed in (`gh auth status`, checked every 5 minutes), else the public REST API (with `GH_TOKEN`/`GITHUB_TOKEN` when set). Both give the same JSON, parsed by one parser. `recent` sorts by update, `popular` by stars (repositories) or comments (issues); `since` becomes `pushed:>=` / `updated:>=`. Code search needs sign-in and says so; it asks for text matches (snippets). A rate limit names when it resets. |
| `reddit` | `/search.json` (posts only, `type=link`) with a descriptive User-Agent; Reddit often refuses unauthenticated JSON (403 "blocked by network security", seen from the recording network), so the same search is then read from `/search.rss` (Atom), and the source's status says so. `since` picks `t=day|week|month|year|all` and filters by date. |
| `hackernews` | Algolia's HN Search API: `/search` (relevance, `popular` re-sorted by points) or `/search_by_date`; `since` is `numericFilters=created_at_i>=`. Hits link the article, with the discussion in `extra.discussion`. |
| `web` | `web_search = "auto"`: DuckDuckGo's HTML endpoint (`html.duckduckgo.com/html/`, results for clients without JavaScript, links unwrapped from its redirector, ads skipped); when it answers with its bot check (HTTP 202/403, the "anomaly" page) or with a page that is not a result page, the same search is opened in the built-in browser as a hidden background tab (`browser.open {background: true}`, an additive `browser@1` field), the result links are read from its DOM with `browser.eval`, and the tab is closed. A CAPTCHA is never solved: blocked is reported as blocked. `http` or `browser` use only one path. Why this order: the HTML endpoint needs no window and is fast, stable markup; the browser is more robust against bot checks but exists only while the CortexCode window runs. |

## Fetch

HTTP through one client (rustls, HTTP/2, the macOS system proxy, gzip/brotli), a descriptive User-Agent, a download limit (`max_download_bytes`; a larger PDF is refused with that name), up to 8 redirects. URLs a user or the model gives are refused when they point at loopback, private, link-local or `.local` addresses (also when a public name resolves to one; a redirect to such an address literal is refused too) unless `allow_private_hosts`. Extraction runs on a blocking thread:

- HTML: `dom_smoothie` (a maintained Rust port of Mozilla's Readability.js) in its formatted text mode; dates from the article metadata or the usual meta tags (Open Graph, Highwire `citation_*` as on arXiv and journals, Dublin Core, schema.org, `<time datetime>`), authors from `citation_author` or the byline; a page that is only a `<meta http-equiv="refresh">` is followed (two hops); a page Readability cannot score falls back to its body blocks without scripts, styles and page chrome.
- PDF: `pdf-extract` (on `lopdf`), page by page, under `catch_unwind`. Text comes in content-stream order, which for LaTeX output (arXiv) is reading order, one column after the other (the two-column fixture checks the order); a PDF whose stream order differs from its visual order can come out interleaved. Cleaned: ligatures expanded, hyphenated line ends joined, paragraph lines joined, bare page numbers dropped. Title from the Title entry when meaningful, else the first substantial line; date from CreationDate. Scanned PDFs have no text (no OCR) and say so. Chosen over `pdf_oxide` (pre-1.0; its layout pass scrambled the fixture's author line) and MuPDF bindings (AGPL, a C build).
- Anything textual (plain text, Markdown, JSON, XML, CSV) as it is; other binary content is refused.

Cache under `$CORTEX_HOME/research/cache`: `urls/<sha256 of the URL>.json` (the latest fetch: time, content hash, extracted text and metadata, extractor version) and `blobs/<sha256 of the content>` (the bytes, shared by URLs with the same content, so a newer extractor re-extracts without the network). Fresh for `cache_ttl_hours`; afterwards fetched again, and the old copy is returned marked `stale` when the network cannot be reached.

## Notes

Kept by the memory plugin in IronGraph's KNOWLEDGE layer through `memory.note.add`, `memory.note.search` and `memory.note.list` (`plugins/memory-irongraph`, README "Notes"): a `Note` node that `CITES` a `Source {url}` node. Research notes are global (the main IronGraph project): a finding about the world is reused by every project, and a deep job on a topic counts notes written for it from any project. A `project_id` keeps a note to that project instead (`cx research note --scoped`). Search is by meaning, with IronGraph's own embedding (Bekko); topics compare case- and punctuation-insensitively. This plugin checks the required fields, normalizes the date and whitespace, and passes memory's errors on with their code (`unknown_method` / `connection_closed` become `unavailable`), prefixed `notes are kept by the memory plugin (memory@1)`.

Notes an earlier version kept as JSON lines (`$CORTEX_PLUGIN_DATA/notes/<project id>.jsonl`) are imported once, as global notes, before the first note call (a torn last line is skipped); each file is renamed `<name>.jsonl.imported` once all its notes are in memory and is never read again. A file memory could not take whole stays, and the next note call imports it again (memory's ids make that idempotent).

## Deep research

A background job, sized so a ~32k-token context is enough for dozens of sources: the model never sees more than one chunk of one source, or a bounded batch of notes.

1. **plan**: one `model.chat` call with a JSON schema: sub-questions (2 to 5, or the caller's), each with 1 to 3 keyword queries and the sources to ask (the caller's `sources` override).
2. **search**: every query; candidates ranked by how many queries found them, then spread across sources; sources the topic's notes already cite, or this job already visited, are skipped; `max_sources` (default `deep_sources_per_round`) are read. Papers are read from their PDF (the landing page when the PDF cannot be read).
3. **read** (map): each source fetched and cut at paragraph boundaries into chunks of `chunk_chars` (at most `max_chunks_per_source`); one model call per chunk returns notes `{question, claim, quote}`. A quote not found in the chunk (after normalizing typography, ligatures and line-break hyphens) is dropped and counted. Notes are saved (`memory.note.add`, global) with the source's URL, title and date.
4. **synthesize** (reduce): from the topic's notes only (`memory.note.list`), labelled `[n]`; findings cite labels, mapped back to note ids; a finding citing nothing real is dropped. More notes than `synth_chars` are condensed in batches first (up to three levels). Confidence `high` needs two distinct sources, else it becomes `medium`.
5. **gap check** (rounds before the last): a call naming at most three unanswered questions with new queries; they join the questions and start the next round, up to `depth` (default 2, at most 4).
6. **report**: written by code from the synthesis and the notes to `<project root>/docs/research/<topic-slug>.md`; then every finding of high or medium confidence becomes a `research`-sourced `fact` in the project's knowledge (`memory.knowledge.upsert`, confidence 0.7 or 0.5, `SUPPORTED_BY` the notes it cites, the report as evidence) and every open question a `question` (docs/PROJECT-KNOWLEDGE-PLAN.md P5); memory refusing costs the proposal, never the job. The report is (root from `frame.project.get`, or `root`): questions, summary, findings with `[n]` citations and confidence, open questions, numbered sources with dates, how they were found and their notes' verbatim quotes, sources that could not be read and why, and the method (sources, queries, discarded quotes).

Progress after every step; the state is saved after every step to `$CORTEX_PLUGIN_DATA/jobs/<id>.json`. `research.job.cancel` stops it (its notes stay). `research.deep` on a topic whose last job is unfinished (interrupted, cancelled, failed) resumes it from the saved phase (`fresh: true` starts over); a finished topic starts a new job that reuses the notes and does not read their sources again. A hot swap hands running jobs to the successor (`export_state` stops them at their last step, `import_state` resumes them); after a cold start they are `interrupted`. Model calls use `priority = model_priority` and `cache_key = research-<job id>`; a cancelled job cancels its model call at the model plugin.

## The model's view and `cx`

The tools plugin's `read` takes `research://search/<query>?sources=arxiv,github&since=2025-01-01&limit=5&sort=recent`, `research://fetch/<url>` (the rest of the URI is the URL; `#from=<offset>` continues a long document, 12000 characters per read) and `research://notes/<topic>[?q=<words>]` (the global notes, searched by meaning with `q`; each hit shows its score) (`plugins/tools/backend/src/research.rs`).

`cx research search|fetch|note|notes|find|deep|job|jobs|cancel|sources` (`cli/src/registry.rs`); `--sources` takes repeated flags or a comma list; `note`, `notes` and `find` are global, `--scoped` keeps them to the current project.

## Configuration (`[backend.config]`)

`timeout_ms`, `max_download_bytes`, `max_text_chars`, `cache_ttl_hours`, `allow_private_hosts`, `web_search` (auto | http | browser), `github` (auto | gh | rest), `gh`, `arxiv_interval_ms`, `max_retry_wait_ms`, `deep_depth`, `deep_sources_per_round`, `deep_results_per_query`, `chunk_chars`, `max_chunks_per_source`, `synth_chars`, `model_max_tokens`, `model_priority`, `user_agent`, and the endpoints `arxiv_api`, `semantic_scholar_api`, `github_api`, `reddit`, `hackernews_api`, `duckduckgo_html` (mirrors, tests).

## Tests and verification

```sh
cd backend && cargo test --release && cargo clippy --release --all-targets -- -D warnings
node verify/network.mjs                    # the real network (with the memory plugin); CC_VERIFY_DEEP=1 adds a deep job with the local model
```

- Unit tests: each source's request building and parsing on its recorded answer, error and block pages, dates, quotes, readability on the Rust blog article and arXiv's abstract page, the two-column arXiv PDF (order, de-hyphenation, ligatures), chunking, source picking, synthesis grounding, the report.
- `tests/notes.rs`: what goes to `memory.note.*` (global unless a project is named, normalized fields), memory unavailable as a plain error with no local file, and the JSON-lines import: skipped while memory is away, then once (a torn line skipped, the same note of two projects one global note, renamed files never read again, also after a restart). The memory side (ranking by meaning) is tested in the memory plugin; here a stand-in (`tests/common`, `FakeMemory`) answers `memory.note.*`.
- `tests/search.rs`: every source over HTTP against a local server (parameters, User-Agent, Reddit's RSS fallback, 429 retries, keys only when configured, GitHub through a fake `gh`, the browser fallback with a fake browser, offline).
- `tests/fetch.rs`: HTML, PDF, cache hits without the network, refresh, a stale copy offline, paging, limits, private addresses, meta refresh.
- `tests/deep.rs`: the job with fake sources, a fake model and the memory stand-in: two rounds to a report, invented quotes dropped, notes written global, events, a cancel and a resume after a restart, offline failing plainly, memory unavailable failing plainly before any model call.
- `tests/plugin.rs`: the binary over CPP/1 with an in-test frame that also answers `model.chat`, `frame.project.get` and `memory.note.*`: probe, search, fetch, notes (and memory away), a deep job to its report.

Fixtures (recorded 2026-10-15): `arxiv_search.xml`, `hn_search.json`, `github_repos.json`, `github_issues.json`, `github_code.json` (through `gh api`), `reddit_search.rss`, `ddg_search.html`, `article_rust_blog.html`, `meta_refresh.html` (blog.rust-lang.org's redirect page), `arxiv_abs.html` and `arxiv_2211.17192.pdf` (Leviathan, Kalman, Matias, "Fast Inference from Transformers via Speculative Decoding", CC BY 4.0, two-column, 575 KB). `s2_rate_limited.json` is Semantic Scholar's real 429 answer. Two are written by hand in the documented shape, because the services refused this network while recording: `s2_search.json` (the shared pool answered 429 for 40 minutes) and the Reddit Listing JSON inside `reddit.rs`'s test (403 to every JSON path); `verify/network.mjs` exercises both live.
