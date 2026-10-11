//! Elasticsearch engine.
//!
//! The editor handles Kibana Console-style request blocks:
//! a method line such as `GET /index/_search` + a JSON body on the following lines (optional).
//! Lines starting with `#` are comments. If the body is several JSON objects (NDJSON),
//! it is sent as is for `_bulk`. Running a selection of multiple blocks executes them in order
//! and returns a request / status / result table.
//!
//! - Does not use sqlx; calls the REST API directly with reqwest (the official elasticsearch
//!   crate has been alpha for years, so it is not used). `EsClient` holds only base_url and credentials.
//! - The readonly guard always allows GET / HEAD, and allows POST only for a whitelist of
//!   search-type endpoints. PUT / DELETE / PATCH and other POSTs require Writable.
//! - The dangerous guard covers index deletion (DELETE on a single-segment path) and
//!   `_delete_by_query` (equivalent to a DELETE without WHERE in SQL).
//! - Every HTTP request has a timeout, and responses are read up to a byte limit
//!   (so an unbounded response cannot flood the webview / memory).
//! - Cancellation drops the future on the client side (`CancelTarget::ClientSide`).
//!   Execution on the server is not stopped, but there is no connection pool, so nothing can end up broken.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use crate::config::ServerConfig;
use crate::db::{
    dangerous_block_error, readonly_block_error, CancelRegistry, CancelTarget, QueryResult,
    ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

pub const DEFAULT_PORT: u16 = 9200;

/// Timeout for the connection check (GET /). A check request without a timeout would stall
/// get_pool (while DbManager holds its lock) indefinitely, so this is required.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Request timeout for query execution.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Limit for reading the response body (memory protection). Exceeding it is an error.
const MAX_RESPONSE_BYTES: usize = 20 * 1024 * 1024;

/// Character limit for pretty JSON / text put into one cell.
/// Anything beyond it is cut off and truncated is set (unbounded data is not sent to the webview).
const MAX_RESPONSE_CHARS: usize = 100_000;

/// Character limit for strings put into a table cell (e.g. _source values of hits).
const MAX_CELL_CHARS: usize = 10_000;

/// Element limit for collections (arrays / objects) put into a table cell.
const MAX_CELL_ITEMS: usize = 1_000;

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// POST API segments allowed even in readonly (Writable OFF).
/// The first segment in the path that starts with `_` is regarded as the API, and the following
/// sub-segments are verified per API by `readonly_post_allowed` (this prevents slipping through
/// with a document ID, as in `/index/_doc/_search`).
const READONLY_POST_APIS: &[&str] = &[
    "_search", "_msearch", "_count", "_analyze", "_mget", "_field_caps",
    "_validate", "_explain", "_termvectors", "_pit", "_sql", "_render",
];

/// Client for connecting to Elasticsearch. It has no pool of its own; each request uses
/// reqwest's internal connection pool.
#[derive(Clone)]
pub struct EsClient {
    /// Example: `http://127.0.0.1:9200` (no trailing slash)
    base_url: String,
    client: reqwest::Client,
    user: Option<String>,
    password: Option<String>,
}

/// One request parsed from the editor input.
#[derive(Debug, PartialEq)]
struct EsRequest {
    /// Uppercase HTTP method
    method: String,
    /// Path normalized to start with `/` (may include a query string)
    path: String,
    /// JSON / NDJSON body (None if absent)
    body: Option<String>,
}

impl EsRequest {
    /// For display (the request column of multi-request results)
    fn display(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// Returns the host name to keep in the URL when using an SSH tunnel + TLS (None otherwise).
/// After establishing the tunnel, DbManager swaps the destination to 127.0.0.1:<local_port>,
/// but if the URL host were also changed to 127.0.0.1, reqwest's SNI / certificate host name
/// verification would be done against 127.0.0.1, and verifying a certificate issued for the real
/// host name would fail. Keep the configured host name in the URL and point only the actual
/// destination to 127.0.0.1:<local_port> with resolve().
fn tls_tunnel_url_host(server: &ServerConfig, dial_host: &str) -> Option<String> {
    if !(server.tls && server.ssh_tunnel.is_some() && dial_host == "127.0.0.1") {
        return None;
    }
    server
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// Establishes the connection and goes as far as the reachability check (GET /).
/// base_url is built from host / port and `tls: true` (a queryfolio-specific extension).
pub async fn connect(
    server: &ServerConfig,
    host: &str,
    port: u16,
) -> Result<EsClient, AppError> {
    let scheme = if server.tls { "https" } else { "http" };
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // Do not follow redirects: the origin check in build_url only covers the first URL,
        // so following would let a 302 send us to a different origin (SSRF-like behavior)
        .redirect(reqwest::redirect::Policy::none());
    // SSH tunnel + TLS: the URL keeps the configured host name (SNI / certificate verification and
    // the Host header are done with the real host), and only the destination goes to the tunnel's local port
    let url_host = match tls_tunnel_url_host(server, host) {
        Some(original) => {
            builder = builder.resolve(
                &original,
                std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            );
            original
        }
        None => host.to_string(),
    };
    let base_url = format!("{scheme}://{url_host}:{port}");
    // If base_url is invalid as a URL (strange characters in the host name etc.), fail early
    reqwest::Url::parse(&base_url)
        .map_err(|e| AppError::Elasticsearch(format!("Invalid server address {base_url}: {e}")))?;
    let client = builder
        .build()
        .map_err(|e| {
            AppError::Elasticsearch(format!("Failed to build the HTTP client: {e}"))
        })?;
    let es = EsClient {
        base_url,
        client,
        user: server
            .user
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(str::to_string),
        password: server.password.clone(),
    };
    // As with sqlx's connect_with, check reachability and authentication at connect time.
    // The check request also gets a timeout: so that get_pool does not stall indefinitely on a peer
    // that accepts TCP but never responds (e.g. a stalled SSH tunnel)
    let root = EsRequest {
        method: "GET".into(),
        path: "/".into(),
        body: None,
    };
    let confirm = send_request(&es, &root);
    match tokio::time::timeout(CONNECT_TIMEOUT, confirm).await {
        Ok(Ok((status, body))) => {
            if !status.is_success() {
                return Err(http_status_error(status, &body));
            }
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AppError::Elasticsearch(format!(
                "The server did not respond within {}s",
                CONNECT_TIMEOUT.as_secs()
            )));
        }
    }
    Ok(es)
}

impl EsClient {
    /// Builds the destination URL from base_url + path.
    /// The path always starts with `/`, so the authority (host:port) does not change.
    /// As defense in depth, also check that the origin of the parsed result matches the base.
    fn build_url(&self, path: &str) -> Result<reqwest::Url, AppError> {
        let url = reqwest::Url::parse(&format!("{}{}", self.base_url, path))
            .map_err(|e| AppError::Elasticsearch(format!("Invalid request path {path}: {e}")))?;
        let base = reqwest::Url::parse(&self.base_url)
            .map_err(|e| AppError::Elasticsearch(format!("Invalid server address: {e}")))?;
        if url.origin() != base.origin() {
            return Err(AppError::Elasticsearch(format!(
                "The request path must stay on the configured server: {path}"
            )));
        }
        Ok(url)
    }
}

/// Turns an HTTP error status into an error, including the ES error JSON.
fn http_status_error(status: reqwest::StatusCode, body: &serde_json::Value) -> AppError {
    let text = match body {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let (text, _) = truncate_chars(&text, MAX_RESPONSE_CHARS);
    if text.trim().is_empty() {
        AppError::Elasticsearch(format!("HTTP {status}"))
    } else {
        AppError::Elasticsearch(format!("HTTP {status}: {text}"))
    }
}

/// If the first token of the line is an HTTP method, returns it (uppercased).
/// A JSON body line never starts with `GET` etc. (lines start with `{` / `"` / whitespace etc.),
/// so a line starting with a method name can be regarded as a request line.
fn leading_method(line: &str) -> Option<String> {
    let first = line.trim().split_whitespace().next()?;
    let upper = first.to_ascii_uppercase();
    METHODS.contains(&upper.as_str()).then_some(upper)
}

/// Splits the editor input into a sequence of Kibana Console-style request blocks.
/// - Block = method line (`GET /path`) + the body on the following lines (up to the next
///   method line / EOF)
/// - Lines starting with `#` are comments. Blank lines are ignored (blank lines inside a body do not separate either)
fn parse_input(input: &str) -> Result<Vec<EsRequest>, AppError> {
    let mut requests: Vec<EsRequest> = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(method) = leading_method(line) {
            // Finalize the previous block
            if let Some((m, p, body)) = current.take() {
                requests.push(finish_request(m, p, body)?);
            }
            let mut parts = trimmed.split_whitespace();
            parts.next(); // method
            let Some(path) = parts.next() else {
                return Err(AppError::Elasticsearch(format!(
                    "Missing path after {method} (expected e.g. \"{method} /_search\")"
                )));
            };
            if parts.next().is_some() {
                return Err(AppError::Elasticsearch(format!(
                    "Unexpected token after the path in \"{trimmed}\" \
                     (the request line must be \"METHOD /path\")"
                )));
            }
            let path = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            current = Some((method, path, Vec::new()));
            continue;
        }
        if trimmed.is_empty() {
            // Blank lines in the middle of a body are not kept (keeps NDJSON line validation simple)
            continue;
        }
        match &mut current {
            Some((_, _, body)) => body.push(line.to_string()),
            None => {
                return Err(AppError::Elasticsearch(format!(
                    "Expected a request line like \"GET /_search\", got: {trimmed}"
                )));
            }
        }
    }
    if let Some((m, p, body)) = current.take() {
        requests.push(finish_request(m, p, body)?);
    }
    Ok(requests)
}

fn finish_request(
    method: String,
    path: String,
    body_lines: Vec<String>,
) -> Result<EsRequest, AppError> {
    let body = body_lines.join("\n");
    let body = body.trim();
    Ok(EsRequest {
        method,
        path,
        body: (!body.is_empty()).then(|| body.to_string()),
    })
}

/// Send format of the body.
enum EsBody {
    /// A single JSON document (application/json)
    Json(String),
    /// Multiple JSON documents (NDJSON, _bulk etc.; application/x-ndjson)
    Ndjson(String),
}

/// Validates and classifies the body as JSON / NDJSON.
/// If it is neither, fail before execution (earlier than sending it to the server and
/// receiving a confusing error).
fn classify_body(body: &str, ndjson_api: bool) -> Result<EsBody, AppError> {
    // NDJSON-type APIs (_bulk / _msearch) send the body as NDJSON even when it is one line.
    // Leaving it to the guess "Json if it parses as JSON" would send a one-action
    // _bulk body as application/json without a trailing newline, which violates the Bulk API's
    // "trailing newline required" rule and fails
    if !ndjson_api && serde_json::from_str::<serde_json::Value>(body).is_ok() {
        return Ok(EsBody::Json(body.to_string()));
    }
    for (i, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(e) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return Err(AppError::Elasticsearch(format!(
                "The request body is not valid JSON (body line {}: {e})",
                i + 1
            )));
        }
    }
    // NDJSON (_bulk) requires a trailing newline
    Ok(EsBody::Ndjson(format!("{}\n", body.trim_end())))
}

/// Whether the path is an API that requires an NDJSON body (_bulk / _msearch).
/// Paths for which guard_segments fails are rejected earlier by the pre-execution validation, so false is fine.
fn path_is_ndjson_api(path: &str) -> bool {
    guard_segments(path)
        .map(|segments| {
            segments
                .iter()
                .any(|s| s == "_bulk" || s == "_msearch")
        })
        .unwrap_or(false)
}

/// Splits a path into a segment list for guard decisions.
/// Removes the query string / fragment and splits on `/`.
/// `.` / `..` segments (including percent-encoded forms) are rejected, because URL normalization
/// at send time makes the validated path differ from the actual path
/// (the validated path must match the actual I/O path).
fn guard_segments(path: &str) -> Result<Vec<String>, AppError> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    // Backslash is treated as `/` by whatwg URL normalization (reqwest::Url), and `..` segments are
    // resolved too, so "the path the guard saw" and "the path actually sent" diverge
    // (e.g. /a\..\_delete_by_query is sent as
    // /_delete_by_query). Reject it unconditionally.
    if path.contains('\\') {
        return Err(AppError::Elasticsearch(format!(
            "Backslashes are not supported in the request path: {path}"
        )));
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = segment.replace("%2e", ".").replace("%2E", ".");
        if decoded == "." || decoded == ".." {
            return Err(AppError::Elasticsearch(format!(
                "Path segments \".\" and \"..\" are not supported: {path}"
            )));
        }
        // ES core route resolution is done on the raw (undecoded) path, so percent-encoding cannot
        // slip past the guard (RestController), but to prepare for setups with an intermediate proxy that
        // decodes or a compatible implementation (OpenSearch derivatives etc.), the encoded forms of
        // separators (%2F %5C) and underscore (%5F) are defensively rejected
        // (completeness of the accident-prevention guard takes priority).
        let lower = segment.to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%5f") {
            return Err(AppError::Elasticsearch(format!(
                "Percent-encoded separators (%2F, %5C) and underscores (%5F) \
                 are not supported in the request path: {path}"
            )));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

/// Whether the request may run even in readonly (Writable OFF / config readonly).
/// GET / HEAD are always allowed. POST only for the whitelist of search-type endpoints.
/// PUT / DELETE / PATCH always require Writable.
/// A percent-encoded API name does not match the whitelist = falls on the rejecting
/// (safe) side.
fn is_readonly_request(method: &str, segments: &[String]) -> bool {
    match method {
        "GET" | "HEAD" => true,
        "POST" => readonly_post_allowed(segments),
        _ => false,
    }
}

/// readonly decision for POST. The first segment starting with `_` is regarded as the API,
/// and the following sub-segments are also verified per API.
/// In ES REST routes an index name never starts with `_`, and the segments after the API are
/// sub-actions or document IDs, so the first `_` segment is the API
/// (we do not decide by "there is a _search somewhere", to prevent slip-throughs such as
/// `POST /index/_doc/_search` = creating a document whose ID is "_search").
fn readonly_post_allowed(segments: &[String]) -> bool {
    let Some(api_index) = segments.iter().position(|s| s.starts_with('_')) else {
        return false;
    };
    let api = segments[api_index].as_str();
    if !READONLY_POST_APIS.contains(&api) {
        return false;
    }
    let subs: Vec<&str> = segments[api_index + 1..].iter().map(String::as_str).collect();
    match api {
        // POST /_search/scroll (continue a scroll) and _search/template are reads
        "_search" => subs.is_empty() || subs == ["scroll"] || subs == ["template"],
        "_msearch" => subs.is_empty() || subs == ["template"],
        "_validate" => subs.is_empty() || subs == ["query"],
        "_render" => subs.is_empty() || subs == ["template"],
        // One document ID can be taken
        "_explain" | "_termvectors" => subs.len() <= 1,
        "_sql" => subs.is_empty() || subs == ["translate"],
        // _count / _analyze / _mget / _field_caps / _pit have no sub-segments
        _ => subs.is_empty(),
    }
}

/// Returns the reason for a request that could cause index loss or deletion of all documents by mistake.
/// Handled the same as dangerous_reason for SQL (rejected if allow_dangerous_statements is
/// disabled; if enabled, the frontend shows a confirmation before execution).
fn dangerous_request_reason(method: &str, segments: &[String]) -> Option<String> {
    if segments.iter().any(|s| s == "_delete_by_query") {
        return Some(
            "_delete_by_query would delete every document matching the query.".to_string(),
        );
    }
    // Equivalent to UPDATE without WHERE: depending on the query it rewrites all documents
    if segments.iter().any(|s| s == "_update_by_query") {
        return Some(
            "_update_by_query can rewrite every document matching the query.".to_string(),
        );
    }
    // DELETE /<index> (single segment) is index deletion.
    // Comma-separated lists, wildcards and _all are also contained in one segment
    if method == "DELETE" && segments.len() == 1 {
        return Some(format!(
            "DELETE /{} would permanently delete the index (and all of its documents).",
            segments[0]
        ));
    }
    // DELETE /_data_stream/<name> deletes the data stream and its backing indices
    // together (data loss equivalent to index deletion)
    if method == "DELETE" && segments.first().is_some_and(|s| s == "_data_stream") {
        return Some(
            "DELETE /_data_stream would permanently delete the data stream \
             and all of its backing indices."
                .to_string(),
        );
    }
    None
}

/// Returns the reason for the first dangerous request in the whole input.
/// For the frontend's pre-execution confirmation dialog (called from db::dangerous_statement_reason).
/// Input that cannot be parsed gives None (it is returned as an error at execution time).
pub fn dangerous_reason_for_input(input: &str) -> Option<String> {
    let requests = parse_input(input).ok()?;
    requests.iter().find_map(|req| {
        let segments = guard_segments(&req.path).ok()?;
        dangerous_request_reason(&req.method, &segments)
    })
}

/// Runs a sequence of request blocks and returns the result (cancellable version).
/// Delegated from db::run_query_cancellable in the case of DbPool::Elasticsearch.
pub async fn run_query_cancellable(
    client: &EsClient,
    registry: &CancelRegistry,
    connection_name: &str,
    input: &str,
    max_rows: usize,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let requests = parse_input(input)?;
    if requests.is_empty() {
        return Err(AppError::Elasticsearch("The request is empty".into()));
    }

    // Validate all requests before executing anything (to avoid only part of them being executed)
    for req in &requests {
        let segments = guard_segments(&req.path)?;
        if readonly != ReadonlyGuard::Off && !is_readonly_request(&req.method, &segments) {
            return Err(readonly_block_error(readonly));
        }
        if !allow_dangerous {
            if let Some(reason) = dangerous_request_reason(&req.method, &segments) {
                return Err(dangerous_block_error(&reason));
            }
        }
        // Also detect paths that are invalid as URLs before execution
        client.build_url(&req.path)?;
        if let Some(body) = &req.body {
            classify_body(body, path_is_ndjson_api(&req.path))?;
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
    // Cancellation drops the execution future. biased checks the execution result first:
    // if the result and the cancel notification are ready at the same time, the completed result wins
    let result = tokio::select! {
        biased;
        result = execute_requests(client, &requests, max_rows) => result,
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

/// Multiple requests are not a transaction: if a request in the middle fails with a transport error,
/// execution stops there and the results so far remain. HTTP error statuses
/// (4xx / 5xx) are put in the status column as a response and execution continues (as with Kibana Console,
/// so you can see in the list which request failed).
async fn execute_requests(
    client: &EsClient,
    requests: &[EsRequest],
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    if requests.len() == 1 {
        let (status, value) = send_request(client, &requests[0]).await?;
        if !status.is_success() {
            return Err(http_status_error(status, &value));
        }
        return Ok(shape_single(value, max_rows));
    }
    let mut rows = Vec::new();
    let mut truncated = false;
    for req in requests {
        let (status, value) = send_request(client, req).await?;
        if rows.len() >= max_rows {
            truncated = true;
            continue;
        }
        rows.push(vec![
            serde_json::Value::String(req.display()),
            serde_json::json!(status.as_u16()),
            limit_cell(&value, &mut truncated),
        ]);
    }
    Ok(shape_result(
        vec![
            "request".to_string(),
            "status".to_string(),
            "result".to_string(),
        ],
        rows,
        truncated,
    ))
}

/// Sends one request and returns (status, response JSON).
/// If the response is not JSON, it is returned as a string value (text responses of `_cat` APIs etc.).
/// The response body is read up to MAX_RESPONSE_BYTES (unbounded responses are not accumulated).
async fn send_request(
    client: &EsClient,
    req: &EsRequest,
) -> Result<(reqwest::StatusCode, serde_json::Value), AppError> {
    let url = client.build_url(&req.path)?;
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|e| AppError::Elasticsearch(format!("Invalid method {}: {e}", req.method)))?;
    let mut builder = client.client.request(method, url);
    if let Some(user) = &client.user {
        builder = builder.basic_auth(user, client.password.as_deref());
    }
    if let Some(body) = &req.body {
        builder = match classify_body(body, path_is_ndjson_api(&req.path))? {
            EsBody::Json(b) => builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b),
            EsBody::Ndjson(b) => builder
                .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                .body(b),
        };
    }
    let mut response = builder.send().await.map_err(request_error)?;
    let status = response.status();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AppError::Elasticsearch(format!(
                "The response is too large (over {} MB). \
                 Narrow the request (e.g. with \"size\" or filters).",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    let value = match serde_json::from_slice::<serde_json::Value>(&buf) {
        Ok(value) => value,
        Err(_) => {
            let text = String::from_utf8_lossy(&buf).into_owned();
            serde_json::Value::String(text)
        }
    };
    Ok((status, value))
}

/// Converts a reqwest error to the app's error type (making timeouts easy to understand).
fn request_error(e: reqwest::Error) -> AppError {
    if e.is_timeout() {
        AppError::Elasticsearch(format!(
            "The request timed out after {}s",
            REQUEST_TIMEOUT.as_secs()
        ))
    } else {
        AppError::Elasticsearch(e.to_string())
    }
}

/// Formats the response of a single request into a table.
/// - `hits.hits` array -> a table of _index / _id / _score + the union of _source keys
/// - array of objects (`_cat/...?format=json` etc.) -> a table of the union of keys
/// - anything else -> pretty JSON in one cell of a single "response" column (with a character limit)
fn shape_single(value: serde_json::Value, max_rows: usize) -> QueryResult {
    if let Some(hits) = value
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|v| v.as_array())
    {
        return shape_hits(hits, max_rows);
    }
    if let Some(items) = value.as_array() {
        if !items.is_empty() && items.iter().all(|v| v.is_object()) {
            return shape_object_array(items, max_rows);
        }
    }
    let text = match &value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let (text, truncated) = truncate_chars(&text, MAX_RESPONSE_CHARS);
    shape_result(
        vec!["response".to_string()],
        vec![vec![serde_json::Value::String(text)]],
        truncated,
    )
}

/// Formats hits.hits of a search response into a table.
/// columns are `_index` / `_id` / `_score` + the union of the `_source` keys of all hits
/// (in order of appearance). Cut off at max_rows + truncated.
fn shape_hits(hits: &[serde_json::Value], max_rows: usize) -> QueryResult {
    let mut source_keys: Vec<String> = Vec::new();
    for hit in hits {
        if let Some(source) = hit.get("_source").and_then(|s| s.as_object()) {
            for key in source.keys() {
                if !source_keys.iter().any(|k| k == key) {
                    source_keys.push(key.clone());
                }
            }
        }
    }
    let mut columns = vec![
        "_index".to_string(),
        "_id".to_string(),
        "_score".to_string(),
    ];
    columns.extend(source_keys.iter().cloned());

    let mut truncated = hits.len() > max_rows;
    let mut rows = Vec::with_capacity(hits.len().min(max_rows));
    for hit in hits.iter().take(max_rows) {
        let mut row = Vec::with_capacity(columns.len());
        for meta in ["_index", "_id", "_score"] {
            row.push(limit_cell(
                hit.get(meta).unwrap_or(&serde_json::Value::Null),
                &mut truncated,
            ));
        }
        let source = hit.get("_source").and_then(|s| s.as_object());
        for key in &source_keys {
            let value = source
                .and_then(|s| s.get(key))
                .unwrap_or(&serde_json::Value::Null);
            row.push(limit_cell(value, &mut truncated));
        }
        rows.push(row);
    }
    shape_result(columns, rows, truncated)
}

/// Formats an array of objects (`_cat/indices?format=json` etc.) into a table.
/// columns are the union of the keys of all elements (in order of appearance).
fn shape_object_array(items: &[serde_json::Value], max_rows: usize) -> QueryResult {
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Some(object) = item.as_object() {
            for key in object.keys() {
                if !columns.iter().any(|k| k == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let mut truncated = items.len() > max_rows;
    let mut rows = Vec::with_capacity(items.len().min(max_rows));
    for item in items.iter().take(max_rows) {
        let object = item.as_object();
        let row = columns
            .iter()
            .map(|key| {
                limit_cell(
                    object
                        .and_then(|o| o.get(key))
                        .unwrap_or(&serde_json::Value::Null),
                    &mut truncated,
                )
            })
            .collect();
        rows.push(row);
    }
    shape_result(columns, rows, truncated)
}

fn shape_result(
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    truncated: bool,
) -> QueryResult {
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

/// Recursively truncates a JSON value to be put in a cell.
/// - Long strings are cut off at MAX_CELL_CHARS
/// - Arrays / objects are cut off at MAX_CELL_ITEMS elements (with a marker at the end)
/// - Integers beyond JS's safe integer range are turned into strings (to counter rounding at the Tauri invoke boundary)
fn limit_cell(value: &serde_json::Value, truncated: &mut bool) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let (text, cut) = truncate_chars(s, MAX_CELL_CHARS);
            if cut {
                *truncated = true;
            }
            serde_json::Value::String(text)
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                crate::db::json_i64(i)
            } else if let Some(u) = n.as_u64() {
                // A u64 for which as_i64 failed is above i64::MAX = outside the safe integer range
                serde_json::Value::String(u.to_string())
            } else {
                serde_json::Value::Number(n.clone())
            }
        }
        serde_json::Value::Array(items) => {
            let total = items.len();
            let mut out: Vec<serde_json::Value> = items
                .iter()
                .take(MAX_CELL_ITEMS)
                .map(|item| limit_cell(item, truncated))
                .collect();
            if total > MAX_CELL_ITEMS {
                *truncated = true;
                out.push(serde_json::Value::String(format!(
                    "... ({} more items truncated)",
                    total - MAX_CELL_ITEMS
                )));
            }
            serde_json::Value::Array(out)
        }
        serde_json::Value::Object(map) => {
            let total = map.len();
            let mut out = serde_json::Map::new();
            for (key, value) in map.iter().take(MAX_CELL_ITEMS) {
                out.insert(key.clone(), limit_cell(value, truncated));
            }
            if total > MAX_CELL_ITEMS {
                *truncated = true;
                out.insert(
                    "...".to_string(),
                    serde_json::Value::String(format!(
                        "({} more entries truncated)",
                        total - MAX_CELL_ITEMS
                    )),
                );
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Cuts off at the character limit (char-boundary safe). If cut off, a marker is appended at the end.
fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => {
            let mut out = text[..byte_index].to_string();
            out.push_str("\n... (response truncated)");
            (out, true)
        }
        None => (text.to_string(), false),
    }
}

// ---------------------------------------------------------------------------
// For the schema browser (TABLES pane): index list and mapping field
// ---------------------------------------------------------------------------

/// Returns the index list (for the TABLES pane).
/// System indices starting with `.` are excluded, and the result is sorted by name ascending.
pub async fn fetch_indices(client: &EsClient) -> Result<Vec<TableInfo>, AppError> {
    let req = EsRequest {
        method: "GET".into(),
        path: "/_cat/indices?format=json&h=index,status".into(),
        body: None,
    };
    let (status, value) = send_request(client, &req).await?;
    if !status.is_success() {
        return Err(http_status_error(status, &value));
    }
    Ok(parse_cat_indices(&value))
}

/// Upper limit on the number of indices returned to the TABLES pane. On a daily-rotating
/// log cluster etc. indices can reach tens of thousands, so, as in the other result-formatting
/// paths, unbounded data is not sent over IPC / to the UI (cut from the head of the name-ascending order).
const MAX_INDICES: usize = 5_000;

/// Converts the `_cat/indices?format=json` response into an index list.
fn parse_cat_indices(value: &serde_json::Value) -> Vec<TableInfo> {
    let mut indices: Vec<TableInfo> = value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("index").and_then(|v| v.as_str()))
                .filter(|name| !name.starts_with('.'))
                .map(|name| TableInfo {
                    name: name.to_string(),
                    schema: None,
                    kind: "index".to_string(),
                    qualified_name: name.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    indices.sort_by(|a, b| a.name.cmp(&b.name));
    indices.truncate(MAX_INDICES);
    indices
}

/// Validates an index name (because it is embedded in a URL path).
/// A conservative whitelist matching ES's index-name character set (lowercase alphanumerics and
/// `.` `_` `-` `+` etc.). Path separators and percent-encoding are rejected.
fn validate_index_name(name: &str) -> Result<&str, AppError> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    if !valid {
        return Err(AppError::Elasticsearch(format!(
            "Invalid index name: {name}"
        )));
    }
    Ok(name)
}

/// Returns the field list from an index's mapping (for the TABLES pane).
/// Nested fields are flattened to the `a.b` form, data_type is the mapping's type field,
/// and nullable is always true (ES has no concept of NOT NULL).
pub async fn fetch_index_columns(
    client: &EsClient,
    index: &str,
) -> Result<Vec<ColumnInfo>, AppError> {
    let index = validate_index_name(index)?;
    let req = EsRequest {
        method: "GET".into(),
        path: format!("/{index}/_mapping"),
        body: None,
    };
    let (status, value) = send_request(client, &req).await?;
    if !status.is_success() {
        return Err(http_status_error(status, &value));
    }
    Ok(parse_mapping_response(&value))
}

/// The `GET /<index>/_mapping` response (`{ "<index>": { "mappings": { "properties":
/// {...} } } }`) is converted into a field list.
fn parse_mapping_response(value: &serde_json::Value) -> Vec<ColumnInfo> {
    let mut out = Vec::new();
    // The response key is the real index name (after alias resolution), so use the first value
    let properties = value
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|v| v.get("mappings"))
        .and_then(|m| m.get("properties"))
        .and_then(|p| p.as_object());
    if let Some(properties) = properties {
        flatten_properties(properties, "", &mut out);
    }
    out
}

/// Recursively flattens mapping properties into the `a.b` form.
fn flatten_properties(
    properties: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
    out: &mut Vec<ColumnInfo>,
) {
    for (name, definition) in properties {
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(children) = definition.get("properties").and_then(|p| p.as_object()) {
            // object / nested expand their child fields
            flatten_properties(children, &full, out);
            continue;
        }
        let data_type = definition
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("object")
            .to_string();
        out.push(ColumnInfo {
            name: full,
            data_type,
            nullable: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(path: &str) -> Vec<String> {
        guard_segments(path).unwrap()
    }

    #[test]
    fn test_parse_input_single_block() {
        let requests = parse_input("GET /_cat/indices?format=json").unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/_cat/indices?format=json");
        assert_eq!(requests[0].body, None);
    }

    #[test]
    fn test_parse_input_with_body() {
        let input = "POST /books/_search\n{\n  \"query\": { \"match_all\": {} }\n}\n";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/books/_search");
        assert_eq!(
            requests[0].body.as_deref(),
            Some("{\n  \"query\": { \"match_all\": {} }\n}")
        );
    }

    #[test]
    fn test_parse_input_multiple_blocks_and_comments() {
        let input = "# comment\nGET /\n\nPUT books/_doc/1\n{\"title\": \"a\"}\n# tail comment\nHEAD /books\n";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].display(), "GET /");
        // The leading / of the path is filled in
        assert_eq!(requests[1].path, "/books/_doc/1");
        assert_eq!(requests[1].body.as_deref(), Some("{\"title\": \"a\"}"));
        assert_eq!(requests[2].method, "HEAD");
        assert_eq!(requests[2].body, None);
    }

    #[test]
    fn test_parse_input_ndjson_body() {
        let input = "POST /_bulk\n{\"index\":{\"_id\":\"1\"}}\n{\"title\":\"a\"}\n{\"index\":{\"_id\":\"2\"}}\n{\"title\":\"b\"}";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 1);
        let body = requests[0].body.as_deref().unwrap();
        assert_eq!(body.lines().count(), 4);
        // Classified as NDJSON, and a trailing newline is added
        match classify_body(body, true).unwrap() {
            EsBody::Ndjson(b) => assert!(b.ends_with('\n')),
            EsBody::Json(_) => panic!("expected NDJSON"),
        }
    }

    #[test]
    fn test_parse_input_lowercase_method() {
        let requests = parse_input("get /_cluster/health").unwrap();
        assert_eq!(requests[0].method, "GET");
    }

    #[test]
    fn test_parse_input_errors() {
        // There is body text before the method line
        assert!(parse_input("{\"a\": 1}").is_err());
        // No path
        assert!(parse_input("GET").is_err());
        // Extra token after the path
        assert!(parse_input("GET /a extra").is_err());
        // Empty input is not an error but empty (the error is raised on the run_query_cancellable side)
        assert!(parse_input("").unwrap().is_empty());
        assert!(parse_input("# only comment\n").unwrap().is_empty());
    }

    #[test]
    fn test_classify_body() {
        assert!(matches!(
            classify_body("{\"a\": 1}", false).unwrap(),
            EsBody::Json(_)
        ));
        // Pretty JSON is also a single document
        assert!(matches!(
            classify_body("{\n  \"a\": 1\n}", false).unwrap(),
            EsBody::Json(_)
        ));
        assert!(matches!(
            classify_body("{\"a\":1}\n{\"b\":2}", false).unwrap(),
            EsBody::Ndjson(_)
        ));
        assert!(classify_body("not json", false).is_err());
        assert!(classify_body("{\"a\":1}\nbroken", false).is_err());
        // A body for NDJSON-type APIs (_bulk / _msearch) is classified as NDJSON even when it is one line,
        // and gets a trailing newline (the Bulk API's trailing-newline-required rule)
        match classify_body("{\"delete\":{\"_id\":\"1\"}}", true).unwrap() {
            EsBody::Ndjson(b) => assert_eq!(b, "{\"delete\":{\"_id\":\"1\"}}\n"),
            EsBody::Json(_) => panic!("expected NDJSON for a bulk body"),
        }
        // A pretty JSON multi-line body is invalid line by line for NDJSON APIs -> error
        assert!(classify_body("{\n  \"a\": 1\n}", true).is_err());
    }

    #[test]
    fn test_tls_tunnel_url_host() {
        use crate::config::SshTunnelConfig;
        let mut server = ServerConfig {
            name: "es".into(),
            description: None,
            folder_name: None,
            engine: "elasticsearch".into(),
            host: Some("es.example.com".into()),
            port: Some(9200),
            schema: None,
            user: None,
            password: None,
            ssh_tunnel: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
            tls: true,
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
        };
        // Without a tunnel the URL host is not swapped
        assert_eq!(tls_tunnel_url_host(&server, "es.example.com"), None);
        // With a tunnel + TLS + destination on a local port, return the original host name
        server.ssh_tunnel = Some(SshTunnelConfig {
            host: "bastion".into(),
            port: 22,
            user: "u".into(),
            ssh_config: None,
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: None,
        });
        assert_eq!(
            tls_tunnel_url_host(&server, "127.0.0.1"),
            Some("es.example.com".to_string())
        );
        // Without TLS no swap is needed (there is no certificate verification)
        server.tls = false;
        assert_eq!(tls_tunnel_url_host(&server, "127.0.0.1"), None);
    }

    #[test]
    fn test_path_is_ndjson_api() {
        assert!(path_is_ndjson_api("/_bulk"));
        assert!(path_is_ndjson_api("/books/_bulk"));
        assert!(path_is_ndjson_api("/_msearch"));
        assert!(path_is_ndjson_api("/books/_msearch?typed_keys=true"));
        assert!(!path_is_ndjson_api("/books/_search"));
        assert!(!path_is_ndjson_api("/"));
    }

    #[test]
    fn test_guard_segments() {
        assert_eq!(segs("/books/_search?size=1"), vec!["books", "_search"]);
        assert_eq!(segs("/"), Vec::<String>::new());
        assert_eq!(segs("//a//b"), vec!["a", "b"]);
        // Dot segments (including encoded forms) are rejected
        assert!(guard_segments("/a/../_bulk").is_err());
        assert!(guard_segments("/a/./b").is_err());
        assert!(guard_segments("/a/%2e%2e/_bulk").is_err());
        assert!(guard_segments("/a/.%2E/b").is_err());
        // Backslash becomes / in URL normalization and diverges from the validated path, so reject
        assert!(guard_segments("/a\\..\\_delete_by_query").is_err());
        assert!(guard_segments("/books\\x").is_err());
        // Encoded separators / underscore are defensively rejected
        assert!(guard_segments("/books%2F_delete_by_query").is_err());
        assert!(guard_segments("/books/%5Fdelete_by_query").is_err());
        assert!(guard_segments("/books/%5c..").is_err());
    }

    #[test]
    fn test_is_readonly_request() {
        let ro = |method: &str, path: &str| is_readonly_request(method, &segs(path));
        // GET / HEAD are always allowed
        assert!(ro("GET", "/books/_search"));
        assert!(ro("GET", "/"));
        assert!(ro("HEAD", "/books"));
        // POST only for search-type
        assert!(ro("POST", "/_search"));
        assert!(ro("POST", "/books/_search?size=1"));
        assert!(ro("POST", "/_search/scroll"));
        assert!(ro("POST", "/books/_msearch"));
        assert!(ro("POST", "/books/_count"));
        assert!(ro("POST", "/books/_analyze"));
        assert!(ro("POST", "/_mget"));
        assert!(ro("POST", "/books/_field_caps"));
        assert!(ro("POST", "/books/_validate/query"));
        assert!(ro("POST", "/books/_explain/1"));
        assert!(ro("POST", "/books/_termvectors/1"));
        assert!(ro("POST", "/books/_pit"));
        assert!(ro("POST", "/_sql"));
        assert!(ro("POST", "/_sql/translate"));
        assert!(ro("POST", "/_render/template"));
        // Write-type POST is rejected
        assert!(!ro("POST", "/books/_doc"));
        assert!(!ro("POST", "/_bulk"));
        assert!(!ro("POST", "/books/_update/1"));
        assert!(!ro("POST", "/books/_delete_by_query"));
        assert!(!ro("POST", "/books"));
        // Slip-through using a whitelisted name as a document ID is rejected
        assert!(!ro("POST", "/books/_doc/_search"));
        assert!(!ro("POST", "/books/_update/_count"));
        // PUT / DELETE / PATCH always require Writable
        assert!(!ro("PUT", "/books/_doc/1"));
        assert!(!ro("DELETE", "/books/_doc/1"));
        assert!(!ro("PATCH", "/books"));
    }

    #[test]
    fn test_dangerous_request_reason() {
        let danger =
            |method: &str, path: &str| dangerous_request_reason(method, &segs(path));
        // Index deletion (single segment; includes wildcard / comma / _all)
        assert!(danger("DELETE", "/books").is_some());
        assert!(danger("DELETE", "/logs-*,metrics-*").is_some());
        assert!(danger("DELETE", "/_all").is_some());
        // Document deletion and scroll release are out of scope (the readonly guard side requires Writable)
        assert!(danger("DELETE", "/books/_doc/1").is_none());
        assert!(danger("DELETE", "/_search/scroll").is_none());
        // _delete_by_query is dangerous regardless of the method
        assert!(danger("POST", "/books/_delete_by_query").is_some());
        assert!(danger("POST", "/books/_delete_by_query?conflicts=proceed").is_some());
        // _update_by_query, equivalent to UPDATE without WHERE, is also dangerous
        assert!(danger("POST", "/books/_update_by_query").is_some());
        // Data stream deletion (backing indices vanish with it) is also dangerous
        assert!(danger("DELETE", "/_data_stream/logs").is_some());
        assert!(danger("DELETE", "/_data_stream/logs-*").is_some());
        // GET /_data_stream (listing) is not dangerous
        assert!(danger("GET", "/_data_stream/logs").is_none());
        // Reads are out of scope
        assert!(danger("GET", "/books/_search").is_none());
        assert!(danger("PUT", "/books/_doc/1").is_none());
    }

    #[test]
    fn test_dangerous_reason_for_input() {
        assert!(dangerous_reason_for_input("GET /books/_search").is_none());
        assert!(dangerous_reason_for_input("DELETE /books").is_some());
        assert!(dangerous_reason_for_input(
            "GET /\nPOST /books/_delete_by_query\n{\"query\":{\"match_all\":{}}}"
        )
        .is_some());
        // Input that cannot be parsed gives None (returned as an error at execution time)
        assert!(dangerous_reason_for_input("{\"a\": 1}").is_none());
    }

    #[test]
    fn test_shape_single_hits() {
        let value = serde_json::json!({
            "took": 3,
            "hits": {
                "total": {"value": 3},
                "hits": [
                    {"_index": "books", "_id": "1", "_score": 1.0,
                     "_source": {"title": "a", "year": 2001}},
                    {"_index": "books", "_id": "2", "_score": 0.5,
                     "_source": {"title": "b", "author": "x"}},
                    {"_index": "books", "_id": "3", "_score": null,
                     "_source": {"title": "c"}}
                ]
            }
        });
        let result = shape_single(value, 10);
        assert_eq!(
            result.columns,
            vec!["_index", "_id", "_score", "title", "year", "author"]
        );
        assert_eq!(result.row_count, 3);
        assert!(!result.truncated);
        assert_eq!(result.rows[0][0], serde_json::json!("books"));
        assert_eq!(result.rows[0][3], serde_json::json!("a"));
        assert_eq!(result.rows[0][4], serde_json::json!(2001));
        // A key that is in the union but missing from its own _source is null
        assert_eq!(result.rows[0][5], serde_json::Value::Null);
        assert_eq!(result.rows[2][2], serde_json::Value::Null);
    }

    #[test]
    fn test_shape_single_hits_truncation() {
        let hits: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({"_id": i.to_string(), "_source": {"n": i}}))
            .collect();
        let value = serde_json::json!({"hits": {"hits": hits}});
        let result = shape_single(value, 2);
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);
    }

    #[test]
    fn test_shape_single_object_array() {
        // Equivalent to _cat/indices?format=json
        let value = serde_json::json!([
            {"index": "books", "status": "open"},
            {"index": "logs", "status": "open", "extra": 1}
        ]);
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["index", "status", "extra"]);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.rows[0][2], serde_json::Value::Null);

        // Truncation
        let value = serde_json::json!([{"a": 1}, {"a": 2}, {"a": 3}]);
        let result = shape_single(value, 2);
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);
    }

    #[test]
    fn test_shape_single_scalar_response() {
        let value = serde_json::json!({"acknowledged": true});
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["response"]);
        assert_eq!(result.row_count, 1);
        assert!(!result.truncated);
        let cell = result.rows[0][0].as_str().unwrap();
        assert!(cell.contains("\"acknowledged\": true"));

        // A text response (a non-JSON _cat response etc.) goes as is into one cell
        let value = serde_json::Value::String("green open books".into());
        let result = shape_single(value, 10);
        assert_eq!(result.rows[0][0], serde_json::json!("green open books"));

        // A mixed array (containing non-object elements) is not turned into a table
        let value = serde_json::json!([{"a": 1}, 2]);
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["response"]);
    }

    #[test]
    fn test_truncate_chars() {
        let (text, cut) = truncate_chars("hello", 10);
        assert_eq!(text, "hello");
        assert!(!cut);
        let (text, cut) = truncate_chars(&"x".repeat(20), 5);
        assert!(cut);
        assert!(text.starts_with("xxxxx"));
        assert!(text.ends_with("(response truncated)"));
        // Does not panic even at a multibyte boundary
        let (text, cut) = truncate_chars("あいうえお", 2);
        assert!(cut);
        assert!(text.starts_with("あい"));
    }

    #[test]
    fn test_limit_cell() {
        let mut truncated = false;
        // Integers beyond the safe integer range are turned into strings
        let v = limit_cell(&serde_json::json!(9007199254740993_i64), &mut truncated);
        assert_eq!(v, serde_json::json!("9007199254740993"));
        assert!(!truncated);
        let v = limit_cell(&serde_json::json!(18446744073709551615_u64), &mut truncated);
        assert_eq!(v, serde_json::json!("18446744073709551615"));
        // Integers and floats within range stay as they are
        assert_eq!(
            limit_cell(&serde_json::json!(42), &mut truncated),
            serde_json::json!(42)
        );
        assert_eq!(
            limit_cell(&serde_json::json!(1.5), &mut truncated),
            serde_json::json!(1.5)
        );
        assert!(!truncated);

        // Long strings are cut off
        let mut truncated = false;
        let long = "y".repeat(MAX_CELL_CHARS + 10);
        let v = limit_cell(&serde_json::json!(long), &mut truncated);
        assert!(truncated);
        assert!(v.as_str().unwrap().len() < MAX_CELL_CHARS + 100);

        // A large array is cut off by element count + marker
        let mut truncated = false;
        let big: Vec<u32> = (0..(MAX_CELL_ITEMS as u32 + 5)).collect();
        let v = limit_cell(&serde_json::json!(big), &mut truncated);
        assert!(truncated);
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), MAX_CELL_ITEMS + 1);
        assert!(items[MAX_CELL_ITEMS]
            .as_str()
            .unwrap()
            .contains("truncated"));

        // Nested objects are processed recursively too
        let mut truncated = false;
        let v = limit_cell(
            &serde_json::json!({"nested": {"big": 9007199254740993_i64}}),
            &mut truncated,
        );
        assert_eq!(v["nested"]["big"], serde_json::json!("9007199254740993"));
    }

    #[test]
    fn test_parse_cat_indices() {
        let value = serde_json::json!([
            {"index": "logs", "status": "open"},
            {"index": ".internal-system", "status": "open"},
            {"index": "books", "status": "open"}
        ]);
        let indices = parse_cat_indices(&value);
        // System indices (starting with .) are excluded, sorted by name ascending
        let names: Vec<&str> = indices.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["books", "logs"]);
        assert!(indices.iter().all(|t| t.kind == "index"));
        assert!(indices.iter().all(|t| t.schema.is_none()));
        assert!(indices.iter().all(|t| t.name == t.qualified_name));
        // A non-array response is empty
        assert!(parse_cat_indices(&serde_json::json!({"error": "x"})).is_empty());
    }

    #[test]
    fn test_validate_index_name() {
        assert!(validate_index_name("books").is_ok());
        assert!(validate_index_name("logs-2026.07.24").is_ok());
        assert!(validate_index_name("my_index+v2").is_ok());
        for bad in ["", ".", "..", "a/b", "a b", "a?b", "a%2Fb", "a#b", "a*"] {
            assert!(validate_index_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn test_parse_mapping_response() {
        let value = serde_json::json!({
            "books": {
                "mappings": {
                    "properties": {
                        "title": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
                        "year": {"type": "integer"},
                        "author": {
                            "properties": {
                                "name": {"type": "text"},
                                "age": {"type": "integer"}
                            }
                        },
                        "misc": {}
                    }
                }
            }
        });
        let columns = parse_mapping_response(&value);
        let summary: Vec<(&str, &str)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str()))
            .collect();
        // serde_json's Map is in ascending key order. Nesting is flattened to the a.b form
        assert_eq!(
            summary,
            vec![
                ("author.age", "integer"),
                ("author.name", "text"),
                ("misc", "object"),
                ("title", "text"),
                ("year", "integer"),
            ]
        );
        assert!(columns.iter().all(|c| c.nullable));
        // A response without mappings is empty
        assert!(parse_mapping_response(&serde_json::json!({})).is_empty());
        assert!(
            parse_mapping_response(&serde_json::json!({"books": {"mappings": {}}}))
                .is_empty()
        );
    }
}
