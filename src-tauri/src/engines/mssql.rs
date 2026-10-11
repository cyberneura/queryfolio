//! Microsoft SQL Server engine (engine: mssql / sqlserver).
//!
//! A SQL engine, but sqlx has no driver for it, so it is wired up directly with the `tiberius` crate
//! (a pure Rust implementation of the TDS protocol). The shared SQL guards (meta-command conversion ->
//! readonly -> dangerous) reuse the existing logic in db.rs as is; only the parts T-SQL lacks are
//! added here:
//!
//! - **auto LIMIT is `TOP (n)`**, inserted right after SELECT (after DISTINCT / ALL)
//!   (`apply_auto_top`). T-SQL has no LIMIT clause. It is not added to statements containing TOP /
//!   OFFSET / FETCH / UNION etc., since the meaning could change (the conservative side; even
//!   without it, the max_rows cutoff acts as a safety net).
//! - **EXPLAIN is a queryfolio pseudo-statement**. T-SQL has no EXPLAIN, so on receiving
//!   `EXPLAIN <select>` we run three batches on the same connection: `SET SHOWPLAN_ALL ON` ->
//!   the target statement -> `SET SHOWPLAN_ALL OFF`, and return the estimated execution plan rows
//!   as the result (SHOWPLAN does not execute the target statement).
//! - **There is no read-only transaction**. The agent path
//!   (ReadonlyGuard::Agent) wraps the statement in `BEGIN TRANSACTION` and always issues
//!   `ROLLBACK` regardless of the result. Writes that slip past the statement-level guard are undone,
//!   but `NEXT VALUE FOR` (sequences) and IDENTITY consumption are not rolled back
//!   (SQL Server behavior). The README states that this is weaker than READ ONLY in Postgres / DuckDB.
//!   **This path opens a dedicated connection and discards it after execution**: SQL Server's
//!   nested transactions are not independent, so if the user left a `BEGIN TRANSACTION` open on the
//!   same connection, the agent's ROLLBACK would also undo those uncommitted changes.
//! - **The user's connection is a single one**, kept in a `Mutex<Option<Client>>` to serialize
//!   execution. Cancellation aborts the execution future (`CancelTarget::ClientSide`), then sends
//!   `cancel_query` (TDS Attention) on the same Client to stop server-side execution, and
//!   **discards that connection** (it is re-established on the next execution). The cleanup of
//!   `SET SHOWPLAN_ALL ON` or a transaction opened inside the aborted future has not run, so
//!   reusing the connection would carry that state into the next statement. Attention does not
//!   roll back transactions, but closing the connection makes the server clean up the whole session.
//! - **Connections support only SQL Server authentication with `user` / `password`**. Windows
//!   integrated authentication, Azure AD and named instances (SQL Browser) are not supported.
//! - **TLS maps `ssl_mode` / `tls` to tiberius's EncryptionLevel**. TDS encryption is decided
//!   by the combination of what the client and server offer (tiberius's
//!   `negotiated_encryption`): disable = NotSupported (still encrypted if the server requires it),
//!   prefer = Off (the login packet is always encrypted, and the rest is encrypted only if the
//!   server offers On / Required; the certificate is not verified. Offering `On` causes a
//!   protocol error with servers offering Off / NotSupported, so it cannot downgrade to
//!   plaintext), require = Required (no verification), verify-ca /
//!   verify-full = Required + verification (`ssl_root_cert`, if set, is trusted as an extra CA).
//!   SQL Server's TLS has no setting to verify only the chain without checking the hostname, so
//!   verify-ca is the same as verify-full (erring on the strict side). Over an SSH tunnel the
//!   destination becomes 127.0.0.1, so the configured `host` is used for certificate hostname
//!   verification (`hostname_in_certificate`).
//! - **The TLS backend is vendored OpenSSL** (tiberius's `vendored-openssl` =
//!   opentls). The default native-tls does not work with SQL Server's TLS on macOS's Security
//!   Framework (stated in tiberius's README). rustls, with tokio-rustls's default
//!   feature (aws_lc_rs), would bring a native build of aws-lc-sys into CI.
//!   This OpenSSL is the same openssl-src that ssh2 already links statically.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql, ToSql};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::{ServerConfig, SqlSslMode};
use crate::db::{
    bytes_to_json, dangerous_block_error, dangerous_reason, is_fetch_statement,
    is_readonly_allowed, json_i64, leading_keyword, readonly_block_error, scan_sql,
    strip_leading_comments, CancelRegistry, CancelTarget, Engine, QueryResult, ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

/// The default SQL Server port.
pub const DEFAULT_PORT: u16 = 1433;

/// Timeout for the whole connection (TCP + TDS handshake + login).
/// Required so that get_pool (while holding DbManager's lock) is never blocked indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper limit for sending cancellation (Attention) and draining the response.
/// If exceeded, the connection is discarded and re-established.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of characters for a string in one cell (NVARCHAR / VARBINARY / XML).
/// The excess is cut off and truncated is set (no unbounded values are sent to the webview).
const MAX_TEXT_CHARS: usize = 10_000;

/// The default schema that unqualified table names belong to.
/// (Treated like public in schema_info::build_qualified_name, i.e. not qualified.)
const DEFAULT_SCHEMA: &str = "dbo";

/// The body of the `EXPLAIN` pseudo-statement. build_explain_sql (db.rs) prefixes this word.
const EXPLAIN_KEYWORD: &str = "explain";

type MsSqlClient = Client<Compat<TcpStream>>;

/// Handle for a SQL Server connection. Held as DbPool::MsSql.
/// `client` is the single connection for user operations (if absent, it is re-established on the
/// next execution; the agent path opens a dedicated connection each time, so it never goes here).
/// The tokio Mutex serializes execution per connection: cancel registration (CancelRegistry) holds
/// one entry per connection name, so if a second execution on the same connection ran concurrently,
/// the registration would be overwritten and cancellation would get mixed up (same reason as duckdb).
#[derive(Clone)]
pub struct MsSqlHandle {
    config: Arc<Config>,
    client: Arc<tokio::sync::Mutex<Option<MsSqlClient>>>,
}

// Config holds the password, so Debug is not derived; only the name is shown
impl std::fmt::Debug for MsSqlHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MsSqlHandle")
    }
}

