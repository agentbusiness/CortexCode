SQL work: wrong rows or counts, dialect differences, SQL in app code, slow queries, migrations.

## 1. Engine and schema first

1. Find the engine (manifest driver, connection URL, migrations). Passing on SQLite proves nothing about PostgreSQL.
2. Read the real schema and values. Never guess column names, codes or units.
3. Run SQL non-interactively, stopping on the first error:

```bash
sqlite3 -header -column app.db '.schema orders' "SELECT DISTINCT status FROM orders LIMIT 20"
sqlite3 -bail app.db < script.sql
psql "$DATABASE_URL" -X -v ON_ERROR_STOP=1 -P pager=off -c '\d orders' -c "SELECT ..."
mysql --batch -e "SHOW CREATE TABLE orders; SELECT ..." app
```

## 2. Joins and aggregates

Wrong counts:
1. Run the query on one reported case.
2. Remove the aggregate; the raw joined rows show which join adds rows or which filter drops them.
3. After the fix, compare with an independent `COUNT(*)`.

- Two one-to-many joins multiply rows, so `SUM` doubles. Aggregate each child first:
  ```sql
  SELECT c.id, COALESCE(o.total, 0) AS total, COALESCE(p.paid, 0) AS paid
  FROM c
  LEFT JOIN (SELECT cid, SUM(amt) AS total FROM o GROUP BY cid) o ON o.cid = c.id
  LEFT JOIN (SELECT cid, SUM(amt) AS paid  FROM p GROUP BY cid) p ON p.cid = c.id;
  ```
- `LEFT JOIN o ... WHERE o.amt > 15` becomes an inner join: put right-table conditions in `ON`.
- Children through a `LEFT JOIN`: `COUNT(o.id)`. `COUNT(*)` counts the NULL row as 1.
- Never hide a bad join with `DISTINCT`. Find the duplicate key: `SELECT k, COUNT(*) FROM t GROUP BY k HAVING COUNT(*) > 1`.
- Rows of A with no match in B: `NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.id)`, or `LEFT JOIN b ... WHERE b.id IS NULL`.
- Every selected non-aggregate column belongs in `GROUP BY`; SQLite and MySQL otherwise return an arbitrary row's value. `SELECT id, MAX(v) ... GROUP BY g` does not give the max row's id: use a window function.
- `HAVING`: repeat the aggregate (`HAVING COUNT(*) > 1`). PostgreSQL rejects select aliases there.
- Integer / integer truncates in PostgreSQL and SQLite: `CAST(a AS REAL) * 100 / NULLIF(b, 0)` (PostgreSQL `a::numeric`).
- `ORDER BY x DESC LIMIT 1` drops ties; `WHERE x = (SELECT MAX(x) ...)` keeps them.
- `LIMIT`/`OFFSET` needs an `ORDER BY` ending in a unique column, or pages repeat rows.
- Dates as half-open ranges: `d >= '2024-01-01' AND d < '2024-02-01'`. `d <= '2024-01-31'` misses times on the 31st.

Window functions (SQLite 3.25+, MySQL 8+):

```sql
WITH r AS (
  SELECT *, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC, id) AS rn FROM emp)
SELECT * FROM r WHERE rn <= 3;                    -- top 3 per group; RANK() keeps ties

SELECT d, amt,
       SUM(amt) OVER (ORDER BY d, id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,
       amt - LAG(amt) OVER (ORDER BY d, id) AS delta
FROM sales;
```

- `WHERE` cannot see a window result: filter in an outer query.
- With `ORDER BY`, the default frame is `RANGE ... CURRENT ROW`: ties share one running total; `LAST_VALUE` returns the current row. Write the `ROWS BETWEEN` frame.

## 3. NULL

| Wrong | Right |
|---|---|
| `x = NULL`, `x <> NULL` | `x IS NULL`, `x IS NOT NULL` |
| `x NOT IN (SELECT y ...)`: one NULL `y` returns no rows | `NOT EXISTS (SELECT 1 ... WHERE y = x)` |
| `status <> 'done'` drops NULL status | `status IS DISTINCT FROM 'done'` (SQLite `IS NOT`; MySQL `NOT status <=> 'done'`) |
| `SUM(x)` over no rows is NULL | `COALESCE(SUM(x), 0)` |
| `a || b` with a NULL part is NULL | `COALESCE(b, '')`, or `CONCAT_WS` |

