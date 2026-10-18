//! DuckDB engine.
//!
//! DuckDB is a SQL engine, but sqlx has no driver for it, so it is wired up directly with the
//! `duckdb` crate (duckdb-rs, bundled). The common SQL guards (readonly / dangerous / auto LIMIT /
//! meta commands / EXPLAIN) reuse the existing logic in db.rs as is (scan_sql uses the Postgres
//! dialect).
//!
//! - The connection mirrors sqlite: `schema` (or `host` if absent) is opened as the DB file path.
//!   An error is returned if the file does not exist (we never silently create a new one).
//!   SSH tunnels are not supported (file-based).
//! - duckdb-rs has a synchronous API, so execution is wrapped in `spawn_blocking`.
//!   A single connection is kept as `Arc<Mutex<Connection>>`
//!   (duckdb::Connection is Send but not Sync).
//! - Cancellation requires an interrupt on the server (engine) side:
//!   spawn_blocking is not stopped by dropping the future, so the running statement is aborted
//!   with `InterruptHandle::interrupt()` (`CancelTarget::DuckDb`). An interrupt only affects a
//!   statement that is currently running, so a cancel that arrives before execution starts is
//!   caught by a flag check on the blocking side.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use duckdb::types::Value;
use duckdb::Connection;

use crate::config::{expand_tilde, ServerConfig};
use crate::db::{
    bytes_to_json, contains_returning, dangerous_block_error, dangerous_reason,
    is_fetch_statement, is_readonly_allowed, json_i64, json_u64, leading_keyword,
    readonly_block_error, scan_sql, should_auto_limit, CancelRegistry, CancelTarget,
    Engine, QueryResult, ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

/// Upper bound on the number of elements of a collection (LIST / STRUCT / MAP) in one cell.
/// Anything beyond it is cut off and reported via QueryResult.truncated
/// (so unbounded nested values are never sent to the webview).
const MAX_COLLECTION_ELEMENTS: usize = 1000;

/// Upper bound on the number of characters of a string (TEXT / BLOB) in one cell. Anything beyond
/// it is cut off and `truncated` is set.
/// Known limitation: duckdb-rs's row.get fully materializes the value before returning it, so this
/// limit protects the size sent to the webview, while temporary memory on the Rust side is still
/// consumed for the materialized value (limit the size on the query side for a huge single cell).
const MAX_TEXT_CHARS: usize = 10_000;

/// Upper bound on the recursion depth when converting nested values (LIST / STRUCT / MAP / UNION)
/// to JSON. read_json_auto and the like can return nesting of arbitrary depth derived from the
/// data, so this prevents stack overflow (beyond it: placeholder + truncated).
const MAX_NESTING_DEPTH: usize = 32;

/// Handle of a DuckDB connection. Held as DbPool::DuckDb.
/// interrupt can abort the running statement without taking the Mutex (used for cancellation).
/// exec is an async lock that serializes query execution (run_query_cancellable) per connection:
/// the cancel registration (CancelRegistry) holds one entry per connection name, so if a second
/// query ran concurrently on the same connection the registration would be overwritten and the
/// shared InterruptHandle would take down the first, running one (cancellation crosstalk).
/// Serializing execution with this lock guarantees the registration always matches the statement
/// that is actually running (without relying on the frontend preventing parallel runs).
#[derive(Clone)]
pub struct DuckDbHandle {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<duckdb::InterruptHandle>,
    exec: Arc<tokio::sync::Mutex<()>>,
}

// Written by hand because Connection does not implement Debug (needed for unwrap_err in tests)
impl std::fmt::Debug for DuckDbHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DuckDbHandle")
    }
}

/// Resolves the DB file path from the settings (schema takes priority, as with sqlite).
fn database_path(server: &ServerConfig) -> Result<std::path::PathBuf, AppError> {
    let path = server
        .schema
        .as_deref()
        .or(server.host.as_deref())
        .ok_or_else(|| {
            AppError::Config("For duckdb, set schema to the database file path".into())
        })?;
    Ok(expand_tilde(path))
}

/// Establishes the connection. Returns an error if the file does not exist
/// (Connection::open silently creates a new file when it is missing, so check first).
pub async fn connect(server: &ServerConfig) -> Result<DuckDbHandle, AppError> {
    let file_path = database_path(server)?;
    if !file_path.exists() {
        return Err(AppError::Config(format!(
            "DuckDB database file not found: {}",
            file_path.display()
        )));
    }
    // Opening the file is local I/O only, but WAL replay and the like can take
    // a while, so do it on a blocking thread
    let conn = tokio::task::spawn_blocking(move || Connection::open(&file_path))
        .await
        .map_err(|e| AppError::DuckDb(format!("DuckDB open task failed: {e}")))??;
    let interrupt = conn.interrupt_handle();
    Ok(DuckDbHandle {
        conn: Arc::new(Mutex::new(conn)),
        interrupt,
        exec: Arc::new(tokio::sync::Mutex::new(())),
    })
}

