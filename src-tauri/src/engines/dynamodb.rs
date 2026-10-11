//! DynamoDB engine.
//!
//! Executes PartiQL (a SQL-compatible subset) via the ExecuteStatement API.
//! The editor uses plain SQL (editor_language "sql" / .sql extension), and the
//! readonly / dangerous guards reuse the SQL logic in db.rs as is
//! (PartiQL has only SELECT / INSERT / UPDATE / DELETE, so scan_sql's standard-SQL
//! dialect is sufficient. scan_sql blanks double-quoted identifiers as strings,
//! but keywords such as WHERE are never quoted, so detection is not affected).
//!
//! - Connection: `schema` = AWS region (required). `host` / `port` override the
//!   endpoint for dynamodb-local etc. (the standard AWS endpoint when omitted).
//!   Credentials are resolved in the order user / password (static access key) ->
//!   `aws_profile` -> the default credentials chain.
//! - PartiQL has no LIMIT clause, so no auto LIMIT is added. Instead, the
//!   ExecuteStatement `limit` parameter + NextToken pagination fetches up to
//!   max_rows + 1 items, stops there, and reports truncated.
//! - INSERT / UPDATE / DELETE cannot get the affected row count from the API, so
//!   they return affected_rows = None + an empty result (statements that return
//!   Items, such as `UPDATE ... RETURNING ALL OLD *`, are shown as a table as is).
//! - Every request gets the SDK timeouts (connect 15 s / operation 120 s), and the
//!   whole pagination also gets a 120 s deadline (a heavily filtered SELECT can
//!   keep scanning empty pages indefinitely).
//! - Cancellation aborts the future on the client side (`CancelTarget::ClientSide`).
//!   There is no connection pool, so aborting leaves no broken state.
//! - The HTTPS client explicitly uses the same ring-based rustls as the existing
//!   dependencies (the SDK default aws-lc would add a native build (cmake / NASM) to CI).

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use aws_sdk_dynamodb::types::{AttributeValue, KeyType};

use crate::config::ServerConfig;
use crate::db::{
    bytes_to_json, dangerous_block_error, dangerous_reason, is_readonly_allowed,
    json_i64, leading_keyword, readonly_block_error, CancelRegistry, CancelTarget,
    Engine, QueryResult, ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

/// Timeout for connecting (TCP) and for the connectivity check (ListTables).
/// A check request without a timeout would block get_pool (while DbManager holds
/// its lock) indefinitely, so this is required.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Timeout for a single API operation (the SDK operation timeout).
/// The same value is used for the deadline of the whole pagination.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// When the endpoint is overridden (host given) and port is omitted, use the
/// default dynamodb-local port.
const DEFAULT_LOCAL_PORT: u16 = 8000;

/// Upper limit on the number of tables shown in the TABLES pane (never build an unbounded list).
const MAX_TABLES: usize = 5000;

/// Number of items fetched per ListTables page (the API limit is 100).
const LIST_TABLES_PAGE: i32 = 100;

/// Shared budget for the number of collection (L / M / SS / NS / BS) elements in
/// one cell. It is shared across the whole cell rather than being an independent
/// limit per level (so a 1,000 x 1,000 nest does not serialize 1 million values).
const MAX_CELL_ELEMENTS: usize = 1000;

/// Upper limit on the number of columns in a result table. DynamoDB is schemaless
/// and each item has its own attribute set, so the union over sparse items can
/// blow up the column count (1,000 rows x 1,000 attributes would fill 1 million
/// cells with NULL). Truncate at this count from the front in name order and
/// report truncated.
const MAX_COLUMNS: usize = 500;

/// Upper limit on the character count of a string (S / B) in one cell. Excess is truncated + truncated flag.
const MAX_TEXT_CHARS: usize = 10_000;

/// Recursion depth limit when converting nested values (L / M) to JSON (stack protection).
const MAX_NESTING_DEPTH: usize = 32;

/// Client connected to DynamoDB. Held as DbPool::DynamoDb.
/// The SDK client internally holds an HTTP connection pool.
#[derive(Clone)]
pub struct DynamoClient {
    client: aws_sdk_dynamodb::Client,
}

impl std::fmt::Debug for DynamoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DynamoClient")
    }
}

/// Convert an SDK error into the app's error type. DisplayErrorContext expands the
/// error chain (service error kind and message). SDK errors contain no credentials
/// or signatures, so they can be shown as is.
fn sdk_error(context: &str, e: impl std::error::Error) -> AppError {
    AppError::DynamoDb(format!(
        "{context}: {}",
        aws_sdk_dynamodb::error::DisplayErrorContext(&e)
    ))
}

/// Build a ring-based rustls HTTPS client.
/// The SDK default aws-lc (aws-lc-sys) needs cmake / NASM for its native build,
/// which is a build risk for CI (macOS universal / Windows), so explicitly use ring,
/// the same as the existing dependencies (reqwest / rustls).
/// (The SharedHttpClient type uses the re-export from the SDK's config module, so
/// we do not add a direct dependency on aws-smithy-runtime-api)
fn build_http_client() -> aws_sdk_dynamodb::config::SharedHttpClient {
    aws_smithy_http_client::Builder::new()
        .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
            aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https()
}