- `COUNT(col)`, `AVG`, `MIN`, `MAX` skip NULLs.
- `GROUP BY` puts all NULLs in one group.
- A `UNIQUE` column allows many NULLs.

## 4. Dialect differences

| Topic | PostgreSQL | SQLite | MySQL |
|---|---|---|---|
| Auto id | `bigint GENERATED ALWAYS AS IDENTITY` | `INTEGER PRIMARY KEY` | `BIGINT AUTO_INCREMENT PRIMARY KEY` |
| New id | `RETURNING id` | `RETURNING id` (3.35+), `lastrowid` | `LAST_INSERT_ID()` |
| `"x"` | identifier | identifier, or a string when no such column (typos pass) | string; identifiers use backticks |
| `a \|\| b` | concat | concat | logical OR: `CONCAT(a, b)` |
| Case in `=` / `LIKE` | both sensitive; `ILIKE` | `=` sensitive, `LIKE` insensitive (ASCII) | both insensitive |
| `5 / 2` | `2` | `2` | `2.5` (`DIV` for integer) |
| NULLs in `ORDER BY x` | last | first | first |
| Types | enforced | a hint: `'abc'` fits an INTEGER column unless `STRICT` | coerced or truncated unless strict mode |
| Foreign keys | enforced | off unless `PRAGMA foreign_keys = ON` per connection | enforced (InnoDB) |
| DDL in a transaction | rolls back | rolls back | each DDL commits |
| Month start | `date_trunc('month', d)` | `strftime('%Y-%m-01', d)` | `DATE_FORMAT(d, '%Y-%m-01')` |
| Days between | `d2::date - d1::date` | `julianday(d2) - julianday(d1)` | `DATEDIFF(d2, d1)` |
| Unquoted names | folded to lower case: `"UserId"` must be quoted everywhere | case-insensitive | table names case-sensitive on Linux |
| `UPDATE` with a join | `UPDATE t SET .. FROM s WHERE ..` | same (3.33+) | `UPDATE t JOIN s ON .. SET ..` |
| JSON field | `data->>'name'` | `json_extract(data, '$.name')` | `data->>'$.name'` |
| String aggregate | `string_agg(x, ',' ORDER BY x)` | `group_concat(x, ',')` | `GROUP_CONCAT(x ORDER BY x SEPARATOR ',')` (cut at 1024 bytes) |

Upsert (PostgreSQL, SQLite 3.24+). `ON CONFLICT (cols)` needs a unique index on exactly those columns:

```sql
INSERT INTO product (sku, name, price) VALUES (?, ?, ?)
ON CONFLICT (sku) DO UPDATE SET name = excluded.name, price = excluded.price
RETURNING id;
```

- Columns not in `SET` keep their value.
- `INSERT OR REPLACE` (SQLite) and `REPLACE INTO` (MySQL) delete the old row: new id, cascades fire, other columns reset.
- `DO NOTHING RETURNING id` returns no row for an existing key.
- MySQL `ON DUPLICATE KEY UPDATE price = VALUES(price)` fires on ANY unique key.
- SQLite: store dates as ISO text (`YYYY-MM-DD HH:MM:SS`) so text order is time order. Bind values with the column's type (`'1'` and `1` differ).
- MySQL: `utf8mb4`, never `utf8`. A string column compared with a number converts every row (`'1abc' = 1` is true) and skips the index: quote the value.

## 5. In application code

| Driver | Placeholder |
|---|---|
| Python `sqlite3` | `?` or `:name`; `cur.execute(q, (x,))` (one-element tuple needs the comma) |
| `psycopg`, `psycopg2`, `PyMySQL` | `%s` or `%(name)s`; a literal `%` becomes `%%` |
| `asyncpg`, node `pg`, Go `pgx`/`lib/pq` | `$1` |
| SQLAlchemy `text()` | `:name` |
| `mysql2`, `better-sqlite3`, JDBC, Go mysql/sqlite | `?` |
| `rusqlite` | `?1` with `params![x]` |