/// Executes SQL and returns the result (cancellable version).
/// db::run_query_cancellable delegates here for DbPool::DuckDb.
/// Since this is a SQL engine, meta commands, readonly / dangerous guards and auto LIMIT are
/// applied in the same flow as the SQL engines in db.rs.
#[allow(clippy::too_many_arguments)]
pub async fn run_query_cancellable(
    handle: &DuckDbHandle,
    registry: &CancelRegistry,
    connection_name: &str,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    // psql-style meta commands are converted into catalog queries.
    // \c is rejected for DuckDB in meta_commands, so Connect never arrives here
    let translated = match crate::meta_commands::translate(Engine::DuckDb, sql)? {
        Some(crate::meta_commands::MetaCommand::Sql(sql)) => Some(sql),
        Some(crate::meta_commands::MetaCommand::Connect(_)) => {
            return Err(AppError::Config(
                "\\c is not supported for DuckDB".into(),
            ));
        }
        None => None,
    };
    let sql = translated.as_deref().unwrap_or(sql);

    if leading_keyword(sql).is_empty() {
        return Err(AppError::Config("The SQL statement is empty".into()));
    }

    // The agent path uses a narrow whitelist (same reason as run_query_on in db.rs;
    // ReadonlyGuard::Agent itself holds the policy, not the caller)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) =
            crate::db::agent_rejection_reason(sql, Engine::DuckDb)
        {
            return Err(AppError::Readonly(reason));
        }
    }

    // Multi-statement input slips past the guard (only the first statement is inspected). As in
    // run_query_on in db.rs, multi-statement input is rejected when the guard is active.
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && crate::db::contains_multiple_statements(sql, Engine::DuckDb)
    {
        return Err(crate::db::multi_statement_block_error());
    }

    // Apply the guard to the SQL after meta command conversion as well (to prevent bypass.
    // The converted SQL is read-only so it always passes, but make the order explicit)
    if readonly != ReadonlyGuard::Off
        && !is_readonly_allowed(sql, Engine::DuckDb)
        && !is_duckdb_readonly_statement(sql)
    {
        return Err(readonly_block_error(readonly));
    }
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, Engine::DuckDb) {
            return Err(dangerous_block_error(reason));
        }
    }

    // Add the default LIMIT to a SELECT without one
    // (not applied to SQL after meta command conversion; same as run_query_on in db.rs)
    let mut applied_limit = None;
    let limited_sql;
    let sql = match auto_limit {
        Some(limit)
            if limit > 0
                && translated.is_none()
                && should_auto_limit(sql, Engine::DuckDb) =>
        {
            let body = &sql[..scan_sql(sql, Engine::DuckDb).body_end];
            limited_sql = format!("{body} LIMIT {limit}");
            applied_limit = Some(limit);
            limited_sql.as_str()
        }
        _ => sql,
    };

    // Serialize execution per connection, then register the cancel target
    // (prevents cancellation crosstalk where the registration and the running statement diverge)
    let _exec = handle.exec.lock().await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = registry.register(
        connection_name,
        CancelTarget::DuckDb {
            interrupt: handle.interrupt.clone(),
        },
        cancelled.clone(),
    );
    let started = Instant::now();

    let conn = handle.conn.clone();
    let sql_owned = sql.to_string();
    let fetch = is_fetch_statement(sql)
        || is_duckdb_readonly_statement(sql)
        || contains_returning(sql);
    // spawn_blocking is not stopped by dropping the future, but on cancel the
    // CancelRegistry issues an interrupt that ends the running statement with an error,
    // so this await never stays pending indefinitely.
    // The agent path is wrapped in a read-only transaction (enforced at the DB level)
    let readonly_tx = readonly == ReadonlyGuard::Agent;
    let result = tokio::task::spawn_blocking(move || {
        execute_blocking(&conn, &sql_owned, max_rows, fetch, readonly_tx, &cancelled)
    })
    .await
    .map_err(|e| AppError::DuckDb(format!("DuckDB task failed: {e}")))?;

    let was_cancelled = guard.was_cancelled();
    drop(guard);

    // An error after a cancel request is returned as "cancelled"
    // (if the query completed before the cancel took effect, the successful result wins)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.applied_limit = applied_limit;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// Whether this is a read statement returning rows that is specific to DuckDB. The common
/// is_fetch_statement only knows the standard leading SQL keywords, so DuckDB's FROM-first syntax
/// (`FROM t`) / SUMMARIZE / PIVOT / UNPIVOT are covered here.
/// All of them are read-only query forms that cannot express writes
/// (DuckDB has no SELECT INTO), so this is also used for the readonly check.
fn is_duckdb_readonly_statement(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "from" | "summarize" | "pivot" | "unpivot"
    )
}

/// Executes one statement on a blocking thread.
/// fetch = a statement that returns rows (SELECT family / with RETURNING). Anything else uses
/// execute and only gets the affected row count.
/// readonly_tx = wrap in a read-only transaction (agent path).
/// DuckDB's `BEGIN TRANSACTION READ ONLY` rejects all writes, including `SELECT nextval(...)`.
/// Because it runs synchronously while holding the connection Mutex, everything up to ROLLBACK
/// always completes inside this function (even when interrupted by a cancel, what gets aborted
/// is the running statement; this function itself runs to the end).
fn execute_blocking(
    conn: &Mutex<Connection>,
    sql: &str,
    max_rows: usize,
    fetch: bool,
    readonly_tx: bool,
    cancelled: &AtomicBool,
) -> Result<QueryResult, AppError> {
    let conn = conn.lock().map_err(|_| {
        AppError::DuckDb("The DuckDB connection is poisoned".into())
    })?;
    // interrupt only affects a running statement, so a cancel that arrived
    // before execution started is caught here
    if cancelled.load(Ordering::SeqCst) {
        return Err(AppError::Cancelled);
    }

    if readonly_tx {
        conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
        let result = execute_statement_blocking(&conn, sql, max_rows, fetch);
        // Only reads were done, so COMMIT is unnecessary. ROLLBACK is accepted even if the
        // transaction was aborted by an interrupt
        // (on failure, prefer returning the original error / result)
        let _ = conn.execute_batch("ROLLBACK");
        return result;
    }
    execute_statement_blocking(&conn, sql, max_rows, fetch)
}

