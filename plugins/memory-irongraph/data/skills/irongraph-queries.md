Whole-repo questions graph:// URIs can't answer (unused code, all routes, import cycles): Cypher.

## 1. URIs first

One symbol or file: `read` a URI. Questions about the whole repository: a Cypher query (step 2).

| URI | Gives |
|---|---|
| `graph://search/<words>` (`?kinds=function,method`, `?limit=10`) | code by name, then by meaning (at most 10) |
| `symbol://<name>` | a symbol's signature, description, caller and callee counts, code |
| `graph://used-by/<name>` (every place that uses it), `graph://uses/<name>` (everything it uses) (`?depth=2`) | each call site with its line |
| `graph://impact/<name>` or `graph://impact?paths=src/a.rs,src/b.rs` (`?depth=3`) | call sites, tests, tasks and rules a change affects |
| `graph://path/<name>/<name>` (`?max_hops=8`) | how one symbol reaches another |
| `outline://<path>` | a file's definitions |

## 2. Run a query

```bash
cx call memory.query - <<EOF
{"project_id":"$CORTEX_PROJECT_ID","workspace":"$CORTEX_WORKSPACE",
 "cypher":"USE LAYER OBSERVED MATCH (r:Route) RETURN r.http_method, r.route_path, r.path, r.start_line"}
EOF
```

- Output: `{"columns": [...], "rows": [[...]]}`. Read-only: a write is refused.
- `workspace` adds your own edited files to the graph.
- Strings in single quotes. Parameters: `\$name` in this heredoc plus `"params":{"name":"..."}`.
- Return properties, not whole nodes (`code` holds whole bodies). Add `LIMIT`.
- Zero rows: check the spelling against the schema below. Paths are repository-relative.

## 3. Layers

Every query starts with the layers it reads; without `USE LAYER` it reads all three.

| Layer | Holds |
|---|---|
| `OBSERVED` | the parsed code and documents (section 4) |
| `KNOWLEDGE` | symbol descriptions, project knowledge, cited notes (section 5) |
| `WORKSPACE` | tasks, rules, board messages, agents, runs (section 5) |

Across layers: `USE LAYER OBSERVED, WORKSPACE MATCH ...`.

## 4. Code schema (OBSERVED)

Labels:
- Every node: `Code`.
- Definitions: `Sym` plus one of `Function Method Class Struct Trait Interface Enum TypeAlias Const Static Field Variant Variable Component Prop Module Macro Test Route Command ShellFunction IpcChannel`.
- Document headings: `Section` (not `Sym`); `signature` is the heading path, `doc` its text.
- Other nodes: `File Directory Repo Param Package Dependency EnvVar Commit`.

Properties:
- All: `sid kind name qname`. Code and files also: `path start_line end_line`.
- `Sym`: `language signature doc code return_type type_text value is_async is_static exported visibility decorators param_count body_hash`. `Route` adds `http_method route_path framework`.
- `File`: `language line_count size package parse_errors note` (`note`: why a file was read as text).
- `Param`: `position type_text default_value optional variadic`.
- `Dependency`: `ecosystem installed_version`.
- `Commit`: `qname` = sha, `name` = subject, `author time_ms`.
- sids: `<prefix>:<path>#<qname>`, prefix `rs py js go java cpp c sh html css` (`js` also for TypeScript), e.g. `py:server/store.py#load_user`. Others: `file:<path>`, `dir:<path>`, `dep:npm:<name>`, `env:<NAME>`, `commit:<sha>`.

Relationships:
- Calls (code -> code): `CALLS`, `INSTANTIATES`, `RENDERS`, `USES_HOOK`, `HANDLES` (Route -> handler), `REQUESTS` (HTTP client call -> Route), `INVOKES` (-> IpcChannel), `RUNS` (Command -> file or command). Properties `line args confidence` (`heuristic`, or `ambiguous`: an untyped receiver linked to the only method of that name).
- Data: `READS`, `WRITES`, `THROWS`, `READS_CONFIG` (-> EnvVar).
- Types: `RETURNS`, `OF_TYPE`, `HAS_PARAM`, `EXTENDS`, `IMPLEMENTS`, `OVERRIDES`.
- Structure: `DEFINES` (File -> Sym), `CONTAINS` (Directory -> File, parent -> member), `EXPORTS`, `IMPORTS {line, module}` (File -> File or Dependency), `DEPENDS_ON` (manifest -> Dependency), `TESTS` (Test -> code), `MODIFIED_IN` (File -> Commit).
- Documents: `MENTIONS {line, confidence}` (Section -> the symbol a backtick name in its text names).

## 5. Knowledge and work schema

KNOWLEDGE:
- `Description {sid, text, body_hash}` `-[:DESCRIBES]->` a symbol. Stale when its `body_hash` differs from the symbol's.
- `Knowledge` plus one of `Concept Fact Decision Hypothesis Question`: `id name kind text state source confidence`. `state`: `asserted accepted open supported refuted answered superseded withdrawn`. Edges `IMPLEMENTED_BY`, `ABOUT` (-> Code), `DESCRIBED_IN` (-> Section), `SUPPORTED_BY` (-> Note), `TESTED_BY` (-> Run), `ANSWERS`, `SUPERSEDES` (-> Knowledge).
- `Note {claim, quote, url, topic}` `-[:CITES]->` `Source {url, title}`.

