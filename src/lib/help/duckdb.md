# DuckDB

Ordinary SQL with a PostgreSQL-flavoured dialect, plus DuckDB's own extensions
for reading files directly. The connection's `schema` (or `host`) is the path to
the database file; it has to exist already — QueryFolio will not create one.

## Reading files as tables

A path in `FROM` is a table. Most of the time the format is inferred and no
declaration is needed.

```sql
SELECT * FROM 'data/orders.csv' LIMIT 20;
SELECT * FROM 'data/orders.parquet';
SELECT * FROM 'data/*.parquet';           -- a glob is one table
SELECT * FROM read_json_auto('data/events.ndjson');

-- When inference gets it wrong, say what you meant
SELECT * FROM read_csv('data/orders.csv',
                       header = true,
                       delim = ',',
                       columns = {'id': 'INTEGER', 'total': 'DOUBLE'});

-- What did it infer?
DESCRIBE SELECT * FROM 'data/orders.csv';
SELECT * FROM sniff_csv('data/orders.csv');
```

## Shapes that are awkward in other engines

```sql
-- Every column except a few
SELECT * EXCLUDE (created_at, updated_at) FROM orders;

-- Transform a set of columns in place
SELECT * REPLACE (upper(status) AS status) FROM orders;

-- Pick columns by pattern
SELECT COLUMNS('total.*') FROM orders;

-- Reuse an alias in the same SELECT
SELECT total * 1.1 AS with_tax, with_tax - total AS tax FROM orders;

-- GROUP BY every non-aggregated column
SELECT status, region, count(*) FROM orders GROUP BY ALL;
```

## Aggregates and windows

```sql
SELECT status,
       count(*)                        AS n,
       sum(total)                      AS revenue,
       quantile_cont(total, 0.95)      AS p95,
       list(id ORDER BY total DESC)[1:3] AS top_ids
FROM orders
GROUP BY ALL
ORDER BY revenue DESC;

SELECT id, user_id, total,
       sum(total) OVER (PARTITION BY user_id ORDER BY created_at) AS running
FROM orders;
```

## Nested values

```sql
SELECT items[1].sku            FROM orders;   -- lists are 1-indexed
SELECT unnest(items) AS item   FROM orders;
SELECT address.country         FROM orders;   -- struct field
SELECT json_extract(payload, '$.user.id')     FROM events;
```

## Inspecting the database

```sql
SHOW TABLES;
DESCRIBE orders;
SELECT * FROM duckdb_constraints();
SELECT * FROM duckdb_columns() WHERE table_name = 'orders';
```

`\l`, `\dt`, `\d orders` work as well and map onto the same catalog queries.

## What is blocked, and why

With **Writable off** (the default) only read-shaped statements run, and the
same guards as the other SQL engines apply: no `SELECT … INTO`, no CTE that
wraps a write, no `EXPLAIN ANALYZE` of a write, no assignment-form `PRAGMA`.

`EXPLAIN` is available; `EXPLAIN ANALYZE` is not, because it executes the
statement it is explaining.

Without `allow_dangerous_statements`, `UPDATE`/`DELETE` with no `WHERE`, and any
`DROP` or `TRUNCATE`, are refused.

Cell editing and `\c` do not apply here — the schema is a file path, so there is
no database to switch to — and SSH tunnels are not used since the file is local.