/// Body of execute_blocking (shared between inside and outside a transaction).
fn execute_statement_blocking(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    fetch: bool,
) -> Result<QueryResult, AppError> {
    if !fetch {
        let affected = conn.execute(sql, [])? as u64;
        return Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            row_count: 0,
            affected_rows: Some(affected),
            truncated: false,
            elapsed_ms: 0,
            applied_limit: None,
            switched_schema: None,
        });
    }

    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    // query materializes the result, so the column info is already settled at this point
    let columns: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
    let column_count = columns.len();

    let mut out: Vec<Vec<serde_json::Value>> = vec![];
    let mut truncated = false;
    while let Some(row) = rows.next()? {
        if out.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(column_count);
        for i in 0..column_count {
            let value: Value = row.get(i)?;
            values.push(value_to_json_limited(value, &mut truncated));
        }
        out.push(values);
    }

    Ok(QueryResult {
        row_count: out.len(),
        columns,
        rows: out,
        affected_rows: None,
        truncated,
        elapsed_ms: 0,
        applied_limit: None,
        switched_schema: None,
    })
}

/// i128 (HUGEINT) to JSON. A number if within the JS safe integer range, otherwise a string.
fn json_i128(v: i128) -> serde_json::Value {
    match i64::try_from(v) {
        Ok(v) => json_i64(v),
        Err(_) => serde_json::Value::String(v.to_string()),
    }
}

fn json_f64(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(v.to_string()))
}

/// TIMESTAMP (elapsed time since the epoch) to a "%Y-%m-%d %H:%M:%S%.f" string.
/// Out-of-range values are returned as the raw microsecond value in a string.
fn timestamp_to_json(unit: duckdb::types::TimeUnit, v: i64) -> serde_json::Value {
    let micros = unit.to_micros(v);
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(dt) => serde_json::Value::String(
            dt.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        ),
        None => serde_json::Value::String(format!("{micros}us")),
    }
}

/// DATE (days since the epoch) to a "%Y-%m-%d" string.
fn date_to_json(days: i32) -> serde_json::Value {
    let base = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let date = if days >= 0 {
        base.checked_add_days(chrono::Days::new(days as u64))
    } else {
        base.checked_sub_days(chrono::Days::new(days.unsigned_abs() as u64))
    };
    match date {
        Some(d) => serde_json::Value::String(d.format("%Y-%m-%d").to_string()),
        None => serde_json::Value::String(format!("{days} days")),
    }
}

/// TIME (elapsed time since midnight) to a "%H:%M:%S%.f" string.
fn time_to_json(unit: duckdb::types::TimeUnit, v: i64) -> serde_json::Value {
    let micros = unit.to_micros(v);
    let secs = (micros / 1_000_000) as u32;
    let nanos = ((micros % 1_000_000) * 1000) as u32;
    match chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos) {
        Some(t) => serde_json::Value::String(t.format("%H:%M:%S%.f").to_string()),
        None => serde_json::Value::String(format!("{micros}us")),
    }
}

/// Converts a duckdb value to JSON.
/// - Integers beyond 64 bits (outside BIGINT's safe range / HUGEINT / UBIGINT) and DECIMAL are
///   strings to preserve precision
/// - LIST / STRUCT / MAP / ARRAY are converted to JSON, but the element count is cut off at
///   MAX_COLLECTION_ELEMENTS and `truncated` is set when that happens
fn value_to_json_limited(value: Value, truncated: &mut bool) -> serde_json::Value {
    // The element limit is a budget shared across the whole cell: with an independent limit per
    // level, a 1,000 x 1,000 nesting would serialize 1 million values
    let mut budget = MAX_COLLECTION_ELEMENTS;
    value_to_json_at_depth(value, truncated, 0, &mut budget)
}

/// Cuts a string off at the character limit (if exceeded, sets truncated and appends an ellipsis).
fn text_to_json_limited(v: String, truncated: &mut bool) -> serde_json::Value {
    if v.chars().count() <= MAX_TEXT_CHARS {
        return serde_json::Value::String(v);
    }
    *truncated = true;
    let cut: String = v.chars().take(MAX_TEXT_CHARS).collect();
    serde_json::Value::String(format!("{cut}…"))
}