/// Builds a tiberius Config from the settings (does not connect).
/// `host` / `port` are the destination after any SSH tunnel substitution. The configured
/// `server.host` is used for certificate hostname verification.
fn build_config(server: &ServerConfig, host: &str, port: u16) -> Result<Config, AppError> {
    let Some(user) = server
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return Err(AppError::Config(format!(
            "Server '{}': mssql needs user and password (SQL Server authentication)",
            server.name
        )));
    };
    let mut config = Config::new();
    config.host(host);
    config.port(port);
    if let Some(database) = server
        .schema
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        config.database(database);
    }
    config.authentication(AuthMethod::sql_server(
        user,
        server.password.clone().unwrap_or_default(),
    ));
    config.application_name("queryfolio");
    // The default 30 seconds is the limit for waiting for the next response from the server, and
    // heavy aggregations fail on it. Cancellation exists for stopping, so make it unlimited
    config.command_timeout(None);
    config.handshake_timeout(Some(CONNECT_TIMEOUT));

    match server.sql_ssl_mode()? {
        SqlSslMode::Disable => config.encryption(EncryptionLevel::NotSupported),
        // Off = "encrypt only the login packet and follow the server's offer for the rest".
        // With On, servers returning Off / NotSupported cause a protocol error, which would break
        // the prefer promise (downgrade if a connection cannot be made) (Codex review finding)
        SqlSslMode::Prefer => {
            config.encryption(EncryptionLevel::Off);
            config.trust_cert();
        }
        SqlSslMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        SqlSslMode::VerifyCa | SqlSslMode::VerifyFull => {
            config.encryption(EncryptionLevel::Required);
            if let Some(path) = crate::db::ssl_root_cert_path(server)? {
                config.trust_cert_ca(path.display().to_string());
            }
            // Even through a tunnel (destination 127.0.0.1), the certificate is verified against the
            // configured hostname. If host is unset it targets localhost, so leave it as is
            if let Some(configured) = server
                .host
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
            {
                if configured != host {
                    config.hostname_in_certificate(configured);
                }
            }
        }
    }
    Ok(config)
}

/// Returns a Client that has opened TCP and completed the TDS login.
async fn open_client(config: &Config) -> Result<MsSqlClient, AppError> {
    let connect = async {
        let tcp = TcpStream::connect(config.get_addr()).await.map_err(|e| {
            AppError::MsSql(format!("Could not connect to {}: {e}", config.get_addr()))
        })?;
        tcp.set_nodelay(true)
            .map_err(|e| AppError::MsSql(format!("Could not configure the socket: {e}")))?;
        let client = Client::connect(config.clone(), tcp.compat_write()).await?;
        Ok::<_, AppError>(client)
    };
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| {
            AppError::MsSql(format!(
                "Connection to {} timed out after {}s",
                config.get_addr(),
                CONNECT_TIMEOUT.as_secs()
            ))
        })?
}

/// Establishes a connection. So that configuration errors (auth, TLS, database) show up here,
/// returns a Client that has already completed the login.
pub async fn connect(
    server: &ServerConfig,
    host: &str,
    port: u16,
) -> Result<MsSqlHandle, AppError> {
    let config = build_config(server, host, port)?;
    let client = open_client(&config).await?;
    Ok(MsSqlHandle {
        config: Arc::new(config),
        client: Arc::new(tokio::sync::Mutex::new(Some(client))),
    })
}

/// Takes the Client out of the slot (re-establishes it if absent).
async fn take_client(
    handle: &MsSqlHandle,
    slot: &mut Option<MsSqlClient>,
) -> Result<MsSqlClient, AppError> {
    match slot.take() {
        Some(client) => Ok(client),
        None => open_client(&handle.config).await,
    }
}

/// A failed execution. `reusable` says whether the connection may be returned to the slot (a SQL
/// error returned by the server does not break the connection, but I/O and protocol errors mean
/// a connection cut off midway, so it is discarded).
struct ExecFailure {
    error: AppError,
    reusable: bool,
}

impl From<tiberius::error::Error> for ExecFailure {
    fn from(e: tiberius::error::Error) -> Self {
        let reusable = matches!(e, tiberius::error::Error::Server(_));
        ExecFailure {
            error: e.into(),
            reusable,
        }
    }
}

impl From<AppError> for ExecFailure {
    fn from(error: AppError) -> Self {
        ExecFailure {
            error,
            reusable: true,
        }
    }
}

type ExecResult = Result<QueryResult, ExecFailure>;

/// Runs SQL and returns the result (cancellable version).
/// Delegated from db::run_query_cancellable for DbPool::MsSql.
#[allow(clippy::too_many_arguments)]
pub async fn run_query_cancellable(
    handle: &MsSqlHandle,
    registry: &CancelRegistry,
    connection_name: &str,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    // psql-style meta-commands are converted to catalog query SQL. \c / USE are handled first by
    // run_query in lib.rs (reaching here means the agent path, so they are rejected)
    let translated = match crate::meta_commands::translate(Engine::MsSql, sql)? {
        Some(crate::meta_commands::MetaCommand::Sql(sql)) => Some(sql),
        Some(crate::meta_commands::MetaCommand::Connect(_)) => {
            return Err(AppError::Config(
                "Switching the active database (\\c / USE) is not available here".into(),
            ));
        }
        None => None,
    };
    let sql = translated.as_deref().unwrap_or(sql);

    if leading_keyword(sql).is_empty() {
        return Err(AppError::Config("The SQL statement is empty".into()));
    }

    // The agent path uses a narrow whitelist (same reason as run_query_on in db.rs)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) = crate::db::agent_rejection_reason(sql, Engine::MsSql) {
            return Err(AppError::Readonly(reason));
        }
    }
    // A multi-statement batch slips past a guard that only looks at the first statement, so reject it
    // if the guard is enabled. T-SQL allows omitting semicolons, so besides the presence of `;`, also
    // check whether a keyword that could start a second statement follows (contains_trailing_statement)
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && (crate::db::contains_multiple_statements(sql, Engine::MsSql)
            || contains_trailing_statement(sql))
    {
        return Err(crate::db::multi_statement_block_error());
    }
    if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, Engine::MsSql) {
        return Err(readonly_block_error(readonly));
    }
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, Engine::MsSql) {
            return Err(dangerous_block_error(reason));
        }
    }

    // EXPLAIN is a queryfolio pseudo-statement (SHOWPLAN). Stripped after the guard
    // (the check runs before stripping, so the readonly guard lets EXPLAIN through as a fetch statement)
    let explain_target = explain_target(sql);

    // Insert TOP (n) into a SELECT without a LIMIT (not applied to SQL after meta-command
    // conversion or to EXPLAIN; same as run_query_on in db.rs)
    let mut applied_limit = None;
    let limited_sql;
    let sql = match auto_limit {
        Some(limit) if limit > 0 && translated.is_none() && explain_target.is_none() => {
            match apply_auto_top(sql, limit) {
                Some(with_top) => {
                    limited_sql = with_top;
                    applied_limit = Some(limit);
                    limited_sql.as_str()
                }
                None => sql,
            }
        }
        _ => sql,
    };

    // Serialize execution per connection, then register the cancel target
    let mut slot = handle.client.lock().await;
    let readonly_tx = readonly == ReadonlyGuard::Agent;
    // The agent path runs on a dedicated connection and discards it afterwards. Running on the user's
    // connection would let the agent's ROLLBACK catch a transaction the user left open
    // (SQL Server's nested transactions are not independent)
    let mut client = if readonly_tx {
        open_client(&handle.config).await?
    } else {
        take_client(handle, &mut slot).await?
    };

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

    // Cancellation aborts the execution future. biased checks the result side first
    // (if the result and the cancel notification are ready at the same time, the completed result wins)
    let result = tokio::select! {
        biased;
        result = execute(&mut client, sql, explain_target, max_rows, readonly_tx) => Some(result),
        _ = notify.notified() => None,
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);

    let result = match result {
        None => {
            // Merely dropping the future leaves the server running the statement. Send Attention to
            // stop it (waiting for the response up to CANCEL_TIMEOUT). The connection is discarded, not returned:
            // the cleanup of `SET SHOWPLAN_ALL ON` or a transaction opened inside the aborted future
            // has not run, so returning it would carry that state into the next statement.
            // Closing it makes the server clean up the whole session
            let _ = tokio::time::timeout(CANCEL_TIMEOUT, client.cancel_query()).await;
            drop(client);
            return Err(AppError::Cancelled);
        }
        Some(result) => result,
    };

    // Dedicated connections (agent path) are not returned. The user's connection is healthy if it
    // was a SQL error returned by the server, so return it; discard it on I/O or protocol errors
    let reusable = !readonly_tx
        && match &result {
            Ok(_) => true,
            Err(failure) => failure.reusable,
        };
    if reusable {
        *slot = Some(client);
    }

    match result {
        Ok(mut result) => {
            result.applied_limit = applied_limit;
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        }
        Err(failure) => {
            // An error after a cancel request is returned as "cancelled"
            if was_cancelled {
                return Err(AppError::Cancelled);
            }
            Err(failure.error)
        }
    }
}