- Identifiers and `ASC`/`DESC` cannot be parameters: pick from a fixed allow-list.
- `IN` lists: PostgreSQL `= ANY($1)` with an array. Elsewhere one placeholder per item; handle an empty list before querying.
- Read-modify-write in one statement (`SET n = n + 1`), or `SELECT ... FOR UPDATE` in the transaction.
- Check-then-insert races: a unique constraint plus upsert.
- PostgreSQL: after any error the transaction is aborted until `ROLLBACK`.
- SQLite "database is locked": a connection holds a write transaction (Python `sqlite3` holds one from a write until `commit()`). Commit after each unit of work; `PRAGMA journal_mode=WAL` once; `timeout=30`; `BEGIN IMMEDIATE` for read-then-write.

## 6. Reading a query plan

Time the query before and after each change:

```bash
printf '.timer on\nSELECT ...;\n' | sqlite3 app.db
sqlite3 app.db "EXPLAIN QUERY PLAN SELECT ..."
psql "$DATABASE_URL" -X -c "EXPLAIN (ANALYZE, BUFFERS) SELECT ..."   # executes it: wrap writes in BEGIN; ... ROLLBACK;
mysql -e "EXPLAIN ANALYZE SELECT ..." app
```

| Plan says | Fix |
|---|---|
| SQLite `SCAN t`, PG `Seq Scan` with large `Rows Removed by Filter`, MySQL `type: ALL` | index the filtered columns |
| `USE TEMP B-TREE FOR ORDER BY`, `Sort Method: external merge`, `Using filesort` | index ending in the `ORDER BY` columns |
| PG estimated `rows=10`, actual `rows=200000` | `ANALYZE t;`, then plan again |
| `Nested Loop` with a scan inside and many `loops` | index the inner join key |

- List existing indexes first: `.indexes t`, `\d t`, `SHOW INDEX FROM t`.
- Composite index: equality columns first, then the range or sort column. `(a, b)` serves `a` and `a, b`, not `b` alone.
- A function on the column skips its index. `WHERE date(created) = '2024-05-01'` -> `created >= '2024-05-01' AND created < '2024-05-02'`. `lower(email) = ?` needs an index `ON t (lower(email))`.
- `LIKE 'abc%'` can use an index; `LIKE '%abc'` cannot.
- PostgreSQL does not index foreign-key columns: add one for joins and cascading deletes.
- A query per row in a loop: one join or `IN` query.
- Deep pages: keyset `WHERE (created, id) < (?, ?) ORDER BY created DESC, id DESC LIMIT 20`.
- Bulk inserts: one transaction around them.

## 7. Safe migrations

1. Use the project's tool (`migrations/`, `alembic/`, `prisma/migrations/`, `db/migrate/`). Never edit an applied migration; add a new one.
2. Read generated SQL. Autogenerators turn renames into drop + add (data loss) and miss defaults, enums, indexes and backfills.
3. On a scratch database: upgrade, downgrade, upgrade. Then run the tests.

Large live PostgreSQL tables (DDL takes an exclusive lock):

- Start with `SET lock_timeout = '5s';`.
- Add columns nullable or with a constant default.
- `CREATE INDEX CONCURRENTLY`, outside a transaction.
- NOT NULL or foreign key: add it `NOT VALID`, then `VALIDATE CONSTRAINT`.
- Rename or type change: add the new column, write both, backfill, switch reads, drop the old one later.

MySQL: a failed migration leaves earlier DDL applied: make each step idempotent (`IF NOT EXISTS`).

SQLite `ALTER TABLE` only adds, renames and drops columns. Anything else is a rebuild:

```sql
PRAGMA foreign_keys = OFF;
BEGIN;
CREATE TABLE t_new (...);
INSERT INTO t_new (a, b) SELECT a, b FROM t;
DROP TABLE t;
ALTER TABLE t_new RENAME TO t;
-- recreate indexes, triggers, views
PRAGMA foreign_key_check;
COMMIT;
PRAGMA foreign_keys = ON;
```

`ADD COLUMN ... NOT NULL` on a non-empty table needs a non-NULL default.
