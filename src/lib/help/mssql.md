# Microsoft SQL Server

Ordinary T-SQL over the TDS protocol. The connection's `schema` is the database
to open (`master` when omitted), and `user` / `password` are SQL Server
authentication — Windows integrated authentication and named instances are not
supported.

## Limiting rows

T-SQL has no `LIMIT`. Use `TOP`, or `OFFSET ... FETCH` with an `ORDER BY`:

```sql
SELECT TOP (20) * FROM orders ORDER BY created_at DESC;

SELECT * FROM orders
ORDER BY created_at DESC
OFFSET 20 ROWS FETCH NEXT 20 ROWS ONLY;
```

A `SELECT` without any of those gets `TOP (n)` inserted automatically (500 rows
unless the connection sets `default_limit`); the result header says when that
happened. Statements with `UNION` / `EXCEPT` / `INTERSECT`, `INTO`, `FOR XML` /
`FOR JSON` or a `WITH` clause are left alone, because `TOP` on the first
`SELECT` would change their meaning.

## Explain

T-SQL has no `EXPLAIN`. Queryfolio accepts `EXPLAIN <select>` anyway and turns
it into `SET SHOWPLAN_ALL ON` → the statement → `SET SHOWPLAN_ALL OFF`, so the
result is the **estimated** plan (one row per operator) and the statement is not
executed. The login needs `SHOWPLAN` permission on the database. The Explain
button does the same.

## Meta commands

```
\l             -- databases (sys.databases)
\dt            -- tables
\dv            -- views
\dn            -- schemas (sys.schemas)
\du            -- database principals
\d sales.orders -- one table's columns (dbo when the schema is omitted)
```

Switching database is `\c reporting` or `USE reporting` (also `USE [reporting]`).
Both reconnect to that database, so the TABLES pane and completion follow.

## Reading

Identifiers go in square brackets (`[order id]`), and strings in single quotes
(`N'...'` for Unicode). Results from `EXEC` / `EXECUTE` are shown as a table
when the procedure returns rows; the Writable switch has to be on, because a
procedure can write.

## Writing

The AI assistant runs its queries on a connection of its own, inside
`BEGIN TRANSACTION ... ROLLBACK`, because SQL Server has no read-only
transaction. That undoes writes that get past the statement guard without
touching a transaction you left open in the editor, but a sequence consumed
with `NEXT VALUE FOR` and an identity value are not rolled back.