/// Words that, when they appear anywhere but the start, mean "a second statement has started". T-SQL can
/// omit the statement-separating `;`, so `SELECT 1\nDROP TABLE t` slips past
/// contains_multiple_statements (which counts `;`), passes the readonly / dangerous guards on just the
/// leading SELECT, and the whole batch is executed (Codex review finding). If a write, control or
/// execute keyword follows, reject it on connections where the guard is enabled.
/// `select` / `with` are not included: subqueries and `WITH (NOLOCK)` hints appear
/// normally within one statement (a following DML is caught by the delete word even in `WITH ... DELETE`).
/// `set` is not included either: it always appears inside `UPDATE ... SET` (a following
/// `SET NOCOUNT ON` is not a write, so missing it is fine).
/// A statement written with a bare column name like `update` is wrongly rejected
/// (it passes if written in square brackets; it can be lifted with Writable ON + allow_dangerous_statements).
const TRAILING_STATEMENT_KEYWORDS: &[&str] = &[
    "insert", "update", "delete", "merge", "create", "alter", "drop", "truncate", "grant",
    "revoke", "deny", "exec", "execute", "declare", "use", "begin", "commit", "rollback", "save",
    "backup", "restore", "bulk", "kill", "go", "dbcc", "shutdown",
];

/// Whether another statement starts after the first one (a multi-statement batch with no semicolon).
/// The check uses word boundaries on cleaned, where literals, comments and brackets are blanked.
fn contains_trailing_statement(sql: &str) -> bool {
    let cleaned = scan_sql(sql, Engine::MsSql).cleaned;
    cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '#')
        .filter(|w| !w.is_empty())
        .skip(1)
        .any(|word| TRAILING_STATEMENT_KEYWORDS.contains(&word))
}

/// If `EXPLAIN <sql>`, returns the target SQL (leading comments are not kept).
fn explain_target(sql: &str) -> Option<&str> {
    if leading_keyword(sql) != EXPLAIN_KEYWORD {
        return None;
    }
    let rest = strip_leading_comments(sql);
    let target = rest[EXPLAIN_KEYWORD.len()..].trim_start();
    if target.is_empty() {
        return None;
    }
    Some(target)
}

/// Runs one statement (dispatches inside/outside a transaction and SHOWPLAN).
async fn execute(
    client: &mut MsSqlClient,
    sql: &str,
    explain_target: Option<&str>,
    max_rows: usize,
    readonly_tx: bool,
) -> ExecResult {
    if let Some(target) = explain_target {
        // SHOWPLAN does not execute the target statement, so no transaction wrapping is needed
        return run_showplan(client, target, max_rows).await;
    }
    if !readonly_tx {
        return execute_statement(client, sql, max_rows).await;
    }
    // Agent path (dedicated connection): even if a write slips through, the rollback undoes it.
    // This connection never runs user statements, so there is no outer
    // transaction for the ROLLBACK to catch
    drain(client.simple_query("BEGIN TRANSACTION").await?).await?;
    let result = execute_statement(client, sql, max_rows).await;
    // Only reads were done, so COMMIT is not needed. ROLLBACK is accepted even if the
    // transaction is aborted by a statement error. If ROLLBACK itself fails, the connection is
    // discarded so as not to return one with a transaction left open
    let rollback = async {
        drain(
            client
                .simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
                .await?,
        )
        .await
    }
    .await;
    match (result, rollback) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(failure)) => Err(ExecFailure {
            error: failure.error,
            reusable: false,
        }),
        (Err(failure), Ok(())) => Err(failure),
        (Err(failure), Err(_)) => Err(ExecFailure {
            error: failure.error,
            reusable: false,
        }),
    }
}

/// If the leading keyword is one of these, the statement is treated as "returns no rows, and the affected row
/// count is meaningful" and run via sp_executesql (execute). Anything else is run as a batch (simple_query)
/// and any result set is turned into a table (affected rows is None). Two reasons for leaning toward the batch side:
/// - For scripts starting with SELECT-like statements, EXEC, `IF EXISTS (...) SELECT ...` or
///   DECLARE, the leading keyword does not reveal the contents. Lean toward losing the affected
///   row count rather than losing rows
/// - **sp_executesql runs in a procedure scope**, so session settings made with `SET`,
///   `BEGIN TRANSACTION` and `#temp` creation inside it vanish when the call
///   ends (a transaction fails with error 266). Statements that must leave state on the connection
///   have to be run as a batch, or they become "gone by the next statement" (Codex review finding)
const NO_ROWS_KEYWORDS: &[&str] = &[
    "insert", "update", "delete", "merge", "create", "alter", "drop", "truncate", "grant",
    "revoke", "deny", "backup", "restore", "bulk",
];

/// Whether the statement can return rows / leaves state on the connection (= run as a batch).
/// The readonly guard is separate; only Writable connections or read statements reach here.
fn is_mssql_fetch(sql: &str) -> bool {
    if is_fetch_statement(sql) {
        return true;
    }
    let cleaned = scan_sql(sql, Engine::MsSql).cleaned;
    let first = cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '#')
        .find(|w| !w.is_empty())
        .unwrap_or("");
    if !NO_ROWS_KEYWORDS.contains(&first) {
        return true;
    }
    // Statements touching local temp tables (`#t` / `##t`) are run as a batch, since creating them in
    // sp_executesql's scope makes them vanish when the call ends. `#` is checked after
    // scan_sql has blanked the inside of strings, comments and brackets.
    // Temp tables written in brackets (`[#t]`) are blanked and invisible, so the raw `[#` is
    // also checked (it also reacts to `[#` inside a string, but that merely leans toward the batch
    // side with no harm: the affected row count becomes None)
    if cleaned.contains('#') || sql.contains("[#") {
        return true;
    }
    // INSERT / UPDATE / DELETE / MERGE ... OUTPUT return rows
    cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == "output")
}

