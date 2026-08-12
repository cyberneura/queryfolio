# SQL

Cmd+Enter (Ctrl+Enter) runs the statement under the cursor. With a selection it
runs the selection instead, so you can execute part of a longer file.

`SELECT` without a `LIMIT` gets one appended automatically — 500 rows unless the
connection sets `default_limit`. The result header says when that happened.

## Meta commands

psql-style shortcuts, translated to whichever catalog the engine uses:

```
\l          -- databases
\dt         -- tables
\dv         -- views
\d users    -- one table's columns, types and indexes
```

Two more exist, but not everywhere: `\dn` (schemas) is PostgreSQL only, and
`\du` (roles) is PostgreSQL and MySQL. Asking an engine for one it does not
have returns an "unsupported meta command" error listing what it does support.

Switching database is `\c reporting` or `USE reporting` (MySQL and PostgreSQL).
Both rebuild the connection pool, so they take effect for every following
statement — sending `USE` as plain SQL would only affect one pooled connection
and would not stick.

## Reading

This page covers MySQL, PostgreSQL and SQLite, so the examples below stick to
syntax all three accept. Anything dialect-specific is marked as such.

```sql
SELECT * FROM users WHERE created_at >= '2026-08-01' ORDER BY created_at DESC;

SELECT status, count(*) AS n, sum(total) AS revenue
FROM orders
WHERE created_at >= '2026-08-01'
GROUP BY status
ORDER BY n DESC;

SELECT u.id, u.email, count(o.id) AS orders
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
GROUP BY u.id, u.email
HAVING count(o.id) > 5
ORDER BY orders DESC;
```

Relative dates are where the three diverge:

```sql
-- PostgreSQL
SELECT * FROM users WHERE created_at >= now() - interval '7 days';
-- MySQL
SELECT * FROM users WHERE created_at >= now() - interval 7 day;
-- SQLite
SELECT * FROM users WHERE created_at >= datetime('now', '-7 days');
```

Window functions work on all three (MySQL 8, SQLite 3.25 and later):

```sql
-- Per-group ranking without a self-join
SELECT id, user_id, total,
       row_number() OVER (PARTITION BY user_id ORDER BY created_at DESC) AS n
FROM orders;

-- Latest row per group: rank first, filter outside
WITH ranked AS (
  SELECT *, row_number() OVER (PARTITION BY user_id ORDER BY created_at DESC) AS n
  FROM orders
)
SELECT * FROM ranked WHERE n = 1;
```

```sql
-- Counts per day. Days with no rows are absent rather than zero; filling them
-- in needs a generated series, which each engine spells differently.
SELECT substr(created_at, 1, 10) AS day, count(*) AS orders
FROM orders
WHERE created_at >= '2026-08-01'
GROUP BY day
ORDER BY day;
```

## Understanding a slow query

```sql
EXPLAIN SELECT * FROM orders WHERE user_id = 42;
```

The Explain button runs this for you and can hand the plan to the AI for a
reading.

`EXPLAIN ANALYZE` really runs the statement it is explaining, so with Writable
off it is allowed for a read (`EXPLAIN ANALYZE SELECT …`) and refused when the
analysed statement writes. The Explain button always uses plain `EXPLAIN`.

## Editing results

With Writable on, a cell in the result grid can be edited in place when the
result carries enough key information to identify its row; QueryFolio generates
the `UPDATE`. Engines without that support hide the affordance.

## What is blocked, and why

With **Writable off** (the default) the leading keyword has to be one of
`SELECT`, `WITH`, `SHOW`, `DESCRIBE`, `EXPLAIN`, `PRAGMA`, `VALUES`, `TABLE`,
`CALL`. Write-shaped variants are rejected even when they start that way: a
`WITH` whose body is an `INSERT`/`UPDATE`/`DELETE`, `SELECT … INTO`,
`EXPLAIN ANALYZE` of a statement that writes, and assignment-form `PRAGMA`.

`readonly: true` on a connection outranks the Writable switch and cannot be
turned off from the toolbar — the lock icon means the setting, not the switch.

Without `allow_dangerous_statements`, an `UPDATE` or `DELETE` with no `WHERE`,
and any `DROP` or `TRUNCATE`, are refused. With it enabled they still raise a
confirmation dialog first.

These checks read the statement, not the database, so they are a guard against
accidents rather than a security boundary: a `SELECT` calling a function with
side effects still runs.