fn value_to_json_at_depth(
    value: Value,
    truncated: &mut bool,
    depth: usize,
    budget: &mut usize,
) -> serde_json::Value {
    // Do not overflow the stack on nesting of arbitrary depth derived from data (read_json_auto etc.)
    if depth >= MAX_NESTING_DEPTH
        && matches!(
            value,
            Value::List(_) | Value::Array(_) | Value::Struct(_) | Value::Map(_) | Value::Union(_)
        )
    {
        *truncated = true;
        return serde_json::Value::String("… (nesting too deep, truncated)".to_string());
    }
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(v) => serde_json::Value::Bool(v),
        Value::TinyInt(v) => serde_json::json!(v),
        Value::SmallInt(v) => serde_json::json!(v),
        Value::Int(v) => serde_json::json!(v),
        Value::BigInt(v) => json_i64(v),
        Value::HugeInt(v) => json_i128(v),
        Value::UTinyInt(v) => serde_json::json!(v),
        Value::USmallInt(v) => serde_json::json!(v),
        Value::UInt(v) => serde_json::json!(v),
        Value::UBigInt(v) => json_u64(v),
        Value::Float(v) => json_f64(v as f64),
        Value::Double(v) => json_f64(v),
        Value::Decimal(v) => serde_json::Value::String(v.to_string()),
        Value::Timestamp(unit, v) => timestamp_to_json(unit, v),
        Value::Text(v) => text_to_json_limited(v, truncated),
        Value::Blob(v) => {
            // For BLOB, convert only the head and apply the limit (bytes_to_json yields a
            // string if it is UTF-8, otherwise base64)
            if v.len() > MAX_TEXT_CHARS {
                *truncated = true;
                let head = bytes_to_json(v[..MAX_TEXT_CHARS].to_vec());
                match head {
                    serde_json::Value::String(s) => {
                        serde_json::Value::String(format!("{s}…"))
                    }
                    other => other,
                }
            } else {
                bytes_to_json(v)
            }
        }
        Value::Date32(v) => date_to_json(v),
        Value::Time64(unit, v) => time_to_json(unit, v),
        Value::Interval {
            months,
            days,
            nanos,
        } => serde_json::Value::String(format!(
            "{months} months {days} days {} seconds",
            nanos as f64 / 1_000_000_000.0
        )),
        Value::List(items) | Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(value_to_json_at_depth(item, truncated, depth + 1, budget));
            }
            serde_json::Value::Array(out)
        }
        Value::Enum(v) => serde_json::Value::String(v),
        Value::Struct(map) => {
            let mut obj = serde_json::Map::new();
            for (key, value) in map.iter() {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                obj.insert(
                    key.clone(),
                    value_to_json_at_depth(value.clone(), truncated, depth + 1, budget),
                );
            }
            serde_json::Value::Object(obj)
        }
        Value::Map(map) => {
            let mut obj = serde_json::Map::new();
            for (key, value) in map.iter() {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                obj.insert(
                    map_key_to_string(key),
                    value_to_json_at_depth(value.clone(), truncated, depth + 1, budget),
                );
            }
            serde_json::Value::Object(obj)
        }
        Value::Union(inner) => value_to_json_at_depth(*inner, truncated, depth + 1, budget),
    }
}