/// Runs one statement. Statements returning rows are run as a batch (simple_query) and the first
/// result set becomes a table; everything else uses sp_executesql (execute) to get only the affected row count.
async fn execute_statement(client: &mut MsSqlClient, sql: &str, max_rows: usize) -> ExecResult {
    if !is_mssql_fetch(sql) {
        let affected = client.execute(sql, &[]).await?.total();
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
    fetch_first_result_set(client, sql, max_rows).await
}

/// Runs a batch and returns the first result set as a table.
/// The second and later result sets (multi-statement batches or procedures returning several SELECTs)
/// are not read and are cut off. tiberius drains the rest of the response on the next execution
/// (each Client method calls flush_stream at the start).
async fn fetch_first_result_set(
    client: &mut MsSqlClient,
    sql: &str,
    max_rows: usize,
) -> ExecResult {
    let mut stream = client.simple_query(sql).await?;
    let columns: Vec<String> = stream
        .columns()
        .await?
        .map(|cols| cols.iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();
    let mut rows: Vec<Vec<serde_json::Value>> = vec![];
    let mut truncated = false;
    let mut row_stream = stream.into_row_stream();
    while let Some(row) = row_stream.try_next().await? {
        if row.result_index() != 0 {
            break;
        }
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let values = row
            .cells()
            .map(|(_, data)| column_data_to_json(data, &mut truncated))
            .collect();
        rows.push(values);
    }
    drop(row_stream);
    Ok(QueryResult {
        row_count: rows.len(),
        columns,
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: 0,
        applied_limit: None,
        switched_schema: None,
    })
}

/// Drains result sets (for batches that need no rows, such as SET statements).
async fn drain(stream: tiberius::QueryStream<'_>) -> Result<(), ExecFailure> {
    let mut rows = stream.into_row_stream();
    while rows.try_next().await?.is_some() {}
    Ok(())
}

/// The `EXPLAIN` pseudo-statement: returns the rows of the estimated execution plan (SHOWPLAN_ALL).
/// ON / target statement / OFF are run in sequence on the same connection. Even if the target statement
/// fails, OFF is always attempted, and if OFF fails the connection with SHOWPLAN still on is not
/// returned (all later statements would become plans).
async fn run_showplan(client: &mut MsSqlClient, target: &str, max_rows: usize) -> ExecResult {
    drain(client.simple_query("SET SHOWPLAN_ALL ON").await?).await?;
    let result = fetch_first_result_set(client, target, max_rows).await;
    let off = async { drain(client.simple_query("SET SHOWPLAN_ALL OFF").await?).await }.await;
    match (result, off) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(failure)) => Err(ExecFailure {
            error: failure.error,
            reusable: false,
        }),
        (Err(failure), Ok(())) => Err(failure),
        (Err(failure), Err(_)) => Err(ExecFailure {
            error: failure.error,
            reusable: false,
        }),
    }
}

/// Inserts `TOP (limit)` into a SELECT without a LIMIT.
/// Only statements that start with SELECT are targeted (for WITH, the position of the last SELECT is unknown).
/// It is not added to statements containing TOP / OFFSET / FETCH / INTO / FOR (XML / JSON / BROWSE) / set operations
/// (UNION / EXCEPT / INTERSECT; TOP would apply only to the leading SELECT and change the
/// meaning) / DML / OUTPUT. The check uses word boundaries on cleaned, where scan_sql has removed literals and
/// comments.
pub(crate) fn apply_auto_top(sql: &str, limit: u64) -> Option<String> {
    if leading_keyword(sql) != "select" {
        return None;
    }
    let cleaned = scan_sql(sql, Engine::MsSql).cleaned;
    const VETO_WORDS: &[&str] = &[
        "top",
        "offset",
        "fetch",
        "into",
        "for",
        "union",
        "except",
        "intersect",
        "insert",
        "update",
        "delete",
        "merge",
        "output",
    ];
    if cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| VETO_WORDS.contains(&word))
    {
        return None;
    }
    // Right after SELECT. If there is DISTINCT / ALL, after it (TOP is written after DISTINCT).
    // Comments and whitespace between SELECT and DISTINCT are skipped with strip_leading_comments
    // (inserting between `SELECT /* c */ DISTINCT` would cause a syntax error)
    let rest = strip_leading_comments(sql);
    let select_end = sql.len() - rest.len() + "select".len();
    let tail = &sql[select_end..];
    let after_gap = strip_leading_comments(tail);
    let word_end = after_gap
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(after_gap.len());
    let word = &after_gap[..word_end];
    let insert_at = if word.eq_ignore_ascii_case("distinct") || word.eq_ignore_ascii_case("all") {
        select_end + (tail.len() - after_gap.len()) + word_end
    } else {
        select_end
    };
    Some(format!(
        "{} TOP ({limit}){}",
        &sql[..insert_at],
        &sql[insert_at..]
    ))
}

/// Truncates a string to the character limit (if exceeded, sets truncated and appends an ellipsis).
fn text_to_json_limited(v: &str, truncated: &mut bool) -> serde_json::Value {
    if v.chars().count() <= MAX_TEXT_CHARS {
        return serde_json::Value::String(v.to_string());
    }
    *truncated = true;
    let cut: String = v.chars().take(MAX_TEXT_CHARS).collect();
    serde_json::Value::String(format!("{cut}…"))
}

fn json_f64(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(v.to_string()))
}

/// Converts to a chrono type and formats it (shared path for date/time types).
/// If conversion fails, e.g. out of range, returns the Debug representation of the raw value (does not panic).
fn temporal_to_json<'a, T, F>(data: &'a ColumnData<'static>, format: F) -> serde_json::Value
where
    T: FromSql<'a>,
    F: Fn(T) -> String,
{
    match T::from_sql(data) {
        Ok(Some(v)) => serde_json::Value::String(format(v)),
        Ok(None) => serde_json::Value::Null,
        Err(_) => serde_json::Value::String(format!("<undecodable: {data:?}>")),
    }
}