WORKSPACE:
- `Task {id, title, brief, status, assignee}`: `status` `pending in_progress verifying done failed cancelled`. Edges `TOUCHES` (-> Code), `HAS_SUBTASK`, `DEPENDS_ON`, `HAS_CHECK` (-> ContractCheck).
- `Rule {id, text, scope, source}` `-[:CONSTRAINS]->` Code (`source: 'cortex_md'` for CORTEX.md items).
- `BoardMessage {id, kind, author, text}` `-[:REFERS_TO]->` Code or Task. `Agent {name}` `-[:WORKS_ON]->` Task.
- `Run {id, outcome, summary}` `-[:FOR_TASK]->` Task; changed code `-[:MODIFIED_IN]->` Run.

## 6. Query shapes

```cypher
// functions nothing refers to
USE LAYER OBSERVED MATCH (f:Function) WHERE size([(f)<-[c]-() WHERE type(c) <> 'DEFINES' AND type(c) <> 'CONTAINS' AND type(c) <> 'EXPORTS' | 1]) = 0 RETURN f.sid, f.exported LIMIT 200
// routes and their handlers
USE LAYER OBSERVED MATCH (r:Route)-[:HANDLES]->(h) RETURN r.http_method, r.route_path, h.sid, h.path, h.start_line
// everything reaching save_order within four calls
USE LAYER OBSERVED MATCH (a:Code)-[:CALLS*1..4]->(b:Function {name: 'save_order'}) RETURN DISTINCT a.sid LIMIT 200
// call counts per function (sort them yourself)
USE LAYER OBSERVED MATCH (f:Function)<-[c:CALLS]-() RETURN f.sid, count(c) AS callers LIMIT 500
// exported functions no test covers
USE LAYER OBSERVED MATCH (f:Function) WHERE f.exported AND size([(t:Test)-[:TESTS]->(f) | 1]) = 0 RETURN f.sid LIMIT 200
// tests of a file's code
USE LAYER OBSERVED MATCH (t:Test)-[:TESTS]->(n:Code {path: 'server/store.py'}) RETURN DISTINCT t.sid
// importers of a file, two-file import cycles, users of a dependency
USE LAYER OBSERVED MATCH (a:File)-[:IMPORTS]->(b:File {path: 'src/db.ts'}) RETURN a.path
USE LAYER OBSERVED MATCH (a:File)-[:IMPORTS]->(b:File)-[:IMPORTS]->(a) RETURN a.path, b.path
USE LAYER OBSERVED MATCH (f:File)-[:IMPORTS]->(d:Dependency {name: 'requests'}) RETURN DISTINCT f.path
// who reads which environment variable; client calls matched to server routes
USE LAYER OBSERVED MATCH (c:Code)-[:READS_CONFIG]->(e:EnvVar) RETURN e.name, c.sid
USE LAYER OBSERVED MATCH (c:Code)-[:REQUESTS]->(r:Route) RETURN c.sid, r.http_method, r.route_path
// functions with many parameters; a file's commits
USE LAYER OBSERVED MATCH (n:Sym) WHERE (n:Function OR n:Method) AND n.param_count > 5 RETURN n.sid, n.param_count
USE LAYER OBSERVED MATCH (f:File {path: 'src/db.ts'})-[:MODIFIED_IN]->(c:Commit) RETURN c.qname, c.name, c.time_ms
// documentation sections that mention a symbol
USE LAYER OBSERVED MATCH (s:Section)-[:MENTIONS]->(c:Code {name: 'save_order'}) RETURN s.path, s.signature, s.start_line
// descriptions gone stale after a code change
USE LAYER OBSERVED, KNOWLEDGE MATCH (d:Description)-[:DESCRIBES]->(s:Sym) WHERE d.body_hash <> s.body_hash RETURN s.sid LIMIT 200
// decisions and facts about a file's code
USE LAYER OBSERVED, KNOWLEDGE MATCH (k:Knowledge)-[r]->(c:Code {path: 'server/store.py'}) WHERE k.state IN ['asserted', 'accepted'] RETURN k.kind, k.name, k.text, type(r), c.sid
// open tasks and rules on a file's code
USE LAYER OBSERVED, WORKSPACE MATCH (t:Task)-[:TOUCHES]->(c:Code {path: 'server/store.py'}) WHERE t.status <> 'done' RETURN DISTINCT t.id, t.title, t.status
USE LAYER OBSERVED, WORKSPACE MATCH (r:Rule)-[:CONSTRAINS]->(c:Code {path: 'server/store.py'}) RETURN DISTINCT r.text
```

Known to work: label tests in `WHERE` (`n:Sym OR n:File`), `IN`, `<>`, `AND`, `UNWIND`, `WITH`, `RETURN DISTINCT`, `LIMIT`, `count()`, `size([pattern | 1])`, `type(r)`, `labels(n)`, `coalesce()`, `-[:R*1..5]->`, `CALL graph.pagerank() YIELD node, score` (centrality), `CALL graph.shortestpath(a, b) YIELD path, cost`.
Another construct errors: rewrite it with these and sort the output yourself.

## 7. Reading the results

Unused-code results:
- Calls through callbacks, dynamic dispatch or reflection have no edge.
- Exported, `main` and test functions are entry points.
- Confirm each candidate with `grep -rnw <name> .` before deleting it.

An edge with `confidence: 'ambiguous'` guessed the receiver's type: read the call site before relying on it.
