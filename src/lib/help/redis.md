# Redis / Valkey

One command per line, written the way `redis-cli` takes them — no quotes around
the command itself. Select the lines you want and run; every selected line goes
to the same connection in order.

Arguments follow `redis-cli` quoting: wrap an argument in `"` or `'` when it
contains spaces, and use `\xHH` inside double quotes for raw bytes. A line starting with `#` is a
comment; `#` part-way through a line is **not** — it is tokenised as another
argument, so keep comments on their own lines.

## Looking around

```
# How many keys are in this database, and what does the server look like
DBSIZE
INFO keyspace
INFO memory

# Walk the keyspace without blocking the server. SCAN returns a cursor as the
# first element; pass it back to continue. Prefer this over KEYS.
SCAN 0 MATCH session:* COUNT 100
SCAN 17408 MATCH session:* COUNT 100

# What is this key, how big is it, and when does it die
TYPE session:abc123
OBJECT ENCODING session:abc123
MEMORY USAGE session:abc123
TTL session:abc123
```

`KEYS *` scans the entire keyspace in one blocking pass. It is fine on a laptop
with a few thousand keys and a bad idea on anything shared.

## Strings and counters

```
GET  feature:new-billing
SET  feature:new-billing enabled
SETEX cache:homepage 300 "<html>…</html>"
SET  lock:import 1 NX EX 30

MGET user:1:name user:2:name user:3:name
INCR  metrics:page-views
INCRBY metrics:bytes-sent 4096
APPEND log:today "line\n"
```

`SET … NX EX 30` is the lock idiom: set only if absent, expire after 30 seconds.

## Hashes

```
HGETALL user:1001
HGET    user:1001 email
HMGET   user:1001 email plan created_at
HSET    user:1001 plan pro updated_at 1735689600
HDEL    user:1001 legacy_token
HLEN    user:1001

# Large hashes: iterate rather than pulling the whole thing
HSCAN user:1001 0 COUNT 50
```

`HGETALL` comes back as field/value pairs and is rendered as two columns.

## Lists, sets, sorted sets

```
# First ten without consuming
LRANGE queue:emails 0 9
LLEN   queue:emails
LPUSH  queue:emails '{"to":"a@example.com"}'
RPOP   queue:emails

SMEMBERS  tags:article:7
SISMEMBER tags:article:7 rust
SCARD     tags:article:7

# Sorted sets: WITHSCORES pairs each member with its score
ZRANGE     leaderboard 0 9 WITHSCORES
ZREVRANGE  leaderboard 0 9 WITHSCORES
ZSCORE     leaderboard user:1001
ZADD       leaderboard 4200 user:1001
ZRANGEBYSCORE leaderboard 1000 2000
ZCOUNT     leaderboard -inf +inf
```

## Streams

```
XLEN     events
XINFO STREAM events
# - and + are the ends of the stream
XRANGE   events - + COUNT 10
XREVRANGE events + - COUNT 10
XINFO GROUPS events
XPENDING events workers
```

`XREAD BLOCK` and the other blocking forms are rejected: they hold the
connection open with nothing to show, and cancelling the query does not release
it.

## Expiry and cleanup

```
# TTL is seconds left: -1 means no expiry, -2 means no such key.
# PTTL is the same in milliseconds.
TTL     session:abc123
PTTL    session:abc123
EXPIRE  session:abc123 3600
PERSIST session:abc123
DEL     session:abc123
# UNLINK frees the memory in the background
UNLINK  big:key
```

## Server and config

These read nothing but a server's own state, yet `CONFIG`, `CLIENT`, `SLOWLOG`
and `LATENCY` are not on the read-only list — the guard works per command, not
per subcommand, and each of these has writing forms (`CONFIG SET`,
`CLIENT KILL`, `SLOWLOG RESET`, `LATENCY RESET`). **Turn Writable on first**, or
they are refused before they reach the server.

```
CONFIG GET maxmemory
CONFIG GET maxmemory-policy
CLIENT LIST
SLOWLOG GET 10
LATENCY LATEST
```

`INFO`, `DBSIZE`, `OBJECT` and `MEMORY USAGE` need no such thing — they are on
the list (`MEMORY` is checked per subcommand, so `MEMORY USAGE` passes while
`MEMORY PURGE` does not).

## What is blocked, and why

With **Writable off** (the default) only read commands run — `GET`, `SCAN`,
`HGETALL`, `ZRANGE`, `INFO` and friends. Anything that writes is refused before
it reaches the server, so switch Writable on deliberately when you mean to
change data.

`FLUSHALL`, `FLUSHDB`, `SHUTDOWN` and `DEBUG` need `allow_dangerous_statements`
on the connection as well. Pub/sub (`SUBSCRIBE`, `PSUBSCRIBE`) and blocking
commands (`BLPOP`, `BRPOP`, `XREAD BLOCK`, `WAIT`) are always rejected.

The `schema` field of the connection is the database number, so switching schema
is `SELECT` under the hood — pick it from the toolbar rather than sending
`SELECT` yourself.
