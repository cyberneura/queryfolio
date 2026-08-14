# DynamoDB (PartiQL)

Statements are PartiQL, sent through the `ExecuteStatement` API. It looks like
SQL but the engine underneath is still DynamoDB: there is no join, no `GROUP BY`,
and no `LIMIT` clause. Table and attribute names that are not plain identifiers
go in double quotes.

## Listing tables

```
tables
```

`tables` is QueryFolio's own statement, not PartiQL — DynamoDB has no
`SHOW TABLES`. It calls `ListTables` directly, so it runs even with Writable
off. The TABLES pane shows the same list along with each table's partition and
sort key.

## Reading

```
-- Whole table (a scan — fine for small tables, expensive for large ones)
SELECT * FROM "orders"

-- By partition key: the cheap path, a single lookup
SELECT * FROM "orders" WHERE "pk" = 'USER#1001'

-- Partition key plus sort key
SELECT * FROM "orders"
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-01'

-- Sort key prefix, the usual single-table pattern
SELECT * FROM "orders"
WHERE "pk" = 'USER#1001' AND begins_with("sk", 'ORDER#')

-- Only the attributes you need
SELECT "sk", "status", "total" FROM "orders" WHERE "pk" = 'USER#1001'
```

Whether a statement is a query or a scan is decided by the `WHERE` clause: give
the partition key an equality and DynamoDB queries; leave it out and it scans
the whole table and filters afterwards, reading (and charging for) everything.

## Indexes

Address a secondary index by name after the table:

```
SELECT * FROM "orders"."gsi_status"
WHERE "status" = 'open'

SELECT * FROM "orders"."gsi_status"
WHERE "status" = 'open' AND "created_at" > '2026-08-01'
```

## Filtering

```
SELECT * FROM "orders"
WHERE "pk" = 'USER#1001' AND "total" > 1000

SELECT * FROM "orders"
WHERE "pk" = 'USER#1001' AND "status" IN ['open', 'pending']

-- Nested attributes and list elements use paths
SELECT * FROM "orders" WHERE "address"."country" = 'JP'
SELECT * FROM "orders" WHERE "items"[0]."sku" = 'ABC'

-- Presence rather than value
SELECT * FROM "orders" WHERE attribute_exists("cancelled_at")
SELECT * FROM "orders" WHERE attribute_not_exists("cancelled_at")

SELECT * FROM "orders" WHERE contains("tags", 'urgent')
SELECT * FROM "orders" WHERE begins_with("sk", 'ORDER#2026-08')
```

A filter is applied after the read, so `WHERE "total" > 1000` without a
partition key still reads the whole table — it only makes the result smaller,
not the work.

## Row limits

PartiQL has no `LIMIT`. QueryFolio requests one page at a time and stops once it
has `default_limit` rows (500 unless the connection says otherwise), then marks
the result truncated. Narrow the `WHERE` clause rather than expecting a limit
clause to save you.

## Writing

```
INSERT INTO "orders" VALUE {
  'pk': 'USER#1001',
  'sk': 'ORDER#2026-08-12',
  'status': 'open',
  'total': 4200
}

UPDATE "orders"
SET "status" = 'shipped'
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-12'

UPDATE "orders"
SET "tags" = list_append("tags", ['priority'])
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-12'

UPDATE "orders" REMOVE "note"
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-12'

DELETE FROM "orders"
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-12'

-- Get the previous state back instead of an empty result
UPDATE "orders" SET "status" = 'shipped'
WHERE "pk" = 'USER#1001' AND "sk" = 'ORDER#2026-08-12'
RETURNING ALL OLD *
```

`INSERT`, `UPDATE` and `DELETE` address exactly one item and need the full
primary key. There is no multi-row update: to change many items, read them and
issue one statement each.

The API does not report how many items a write touched, so the result shows no
row count. `RETURNING ALL OLD *` is the way to see what was there before.

## What is blocked, and why

With **Writable off** (the default) only `SELECT` and `tables` run. `UPDATE` and
`DELETE` without a `WHERE` clause additionally need
`allow_dangerous_statements`, as does dropping data wholesale.

Connection settings worth knowing: `schema` is the AWS region and is required;
`host`/`port` override the endpoint for dynamodb-local; credentials come from
user/password, then `aws_profile`, then the default provider chain. Meta
commands, `\c`, `EXPLAIN`, cell editing, AI features and SSH tunnels do not
apply to this engine and are hidden.