/// Converts a tiberius value to JSON.
/// - BIGINT becomes a string when it exceeds JS's safe integer range (json_i64)
/// - DECIMAL / NUMERIC are strings to preserve precision; MONEY also arrives as DECIMAL
/// - Date/time uses the same format as the other engines in db.rs (DATETIMEOFFSET is RFC 3339)
/// - VARBINARY is a string if UTF-8, otherwise base64 (bytes_to_json)
pub(crate) fn column_data_to_json(
    data: &ColumnData<'static>,
    truncated: &mut bool,
) -> serde_json::Value {
    match data {
        ColumnData::U8(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
        ColumnData::I16(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
        ColumnData::I32(v) => v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
        ColumnData::I64(v) => v.map_or(serde_json::Value::Null, json_i64),
        ColumnData::F32(v) => v.map_or(serde_json::Value::Null, |v| json_f64(v as f64)),
        ColumnData::F64(v) => v.map_or(serde_json::Value::Null, json_f64),
        ColumnData::Bit(v) => v.map_or(serde_json::Value::Null, serde_json::Value::Bool),
        ColumnData::String(v) => v.as_deref().map_or(serde_json::Value::Null, |s| {
            text_to_json_limited(s, truncated)
        }),
        ColumnData::Guid(v) => v.map_or(serde_json::Value::Null, |g| {
            serde_json::Value::String(g.to_string())
        }),
        ColumnData::Binary(v) => match v.as_deref() {
            None => serde_json::Value::Null,
            Some(bytes) if bytes.len() > MAX_TEXT_CHARS => {
                *truncated = true;
                match bytes_to_json(bytes[..MAX_TEXT_CHARS].to_vec()) {
                    serde_json::Value::String(s) => serde_json::Value::String(format!("{s}…")),
                    other => other,
                }
            }
            Some(bytes) => bytes_to_json(bytes.to_vec()),
        },
        ColumnData::Numeric(v) => v.map_or(serde_json::Value::Null, |n| {
            serde_json::Value::String(n.to_string())
        }),
        ColumnData::Xml(v) => v.as_deref().map_or(serde_json::Value::Null, |x| {
            text_to_json_limited(x.as_ref(), truncated)
        }),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            temporal_to_json(data, |v: chrono::NaiveDateTime| {
                v.format("%Y-%m-%d %H:%M:%S%.f").to_string()
            })
        }
        ColumnData::Date(_) => temporal_to_json(data, |v: chrono::NaiveDate| {
            v.format("%Y-%m-%d").to_string()
        }),
        ColumnData::Time(_) => temporal_to_json(data, |v: chrono::NaiveTime| {
            v.format("%H:%M:%S%.f").to_string()
        }),
        ColumnData::DateTimeOffset(_) => {
            temporal_to_json(data, |v: chrono::DateTime<chrono::FixedOffset>| {
                v.to_rfc3339()
            })
        }
    }
}

/// Runs a parameterized SELECT and returns all rows as ColumnData
/// (only for small catalog queries for schema_info; identifiers are bound to @P1 onwards,
/// so they are not embedded in SQL). Serialized with the same Mutex as query execution.
async fn query_rows(
    handle: &MsSqlHandle,
    sql: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<Vec<ColumnData<'static>>>, AppError> {
    let mut slot = handle.client.lock().await;
    let mut client = take_client(handle, &mut slot).await?;
    let result: Result<Vec<Vec<ColumnData<'static>>>, tiberius::error::Error> = async {
        let stream = client.query(sql, params).await?;
        let mut rows = stream.into_row_stream();
        let mut out = Vec::new();
        while let Some(row) = rows.try_next().await? {
            out.push(row.into_iter().collect());
        }
        Ok(out)
    }
    .await;
    match result {
        Ok(rows) => {
            *slot = Some(client);
            Ok(rows)
        }
        Err(e) => {
            if matches!(e, tiberius::error::Error::Server(_)) {
                *slot = Some(client);
            }
            Err(e.into())
        }
    }
}

fn text(value: Option<&ColumnData<'static>>) -> String {
    match value {
        Some(ColumnData::String(Some(s))) => s.to_string(),
        _ => String::new(),
    }
}

fn integer(value: Option<&ColumnData<'static>>) -> Option<i64> {
    match value {
        Some(ColumnData::U8(Some(v))) => Some(i64::from(*v)),
        Some(ColumnData::I16(Some(v))) => Some(i64::from(*v)),
        Some(ColumnData::I32(Some(v))) => Some(i64::from(*v)),
        Some(ColumnData::I64(Some(v))) => Some(*v),
        _ => None,
    }
}

/// Wraps an identifier in square brackets (`]` is escaped as `]]`).
fn bracket(part: &str) -> String {
    format!("[{}]", part.replace(']', "]]"))
}

/// An identifier that can be embedded in SQL. **Always wrapped in square brackets**: looking only at
/// the character classes cannot tell reserved words such as `Order`, and embedding it bare makes the
/// snippet a syntax error (Codex review finding). With brackets, reserved words, spaces and dots
/// can be used as is, and `split_qualified` restores it by the same rule.
fn quote_identifier(part: &str) -> String {
    bracket(part)
}

/// Reads one bracketed identifier at the start. Input that does not start with `[` or is unclosed
/// gives None. Returns the contents (`]]` restored to `]`) and the part after the closing bracket.
pub(crate) fn parse_bracketed(input: &str) -> Option<(String, &str)> {
    let mut rest = input.strip_prefix('[')?;
    let mut out = String::new();
    loop {
        let close = rest.find(']')?;
        out.push_str(&rest[..close]);
        let after = &rest[close + 1..];
        match after.strip_prefix(']') {
            Some(after) => {
                out.push(']');
                rest = after;
            }
            None => return Some((out, after)),
        }
    }
}

/// Reads one leading identifier of a qualified name. If bracketed, returns the contents; otherwise
/// returns up to the first `.`, along with the rest.
fn read_identifier(input: &str) -> (String, &str) {
    if let Some(parsed) = parse_bracketed(input) {
        return parsed;
    }
    match input.find('.') {
        Some(dot) => (input[..dot].to_string(), &input[dot..]),
        None => (input.to_string(), ""),
    }
}

/// Builds a qualified name that can be embedded in SQL. **The schema is not omitted even for dbo**: if the
/// login's default schema is not dbo, a bare `[users]` points to `users` in the default schema
/// rather than the `dbo.users` the catalog enumerated (Codex review finding).
/// Each part is wrapped in brackets (`[dbo].[users]` / `[sales].[orders]`) — the qualified name is
/// inserted into SQL by the frontend and `split_qualified` restores it to (schema, table), so
/// no ambiguity remains in either direction even for names containing spaces, dots or reserved words.
fn qualified_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}

/// Restores a name built by `qualified_name` to (schema, table). Bracketed parts are
/// restored to their contents, and unqualified names (typed by a person, like `\d users`) are
/// treated as the default schema dbo. Forms that cannot be fully read (unclosed brackets etc.) fall
/// back to the old way of splitting at the first dot.
pub(crate) fn split_qualified(table: &str) -> (String, String) {
    let (first, rest) = read_identifier(table);
    if rest.is_empty() {
        return (DEFAULT_SCHEMA.to_string(), first);
    }
    if let Some(rest) = rest.strip_prefix('.') {
        let (second, tail) = read_identifier(rest);
        if tail.is_empty() && !second.is_empty() {
            return (first, second);
        }
    }
    match table.split_once('.') {
        Some((schema, name)) => (schema.to_string(), name.to_string()),
        None => (DEFAULT_SCHEMA.to_string(), table.to_string()),
    }
}

