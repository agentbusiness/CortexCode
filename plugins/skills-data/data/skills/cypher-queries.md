Neo4j/openCypher queries or driver code: duplicates after MERGE, wrong counts, slow or old syntax.

This repository's own code graph: `skill://memory/irongraph-queries`.

## 1. Run queries, read the schema

```bash
cypher-shell -a "${NEO4J_URI:-bolt://localhost:7687}" -u "${NEO4J_USERNAME:-neo4j}" --format plain "RETURN 1"   # password from NEO4J_PASSWORD
cypher-shell --format plain -f import.cypher        # statements end with ;
```

Labels, relationship types and property keys are case-sensitive. Read them first:

```cypher
CALL db.labels(); CALL db.relationshipTypes(); CALL db.propertyKeys();
SHOW CONSTRAINTS; SHOW INDEXES;
MATCH (a)-[r]->(b) RETURN DISTINCT labels(a), type(r), labels(b) LIMIT 50;
```

## 2. MERGE creates duplicates

`MERGE` matches or creates the WHOLE pattern.
`MERGE (a:User {name: row.src})-[:FOLLOWS]->(b:User {name: row.dst})` creates new `a` and `b` when the relationship is missing.
Merge each node on its key alone, then the relationship:

```cypher
CREATE CONSTRAINT user_name IF NOT EXISTS FOR (u:User) REQUIRE u.name IS UNIQUE;

LOAD CSV WITH HEADERS FROM 'file:///follows.csv' AS row
WITH trim(row.src) AS src, trim(row.dst) AS dst
WHERE src <> '' AND dst <> ''
MERGE (a:User {name: src})
MERGE (b:User {name: dst})
MERGE (a)-[:FOLLOWS]->(b);
```

- Other properties: `MERGE (u:User {id: $id}) ON CREATE SET u.created = timestamp() SET u.name = $name`.
- Without a unique constraint, two concurrent `MERGE`s can both create the node.
- The constraint fails while duplicates exist. Find them: `MATCH (u:User) WITH u.name AS k, collect(u) AS ns WHERE size(ns) > 1 RETURN k, size(ns)`. Delete and re-import, or `apoc.refactor.mergeNodes(ns)` with APOC.
- CSV values are strings: `trim()`, `toInteger()`, `toFloat()`, `date()`.
- `MERGE` on a null property fails: filter nulls first.
- `LOAD CSV` reads only from the server's import directory by default.

## 3. Wrong counts: each clause runs once per row

- `MATCH (a:A), (b:B)` with no link between them is a cartesian product.
- Chained `MATCH`/`OPTIONAL MATCH` multiply rows. Aggregate before the next one (`WITH u, count(f) AS followers`), or use `COUNT {}` (Neo4j 5):
  ```cypher
  MATCH (u:User)
  RETURN u.name, COUNT { (u)<-[:FOLLOWS]-() } AS followers, COUNT { (u)-[:WROTE]->(:Post) } AS posts;
  ```
- After `OPTIONAL MATCH`, `count(*)` counts the null row: use `count(x)`. Paths repeat `x`: `count(DISTINCT x)`.
- Aggregation groups by every non-aggregated item in the same `RETURN`/`WITH`.
- `WITH` drops every variable it does not list.
- `WHERE` right after `OPTIONAL MATCH` keeps the row with nulls. `WHERE` after `WITH` drops it.
- Undirected `-[:R]-` matches each relationship twice: give the direction.
- Top N per group: `WITH g, x ORDER BY x.score DESC WITH g, collect(x)[..3] AS top`.

## 4. NULL and values

- `n.x = null` is never true: `n.x IS NULL`.
- `WHERE n.x <> 'a'` also drops nodes without `x`.
- Setting a property to null removes it.
- `5 / 2 = 2`: `toFloat(a) / b`.
- `=~` must match the whole string: `n.name =~ '(?i)bob.*'`. `CONTAINS`, `STARTS WITH`, `ENDS WITH` are faster and use indexes.
- Dates as `date()`/`datetime()` values, not strings: `datetime('2024-05-01T10:00:00Z')`, `date.truncate('month', d)`, `duration.between(a, b).days`.
- `UNWIND []` yields no rows and drops that row. Keep it: `WITH n, coalesce(n.tags, []) AS l UNWIND CASE WHEN size(l) = 0 THEN [null] ELSE l END AS t`.
- `id()`/`elementId()` values are reused after deletes: never store them outside the database.

## 5. Old syntax (Neo4j 4 -> 5)

| Old | Write |
|---|---|
| `exists(n.prop)` | `n.prop IS NOT NULL` |
| `WHERE exists((a)-[:R]->())`, `size((n)--())` | `WHERE EXISTS { (a)-[:R]->() }`, `COUNT { (n)--() }` |
| `id(n)` | `elementId(n)` (a string) |
| `CREATE INDEX ON :Label(prop)` | `CREATE INDEX name IF NOT EXISTS FOR (n:Label) ON (n.prop)` |
| `CREATE CONSTRAINT ON (n:L) ASSERT n.p IS UNIQUE` | `CREATE CONSTRAINT name IF NOT EXISTS FOR (n:L) REQUIRE n.p IS UNIQUE` |
| `USING PERIODIC COMMIT` | `CALL { ... } IN TRANSACTIONS OF 10000 ROWS` |
| `{param}` | `$param` |

`EXISTS {}`, `COUNT {}` and `CALL {} IN TRANSACTIONS` are Neo4j 5 only.

## 6. Paths

- Bound the length: `-[:R*1..5]->`. A bare `*` explodes on dense graphs.
- `shortestPath((a)-[:R*..10]-(b))`: find both ends by an indexed key first. All of them: `allShortestPaths`.
- `nodes(p)`, `relationships(p)`, `length(p)`, `[n IN nodes(p) | n.name]`.
- Cycles: `MATCH p = (a)-[:DEPENDS_ON*1..6]->(a) RETURN p LIMIT 10`.

## 7. Bulk writes

```cypher
UNWIND $rows AS r
MERGE (n:Item {id: r.id}) SET n += r.props
```

- 1k-10k rows per call.
- `SET n = $map` replaces all properties. `SET n += $map` merges.
- `DELETE n` fails while `n` has relationships: `DETACH DELETE n`.
- Large deletes or updates in batches: `MATCH (n:Old) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10000 ROWS`.
- `IN TRANSACTIONS` needs an auto-commit query (`session.run`, `cypher-shell`), not `execute_write`.

## 8. Indexes and plans

- An index is used only when the pattern names the label: `MATCH (u:User {id: $id})`, not `MATCH (u {id: $id})`.
- Labels, relationship types and property keys cannot be parameters: pick from an allow-list in code.
- `PROFILE <query>` runs it: rows and db hits per operator. `EXPLAIN` only plans.
- Bad signs: `AllNodesScan`; `NodeByLabelScan` where `NodeIndexSeek` was expected; `CartesianProduct`; `Eager` in large writes (split into two queries).

## 9. Drivers

- Python: `driver.execute_query(q, params, database_="neo4j")` returns `(records, summary, keys)`.
- Python: `session.execute_read/execute_write(fn)` may retry `fn`. Keep it idempotent; consume results inside it (`result.data()`).
- JavaScript: integers come back as `Integer` (`.toNumber()`, or `disableLosslessIntegers: true`). JS numbers go as floats: `LIMIT $n` needs `neo4j.int(n)`.
- Temporal values are driver types: `.to_native()` (Python), `.toString()` (JS).
