# Elasticsearch / OpenSearch

Requests are written Kibana Console style: a method and path on one line, then
an optional JSON body. Blank lines separate requests, and `#` starts a comment.

Run with the cursor inside a request to send just that one; select several to
send them in order. Without a selection the block is taken from the method line
at or above the cursor down to the next method line.

```
GET /_cluster/health
```

## Finding your way around

```
GET /_cat/indices?v&s=index
GET /_cat/indices?format=json
GET /_cat/aliases?v
GET /_cat/nodes?v
GET /_cat/shards?v&s=state

GET /orders/_mapping
GET /orders/_settings
GET /orders/_count
```

`_cat/…?format=json` comes back as a table; the default plain-text form arrives
as a single cell, so add `format=json` when you want columns.

## Searching

```
# Everything, newest first
GET /orders/_search
{
  "size": 20,
  "sort": [{ "created_at": "desc" }],
  "query": { "match_all": {} }
}
```

```
# Full-text on one field
GET /articles/_search
{
  "query": { "match": { "title": "postgres replication" } }
}
```

```
# Exact term. Use .keyword for a text field that has a keyword sub-field —
# "term" on an analysed field usually matches nothing.
GET /orders/_search
{
  "query": { "term": { "status.keyword": "shipped" } }
}
```

```
# Combining conditions: must contributes to score, filter does not (and caches)
GET /orders/_search
{
  "size": 50,
  "query": {
    "bool": {
      "must":   [{ "match": { "note": "urgent" } }],
      "filter": [
        { "term":  { "status.keyword": "open" } },
        { "range": { "created_at": { "gte": "now-7d/d", "lt": "now/d" } } }
      ],
      "must_not": [{ "term": { "test.keyword": "true" } }]
    }
  }
}
```

```
# Only the fields you need, which is much cheaper than whole documents
GET /orders/_search
{
  "_source": ["id", "status", "total"],
  "query": { "range": { "total": { "gte": 1000 } } }
}
```

```
# Paging. from+size is fine to about 10,000; beyond that use search_after.
GET /orders/_search
{
  "from": 100,
  "size": 50,
  "sort": [{ "created_at": "desc" }, { "_id": "asc" }],
  "query": { "match_all": {} }
}
```

## Aggregations

```
# Counts per value. size caps the number of buckets, not the documents.
GET /orders/_search
{
  "size": 0,
  "aggs": {
    "by_status": { "terms": { "field": "status.keyword", "size": 20 } }
  }
}
```

```
# Over time, with a metric inside each bucket
GET /orders/_search
{
  "size": 0,
  "aggs": {
    "per_day": {
      "date_histogram": { "field": "created_at", "calendar_interval": "day" },
      "aggs": { "revenue": { "sum": { "field": "total" } } }
    }
  }
}
```

```
# Spread of a numeric field
GET /orders/_search
{
  "size": 0,
  "aggs": {
    "totals": { "stats": { "field": "total" } },
    "p95":    { "percentiles": { "field": "total", "percents": [50, 95, 99] } }
  }
}
```

`"size": 0` drops the hits and returns only the aggregation, which is what you
want almost every time you aggregate.

## Reading one document, and explaining a query

```
GET /orders/_doc/1042
GET /orders/_source/1042

# Why did (or didn't) this document match
GET /orders/_explain/1042
{
  "query": { "term": { "status.keyword": "shipped" } }
}

# How was this text analysed
GET /articles/_analyze
{
  "field": "title",
  "text": "Postgres replication lag"
}
```

## Writing

```
POST /orders/_doc/1042
{ "status": "shipped", "total": 4200 }

POST /orders/_update/1042
{ "doc": { "status": "delivered" } }

# _bulk takes NDJSON: an action line and a document line, one per row
POST /_bulk
{"index":{"_index":"orders","_id":"1"}}
{"status":"open","total":100}
{"index":{"_index":"orders","_id":"2"}}
{"status":"open","total":250}
```

## What is blocked, and why

With **Writable off** (the default) only `GET` and `HEAD` run, plus the search
family of `POST` requests (`_search`, `_count`, `_msearch`, `_explain`,
`_analyze` …) since those read despite the method. Everything else is refused
before it is sent.

Deleting an index (`DELETE /orders`) and `_delete_by_query` additionally need
`allow_dangerous_statements` on the connection.

Paths containing `.` or `..` segments are rejected outright, encoded or not.
Responses are read up to 20 MB and long cells are truncated with a marker.