/// Summarizes the INFORMATION_SCHEMA.COLUMNS type info into notation like `nvarchar(50)` / `decimal(10,2)` /
/// `varchar(max)`.
fn format_data_type(
    data_type: &str,
    char_max_length: Option<i64>,
    numeric_precision: Option<i64>,
    numeric_scale: Option<i64>,
) -> String {
    let lower = data_type.to_ascii_lowercase();
    match lower.as_str() {
        "char" | "nchar" | "varchar" | "nvarchar" | "binary" | "varbinary" => match char_max_length
        {
            Some(-1) => format!("{lower}(max)"),
            Some(n) => format!("{lower}({n})"),
            None => lower,
        },
        "decimal" | "numeric" => match (numeric_precision, numeric_scale) {
            (Some(p), Some(s)) => format!("{lower}({p},{s})"),
            _ => lower,
        },
        _ => lower,
    }
}

const COLUMNS_SQL: &str = "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, \
     NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE \
     FROM INFORMATION_SCHEMA.COLUMNS \
     WHERE TABLE_SCHEMA = @P1 AND TABLE_NAME = @P2 \
     ORDER BY ORDINAL_POSITION";

fn column_info(row: &[ColumnData<'static>], offset: usize) -> ColumnInfo {
    ColumnInfo {
        name: text(row.get(offset)),
        data_type: format_data_type(
            &text(row.get(offset + 1)),
            integer(row.get(offset + 2)),
            integer(row.get(offset + 3)),
            integer(row.get(offset + 4)),
        ),
        nullable: text(row.get(offset + 5)).eq_ignore_ascii_case("YES"),
    }
}

/// List of tables / views (for the TABLES pane of the schema browser).
pub async fn fetch_tables(handle: &MsSqlHandle) -> Result<Vec<TableInfo>, AppError> {
    let rows = query_rows(
        handle,
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE \
         FROM INFORMATION_SCHEMA.TABLES ORDER BY TABLE_SCHEMA, TABLE_NAME",
        &[],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let schema = text(row.first());
            let name = text(row.get(1));
            let kind = if text(row.get(2)).eq_ignore_ascii_case("VIEW") {
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

/// Column list of a table. The table name is bound, so it is not embedded in SQL.
pub async fn fetch_columns(handle: &MsSqlHandle, table: &str) -> Result<Vec<ColumnInfo>, AppError> {
    let (schema, name) = split_qualified(table);
    let rows = query_rows(handle, COLUMNS_SQL, &[&schema.as_str(), &name.as_str()]).await?;
    let columns: Vec<ColumnInfo> = rows.iter().map(|row| column_info(row, 0)).collect();
    // A nonexistent table yields an empty result, so make it an explicit error
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// Column names that make up a table's primary key.
/// Cell editing is unsupported (supports_editable_cells = false), so this has no practical use,
/// but it returns what can be obtained from INFORMATION_SCHEMA.
pub async fn fetch_primary_keys(
    handle: &MsSqlHandle,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let (schema, name) = split_qualified(table);
    let rows = query_rows(
        handle,
        "SELECT kcu.COLUMN_NAME \
         FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
         JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
           ON kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
          AND kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
          AND kcu.TABLE_SCHEMA = tc.TABLE_SCHEMA \
          AND kcu.TABLE_NAME = tc.TABLE_NAME \
         WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
           AND tc.TABLE_SCHEMA = @P1 AND tc.TABLE_NAME = @P2 \
         ORDER BY kcu.ORDINAL_POSITION",
        &[&schema.as_str(), &name.as_str()],
    )
    .await?;
    Ok(rows.iter().map(|row| text(row.first())).collect())
}

/// All columns of all tables (for the schema map used in SQL completion).
pub async fn fetch_all_columns(
    handle: &MsSqlHandle,
) -> Result<std::collections::BTreeMap<String, Vec<ColumnInfo>>, AppError> {
    let rows = query_rows(
        handle,
        "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE, \
         CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE \
         FROM INFORMATION_SCHEMA.COLUMNS \
         ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION",
        &[],
    )
    .await?;
    let mut map: std::collections::BTreeMap<String, Vec<ColumnInfo>> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let schema = text(row.first());
        let name = text(row.get(1));
        map.entry(qualified_name(&schema, &name))
            .or_default()
            .push(column_info(row, 2));
    }
    Ok(map)
}

/// List of databases on the server (for the Database field dropdown).
pub async fn list_databases(handle: &MsSqlHandle) -> Result<Vec<String>, AppError> {
    let rows = query_rows(handle, "SELECT name FROM sys.databases ORDER BY name", &[]).await?;
    Ok(rows.iter().map(|row| text(row.first())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn server(yaml: &str) -> ServerConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_build_config_requires_user() {
        let err = build_config(&server("{name: x, engine: mssql, host: h}"), "h", 1433)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user and password"), "{err}");
    }

    #[test]
    fn test_build_config_maps_ssl_mode() {
        // The default (no ssl_mode, no tls) is prefer = encrypt if possible
        let config = build_config(
            &server("{name: x, engine: mssql, host: h, user: sa, password: p}"),
            "h",
            1433,
        )
        .unwrap();
        assert_eq!(config.get_addr(), "h:1433");
        // Drop the default 30-second command timeout (so heavy aggregations are not killed)
        assert_eq!(config.get_command_timeout(), None);

        // disable -> no encryption (the connection can still be built)
        build_config(
            &server("{name: x, engine: mssql, host: h, user: sa, password: p, ssl_mode: disable}"),
            "h",
            1433,
        )
        .unwrap();
        // tls: true -> verify-full. Buildable even through a tunnel (where the destination is swapped)
        build_config(
            &server(
                "{name: x, engine: mssql, host: db.example.com, user: sa, password: p, tls: true}",
            ),
            "127.0.0.1",
            50000,
        )
        .unwrap();
        // An invalid ssl_mode is a configuration error before connecting
        assert!(build_config(
            &server("{name: x, engine: mssql, host: h, user: sa, password: p, ssl_mode: nope}"),
            "h",
            1433,
        )
        .is_err());
    }

    #[test]
    fn test_apply_auto_top() {
        assert_eq!(
            apply_auto_top("SELECT * FROM users", 500).as_deref(),
            Some("SELECT TOP (500) * FROM users")
        );
        assert_eq!(
            apply_auto_top("select distinct name from users", 10).as_deref(),
            Some("select distinct TOP (10) name from users")
        );
        assert_eq!(
            apply_auto_top("SELECT ALL name FROM users;", 10).as_deref(),
            Some("SELECT ALL TOP (10) name FROM users;")
        );
        // Leading comments are kept
        assert_eq!(
            apply_auto_top("-- recent\nSELECT id FROM t", 5).as_deref(),
            Some("-- recent\nSELECT TOP (5) id FROM t")
        );
        // Even with a comment between SELECT and DISTINCT, it goes after DISTINCT
        assert_eq!(
            apply_auto_top("SELECT /* c */ DISTINCT name FROM t", 5).as_deref(),
            Some("SELECT /* c */ DISTINCT TOP (5) name FROM t")
        );
        assert_eq!(
            apply_auto_top("SELECT -- c\n  name FROM t", 5).as_deref(),
            Some("SELECT TOP (5) -- c\n  name FROM t")
        );
        // Not added to statements that already have TOP / OFFSET-FETCH / set operations / INTO / FOR XML
        for sql in [
            "SELECT TOP 10 * FROM t",
            "SELECT * FROM t ORDER BY id OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY",
            "SELECT a FROM t UNION SELECT a FROM u",
            "SELECT a FROM t EXCEPT SELECT a FROM u",
            "SELECT * INTO backup FROM t",
            "SELECT * FROM t FOR XML AUTO",
            "WITH c AS (SELECT 1 AS n) SELECT n FROM c",
            "INSERT INTO t OUTPUT inserted.id VALUES (1)",
        ] {
            assert!(
                apply_auto_top(sql, 10).is_none(),
                "should not add TOP: {sql}"
            );
        }
        // Does not react to words inside literals
        assert_eq!(
            apply_auto_top("SELECT 'union' FROM t", 3).as_deref(),
            Some("SELECT TOP (3) 'union' FROM t")
        );
        // Does not react to words inside bracketed identifiers either (scan_sql's MsSql dialect)
        assert_eq!(
            apply_auto_top("SELECT [top] FROM [for]", 3).as_deref(),
            Some("SELECT TOP (3) [top] FROM [for]")
        );
    }

    /// lib.rs looks ahead with should_auto_limit for "will a LIMIT be added" to decide max_rows.
    /// For T-SQL it must use the same decision as the side that inserts TOP, otherwise default_limit
    /// stops working for statements to which TOP is not added (Codex review finding)
    #[test]
    fn test_should_auto_limit_matches_apply_auto_top() {
        for sql in [
            "SELECT * FROM t",
            "select distinct a from t",
            "SELECT TOP 10 * FROM t",
            "SELECT a FROM t UNION SELECT a FROM u",
            "WITH c AS (SELECT 1 AS n) SELECT n FROM c",
            "SELECT * FROM t ORDER BY id OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY",
            "INSERT INTO t VALUES (1)",
        ] {
            assert_eq!(
                crate::db::should_auto_limit(sql, Engine::MsSql),
                apply_auto_top(sql, 10).is_some(),
                "{sql}"
            );
        }
    }

    /// T-SQL can list statements without semicolons, so on connections where the guard is enabled
    /// the start of a following statement is also rejected as a multi-statement batch (Codex review finding)
    #[test]
    fn test_contains_trailing_statement() {
        for sql in [
            "SELECT 1\nDROP TABLE dbo.t",
            "SELECT 1 DELETE FROM t",
            "SELECT 1\nCOMMIT TRANSACTION",
            "SELECT 1 EXEC sp_who",
            "select 1 go",
            "SELECT 1\nDECLARE @x INT",
        ] {
            assert!(contains_trailing_statement(sql), "{sql}");
        }
        for sql in [
            "SELECT 1",
            "SELECT * FROM t WITH (NOLOCK) WHERE id = 1",
            "SELECT a FROM t WHERE b IN (SELECT b FROM u)",
            // Words inside literals, comments and brackets are not statements
            "SELECT 'drop table t' FROM t",
            "SELECT 1 -- drop table t",
            "SELECT [drop] FROM [update]",
            "SELECT created, updated_at FROM t",
            // The leading word itself is not counted
            "DELETE FROM t WHERE id = 1",
            "UPDATE t SET x = 1 WHERE id = 1",
        ] {
            assert!(!contains_trailing_statement(sql), "{sql}");
        }
    }

    #[test]
    fn test_explain_target() {
        assert_eq!(explain_target("EXPLAIN SELECT 1"), Some("SELECT 1"));
        assert_eq!(
            explain_target("/* plan */ explain\n  SELECT * FROM t"),
            Some("SELECT * FROM t")
        );
        assert_eq!(explain_target("EXPLAIN"), None);
        assert_eq!(explain_target("SELECT 1"), None);
    }

    #[test]
    fn test_is_mssql_fetch() {
        assert!(is_mssql_fetch("SELECT 1"));
        assert!(is_mssql_fetch("EXEC sp_who"));
        assert!(is_mssql_fetch("execute dbo.report @year = 2026"));
        assert!(is_mssql_fetch(
            "INSERT INTO t OUTPUT inserted.id VALUES (1)"
        ));
        // Control flow and scripts can contain a SELECT inside, so run them as a batch
        // (Codex review finding)
        assert!(is_mssql_fetch(
            "IF EXISTS (SELECT 1 FROM t) SELECT * FROM t"
        ));
        assert!(is_mssql_fetch("DECLARE @n INT = 1; SELECT @n"));
        assert!(is_mssql_fetch("BEGIN SELECT 1 END"));
        assert!(is_mssql_fetch("PRINT 'x'"));
        // Statements that leave state on the connection are also run as a batch so their effects are not confined to sp_executesql's scope
        // (Codex review finding: #temp vanishes when the call ends)
        assert!(is_mssql_fetch("BEGIN TRANSACTION"));
        assert!(is_mssql_fetch("SET NOCOUNT ON"));
        assert!(is_mssql_fetch("CREATE TABLE #stage (id INT)"));
        assert!(is_mssql_fetch("INSERT INTO #stage VALUES (1)"));
        assert!(is_mssql_fetch("DROP TABLE ##global_tmp"));
        // Temp tables written in brackets too (scan_sql blanks the inside of brackets)
        assert!(is_mssql_fetch("CREATE TABLE [#stage] (id INT)"));
        assert!(is_mssql_fetch("INSERT INTO [dbo].[#stage] VALUES (1)"));
        // Statements that return no rows and leave nothing on the connection take the affected-row-count path
        assert!(!is_mssql_fetch("INSERT INTO t VALUES (1)"));
        assert!(!is_mssql_fetch("UPDATE t SET x = 'output' WHERE id = 1"));
        assert!(!is_mssql_fetch("-- note\nCREATE TABLE t (id INT)"));
        // A # inside a string or brackets is not a temp table
        assert!(!is_mssql_fetch("INSERT INTO t VALUES ('#1')"));
        assert!(!is_mssql_fetch("DELETE FROM [a#b] WHERE id = 1"));
    }

    #[test]
    fn test_column_data_to_json_scalars() {
        let mut truncated = false;
        let f = |d: ColumnData<'static>, t: &mut bool| column_data_to_json(&d, t);
        assert_eq!(
            f(ColumnData::I32(Some(42)), &mut truncated),
            serde_json::json!(42)
        );
        assert_eq!(
            f(ColumnData::I32(None), &mut truncated),
            serde_json::Value::Null
        );
        assert_eq!(
            f(ColumnData::Bit(Some(true)), &mut truncated),
            serde_json::json!(true)
        );
        assert_eq!(
            f(ColumnData::F64(Some(1.5)), &mut truncated),
            serde_json::json!(1.5)
        );
        // BIGINT above 2^53 becomes a string
        assert_eq!(
            f(ColumnData::I64(Some(9007199254740993)), &mut truncated),
            serde_json::json!("9007199254740993")
        );
        assert_eq!(
            f(
                ColumnData::String(Some(Cow::Borrowed("hello"))),
                &mut truncated
            ),
            serde_json::json!("hello")
        );
        // DECIMAL is a string to preserve precision
        assert_eq!(
            f(
                ColumnData::Numeric(Some(tiberius::numeric::Numeric::new_with_scale(12345, 2))),
                &mut truncated
            ),
            serde_json::json!("123.45")
        );
        // VARBINARY is a string if UTF-8, otherwise base64
        assert_eq!(
            f(
                ColumnData::Binary(Some(Cow::Borrowed(b"abc"))),
                &mut truncated
            ),
            serde_json::json!("abc")
        );
        assert_eq!(
            f(
                ColumnData::Binary(Some(Cow::Borrowed(&[0xff, 0xfe]))),
                &mut truncated
            ),
            serde_json::json!("base64://4=")
        );
        assert!(!truncated);
    }

    #[test]
    fn test_column_data_to_json_temporal() {
        let mut truncated = false;
        // Date is the number of days since 0001-01-01
        let base = chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap();
        let days = chrono::NaiveDate::from_ymd_opt(2026, 10, 7)
            .unwrap()
            .signed_duration_since(base)
            .num_days() as u32;
        assert_eq!(
            column_data_to_json(
                &ColumnData::Date(Some(tiberius::time::Date::new(days))),
                &mut truncated
            ),
            serde_json::json!("2026-10-07")
        );
        // Time is increments x 10^-scale seconds
        assert_eq!(
            column_data_to_json(
                &ColumnData::Time(Some(tiberius::time::Time::new(12 * 3600 + 34 * 60 + 56, 0))),
                &mut truncated
            ),
            serde_json::json!("12:34:56")
        );
        // The old DATETIME is days since 1900-01-01 + 1/300 seconds
        assert_eq!(
            column_data_to_json(
                &ColumnData::DateTime(Some(tiberius::time::DateTime::new(0, 300))),
                &mut truncated
            ),
            serde_json::json!("1900-01-01 00:00:01")
        );
        assert_eq!(
            column_data_to_json(&ColumnData::DateTime2(None), &mut truncated),
            serde_json::Value::Null
        );
        assert!(!truncated);
    }

    #[test]
    fn test_text_is_truncated() {
        let mut truncated = false;
        let long = "x".repeat(MAX_TEXT_CHARS + 5);
        let value =
            column_data_to_json(&ColumnData::String(Some(Cow::Owned(long))), &mut truncated);
        assert!(truncated);
        assert_eq!(value.as_str().unwrap().chars().count(), MAX_TEXT_CHARS + 1);
    }

    #[test]
    fn test_qualified_names() {
        // Always bracketed + always schema-qualified: reserved words (`Order`), spaces, dots and `]` can all be
        // embedded in SQL in the same form, and it points to the table the catalog enumerated even
        // for logins whose default schema is not dbo
        assert_eq!(qualified_name("dbo", "users"), "[dbo].[users]");
        assert_eq!(qualified_name("dbo", "Order"), "[dbo].[Order]");
        assert_eq!(qualified_name("sales", "orders"), "[sales].[orders]");
        assert_eq!(
            qualified_name("dbo", "Order Details"),
            "[dbo].[Order Details]"
        );
        assert_eq!(qualified_name("dbo", "a.b"), "[dbo].[a.b]");
        assert_eq!(qualified_name("my schema", "t]x"), "[my schema].[t]]x]");
        assert_eq!(
            split_qualified("users"),
            ("dbo".to_string(), "users".to_string())
        );
        assert_eq!(
            split_qualified("sales.orders"),
            ("sales".to_string(), "orders".to_string())
        );
        // Round trip: the catalog's name comes back unchanged (Codex review finding: with a bare
        // `a.b`, table a.b in dbo and table b in schema a cannot be told apart)
        for (schema, name) in [
            ("dbo", "Order Details"),
            ("dbo", "Order"),
            ("dbo", "a.b"),
            ("my schema", "t]x"),
            ("sales", "orders"),
            ("a.b", "c"),
        ] {
            assert_eq!(
                split_qualified(&qualified_name(schema, name)),
                (schema.to_string(), name.to_string()),
                "{schema} / {name}"
            );
        }
        // Unclosed brackets fall back to the old way of reading (does not panic)
        assert_eq!(
            split_qualified("[broken"),
            ("dbo".to_string(), "[broken".to_string())
        );
        assert_eq!(
            parse_bracketed("[a]]b].rest"),
            Some(("a]b".to_string(), ".rest"))
        );
        assert_eq!(parse_bracketed("plain"), None);
        assert_eq!(parse_bracketed("[unclosed"), None);
    }

    #[test]
    fn test_format_data_type() {
        assert_eq!(
            format_data_type("nvarchar", Some(50), None, None),
            "nvarchar(50)"
        );
        assert_eq!(
            format_data_type("varchar", Some(-1), None, None),
            "varchar(max)"
        );
        assert_eq!(
            format_data_type("decimal", None, Some(10), Some(2)),
            "decimal(10,2)"
        );
        assert_eq!(format_data_type("INT", None, Some(10), Some(0)), "int");
        assert_eq!(format_data_type("datetime2", None, None, None), "datetime2");
    }

    #[test]
    fn test_sql_guards_use_mssql_dialect() {
        // Semicolons and keywords inside bracketed identifiers do not affect the guard
        assert!(!crate::db::contains_multiple_statements(
            "SELECT [a;b] FROM t",
            Engine::MsSql
        ));
        assert!(crate::db::contains_multiple_statements(
            "SELECT 1; DROP TABLE t",
            Engine::MsSql
        ));
        // [where] as an identifier is not a WHERE clause -> errs on the dangerous side
        assert!(dangerous_reason("DELETE FROM [where]", Engine::MsSql).is_some());
        assert!(dangerous_reason("DELETE FROM t WHERE id = 1", Engine::MsSql).is_none());
        // EXEC is not treated as read-only (it can run anything inside)
        assert!(!is_readonly_allowed("EXEC sp_who", Engine::MsSql));
        assert!(is_readonly_allowed("EXPLAIN SELECT 1", Engine::MsSql));
    }
}