/// Build the SDK client from the config (no connectivity check).
async fn build_client(server: &ServerConfig) -> Result<DynamoClient, AppError> {
    // schema = AWS region (required). Even dynamodb-local needs a region name for
    // SigV4 signing, so omitting it is a config error
    let region = server
        .schema
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Config(
                "For dynamodb, set schema to the AWS region (e.g. ap-northeast-1)"
                    .into(),
            )
        })?;

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .http_client(build_http_client())
        .timeout_config(
            aws_config::timeout::TimeoutConfig::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .operation_timeout(REQUEST_TIMEOUT)
                .build(),
        );

    // Credential resolution order: user / password (static access key ID / secret) ->
    // aws_profile (a profile in ~/.aws) -> the default credentials chain
    // (environment variables -> default profile -> IMDS).
    // user / password is a queryfolio-specific extension that can be written with the
    // same keys as other engines (dummy values are fine for dynamodb-local).
    let user = server.user.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let password = server.password.as_deref().filter(|s| !s.is_empty());
    match (user, password) {
        (Some(access_key), Some(secret_key)) => {
            loader = loader.credentials_provider(
                aws_sdk_dynamodb::config::Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "queryfolio-config",
                ),
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(AppError::Config(
                "For dynamodb, set both user (access key ID) and password \
                 (secret access key), or neither"
                    .into(),
            ));
        }
        (None, None) => {
            if let Some(profile) = server
                .aws_profile
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                // profile_name alone only changes where the default chain looks, and the
                // environment variable provider (AWS_ACCESS_KEY_ID etc.) still wins first.
                // When aws_profile is given, set up an explicit profile provider so that the
                // priority "user/password -> aws_profile -> default chain" holds regardless of
                // the environment
                loader = loader.credentials_provider(
                    aws_config::profile::ProfileFileCredentialsProvider::builder()
                        .profile_name(profile)
                        .build(),
                );
            }
        }
    }

    // host / port override the endpoint (for dynamodb-local).
    // tls: true uses https (http when omitted. For the standard AWS endpoint, the SDK
    // resolves it over https as long as host is not written)
    if let Some(host) = server
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let scheme = if server.tls { "https" } else { "http" };
        let port = server.port.unwrap_or(DEFAULT_LOCAL_PORT);
        loader = loader.endpoint_url(format!("{scheme}://{host}:{port}"));
    }

    let sdk_config = loader.load().await;
    Ok(DynamoClient {
        client: aws_sdk_dynamodb::Client::new(&sdk_config),
    })
}