/// Converts a MAP key into a JSON object key string.
/// String keys are used as is; anything else becomes the string of its JSON representation.
fn map_key_to_string(key: &Value) -> String {
    let mut ignored = false;
    match value_to_json_limited(key.clone(), &mut ignored) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Runs a SELECT with parameter binding on a blocking thread and
/// returns all rows as Value (only for the small catalog queries used by schema_info).
async fn query_rows(
    handle: &DuckDbHandle,
    sql: &'static str,
    params: Vec<String>,
) -> Result<Vec<Vec<Value>>, AppError> {
    // Take part in the same serialization as query execution: without this, while a catalog query
    // holds conn a user query could take exec and register, and that query's
    // cancel (interrupt) would take down the running catalog statement
    let _exec = handle.exec.lock().await;
    let conn = handle.conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|_| {
            AppError::DuckDb("The DuckDB connection is poisoned".into())
        })?;
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(duckdb::params_from_iter(params.iter().map(String::as_str)))?;
        let column_count = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut values = Vec::with_capacity(column_count);
            for i in 0..column_count {
                values.push(row.get::<_, Value>(i)?);
            }
            out.push(values);
        }
        Ok(out)
    })
    .await
    .map_err(|e| AppError::DuckDb(format!("DuckDB task failed: {e}")))?
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Enum(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Builds a qualified name that can be embedded in SQL. The DuckDB default schema, main, is not
/// qualified (same treatment as public in schema_info::build_qualified_name).
fn qualified_name(schema: &str, name: &str) -> String {
    if schema == "main" {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// Splits a qualified name (schema.table or table) into (schema, table).
/// An unqualified name is treated as being in the default schema, main.
fn split_qualified(table: &str) -> (String, String) {
    match table.split_once('.') {
        Some((schema, name)) => (schema.to_string(), name.to_string()),
        None => ("main".to_string(), table.to_string()),
    }
}

/// List of tables / views (for the TABLES pane of the schema browser).
pub async fn fetch_tables(handle: &DuckDbHandle) -> Result<Vec<TableInfo>, AppError> {
    let rows = query_rows(
        handle,
        "SELECT table_schema, table_name, table_type \
         FROM information_schema.tables \
         ORDER BY table_schema, table_name",
        vec![],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let schema = value_text(row.first());
            let name = value_text(row.get(1));
            let table_type = value_text(row.get(2));
            let kind = if table_type.eq_ignore_ascii_case("VIEW") {
                "view"
            } else {
                "table"
            };
            TableInfo {
                qualified_name: qualified_name(&schema, &name),
                name,
                schema: Some(schema),
                kind: kind.to_string(),
            }
        })
        .collect())
}

/// List of columns of a table. The table name is bound, so it is not embedded in the SQL.
pub async fn fetch_columns(
    handle: &DuckDbHandle,
    table: &str,
) -> Result<Vec<ColumnInfo>, AppError> {
    let (schema, name) = split_qualified(table);
    let rows = query_rows(
        handle,
        "SELECT column_name, data_type, is_nullable \
         FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ? \
         ORDER BY ordinal_position",
        vec![schema, name],
    )
    .await?;
    let columns: Vec<ColumnInfo> = rows
        .iter()
        .map(|row| ColumnInfo {
            name: value_text(row.first()),
            data_type: value_text(row.get(1)),
            nullable: value_text(row.get(2)).eq_ignore_ascii_case("YES"),
        })
        .collect();
    // A nonexistent table yields an empty result, so make it an explicit error
    // (same treatment as MySQL / SQLite in schema_info)
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// Column names that make up a table's primary key.
/// Cell editing is not supported (supports_editable_cells = false), so this has no real use,
/// but it returns what can be obtained from duckdb_constraints().
pub async fn fetch_primary_keys(
    handle: &DuckDbHandle,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let (schema, name) = split_qualified(table);
    let rows = query_rows(
        handle,
        "SELECT unnest(constraint_column_names) \
         FROM duckdb_constraints() \
         WHERE constraint_type = 'PRIMARY KEY' \
           AND schema_name = ? AND table_name = ?",
        vec![schema, name],
    )
    .await?;
    Ok(rows.iter().map(|row| value_text(row.first())).collect())
}

/// All columns of all tables (for the schema map used by SQL completion).
pub async fn fetch_all_columns(
    handle: &DuckDbHandle,
) -> Result<std::collections::BTreeMap<String, Vec<ColumnInfo>>, AppError> {
    let rows = query_rows(
        handle,
        "SELECT table_schema, table_name, column_name, data_type, is_nullable \
         FROM information_schema.columns \
         ORDER BY table_schema, table_name, ordinal_position",
        vec![],
    )
    .await?;
    let mut map: std::collections::BTreeMap<String, Vec<ColumnInfo>> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let schema = value_text(row.first());
        let name = value_text(row.get(1));
        map.entry(qualified_name(&schema, &name))
            .or_default()
            .push(ColumnInfo {
                name: value_text(row.get(2)),
                data_type: value_text(row.get(3)),
                nullable: value_text(row.get(4)).eq_ignore_ascii_case("YES"),
            });
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a DuckDB file for tests and returns a connection handle.
    /// (_dir deletes the file on drop, so the caller must keep it alive)
    async fn test_handle() -> (tempfile::TempDir, DuckDbHandle) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.duckdb");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL, \
                     score DOUBLE, \
                     tags TEXT[], \
                     created_at TIMESTAMP\
                 );\
                 INSERT INTO users VALUES \
                   (1, 'alice', 12.5, ['a', 'b'], TIMESTAMP '2026-01-02 03:04:05'), \
                   (2, 'bob', NULL, [], NULL), \
                   (3, 'carol', 99.0, NULL, NULL);\
                 CREATE VIEW user_names AS SELECT name FROM users;",
            )
            .unwrap();
        }
        let server = ServerConfig {
            name: "duck-test".into(),
            engine: "duckdb".into(),
            schema: Some(path.to_string_lossy().into_owned()),
            ..test_server_config()
        };
        let handle = connect(&server).await.unwrap();
        (dir, handle)
    }

    fn test_server_config() -> ServerConfig {
        serde_yaml::from_str("{name: x, engine: duckdb}").unwrap()
    }

    async fn run(
        handle: &DuckDbHandle,
        sql: &str,
        max_rows: usize,
        auto_limit: Option<u64>,
        readonly: ReadonlyGuard,
        allow_dangerous: bool,
    ) -> Result<QueryResult, AppError> {
        let registry = CancelRegistry::default();
        run_query_cancellable(
            handle,
            &registry,
            "duck-test",
            sql,
            max_rows,
            auto_limit,
            readonly,
            allow_dangerous,
        )
        .await
    }

    /// The agent path (ReadonlyGuard::Agent) runs in a read-only transaction.
    /// The purpose is to have the DB itself reject side-effecting SELECTs
    /// (`SELECT nextval(...)`) that slip through the statement-level guard.
    #[tokio::test]
    async fn test_agent_guard_blocks_side_effecting_select() {
        let (_dir, handle) = test_handle().await;
        run(&handle, "CREATE SEQUENCE s", 10, None, ReadonlyGuard::Off, false)
            .await
            .unwrap();

        // The statement-level guard lets SELECT through (it only looks at the leading keyword)
        assert!(crate::db::agent_rejection_reason(
            "SELECT nextval('s')",
            Engine::DuckDb
        )
        .is_none());

        // The DB-level read-only mode rejects the write
        let err = run(
            &handle,
            "SELECT nextval('s')",
            10,
            None,
            ReadonlyGuard::Agent,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("read-only"), "unexpected error: {err}");

        // Normal reads pass, and the connection stays healthy after the rollback
        let result = run(&handle, "SELECT 1", 10, None, ReadonlyGuard::Agent, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        // The Writable path executes as before (this also confirms that no transaction
        // is left open)
        let result = run(
            &handle,
            "SELECT nextval('s')",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);
    }

    #[tokio::test]
    async fn test_connect_rejects_missing_file() {
        let server = ServerConfig {
            schema: Some("/nonexistent/path/to/missing.duckdb".into()),
            ..test_server_config()
        };
        let err = connect(&server).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");

        // A missing path is also an error
        let server = test_server_config();
        let err = connect(&server).await.unwrap_err();
        assert!(err.to_string().contains("set schema"), "{err}");
    }

    #[tokio::test]
    async fn test_select_rows_and_types() {
        let (_dir, handle) = test_handle().await;
        let result = run(
            &handle,
            "SELECT id, name, score, tags, created_at FROM users ORDER BY id",
            100,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            result.columns,
            vec!["id", "name", "score", "tags", "created_at"]
        );
        assert_eq!(result.row_count, 3);
        assert!(!result.truncated);
        assert_eq!(result.rows[0][0], serde_json::json!(1));
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
        assert_eq!(result.rows[0][2], serde_json::json!(12.5));
        assert_eq!(result.rows[0][3], serde_json::json!(["a", "b"]));
        assert_eq!(
            result.rows[0][4],
            serde_json::json!("2026-01-02 03:04:05")
        );
        // NULL becomes JSON null
        assert_eq!(result.rows[1][2], serde_json::Value::Null);
        assert_eq!(result.rows[1][4], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_value_conversion_extremes() {
        let (_dir, handle) = test_handle().await;
        let result = run(
            &handle,
            "SELECT 170141183460469231731687303715884105727::HUGEINT AS huge, \
                    9007199254740993::BIGINT AS big, \
                    123::BIGINT AS small_big, \
                    18446744073709551615::UBIGINT AS ubig, \
                    1.5::DECIMAL(10, 2) AS dec, \
                    DATE '2026-07-25' AS d, \
                    TIME '12:34:56.789' AS t, \
                    '\\xC3\\x28'::BLOB AS b, \
                    {'a': 1, 'b': 'x'} AS st, \
                    MAP {'k': 42} AS m",
            10,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        let row = &result.rows[0];
        // Integers above 2^53 are stringified (guards against rounding at the invoke boundary)
        assert_eq!(
            row[0],
            serde_json::json!("170141183460469231731687303715884105727")
        );
        assert_eq!(row[1], serde_json::json!("9007199254740993"));
        assert_eq!(row[2], serde_json::json!(123));
        assert_eq!(row[3], serde_json::json!("18446744073709551615"));
        assert_eq!(row[4], serde_json::json!("1.50"));
        assert_eq!(row[5], serde_json::json!("2026-07-25"));
        assert_eq!(row[6], serde_json::json!("12:34:56.789"));
        // A BLOB with invalid UTF-8 is converted to base64
        assert!(row[7].as_str().unwrap().starts_with("base64:"));
        assert_eq!(row[8], serde_json::json!({"a": 1, "b": "x"}));
        assert_eq!(row[9], serde_json::json!({"k": 42}));
    }

    #[tokio::test]
    async fn test_insert_update_delete_roundtrip() {
        let (_dir, handle) = test_handle().await;
        let result = run(
            &handle,
            "INSERT INTO users VALUES (4, 'dave', 1.0, NULL, NULL)",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.affected_rows, Some(1));

        let result = run(
            &handle,
            "UPDATE users SET score = 2.0 WHERE id = 4",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.affected_rows, Some(1));

        // RETURNING comes back as rows
        let result = run(
            &handle,
            "DELETE FROM users WHERE id = 4 RETURNING name",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.rows, vec![vec![serde_json::json!("dave")]]);

        let result = run(
            &handle,
            "SELECT count(*) FROM users",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(3));
    }

    #[tokio::test]
    async fn test_readonly_guard() {
        let (_dir, handle) = test_handle().await;
        for sql in [
            "INSERT INTO users VALUES (9, 'x', 0, NULL, NULL)",
            "UPDATE users SET name = 'x' WHERE id = 1",
            "DROP TABLE users",
            "CREATE TABLE t (id INTEGER)",
        ] {
            let err = run(&handle, sql, 10, None, ReadonlyGuard::Switch, true)
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Readonly(_)), "{sql}: {err}");
        }
        // Reads pass
        assert!(run(&handle, "SELECT 1", 10, None, ReadonlyGuard::Switch, false)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_dangerous_guard() {
        let (_dir, handle) = test_handle().await;
        for sql in [
            "DELETE FROM users",
            "UPDATE users SET name = 'x'",
            "DROP TABLE users",
            "TRUNCATE users",
        ] {
            let err = run(&handle, sql, 10, None, ReadonlyGuard::Off, false)
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::Dangerous(_)), "{sql}: {err}");
        }
        // With WHERE it passes (rows get deleted, but it is not judged dangerous)
        assert!(run(
            &handle,
            "DELETE FROM users WHERE id = 999",
            10,
            None,
            ReadonlyGuard::Off,
            false
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn test_auto_limit_and_truncation() {
        let (_dir, handle) = test_handle().await;
        // auto LIMIT is added
        let result = run(
            &handle,
            "SELECT * FROM range(100)",
            1000,
            Some(2),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_limit, Some(2));
        assert_eq!(result.row_count, 2);

        // Not added when LIMIT is already specified
        let result = run(
            &handle,
            "SELECT * FROM range(100) LIMIT 5",
            1000,
            Some(2),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_limit, None);
        assert_eq!(result.row_count, 5);

        // Exceeding max_rows is cut off and truncated is set
        let result = run(
            &handle,
            "SELECT * FROM range(100)",
            10,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 10);
        assert!(result.truncated);

        // auto LIMIT is also added to FROM-first syntax (DuckDB-specific)
        let result = run(
            &handle,
            "FROM range(100)",
            1000,
            Some(3),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_limit, Some(3));
        assert_eq!(result.row_count, 3);

        // Not added to FROM-first either when LIMIT is already specified
        let result = run(
            &handle,
            "FROM range(100) LIMIT 4",
            1000,
            Some(3),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_limit, None);
        assert_eq!(result.row_count, 4);
    }

    #[tokio::test]
    async fn test_collection_truncation() {
        let (_dir, handle) = test_handle().await;
        // A LIST within one cell is also cut off at the element limit and truncated is set
        let result = run(
            &handle,
            "SELECT range(3000) AS xs",
            10,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert!(result.truncated);
        assert_eq!(
            result.rows[0][0].as_array().unwrap().len(),
            MAX_COLLECTION_ELEMENTS
        );
    }

    #[test]
    fn test_text_and_blob_truncation() {
        // TEXT is cut off at the character limit + truncated
        let mut truncated = false;
        let long = "x".repeat(MAX_TEXT_CHARS + 5);
        let v = value_to_json_limited(Value::Text(long), &mut truncated);
        assert!(truncated);
        let s = v.as_str().unwrap();
        assert_eq!(s.chars().count(), MAX_TEXT_CHARS + 1); // +1 is the ellipsis
        assert!(s.ends_with('…'));

        // Within the limit it is kept as is
        let mut truncated = false;
        let v = value_to_json_limited(Value::Text("hello".into()), &mut truncated);
        assert_eq!(v, serde_json::json!("hello"));
        assert!(!truncated);

        // BLOB is also cut off at the limit
        let mut truncated = false;
        let v = value_to_json_limited(
            Value::Blob(vec![b'a'; MAX_TEXT_CHARS + 10]),
            &mut truncated,
        );
        assert!(truncated);
        assert!(v.as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn test_collection_budget_is_shared_across_nesting() {
        // Even a 1,000 x 2 nesting is cut off by the total (budget)
        let inner: Vec<Value> = (0..600).map(Value::Int).collect();
        let value = Value::List(vec![
            Value::List(inner.clone()),
            Value::List(inner),
        ]);
        let mut truncated = false;
        let v = value_to_json_limited(value, &mut truncated);
        assert!(truncated);
        // The total number of serialized values does not greatly exceed the budget (1,000)
        fn count(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Array(items) => {
                    1 + items.iter().map(count).sum::<usize>()
                }
                serde_json::Value::Object(map) => {
                    1 + map.values().map(count).sum::<usize>()
                }
                _ => 1,
            }
        }
        assert!(count(&v) <= MAX_COLLECTION_ELEMENTS + 10);
    }

    #[test]
    fn test_is_duckdb_readonly_statement() {
        assert!(is_duckdb_readonly_statement("FROM books"));
        assert!(is_duckdb_readonly_statement("from books SELECT title"));
        assert!(is_duckdb_readonly_statement("SUMMARIZE books"));
        assert!(is_duckdb_readonly_statement("PIVOT sales ON month"));
        assert!(is_duckdb_readonly_statement("UNPIVOT t ON a, b"));
        assert!(!is_duckdb_readonly_statement("SELECT 1"));
        assert!(!is_duckdb_readonly_statement("INSERT INTO t VALUES (1)"));
        assert!(!is_duckdb_readonly_statement("DELETE FROM t"));
    }

    #[test]
    fn test_nesting_depth_cap() {
        // Nesting beyond MAX_NESTING_DEPTH is replaced by a placeholder,
        // without overflowing the stack
        let mut value = Value::Int(1);
        for _ in 0..(MAX_NESTING_DEPTH + 10) {
            value = Value::List(vec![value]);
        }
        let mut truncated = false;
        let v = value_to_json_limited(value, &mut truncated);
        assert!(truncated);
        // The truncation placeholder appears at some depth
        let text = v.to_string();
        assert!(text.contains("nesting too deep"));
    }

    #[tokio::test]
    async fn test_meta_commands() {
        let (_dir, handle) = test_handle().await;
        // \dt: base tables only
        let result = run(&handle, "\\dt", 100, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        let names: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert!(names.contains(&"users"));
        assert!(!names.contains(&"user_names"));

        // \dv: views only
        let result = run(&handle, "\\dv", 100, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        let names: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert!(names.contains(&"user_names"));
        assert!(!names.contains(&"users"));

        // \d <table>: column definitions
        let result = run(&handle, "\\d users", 100, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        let columns: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(columns, vec!["id", "name", "score", "tags", "created_at"]);

        // \c is an error
        let err = run(&handle, "\\c other", 100, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[tokio::test]
    async fn test_cancel_interrupts_running_query() {
        let (_dir, handle) = test_handle().await;
        let registry = Arc::new(CancelRegistry::default());
        let handle2 = handle.clone();
        let registry2 = registry.clone();
        // Start an aggregate that takes tens of seconds in the background
        let task = tokio::spawn(async move {
            run_query_cancellable(
                &handle2,
                &registry2,
                "duck-test",
                // count(*) can be short-circuited by the optimizer into a cardinality computation,
                // so use an aggregate that does real work (prevents flakiness in the cancel test)
                "SELECT sum(a.range * b.range) FROM range(200000000) a, range(1000) b",
                10,
                None,
                ReadonlyGuard::Switch,
                false,
            )
            .await
        });
        // Wait for the execution to be registered, then cancel
        let mut cancelled = false;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if registry.cancel("duck-test").await.unwrap() {
                cancelled = true;
                break;
            }
        }
        assert!(cancelled, "the query was never registered");
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("the query did not stop after the interrupt")
            .unwrap();
        assert!(matches!(result, Err(AppError::Cancelled)), "{result:?}");

        // The next query can run on the same connection after the cancel
        let result = run(&handle, "SELECT 1", 10, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(1));
    }

    /// An integration test standing in for GUI E2E. It runs the public db.rs path that the frontend's
    /// Tauri commands use (DbManager::get_pool -> delegation of db::run_query_cancellable ->
    /// list_schemas / build_explain_sql) end to end against a DB file with real data
    /// (a self-contained fixture generated on the Rust side in a tempfile).
    #[tokio::test]
    async fn test_integration_via_db_manager() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e2e.duckdb");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE books (\
                     id INTEGER PRIMARY KEY, \
                     title TEXT NOT NULL, \
                     rating DOUBLE, \
                     tags TEXT[], \
                     published_on DATE\
                 );\
                 INSERT INTO books VALUES \
                   (1, 'Dune', 4.3, ['sf', 'classic'], DATE '1965-08-01'), \
                   (2, 'Project Hail Mary', 4.6, ['sf'], DATE '2021-05-04');\
                 CREATE TABLE sales (day DATE, amount BIGINT);\
                 INSERT INTO sales \
                   SELECT DATE '2026-01-01' + INTERVAL (i) DAY, i * 100 \
                   FROM range(600) t(i);",
            )
            .unwrap();
        }
        let server = ServerConfig {
            name: "e2e-duckdb".into(),
            schema: Some(path.to_string_lossy().into_owned()),
            ..test_server_config()
        };
        let manager = crate::db::DbManager::default();
        let registry = CancelRegistry::default();
        let pool = manager.get_pool(&server).await.unwrap();

        // (a) SELECT: row fetching and type conversion (INTEGER / TEXT / DOUBLE / LIST / DATE)
        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "SELECT id, title, rating, tags, published_on FROM books ORDER BY id",
            1000,
            Some(500),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            result.columns,
            vec!["id", "title", "rating", "tags", "published_on"]
        );
        assert_eq!(
            result.rows[0],
            vec![
                serde_json::json!(1),
                serde_json::json!("Dune"),
                serde_json::json!(4.3),
                serde_json::json!(["sf", "classic"]),
                serde_json::json!("1965-08-01"),
            ]
        );
        // No LIMIT was specified, so the default LIMIT has been added
        assert_eq!(result.applied_limit, Some(500));

        // (b) auto LIMIT: default 500 on a 600-row table -> stops at 500 rows
        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "SELECT * FROM sales",
            1000,
            Some(500),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_limit, Some(500));
        assert_eq!(result.row_count, 500);
        assert!(!result.truncated);

        // (c) readonly guard: INSERT is rejected with Writable OFF (Switch)
        let err = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "INSERT INTO books VALUES (9, 'x', 0, NULL, NULL)",
            1000,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Readonly(_)), "{err}");
        assert!(err.to_string().contains("Writable"), "{err}");

        // The dangerous-statement guard also works on the same path
        let err = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "DELETE FROM books",
            1000,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Dangerous(_)), "{err}");

        // (d) meta commands: \dt (table list) / \d books (column definitions)
        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "\\dt",
            1000,
            Some(500),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        let names: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["books", "sales"]);
        // auto LIMIT is not added to SQL after meta command conversion
        assert_eq!(result.applied_limit, None);

        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "\\d books",
            1000,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        let columns: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(
            columns,
            vec!["id", "title", "rating", "tags", "published_on"]
        );

        // (e) max_rows cut-off + truncated
        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            "SELECT * FROM sales LIMIT 600",
            100,
            Some(500),
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 100);
        assert!(result.truncated);

        // EXPLAIN: the prefix is EXPLAIN (not ANALYZE), and it runs and returns rows
        let explain_sql =
            crate::db::build_explain_sql("duckdb", "SELECT * FROM books").unwrap();
        assert!(explain_sql.starts_with("EXPLAIN\n"), "{explain_sql}");
        let result = crate::db::run_query_cancellable(
            &pool,
            &registry,
            "e2e-duckdb",
            &explain_sql,
            1000,
            None,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert!(result.row_count > 0);

        // list_schemas returns the configured file path as one entry (for the Database display)
        let schemas = crate::db::list_schemas(&pool, &server).await.unwrap();
        assert_eq!(schemas, vec![path.to_string_lossy().into_owned()]);

        manager.disconnect("e2e-duckdb").await;
    }

    #[tokio::test]
    async fn test_schema_info() {
        let (_dir, handle) = test_handle().await;
        let tables = fetch_tables(&handle).await.unwrap();
        let names: Vec<(&str, &str)> = tables
            .iter()
            .map(|t| (t.qualified_name.as_str(), t.kind.as_str()))
            .collect();
        assert!(names.contains(&("users", "table")));
        assert!(names.contains(&("user_names", "view")));
        // The main schema is not qualified
        assert!(tables.iter().all(|t| !t.qualified_name.contains('.')));

        let columns = fetch_columns(&handle, "users").await.unwrap();
        let summary: Vec<(&str, bool)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.nullable))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("id", false),
                ("name", false),
                ("score", true),
                ("tags", true),
                ("created_at", true),
            ]
        );

        // A nonexistent table is an error
        let err = fetch_columns(&handle, "missing_table").await.unwrap_err();
        assert!(err.to_string().contains("Table not found"), "{err}");

        let keys = fetch_primary_keys(&handle, "users").await.unwrap();
        assert_eq!(keys, vec!["id"]);

        let map = fetch_all_columns(&handle).await.unwrap();
        assert!(map.contains_key("users"));
        assert!(map.contains_key("user_names"));
        assert_eq!(map["users"].len(), 5);
    }
}