/// Establish the connection and go as far as the connectivity check (ListTables limit 1).
/// The check request also gets a timeout: so that get_pool (while DbManager holds
/// its lock) does not hang forever on a peer that accepts TCP but never responds.
pub async fn connect(server: &ServerConfig) -> Result<DynamoClient, AppError> {
    let client = build_client(server).await?;
    let confirm = client.client.list_tables().limit(1).send();
    match tokio::time::timeout(CONNECT_TIMEOUT, confirm).await {
        Ok(Ok(_)) => Ok(client),
        Ok(Err(e)) => {
            // With a least-privilege IAM policy (allowing only ExecuteStatement on specific
            // tables), ListTables gets AccessDenied. That state means "credentials and
            // reachability are correct but permission is missing", so the connection itself
            // is treated as successful (the permission error is shown again when the TABLES
            // pane is opened). Invalid credentials (UnrecognizedClient etc.) and network
            // errors are connection errors as before
            let text = format!("{:?}", e);
            if text.contains("AccessDenied") {
                return Ok(client);
            }
            Err(sdk_error("Failed to connect to DynamoDB", e))
        }
        Err(_) => Err(AppError::DynamoDb(format!(
            "DynamoDB did not respond within {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

/// Execute a PartiQL statement and return the result (cancellable version).
/// db::run_query_cancellable delegates here for DbPool::DynamoDb.
pub async fn run_query_cancellable(
    client: &DynamoClient,
    registry: &CancelRegistry,
    connection_name: &str,
    sql: &str,
    max_rows: usize,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    // psql-style meta commands (\...) are unsupported (translate returns an error for DynamoDb)
    crate::meta_commands::translate(Engine::DynamoDb, sql)?;

    if leading_keyword(sql).is_empty() {
        return Err(AppError::Config("The SQL statement is empty".into()));
    }

    // `tables` is a queryfolio-specific statement that returns the table list
    // (CYBERNEURA-DEV-406). PartiQL has no syntax equivalent to SHOW TABLES and
    // sending it to DynamoDB gives a syntax error, so it is handled here and routed
    // to ListTables. It is a pure read that does not go through ExecuteStatement, so it
    // may be handled before the readonly / dangerous guards (under the guards, the
    // SQL leading-keyword check would not treat it as a fetch statement and it would
    // be rejected on connections with Writable OFF).
    if is_tables_statement(sql) {
        return list_tables_query(client, registry, connection_name, max_rows).await;
    }

    // The readonly / dangerous guards apply the shared SQL logic to the whole text before execution.
    // PartiQL has only SELECT / INSERT / UPDATE / DELETE, so the detection can be used as is.
    // The guards only look at the first statement of a multi-statement input, so reject it
    // when the guard is enabled (PartiQL itself accepts only one statement, but the check
    // order is kept the same as the SQL engines)
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && crate::db::contains_multiple_statements(sql, Engine::DynamoDb)
    {
        return Err(crate::db::multi_statement_block_error());
    }
    if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, Engine::DynamoDb) {
        return Err(readonly_block_error(readonly));
    }
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, Engine::DynamoDb) {
            return Err(dangerous_block_error(reason));
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let guard = registry.register(
        connection_name,
        CancelTarget::ClientSide {
            notify: notify.clone(),
        },
        cancelled,
    );
    let started = Instant::now();
    // Cancellation aborts the execution future. biased checks the execution result first:
    // if the result and the cancel notification are ready at the same time, the completed result wins
    let result = tokio::select! {
        biased;
        result = execute_statement(client, sql, max_rows) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    // If cancellation races with completion, return the successful result as is (same behavior as the SQL side)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// Run ExecuteStatement and, for a SELECT, follow pages with NextToken and collect up to
/// max_rows + 1 items (shape_items truncates the excess and reports truncated).
async fn execute_statement(
    client: &DynamoClient,
    sql: &str,
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    // The limit parameter and pagination only matter for reads (SELECT).
    // Write statements (INSERT / UPDATE / DELETE) are single-item operations that finish in one call
    let is_select = leading_keyword(sql) == "select";
    let started = Instant::now();
    let deadline_error = || {
        AppError::DynamoDb(format!(
            "The query did not finish within {}s \
             (narrow the statement, e.g. with a key condition)",
            REQUEST_TIMEOUT.as_secs()
        ))
    };
    let mut items: Vec<HashMap<String, AttributeValue>> = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let mut req = client.client.execute_statement().statement(sql);
        if is_select {
            // limit is an upper bound on the "number of items evaluated". Use needed + 1 to
            // detect truncated (clamped to a value that fits in i32 in practice)
            let remaining = max_rows.saturating_add(1).saturating_sub(items.len());
            req = req.limit(remaining.min(i32::MAX as usize) as i32);
        }
        req = req.set_next_token(next_token.take());
        // A heavily filtered SELECT can keep scanning empty pages indefinitely, so put a
        // deadline on the whole pagination. Wrapping each page's request in a timeout of the
        // remaining time makes the deadline apply not only between pages but also while a
        // request is running (so we are not made to wait ~2x including the per-operation SDK timeout)
        let remaining_time = REQUEST_TIMEOUT
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(deadline_error)?;
        let out = tokio::time::timeout(remaining_time, req.send())
            .await
            .map_err(|_| deadline_error())?
            .map_err(|e| sdk_error("ExecuteStatement failed", e))?;
        items.extend(out.items.unwrap_or_default());
        next_token = out.next_token;
        if !is_select || next_token.is_none() || items.len() > max_rows {
            break;
        }
    }
    Ok(shape_items(items, max_rows))
}

/// Shape Items (a list of AttributeValue maps) into a table.
/// columns is the union of the keys of all items. The SDK Item is a HashMap with
/// undefined key order, so sort them to make the display deterministic.
/// For a write statement with empty Items, the result is empty (affected_rows is
/// None because the API cannot provide it).
fn shape_items(
    items: Vec<HashMap<String, AttributeValue>>,
    max_rows: usize,
) -> QueryResult {
    let mut truncated = items.len() > max_rows;
    let items = &items[..items.len().min(max_rows)];

    // Collect the column union in a BTreeSet while capping it at MAX_COLUMNS:
    // dropping the largest element whenever the limit is exceeded fixes "the first MAX_COLUMNS
    // names in ascending order" in O(N log MAX_COLUMNS) without materializing the whole union
    // (intermediate memory and scanning stay bounded even for sparse attribute sets from schemalessness)
    let mut column_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for item in items {
        for key in item.keys() {
            if column_set.contains(key) {
                continue;
            }
            column_set.insert(key.clone());
            if column_set.len() > MAX_COLUMNS {
                column_set.pop_last();
                truncated = true;
            }
        }
    }
    let columns: Vec<String> = column_set.into_iter().collect();

    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let mut row = Vec::with_capacity(columns.len());
        for column in &columns {
            match item.get(column) {
                Some(value) => row.push(attr_to_json_cell(value, &mut truncated)),
                // Schemaless, so attributes missing from an item are treated as NULL
                None => row.push(serde_json::Value::Null),
            }
        }
        rows.push(row);
    }

    QueryResult {
        row_count: rows.len(),
        columns,
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: 0,
        applied_limit: None,
        switched_schema: None,
    }
}

/// Convert an AttributeValue to JSON (with a per-cell shared budget).
fn attr_to_json_cell(value: &AttributeValue, truncated: &mut bool) -> serde_json::Value {
    let mut budget = MAX_CELL_ELEMENTS;
    attr_to_json(value, truncated, 0, &mut budget)
}

/// Convert N (number) to JSON. DynamoDB's N is arbitrary precision (up to 38 digits), so
/// only integers within the JS safe integer range become numbers; everything else
/// (decimals, huge integers) is returned as a string to keep precision (guards against
/// rounding at the invoke boundary).
fn number_to_json(n: &str) -> serde_json::Value {
    if !n.contains(['.', 'e', 'E']) {
        if let Ok(v) = n.parse::<i64>() {
            // json_i64 stringifies values outside the safe range
            return json_i64(v);
        }
    }
    serde_json::Value::String(n.to_string())
}

/// Truncate a string at the character limit (when exceeded, set truncated and append an ellipsis).
fn text_limited(v: &str, truncated: &mut bool) -> serde_json::Value {
    if v.chars().count() <= MAX_TEXT_CHARS {
        return serde_json::Value::String(v.to_string());
    }
    *truncated = true;
    let cut: String = v.chars().take(MAX_TEXT_CHARS).collect();
    serde_json::Value::String(format!("{cut}…"))
}

/// Convert binary (B / BS element) to JSON (bytes_to_json: a string if UTF-8,
/// otherwise base64). Only the head is converted, cut off at the size limit.
fn blob_limited(bytes: &[u8], truncated: &mut bool) -> serde_json::Value {
    if bytes.len() > MAX_TEXT_CHARS {
        *truncated = true;
        match bytes_to_json(bytes[..MAX_TEXT_CHARS].to_vec()) {
            serde_json::Value::String(s) => serde_json::Value::String(format!("{s}…")),
            other => other,
        }
    } else {
        bytes_to_json(bytes.to_vec())
    }
}

fn attr_to_json(
    value: &AttributeValue,
    truncated: &mut bool,
    depth: usize,
    budget: &mut usize,
) -> serde_json::Value {
    // Do not overflow the stack on arbitrary-depth nesting derived from data
    // (DynamoDB allows up to 32 levels)
    if depth >= MAX_NESTING_DEPTH
        && matches!(value, AttributeValue::L(_) | AttributeValue::M(_))
    {
        *truncated = true;
        return serde_json::Value::String("… (nesting too deep, truncated)".into());
    }
    match value {
        AttributeValue::Null(_) => serde_json::Value::Null,
        AttributeValue::Bool(v) => serde_json::Value::Bool(*v),
        AttributeValue::S(v) => text_limited(v, truncated),
        AttributeValue::N(v) => number_to_json(v),
        AttributeValue::B(v) => blob_limited(v.as_ref(), truncated),
        AttributeValue::L(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(attr_to_json(item, truncated, depth + 1, budget));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::M(map) => {
            // HashMap key order is undefined, so sort to output deterministically
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut obj = serde_json::Map::new();
            for key in keys {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                obj.insert(
                    key.clone(),
                    attr_to_json(&map[key], truncated, depth + 1, budget),
                );
            }
            serde_json::Value::Object(obj)
        }
        AttributeValue::Ss(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(text_limited(item, truncated));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::Ns(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(number_to_json(item));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::Bs(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(blob_limited(item.as_ref(), truncated));
            }
            serde_json::Value::Array(out)
        }
        // AttributeValue is non_exhaustive (to allow for future type additions)
        other => serde_json::Value::String(format!("<unsupported: {other:?}>")),
    }
}

/// Validate a DynamoDB table name (alphanumerics and `_ - .`, 3 to 255 characters).
/// It is only sent as a DescribeTable API parameter, so there is no injection surface,
/// but obviously invalid values (typos, contamination) are rejected before calling the API.
fn validate_table_name(name: &str) -> Result<&str, AppError> {
    let valid = (3..=255).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(AppError::Config(format!(
            "Invalid table name: {name} \
             (DynamoDB table names use letters, digits, '_', '-', '.')"
        )));
    }
    Ok(name)
}

/// Whether the editor input is `tables` (a queryfolio-specific statement).
///
/// Only a trailing semicolon and surrounding whitespace are allowed; a continuation such as
/// `tables where ...` is not accepted (so it is not confused with a PartiQL statement). Case-insensitive.
pub(crate) fn is_tables_statement(sql: &str) -> bool {
    let trimmed = sql.trim();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed);
    body.trim_end().eq_ignore_ascii_case("tables")
}

/// Result of the `tables` statement (a table of name / kind).
///
/// Cancellation is handled the same as the PartiQL path (abort the future on the client side).
async fn list_tables_query(
    client: &DynamoClient,
    registry: &CancelRegistry,
    connection_name: &str,
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let guard = registry.register(
        connection_name,
        CancelTarget::ClientSide {
            notify: notify.clone(),
        },
        cancelled,
    );
    let started = Instant::now();
    let result = tokio::select! {
        biased;
        // Show up to max_rows items. Fetch just one extra to determine truncated
        result = fetch_tables_limited(client, max_rows.saturating_add(1)) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let (tables, has_more) = result?;

    // Like other results, truncate at max_rows and report the cut via truncated.
    // Also catch early termination by the pagination deadline via has_more
    // (do not treat a partial list as complete)
    let truncated = has_more || tables.len() > max_rows;
    let rows: Vec<Vec<serde_json::Value>> = tables
        .into_iter()
        .take(max_rows)
        .map(|t| {
            vec![
                serde_json::Value::String(t.name),
                serde_json::Value::String(t.kind),
            ]
        })
        .collect();

    Ok(QueryResult {
        columns: vec!["name".to_string(), "kind".to_string()],
        row_count: rows.len(),
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
        applied_limit: None,
        switched_schema: None,
    })
}

/// Table list (for the schema browser's TABLES pane).
/// Follow ListTables with pagination and cut off at MAX_TABLES items.
pub async fn fetch_tables(client: &DynamoClient) -> Result<Vec<TableInfo>, AppError> {
    Ok(fetch_tables_limited(client, MAX_TABLES).await?.0)
}

/// Fetch up to `limit` tables.
///
/// `limit` is separate so that the `tables` statement, which shows only up to `max_rows`,
/// does not issue ListTables for MAX_TABLES (5,000) items (CYBERNEURA-DEV-406).
/// We can only stop at page granularity, so the actual count fetched is rounded up
/// to the next multiple of LIST_TABLES_PAGE.
/// The returned bool is the "more remain" flag. It is true both when `limit` is
/// reached and when the pagination deadline cut it off, so the caller can report the
/// result as truncated (never treat a deadline cutoff silently as a complete list).
async fn fetch_tables_limited(
    client: &DynamoClient,
    limit: usize,
) -> Result<(Vec<TableInfo>, bool), AppError> {
    let started = Instant::now();
    let mut names: Vec<String> = Vec::new();
    let mut start_name: Option<String> = None;
    let mut has_more = false;
    loop {
        // Besides the per-operation SDK timeout, also put a deadline on the whole listing.
        // Merely checking elapsed time after a page completes would let the request after a
        // page that finished just before the deadline run for a whole SDK timeout again, so
        // **apply the remaining time to each request**. If nothing remains, it is cut off immediately
        let remaining = REQUEST_TIMEOUT.saturating_sub(started.elapsed());
        let page = tokio::time::timeout(
            remaining,
            client
                .client
                .list_tables()
                .set_exclusive_start_table_name(start_name.take())
                .limit(LIST_TABLES_PAGE)
                .send(),
        )
        .await;
        let out = match page {
            Ok(result) => result.map_err(|e| sdk_error("ListTables failed", e))?,
            // Cut off by the deadline. Return what was collected and report that more remain
            Err(_) => {
                has_more = true;
                break;
            }
        };
        names.extend(out.table_names.unwrap_or_default());
        start_name = out.last_evaluated_table_name;
        if start_name.is_none() {
            break;
        }
        if names.len() >= limit {
            has_more = true;
            break;
        }
    }
    if names.len() > limit {
        has_more = true;
    }
    names.truncate(limit);
    let tables: Vec<TableInfo> = names
        .into_iter()
        .map(|name| TableInfo {
            qualified_name: name.clone(),
            name,
            schema: None,
            kind: "table".to_string(),
        })
        .collect();
    Ok((tables, has_more))
}

/// Display string for ScalarAttributeType (S / N / B).
fn scalar_type_label(t: &aws_sdk_dynamodb::types::ScalarAttributeType) -> String {
    t.as_str().to_string()
}

/// List of "columns" of a table (for expanding in the TABLES pane).
/// DynamoDB is schemaless, so return what DescribeTable reveals = the key schema
/// (partition / sort key) + attribute definitions (only attributes used by keys or indexes).
/// data_type is written as S / N / B, with the role appended for keys.
/// Key attributes always exist so nullable = false; the others are true.
pub async fn fetch_columns(
    client: &DynamoClient,
    table: &str,
) -> Result<Vec<ColumnInfo>, AppError> {
    let (key_schema, attribute_definitions) = describe_table(client, table).await?;

    // Map of attribute name -> type (S / N / B)
    let mut types: HashMap<String, String> = HashMap::new();
    for def in &attribute_definitions {
        types.insert(
            def.attribute_name().to_string(),
            scalar_type_label(def.attribute_type()),
        );
    }

    let mut columns: Vec<ColumnInfo> = Vec::new();
    // Put the key schema (returned in HASH -> RANGE order) first
    for element in &key_schema {
        let name = element.attribute_name().to_string();
        let base = types.get(&name).cloned().unwrap_or_else(|| "?".to_string());
        let role = match element.key_type() {
            KeyType::Hash => "partition key",
            KeyType::Range => "sort key",
            _ => "key",
        };
        columns.push(ColumnInfo {
            data_type: format!("{base} ({role})"),
            name,
            nullable: false,
        });
    }
    // The remaining attribute definitions (GSI / LSI key attributes). Table keys are excluded
    for def in &attribute_definitions {
        let name = def.attribute_name();
        if columns.iter().any(|c| c.name == name) {
            continue;
        }
        columns.push(ColumnInfo {
            name: name.to_string(),
            data_type: scalar_type_label(def.attribute_type()),
            nullable: true,
        });
    }
    Ok(columns)
}

/// Primary key of a table (partition key -> sort key order).
pub async fn fetch_primary_keys(
    client: &DynamoClient,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let (key_schema, _) = describe_table(client, table).await?;
    let mut hash: Vec<String> = Vec::new();
    let mut range: Vec<String> = Vec::new();
    for element in &key_schema {
        match element.key_type() {
            KeyType::Range => range.push(element.attribute_name().to_string()),
            _ => hash.push(element.attribute_name().to_string()),
        }
    }
    hash.extend(range);
    Ok(hash)
}

/// Run DescribeTable and return the key schema and attribute definitions.
async fn describe_table(
    client: &DynamoClient,
    table: &str,
) -> Result<
    (
        Vec<aws_sdk_dynamodb::types::KeySchemaElement>,
        Vec<aws_sdk_dynamodb::types::AttributeDefinition>,
    ),
    AppError,
> {
    let table = validate_table_name(table)?;
    let out = client
        .client
        .describe_table()
        .table_name(table)
        .send()
        .await
        .map_err(|e| sdk_error("DescribeTable failed", e))?;
    let Some(desc) = out.table else {
        return Err(AppError::Config(format!("Table not found: {table}")));
    };
    Ok((
        desc.key_schema.unwrap_or_default(),
        desc.attribute_definitions.unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tables` is a queryfolio-specific statement that can run even with Writable OFF
    /// (CYBERNEURA-DEV-406). To avoid confusion with PartiQL statements, only a trailing
    /// semicolon and surrounding whitespace are allowed.
    #[test]
    fn test_is_tables_statement() {
        for ok in ["tables", "TABLES", " tables ", "tables;", "  Tables ;  "] {
            assert!(is_tables_statement(ok), "should accept: {ok:?}");
        }
        for ng in [
            "table",
            "tables where x = 1",
            "select * from tables",
            // Another statement cannot be smuggled in via multiple statements
            "tables; select 1",
            // Only one semicolon is allowed
            "tables;;",
            "",
        ] {
            assert!(!is_tables_statement(ng), "should reject: {ng:?}");
        }
    }
    use crate::db::dangerous_statement_reason;

    fn s(v: &str) -> AttributeValue {
        AttributeValue::S(v.to_string())
    }

    fn n(v: &str) -> AttributeValue {
        AttributeValue::N(v.to_string())
    }

    #[test]
    fn test_shape_items_column_cap() {
        // Column blow-up from schemalessness is cut off by MAX_COLUMNS
        let mut items = Vec::new();
        for i in 0..3 {
            let mut item = HashMap::new();
            for j in 0..(MAX_COLUMNS + 50) {
                item.insert(format!("attr_{i}_{j:04}"), s("v"));
            }
            items.push(item);
        }
        let result = shape_items(items, 100);
        assert_eq!(result.columns.len(), MAX_COLUMNS);
        assert!(result.truncated);
        assert!(result.rows.iter().all(|r| r.len() == MAX_COLUMNS));
    }

    #[test]
    fn test_readonly_guard_partiql() {
        let f = |sql: &str| is_readonly_allowed(sql, Engine::DynamoDb);
        assert!(f("SELECT * FROM \"users\""));
        assert!(f("SELECT * FROM \"users\" WHERE pk = 'a' AND EXISTS(tags)"));
        // A SELECT is a read even if it contains a ? placeholder
        assert!(f("SELECT * FROM \"users\" WHERE pk = ?"));
        assert!(!f("INSERT INTO \"users\" VALUE {'pk': 'a'}"));
        assert!(!f("UPDATE \"users\" SET x = 1 WHERE pk = 'a'"));
        assert!(!f("DELETE FROM \"users\" WHERE pk = 'a'"));
    }

    #[test]
    fn test_dangerous_guard_partiql() {
        let d = |sql: &str| dangerous_reason(sql, Engine::DynamoDb).is_some();
        // UPDATE / DELETE without WHERE is dangerous
        assert!(d("DELETE FROM \"users\""));
        assert!(d("UPDATE \"users\" SET x = 1"));
        // With WHERE it passes. Even if scan_sql blanks double-quoted identifiers (PartiQL's quoted form)
        // as strings, the keyword where is never quoted, so detection is not affected (checklist item 5)
        assert!(!d("DELETE FROM \"users\" WHERE \"pk\" = 'a'"));
        assert!(!d("UPDATE \"users\" SET x = 1 WHERE pk = 'a'"));
        // A quoted "where" as an identifier is not a WHERE clause ->
        // it falls to the dangerous side (treated as no WHERE)
        assert!(d("DELETE FROM \"where\""));
        // INSERT / SELECT are out of scope
        assert!(!d("INSERT INTO \"users\" VALUE {'pk': 'a'}"));
        assert!(!d("SELECT * FROM \"users\""));
        // The same holds via the public wrapper (the frontend's pre-execution confirmation)
        assert!(dangerous_statement_reason("dynamodb", "DELETE FROM \"users\"")
            .unwrap()
            .is_some());
        assert!(dangerous_statement_reason(
            "dynamodb",
            "DELETE FROM \"users\" WHERE pk = 'a'"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn test_number_to_json() {
        assert_eq!(number_to_json("42"), serde_json::json!(42));
        assert_eq!(number_to_json("-7"), serde_json::json!(-7));
        // Decimals are strings to keep precision
        assert_eq!(number_to_json("1.5"), serde_json::json!("1.5"));
        // Above 2^53 is a string (guards against rounding at the invoke boundary)
        assert_eq!(
            number_to_json("9007199254740993"),
            serde_json::json!("9007199254740993")
        );
        // Arbitrary-precision integers beyond i64 are also strings
        assert_eq!(
            number_to_json("170141183460469231731687303715884105727"),
            serde_json::json!("170141183460469231731687303715884105727")
        );
        // Exponent notation stays a string
        assert_eq!(number_to_json("1e10"), serde_json::json!("1e10"));
    }

    #[test]
    fn test_attr_to_json_scalars() {
        let mut truncated = false;
        assert_eq!(
            attr_to_json_cell(&s("hello"), &mut truncated),
            serde_json::json!("hello")
        );
        assert_eq!(
            attr_to_json_cell(&AttributeValue::Bool(true), &mut truncated),
            serde_json::json!(true)
        );
        assert_eq!(
            attr_to_json_cell(&AttributeValue::Null(true), &mut truncated),
            serde_json::Value::Null
        );
        // B: a string if UTF-8, otherwise base64
        assert_eq!(
            attr_to_json_cell(
                &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(b"abc".to_vec())),
                &mut truncated
            ),
            serde_json::json!("abc")
        );
        let b = attr_to_json_cell(
            &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(vec![0xC3, 0x28])),
            &mut truncated,
        );
        assert!(b.as_str().unwrap().starts_with("base64:"));
        assert!(!truncated);
    }

    #[test]
    fn test_attr_to_json_collections() {
        let mut truncated = false;
        let list = AttributeValue::L(vec![s("a"), n("1")]);
        assert_eq!(
            attr_to_json_cell(&list, &mut truncated),
            serde_json::json!(["a", 1])
        );
        let mut map = HashMap::new();
        map.insert("b".to_string(), n("2"));
        map.insert("a".to_string(), s("x"));
        let m = AttributeValue::M(map);
        // M keys are sorted and come out deterministically
        assert_eq!(
            serde_json::to_string(&attr_to_json_cell(&m, &mut truncated)).unwrap(),
            "{\"a\":\"x\",\"b\":2}"
        );
        let ss = AttributeValue::Ss(vec!["x".into(), "y".into()]);
        assert_eq!(
            attr_to_json_cell(&ss, &mut truncated),
            serde_json::json!(["x", "y"])
        );
        let ns = AttributeValue::Ns(vec!["1".into(), "2.5".into()]);
        assert_eq!(
            attr_to_json_cell(&ns, &mut truncated),
            serde_json::json!([1, "2.5"])
        );
        let bs = AttributeValue::Bs(vec![aws_sdk_dynamodb::primitives::Blob::new(
            b"z".to_vec(),
        )]);
        assert_eq!(
            attr_to_json_cell(&bs, &mut truncated),
            serde_json::json!(["z"])
        );
        assert!(!truncated);
    }

    #[test]
    fn test_cell_budget_is_shared_across_nesting() {
        // Even a nest of 600 elements x 2 lists is cut off by the per-cell budget (1,000)
        let inner: Vec<AttributeValue> = (0..600).map(|i| n(&i.to_string())).collect();
        let value = AttributeValue::L(vec![
            AttributeValue::L(inner.clone()),
            AttributeValue::L(inner),
        ]);
        let mut truncated = false;
        let v = attr_to_json_cell(&value, &mut truncated);
        assert!(truncated);
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
        assert!(count(&v) <= MAX_CELL_ELEMENTS + 10);
    }

    #[test]
    fn test_text_and_blob_truncation() {
        let mut truncated = false;
        let long = "x".repeat(MAX_TEXT_CHARS + 5);
        let v = attr_to_json_cell(&s(&long), &mut truncated);
        assert!(truncated);
        let text = v.as_str().unwrap();
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1); // +1 is the ellipsis
        assert!(text.ends_with('…'));

        let mut truncated = false;
        let v = attr_to_json_cell(
            &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(vec![
                b'a';
                MAX_TEXT_CHARS + 10
            ])),
            &mut truncated,
        );
        assert!(truncated);
        assert!(v.as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn test_nesting_depth_cap() {
        let mut value = n("1");
        for _ in 0..(MAX_NESTING_DEPTH + 10) {
            value = AttributeValue::L(vec![value]);
        }
        let mut truncated = false;
        let v = attr_to_json_cell(&value, &mut truncated);
        assert!(truncated);
        assert!(v.to_string().contains("nesting too deep"));
    }

    #[test]
    fn test_shape_items_columns_union() {
        let mut a = HashMap::new();
        a.insert("pk".to_string(), s("u1"));
        a.insert("name".to_string(), s("alice"));
        let mut b = HashMap::new();
        b.insert("pk".to_string(), s("u2"));
        b.insert("age".to_string(), n("30"));
        let result = shape_items(vec![a, b], 100);
        // The union is fixed in sorted order
        assert_eq!(result.columns, vec!["age", "name", "pk"]);
        assert_eq!(result.row_count, 2);
        // Attributes missing from an item are NULL
        assert_eq!(result.rows[0][0], serde_json::Value::Null); // a has no age
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
        assert_eq!(result.rows[1][1], serde_json::Value::Null); // b has no name
        assert_eq!(result.rows[1][0], serde_json::json!(30));
        assert!(!result.truncated);
        assert_eq!(result.affected_rows, None);
    }

    #[test]
    fn test_shape_items_truncation() {
        let items: Vec<HashMap<String, AttributeValue>> = (0..5)
            .map(|i| {
                let mut item = HashMap::new();
                item.insert("pk".to_string(), n(&i.to_string()));
                item
            })
            .collect();
        let result = shape_items(items, 3);
        assert_eq!(result.row_count, 3);
        assert!(result.truncated);

        // Empty Items (a write statement) give an empty result + affected None
        let result = shape_items(vec![], 100);
        assert_eq!(result.row_count, 0);
        assert!(result.columns.is_empty());
        assert_eq!(result.affected_rows, None);
        assert!(!result.truncated);
    }

    #[test]
    fn test_validate_table_name() {
        assert!(validate_table_name("users").is_ok());
        assert!(validate_table_name("my-table.v2_x").is_ok());
        assert!(validate_table_name("ab").is_err()); // fewer than 3 characters
        assert!(validate_table_name("bad name").is_err());
        assert!(validate_table_name("tbl;drop").is_err());
        assert!(validate_table_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn test_meta_commands_are_rejected() {
        let err = crate::meta_commands::translate(Engine::DynamoDb, "\\dt").unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
        let err = crate::meta_commands::translate(Engine::DynamoDb, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
        // Plain SQL is not a meta command
        assert!(crate::meta_commands::translate(Engine::DynamoDb, "SELECT 1")
            .unwrap()
            .is_none());
    }

    // ---- Integration tests (dynamodb-local) ----
    //
    // `docker run -d --name queryfolio-test-ddb -p 127.0.0.1:8100:8000 \
    //    amazon/dynamodb-local` is running. They are skipped if it is not running
    // (so CI and docker-less environments are not broken).

    const LOCAL_ENDPOINT: (&str, u16) = ("127.0.0.1", 8100);

    /// Whether dynamodb-local is running (whether a TCP connection is possible).
    async fn local_available() -> bool {
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::net::TcpStream::connect(LOCAL_ENDPOINT),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    }

    fn local_server_config(name: &str) -> ServerConfig {
        // dynamodb-local does not validate credentials, so dummy static keys are fine
        serde_yaml::from_str(&format!(
            "name: {name}\n\
             engine: dynamodb\n\
             schema: us-east-1\n\
             host: {}\n\
             port: {}\n\
             user: dummyAccessKey\n\
             password: dummySecretKey\n",
            LOCAL_ENDPOINT.0, LOCAL_ENDPOINT.1
        ))
        .unwrap()
    }

    /// Create a test table (partition key pk (S) + sort key sk (N)).
    async fn create_test_table(client: &DynamoClient, table: &str) {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, ScalarAttributeType,
        };
        client
            .client
            .create_table()
            .table_name(table)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("sk")
                    .attribute_type(ScalarAttributeType::N)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("sk")
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            )
            .billing_mode(BillingMode::PayPerRequest)
            .send()
            .await
            .unwrap();
    }

    async fn drop_test_table(client: &DynamoClient, table: &str) {
        let _ = client.client.delete_table().table_name(table).send().await;
    }

    /// Integration test that doubles as a substitute for GUI E2E. It verifies end to end,
    /// against dynamodb-local, the same public path in db.rs as the frontend's Tauri commands
    /// (DbManager::get_pool -> delegation of run_query_cancellable).
    #[tokio::test]
    async fn test_integration_dynamodb_local() {
        if !local_available().await {
            eprintln!(
                "skipping test_integration_dynamodb_local: \
                 dynamodb-local is not listening on {}:{}",
                LOCAL_ENDPOINT.0, LOCAL_ENDPOINT.1
            );
            return;
        }
        // Use a unique table name per run so concurrent runs and reruns do not collide
        let table = format!(
            "qf_it_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = local_server_config("ddb-it");
        let manager = crate::db::DbManager::default();
        let registry = CancelRegistry::default();

        // Connect (including the ListTables connectivity check)
        let pool = manager.get_pool(&server).await.unwrap();
        let crate::db::DbPool::DynamoDb(client) = &pool else {
            panic!("expected a DynamoDb pool");
        };
        create_test_table(client, &table).await;

        let run = |sql: String, max_rows: usize, readonly, dangerous| {
            let pool = pool.clone();
            let registry = &registry;
            async move {
                crate::db::run_query_cancellable(
                    &pool, registry, "ddb-it", &sql, max_rows, Some(500), readonly,
                    dangerous,
                )
                .await
            }
        };

        // (a) INSERT (writable): the affected row count is unavailable, so None + empty result
        for i in 0..30 {
            let result = run(
                format!(
                    "INSERT INTO \"{table}\" VALUE {{\
                     'pk': 'user1', 'sk': {i}, 'name': 'row{i}', \
                     'score': 1.5, 'big': 9007199254740993, \
                     'tags': ['a', 'b'], 'meta': {{'lang': 'ja'}}}}"
                ),
                1000,
                ReadonlyGuard::Off,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result.row_count, 0);
            assert_eq!(result.affected_rows, None);
        }

        // (b) SELECT: table form (columns union) and type conversion
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1' AND sk = 0"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(
            result.columns,
            vec!["big", "meta", "name", "pk", "score", "sk", "tags"]
        );
        let row = &result.rows[0];
        assert_eq!(row[0], serde_json::json!("9007199254740993")); // above 2^53 is a string
        assert_eq!(row[1], serde_json::json!({"lang": "ja"}));
        assert_eq!(row[2], serde_json::json!("row0"));
        assert_eq!(row[3], serde_json::json!("user1"));
        assert_eq!(row[4], serde_json::json!("1.5")); // decimals are strings (arbitrary precision)
        assert_eq!(row[5], serde_json::json!(0));
        assert_eq!(row[6], serde_json::json!(["a", "b"]));

        // (c) Truncation at max_rows + 1 + truncated (10 of 30 rows)
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1'"),
            10,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 10);
        assert!(result.truncated);

        // All 30 rows are fetched without truncated (checks that pagination works)
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1'"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 30);
        assert!(!result.truncated);

        // (d) readonly guard: INSERT is rejected with Writable OFF (Switch)
        let err = run(
            format!("INSERT INTO \"{table}\" VALUE {{'pk': 'x', 'sk': 0}}"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Readonly(_)), "{err}");
        assert!(err.to_string().contains("Writable"), "{err}");

        // (e) dangerous guard: DELETE without WHERE is rejected
        let err = run(
            format!("DELETE FROM \"{table}\""),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Dangerous(_)), "{err}");

        // (f) Meta commands are rejected
        let err = run("\\dt".to_string(), 1000, ReadonlyGuard::Switch, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        // (g) UPDATE / DELETE (with WHERE, writable).
        //     RETURNING ALL OLD * returns the pre-change item as a row
        let result = run(
            format!(
                "UPDATE \"{table}\" SET name = 'renamed' \
                 WHERE pk = 'user1' AND sk = 0"
            ),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 0);
        let result = run(
            format!(
                "DELETE FROM \"{table}\" WHERE pk = 'user1' AND sk = 1 \
                 RETURNING ALL OLD *"
            ),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(result.columns.iter().any(|c| c == "name"));

        // (h) schema_info: table list, columns (key schema), primary key
        let tables = crate::schema_info::fetch_tables(&pool).await.unwrap();
        assert!(tables.iter().any(|t| t.qualified_name == table));
        assert!(tables.iter().all(|t| t.kind == "table"));
        let columns = crate::schema_info::fetch_columns(&pool, &table).await.unwrap();
        let summary: Vec<(&str, &str, bool)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("pk", "S (partition key)", false),
                ("sk", "N (sort key)", false),
            ]
        );
        let keys = crate::schema_info::fetch_primary_keys(&pool, &table)
            .await
            .unwrap();
        assert_eq!(keys, vec!["pk", "sk"]);
        // DescribeTable on a nonexistent table is an error
        assert!(
            crate::schema_info::fetch_columns(&pool, "qf-no-such-table")
                .await
                .is_err()
        );

        // (i) A SELECT response does not break the rejection paths of list_schemas / run_statements
        let schemas = crate::db::list_schemas(&pool, &server).await.unwrap();
        assert!(schemas.is_empty());
        let err = crate::db::run_statements(
            &pool,
            &["UPDATE t SET a = 1 WHERE id = 1".to_string()],
            ReadonlyGuard::Off,
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        drop_test_table(client, &table).await;
        manager.disconnect("ddb-it").await;
    }

    /// Connectivity check (ListTables) fails quickly
    /// with an error on an unreachable endpoint (does not wait indefinitely).
    #[tokio::test]
    async fn test_connect_fails_fast_on_unreachable_endpoint() {
        // A high port where the TCP connection itself is refused (assuming nothing is listening)
        let server: ServerConfig = serde_yaml::from_str(
            "name: x\nengine: dynamodb\nschema: us-east-1\n\
             host: 127.0.0.1\nport: 59998\nuser: a\npassword: b\n",
        )
        .unwrap();
        let started = Instant::now();
        let err = connect(&server).await.unwrap_err();
        assert!(matches!(err, AppError::DynamoDb(_)), "{err}");
        // Connection refusal is immediate; at worst it returns within CONNECT_TIMEOUT + margin
        assert!(started.elapsed() < CONNECT_TIMEOUT + std::time::Duration::from_secs(5));
    }

    /// A missing region (schema) is a config error.
    #[tokio::test]
    async fn test_connect_requires_region_and_paired_credentials() {
        let server: ServerConfig =
            serde_yaml::from_str("name: x\nengine: dynamodb\n").unwrap();
        let err = build_client(&server).await.unwrap_err();
        assert!(err.to_string().contains("AWS region"), "{err}");

        // Only user or only password is a config error (do not silently fall back to the chain)
        let server: ServerConfig = serde_yaml::from_str(
            "name: x\nengine: dynamodb\nschema: us-east-1\nuser: only-key\n",
        )
        .unwrap();
        let err = build_client(&server).await.unwrap_err();
        assert!(err.to_string().contains("both user"), "{err}");
    }

    /// Client-side cancel: cancelling a running query against an endpoint that never
    /// responds returns Cancelled without waiting for the timeout.
    #[tokio::test]
    async fn test_cancel_aborts_running_query() {
        // A local server that accepts but never responds
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = tokio::spawn(async move {
            let mut sockets = Vec::new();
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    sockets.push(socket); // keep the connection open without responding
                }
            }
        });

        let server: ServerConfig = serde_yaml::from_str(&format!(
            "name: ddb-cancel\nengine: dynamodb\nschema: us-east-1\n\
             host: 127.0.0.1\nport: {port}\nuser: a\npassword: b\n"
        ))
        .unwrap();
        let client = build_client(&server).await.unwrap();
        let registry = Arc::new(CancelRegistry::default());

        let registry2 = registry.clone();
        let task = tokio::spawn(async move {
            run_query_cancellable(
                &client,
                &registry2,
                "ddb-cancel",
                "SELECT * FROM \"t\"",
                100,
                ReadonlyGuard::Switch,
                false,
            )
            .await
        });
        // Wait for the execution to be registered, then cancel
        let mut cancelled = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if registry.cancel("ddb-cancel").await.unwrap() {
                cancelled = true;
                break;
            }
        }
        assert!(cancelled, "the query was never registered");
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancel did not abort the request")
            .unwrap();
        assert!(matches!(result, Err(AppError::Cancelled)), "{result:?}");
        hold.abort();
    }
}
