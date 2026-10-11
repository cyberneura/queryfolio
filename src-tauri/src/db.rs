use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::postgres::{
    PgConnectOptions, PgPoolOptions, PgRow, PgSslMode, PgTypeKind, PgValueFormat,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Connection as _, Executor, Row, TypeInfo, ValueRef};

use crate::config::{ServerConfig, SqlSslMode};
use crate::error::AppError;
use crate::config::expand_tilde;
use crate::tunnel::SshTunnel;

/// Default upper limit on the number of rows fetched by a single query.
pub const DEFAULT_MAX_ROWS: usize = 1000;

const POOL_MAX_CONNECTIONS: u32 = 3;
const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Interval, in VM instructions, at which SQLite's progress handler is called.
/// Smaller values make cancellation more responsive but add execution overhead.
const SQLITE_PROGRESS_HANDLER_OPS: i32 = 1000;

#[derive(Clone)]
pub enum DbPool {
    MySql(sqlx::MySqlPool),
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
    /// Redis does not use sqlx. The Client only holds connection info, and
    /// a multiplexed connection is opened on every execution (engines::redis).
    Redis(redis::Client),
    /// Elasticsearch does not use sqlx and calls the REST API via reqwest
    /// (engines::elasticsearch). EsClient only holds base_url and credentials.
    Elasticsearch(crate::engines::elasticsearch::EsClient),
    /// DuckDB does not use sqlx and is wired up with the duckdb crate (engines::duckdb).
    /// It is a SQL engine, but a single connection is kept behind a Mutex.
    DuckDb(crate::engines::duckdb::DuckDbHandle),
    /// DynamoDB does not use sqlx and runs PartiQL (ExecuteStatement) through
    /// the AWS SDK (engines::dynamodb). DynamoClient only holds the SDK client.
    DynamoDb(crate::engines::dynamodb::DynamoClient),
    /// SQL Server does not use sqlx and is wired up with tiberius (engines::mssql).
    /// It is a SQL engine, but a single connection is kept behind a Mutex.
    MsSql(crate::engines::mssql::MsSqlHandle),
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub affected_rows: Option<u64>,
    pub truncated: bool,
    pub elapsed_ms: u64,
    /// The LIMIT value that was added automatically (None if none was added)
    pub applied_limit: Option<u64>,
    /// The active schema after switching with `\c` (None otherwise).
    /// Returned so that the frontend display, schema browser and completion follow along.
    #[serde(default)]
    pub switched_schema: Option<String>,
}

/// Manager that holds the pool and SSH tunnel for each connection name.
/// This is a single-user desktop app, so the whole pool acquisition is
/// serialized with one tokio Mutex to prevent double creation.
#[derive(Default)]
pub struct DbManager {
    inner: tokio::sync::Mutex<DbManagerInner>,
}

#[derive(Default)]
struct DbManagerInner {
    pools: HashMap<String, DbPool>,
    tunnels: HashMap<String, SshTunnel>,
    /// Per-connection override of the active schema (database).
    /// Present only while switched to a database different from the schema in the config.
    schema_overrides: HashMap<String, String>,
}

impl DbManager {
    pub async fn get_pool(&self, server: &ServerConfig) -> Result<DbPool, AppError> {
        let mut inner = self.inner.lock().await;
        if let Some(pool) = inner.pools.get(&server.name) {
            return Ok(pool.clone());
        }

        // If the active schema has been switched, replace the target database
        let mut server = server.clone();
        if let Some(schema) = inner.schema_overrides.get(&server.name) {
            server.schema = Some(schema.clone());
        }
        let server = &server;

        let engine = parse_engine(&server.engine)?;

        // If an SSH tunnel is needed, establish it first and replace the target with the local port
        let (host, port) = match (&server.ssh_tunnel, engine) {
            // Tunnels are meaningless for file-based engines
            (Some(_), Engine::Sqlite) => {
                return Err(AppError::Config(
                    "ssh_tunnel cannot be used with sqlite".into(),
                ));
            }
            (Some(_), Engine::DuckDb) => {
                return Err(AppError::Config(
                    "ssh_tunnel cannot be used with duckdb".into(),
                ));
            }
            // DynamoDB connects directly to the HTTPS AWS endpoint (SigV4 signing
            // requires the regional endpoint). Tunnels are not supported
            (Some(_), Engine::DynamoDb) => {
                return Err(AppError::Config(
                    "ssh_tunnel cannot be used with dynamodb".into(),
                ));
            }
            (Some(tunnel_config), _) => {
                // If only the pool was dropped (e.g. by a schema switch), the existing tunnel
                // points to the same target host, so it is reused as is
                let local_port = match inner.tunnels.get(&server.name) {
                    Some(tunnel) => tunnel.local_port,
                    None => {
                        let target_host =
                            server.host.clone().unwrap_or_else(|| "localhost".into());
                        let target_port = server.port.unwrap_or(default_port(engine));
                        let tunnel_config = tunnel_config.clone();
                        // ssh2 is blocking, so run it with spawn_blocking
                        let tunnel = tokio::task::spawn_blocking(move || {
                            SshTunnel::start(&tunnel_config, &target_host, target_port)
                        })
                        .await
                        .map_err(|e| {
                            AppError::SshTunnel(format!("SSH tunnel task failed: {e}"))
                        })??;
                        let local_port = tunnel.local_port;
                        inner.tunnels.insert(server.name.clone(), tunnel);
                        local_port
                    }
                };
                ("127.0.0.1".to_string(), local_port)
            }
            (None, _) => (
                server.host.clone().unwrap_or_else(|| "localhost".into()),
                server.port.unwrap_or(default_port(engine)),
            ),
        };

        let pool = connect(server, engine, &host, port).await?;
        inner.pools.insert(server.name.clone(), pool.clone());
        Ok(pool)
    }

    /// Drops all pools and tunnels. Called when the config is reloaded.
    pub async fn reset(&self) {
        let mut inner = self.inner.lock().await;
        inner.pools.clear();
        inner.tunnels.clear();
        inner.schema_overrides.clear();
    }

    /// Drops the pool and SSH tunnel of the given connection.
    /// Call this when the connection is judged to be no longer needed (e.g. when
    /// all editor tabs are closed). The tunnel and the pool must always be dropped
    /// together: if a pool kept holding connections to the dead local port of the
    /// tunnel, queries would fail the next time this connection is used.
    /// The active schema selection (schema_overrides) is UI state, so it is kept,
    /// so that the next reconnect uses the same schema.
    pub async fn disconnect(&self, connection: &str) {
        let mut inner = self.inner.lock().await;
        inner.pools.remove(connection);
        inner.tunnels.remove(connection);
    }

    /// Switches the active schema (database) of a connection.
    /// Drops the pool and reconnects to the new database from the next query
    /// (switching by rebuilding the pool rather than SQL USE prevents session
    /// state from diverging between connections in the pool).
    /// The SSH tunnel is kept because the target host does not change.
    pub async fn set_schema_override(&self, connection: &str, schema: String) {
        self.replace_schema_override(connection, Some(schema)).await;
    }

    /// Sets or clears the active schema override.
    /// Passing None goes back to the schema in the config file.
    async fn replace_schema_override(&self, connection: &str, schema: Option<String>) {
        let mut inner = self.inner.lock().await;
        match schema {
            Some(schema) => {
                inner.schema_overrides.insert(connection.to_string(), schema);
            }
            None => {
                inner.schema_overrides.remove(connection);
            }
        }
        inner.pools.remove(connection);
    }

    /// When switching fails (e.g. a nonexistent database was specified), restores
    /// the active schema to previous.
    ///
    /// This is a compare-and-swap that restores only if the current value equals
    /// expected (the value we set). It avoids rolling back a different value the
    /// user may have chosen (e.g. via schema selection) during the switch.
    /// Returns true if it restored.
    pub async fn rollback_schema_override(
        &self,
        connection: &str,
        expected: &str,
        previous: Option<String>,
    ) -> bool {
        // Do the check and write-back in the same lock scope so nothing can interleave
        let mut inner = self.inner.lock().await;
        if inner.schema_overrides.get(connection).map(String::as_str) != Some(expected) {
            return false;
        }
        match previous {
            Some(previous) => {
                inner
                    .schema_overrides
                    .insert(connection.to_string(), previous);
            }
            None => {
                inner.schema_overrides.remove(connection);
            }
        }
        inner.pools.remove(connection);
        true
    }

    /// Returns the active schema override of a connection (None if there is none).
    pub async fn schema_override(&self, connection: &str) -> Option<String> {
        self.inner.lock().await.schema_overrides.get(connection).cloned()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Engine {
    MySql,
    Postgres,
    Sqlite,
    Redis,
    Elasticsearch,
    DuckDb,
    DynamoDb,
    MsSql,
}

/// A single connection acquired from the pool for exclusive use during execution.
/// The cancellation target (backend PID, etc.) is session-level information,
/// so queries run on this connection rather than directly on the pool.
enum DbConnection {
    MySql(sqlx::pool::PoolConnection<sqlx::MySql>),
    Postgres(sqlx::pool::PoolConnection<sqlx::Postgres>),
    Sqlite(sqlx::pool::PoolConnection<sqlx::Sqlite>),
}

impl DbConnection {
    async fn acquire(pool: &DbPool) -> Result<Self, AppError> {
        Ok(match pool {
            DbPool::MySql(p) => DbConnection::MySql(p.acquire().await?),
            DbPool::Postgres(p) => DbConnection::Postgres(p.acquire().await?),
            DbPool::Sqlite(p) => DbConnection::Sqlite(p.acquire().await?),
            // Engines that do not use sqlx are delegated to their engine modules by
            // run_query_cancellable before acquire, so we never get here
            DbPool::Redis(_)
            | DbPool::Elasticsearch(_)
            | DbPool::DuckDb(_)
            | DbPool::DynamoDb(_)
            | DbPool::MsSql(_) => {
                return Err(AppError::Config(
                    "This engine does not use SQL connections".into(),
                ));
            }
        })
    }

    fn engine(&self) -> Engine {
        match self {
            DbConnection::MySql(_) => Engine::MySql,
            DbConnection::Postgres(_) => Engine::Postgres,
            DbConnection::Sqlite(_) => Engine::Sqlite,
        }
    }
}

/// How cancellation is issued (per engine).
/// Postgres / MySQL stop the statement running on the server from another
/// connection in the pool (the connection itself is not closed, so the
/// execution-side connection returns to the pool healthy). SQLite has its
/// progress handler watch the cancelled flag and abort the statement with SQLITE_INTERRUPT.
pub(crate) enum CancelTarget {
    /// Issues SELECT pg_cancel_backend($pid) from another connection
    Postgres { pid: i32, pool: sqlx::PgPool },
    /// Issues KILL QUERY <connection_id> from another connection
    MySql { connection_id: u64, pool: sqlx::MySqlPool },
    /// Only sets the cancelled flag (the progress handler aborts)
    Sqlite,
    /// Aborts the execution future on the client side (for engines with no way to
    /// stop a statement on the server, e.g. Redis). Wakes the execution-side select via notify
    ClientSide { notify: Arc<tokio::sync::Notify> },
    /// Interrupts the running statement with duckdb's InterruptHandle.
    /// Execution under spawn_blocking is not stopped by dropping the future, so
    /// an engine-side interrupt is required (no-op if no statement is running)
    DuckDb { interrupt: Arc<duckdb::InterruptHandle> },
}

/// Registration info for one running query
struct RunningQuery {
    /// Generation id of the registration (to check that a stale guard does not remove a newer registration)
    id: u64,
    target: CancelTarget,
    cancelled: Arc<AtomicBool>,
}

/// Registry of running queries (per connection name).
/// Parallel execution on the same connection is suppressed on the frontend
/// (the isConnectionRunning guard in app.svelte.ts), so it is enough to keep
/// only the most recently registered execution per connection as the cancellation target.
#[derive(Default)]
pub struct CancelRegistry {
    running: std::sync::Mutex<HashMap<String, RunningQuery>>,
    next_id: AtomicU64,
}

impl CancelRegistry {
    /// Registers the start of an execution. The registration is released when the returned guard is dropped.
    pub(crate) fn register(
        &self,
        connection: &str,
        target: CancelTarget,
        cancelled: Arc<AtomicBool>,
    ) -> RunningQueryGuard<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.running.lock().unwrap().insert(
            connection.to_string(),
            RunningQuery {
                id,
                target,
                cancelled: cancelled.clone(),
            },
        );
        RunningQueryGuard {
            registry: self,
            connection: connection.to_string(),
            id,
            cancelled,
        }
    }

    /// Requests cancellation of the query running on a connection.
    /// Does nothing and returns false if no query is running.
    /// Even if the query had just finished, pg_cancel_backend / KILL QUERY
    /// are no-ops against an idle session, so this is safe (KILL CONNECTION,
    /// which would break the connection, is not used).
    pub async fn cancel(&self, connection: &str) -> Result<bool, AppError> {
        // Take only the information needed for issuing, then release the lock, so
        // the Mutex guard is not held across an await
        enum CancelAction {
            Postgres { pid: i32, pool: sqlx::PgPool },
            MySql { connection_id: u64, pool: sqlx::MySqlPool },
            Notify { notify: Arc<tokio::sync::Notify> },
            DuckDbInterrupt { interrupt: Arc<duckdb::InterruptHandle> },
            None,
        }
        let action = {
            let running = self.running.lock().unwrap();
            let Some(query) = running.get(connection) else {
                return Ok(false);
            };
            query.cancelled.store(true, Ordering::SeqCst);
            match &query.target {
                CancelTarget::Postgres { pid, pool } => CancelAction::Postgres {
                    pid: *pid,
                    pool: pool.clone(),
                },
                CancelTarget::MySql {
                    connection_id,
                    pool,
                } => CancelAction::MySql {
                    connection_id: *connection_id,
                    pool: pool.clone(),
                },
                CancelTarget::Sqlite => CancelAction::None,
                CancelTarget::ClientSide { notify } => CancelAction::Notify {
                    notify: notify.clone(),
                },
                CancelTarget::DuckDb { interrupt } => CancelAction::DuckDbInterrupt {
                    interrupt: interrupt.clone(),
                },
            }
        };
        match action {
            CancelAction::Postgres { pid, pool } => {
                sqlx::query("SELECT pg_cancel_backend($1)")
                    .bind(pid)
                    .execute(&pool)
                    .await?;
            }
            CancelAction::MySql {
                connection_id,
                pool,
            } => {
                // KILL cannot use placeholders, but connection_id is a number
                // returned by the server, so embedding it directly is fine
                sqlx::query(&format!("KILL QUERY {connection_id}"))
                    .execute(&pool)
                    .await?;
            }
            CancelAction::Notify { notify } => notify.notify_waiters(),
            CancelAction::DuckDbInterrupt { interrupt } => interrupt.interrupt(),
            CancelAction::None => {}
        }
        Ok(true)
    }

    /// (For tests) Returns whether an execution is registered for the connection
    #[cfg(test)]
    fn is_running(&self, connection: &str) -> bool {
        self.running.lock().unwrap().contains_key(connection)
    }
}

/// Guard that removes the registration from the registry when the execution ends.
/// If a new execution was registered again on the same connection after
/// registration (id mismatch), it does nothing so as not to remove the new registration.
pub(crate) struct RunningQueryGuard<'a> {
    registry: &'a CancelRegistry,
    connection: String,
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl RunningQueryGuard<'_> {
    /// Returns whether cancellation was requested for this execution
    pub(crate) fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for RunningQueryGuard<'_> {
    fn drop(&mut self) {
        let mut running = self.registry.running.lock().unwrap();
        if running
            .get(&self.connection)
            .is_some_and(|q| q.id == self.id)
        {
            running.remove(&self.connection);
        }
    }
}

/// Resolves the engine string in the config into an Engine.
pub fn parse_engine(engine: &str) -> Result<Engine, AppError> {
    match engine.to_ascii_lowercase().as_str() {
        "mysql" | "mariadb" => Ok(Engine::MySql),
        "postgres" | "postgresql" => Ok(Engine::Postgres),
        "sqlite" | "sqlite3" => Ok(Engine::Sqlite),
        "redis" | "valkey" => Ok(Engine::Redis),
        "elasticsearch" | "es" | "opensearch" => Ok(Engine::Elasticsearch),
        "duckdb" => Ok(Engine::DuckDb),
        "dynamodb" => Ok(Engine::DynamoDb),
        "mssql" | "sqlserver" => Ok(Engine::MsSql),
        other => Err(AppError::Config(format!(
            "Unsupported engine: {other} \
             (supported: mysql / postgres / sqlite / duckdb / mssql / redis / \
             elasticsearch / dynamodb)"
        ))),
    }
}

fn default_port(engine: Engine) -> u16 {
    match engine {
        Engine::MySql => 3306,
        Engine::Postgres => 5432,
        // DynamoDb endpoint resolution is done by engines::dynamodb::connect
        Engine::Sqlite | Engine::DuckDb | Engine::DynamoDb => 0,
        Engine::Redis => crate::engines::redis::DEFAULT_PORT,
        Engine::Elasticsearch => crate::engines::elasticsearch::DEFAULT_PORT,
        Engine::MsSql => crate::engines::mssql::DEFAULT_PORT,
    }
}

/// Expands and returns the ssl_root_cert path (None if unset).
/// Validity of the value (empty string, combining with a no-verification mode) is checked on the ServerConfig side;
/// here we only check that it is in a form that can be opened as a file
/// (a nonexistent path is rejected before it turns into a confusing error at connect time).
pub(crate) fn ssl_root_cert_path(server: &ServerConfig) -> Result<Option<PathBuf>, AppError> {
    let Some(raw) = server.sql_ssl_root_cert()? else {
        return Ok(None);
    };
    let path = expand_tilde(raw);
    // If a directory is passed, sqlx gives a confusing read error, so
    // also check that it is a regular file
    if !path.is_file() {
        return Err(AppError::Config(format!(
            "Server '{}': ssl_root_cert is not a file: {}",
            server.name,
            path.display()
        )));
    }
    Ok(Some(path))
}

async fn connect(
    server: &ServerConfig,
    engine: Engine,
    host: &str,
    port: u16,
) -> Result<DbPool, AppError> {
    match engine {
        Engine::MySql => {
            let ssl_mode = match server.sql_ssl_mode()? {
                SqlSslMode::Disable => MySqlSslMode::Disabled,
                SqlSslMode::Prefer => MySqlSslMode::Preferred,
                SqlSslMode::Require => MySqlSslMode::Required,
                SqlSslMode::VerifyCa => MySqlSslMode::VerifyCa,
                // MySQL's VerifyIdentity is equivalent to libpq's verify-full
                SqlSslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
            };
            let mut options = MySqlConnectOptions::new()
                .host(host)
                .port(port)
                .ssl_mode(ssl_mode);
            if let Some(path) = ssl_root_cert_path(server)? {
                options = options.ssl_ca(path);
            }
            if let Some(user) = &server.user {
                options = options.username(user);
            }
            if let Some(password) = &server.password {
                options = options.password(password);
            }
            if let Some(schema) = &server.schema {
                options = options.database(schema);
            }
            let pool = MySqlPoolOptions::new()
                .max_connections(POOL_MAX_CONNECTIONS)
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options)
                .await?;
            Ok(DbPool::MySql(pool))
        }
        Engine::Postgres => {
            let ssl_mode = match server.sql_ssl_mode()? {
                SqlSslMode::Disable => PgSslMode::Disable,
                SqlSslMode::Prefer => PgSslMode::Prefer,
                SqlSslMode::Require => PgSslMode::Require,
                SqlSslMode::VerifyCa => PgSslMode::VerifyCa,
                SqlSslMode::VerifyFull => PgSslMode::VerifyFull,
            };
            let mut options = PgConnectOptions::new()
                .host(host)
                .port(port)
                .ssl_mode(ssl_mode);
            if let Some(path) = ssl_root_cert_path(server)? {
                options = options.ssl_root_cert(path);
            }
            if let Some(user) = &server.user {
                options = options.username(user);
            }
            if let Some(password) = &server.password {
                options = options.password(password);
            }
            if let Some(schema) = &server.schema {
                options = options.database(schema);
            }
            let pool = PgPoolOptions::new()
                .max_connections(POOL_MAX_CONNECTIONS)
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options)
                .await?;
            Ok(DbPool::Postgres(pool))
        }
        Engine::Sqlite => {
            // sqlite treats schema (or host if absent) as the DB file path
            let path = server
                .schema
                .as_deref()
                .or(server.host.as_deref())
                .ok_or_else(|| {
                    AppError::Config(
                        "For sqlite, set schema to the database file path".into(),
                    )
                })?;
            let file_path = expand_tilde(path);
            if !file_path.exists() {
                return Err(AppError::Config(format!(
                    "SQLite database file not found: {}",
                    file_path.display()
                )));
            }
            let options = SqliteConnectOptions::new().filename(&file_path);
            let pool = SqlitePoolOptions::new()
                .max_connections(POOL_MAX_CONNECTIONS)
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options)
                .await?;
            Ok(DbPool::Sqlite(pool))
        }
        Engine::Redis => Ok(DbPool::Redis(
            crate::engines::redis::connect(server, host, port).await?,
        )),
        Engine::Elasticsearch => Ok(DbPool::Elasticsearch(
            crate::engines::elasticsearch::connect(server, host, port).await?,
        )),
        // duckdb is file-based like sqlite, so host / port are not used
        Engine::DuckDb => Ok(DbPool::DuckDb(
            crate::engines::duckdb::connect(server).await?,
        )),
        // dynamodb resolves the region (schema) and the endpoint override (host / port)
        // in the module, so the host / port computed here are not used
        Engine::DynamoDb => Ok(DbPool::DynamoDb(
            crate::engines::dynamodb::connect(server).await?,
        )),
        // mssql is TCP, so the host / port replaced by the SSH tunnel are used as is
        Engine::MsSql => Ok(DbPool::MsSql(
            crate::engines::mssql::connect(server, host, port).await?,
        )),
    }
}

/// Runs SQL and returns the result (a non-cancellable wrapper for tests).
/// The app itself uses the cancellation-aware run_query_cancellable.
#[cfg(test)]
pub(crate) async fn run_query(
    pool: &DbPool,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: bool,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let mut conn = DbConnection::acquire(pool).await?;
    // Tests pass a bool equivalent to config readonly.
    let guard = if readonly {
        ReadonlyGuard::Config
    } else {
        ReadonlyGuard::Off
    };
    run_query_on(&mut conn, sql, max_rows, auto_limit, guard, allow_dangerous).await
}

/// Runs SQL and returns the result (cancellation-aware version).
/// Acquires a dedicated connection for execution from the pool and, before
/// executing, registers the engine-specific cancellation target (backend PID
/// for Postgres, CONNECTION_ID for MySQL, a progress handler with an abort
/// flag for SQLite) in the registry. If the query ends with an error after a
/// cancellation request, AppError::Cancelled is returned. Cancellation only
/// stops the statement on the server and does not close the connection, so
/// the connection returns to the pool healthy and the next query on the same
/// connection runs normally.
// readonly / allow_dangerous are independent execution guards, so they are passed as separate arguments
#[allow(clippy::too_many_arguments)]
pub async fn run_query_cancellable(
    pool: &DbPool,
    registry: &CancelRegistry,
    connection_name: &str,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    // Non-SQL engines are delegated to their engine modules (auto_limit is
    // SQL-specific, so it is not passed)
    if let DbPool::Redis(client) = pool {
        return crate::engines::redis::run_query_cancellable(
            client,
            registry,
            connection_name,
            sql,
            max_rows,
            readonly,
            allow_dangerous,
        )
        .await;
    }
    if let DbPool::Elasticsearch(client) = pool {
        return crate::engines::elasticsearch::run_query_cancellable(
            client,
            registry,
            connection_name,
            sql,
            max_rows,
            readonly,
            allow_dangerous,
        )
        .await;
    }
    // DuckDB is a SQL engine but does not support sqlx, so it is delegated to its module
    // (common SQL guards, including auto_limit, are applied on the module side)
    if let DbPool::DuckDb(handle) = pool {
        return crate::engines::duckdb::run_query_cancellable(
            handle,
            registry,
            connection_name,
            sql,
            max_rows,
            auto_limit,
            readonly,
            allow_dangerous,
        )
        .await;
    }
    // DynamoDB (PartiQL) is also delegated to its module. PartiQL has no LIMIT clause,
    // so auto_limit is not passed; the row count is bounded by the
    // ExecuteStatement limit parameter + max_rows (on the module side)
    if let DbPool::DynamoDb(client) = pool {
        return crate::engines::dynamodb::run_query_cancellable(
            client,
            registry,
            connection_name,
            sql,
            max_rows,
            readonly,
            allow_dangerous,
        )
        .await;
    }
    // SQL Server is also a SQL engine but does not support sqlx, so it is delegated to its module
    // (auto LIMIT is applied on the module side as T-SQL TOP)
    if let DbPool::MsSql(handle) = pool {
        return crate::engines::mssql::run_query_cancellable(
            handle,
            registry,
            connection_name,
            sql,
            max_rows,
            auto_limit,
            readonly,
            allow_dangerous,
        )
        .await;
    }

    let mut conn = DbConnection::acquire(pool).await?;
    let cancelled = Arc::new(AtomicBool::new(false));

    // Note the cancellation target before execution
    let target = match (&mut conn, pool) {
        (DbConnection::Postgres(c), DbPool::Postgres(p)) => {
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut **c)
                .await?;
            CancelTarget::Postgres {
                pid,
                pool: p.clone(),
            }
        }
        (DbConnection::MySql(c), DbPool::MySql(p)) => {
            // CONNECTION_ID() is BIGINT UNSIGNED, but fall back to decoding
            // as i64 as well to allow for implementation differences
            let row = sqlx::query("SELECT CONNECTION_ID()")
                .fetch_one(&mut **c)
                .await?;
            let connection_id: u64 = match row.try_get::<u64, _>(0) {
                Ok(id) => id,
                Err(_) => row.try_get::<i64, _>(0)? as u64,
            };
            CancelTarget::MySql {
                connection_id,
                pool: p.clone(),
            }
        }
        (DbConnection::Sqlite(c), _) => {
            // Once the flag is set, the progress handler returns false and
            // the running statement is aborted with SQLITE_INTERRUPT
            let flag = cancelled.clone();
            c.lock_handle().await?.set_progress_handler(
                SQLITE_PROGRESS_HANDLER_OPS,
                move || !flag.load(Ordering::SeqCst),
            );
            CancelTarget::Sqlite
        }
        // acquire only returns connections of the same engine as the pool
        _ => unreachable!("connection engine mismatch"),
    };

    let guard = registry.register(connection_name, target, cancelled);
    let result =
        run_query_on(&mut conn, sql, max_rows, auto_limit, readonly, allow_dangerous).await;
    let was_cancelled = guard.was_cancelled();
    drop(guard);

    // SQLite: always remove the progress handler before returning the connection to the pool.
    // If it is left behind, a handler with its flag set remains and the next
    // query on this connection is aborted immediately.
    // (lock_handle fails only when the worker thread is dead, in which case
    //  the connection itself is unusable and is discarded on the pool side)
    if let DbConnection::Sqlite(c) = &mut conn {
        if let Ok(mut handle) = c.lock_handle().await {
            handle.remove_progress_handler();
        }
    }

    // An error after a cancellation request is returned as "cancelled"
    // (if the query had already completed before the cancel took effect, the success result is returned)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    result
}

/// Error returned when blocking on the read-only guard (message depends on its origin).
/// Shared by run_query_on and run_statements.
pub(crate) fn readonly_block_error(readonly: ReadonlyGuard) -> AppError {
    let message = match readonly {
        ReadonlyGuard::Config => {
            "This connection is read-only (readonly: true in config). \
             Statement was not executed."
        }
        ReadonlyGuard::Switch => {
            "Read-only mode is on. Turn on the Writable switch in the \
             toolbar to run write statements. Statement was not executed."
        }
        ReadonlyGuard::Agent => {
            "The AI assistant can only run read-only statements. \
             Statement was not executed."
        }
        // Off does not block, so this error is never created
        ReadonlyGuard::Off => unreachable!(),
    };
    AppError::Readonly(message.into())
}

/// Error returned when blocking on a dangerous statement (UPDATE/DELETE without WHERE, etc.).
pub(crate) fn dangerous_block_error(reason: &str) -> AppError {
    AppError::Dangerous(format!(
        "{reason} Set \"allow_dangerous_statements: true\" for this connection \
         in config to run it. Statement was not executed."
    ))
}

/// Runs one statement on an acquired connection and returns the affected row count (does not read the result set).
async fn execute_statement(conn: &mut DbConnection, sql: &str) -> Result<u64, AppError> {
    Ok(match conn {
        DbConnection::MySql(c) => (&mut **c).execute(sql).await?.rows_affected(),
        DbConnection::Postgres(c) => (&mut **c).execute(sql).await?.rows_affected(),
        DbConnection::Sqlite(c) => (&mut **c).execute(sql).await?.rows_affected(),
    })
}

/// Applies cell edits from the result grid as a set of UPDATEs in one transaction.
/// COMMIT if all statements succeed; if one fails, ROLLBACK and return the first error
/// (all-or-nothing). So that this path does not become a back door for general
/// multi-statement execution, every statement is required to be an UPDATE, and the
/// readonly / dangerous-statement guards apply just as in run_query.
/// Returns the total affected row count.
pub async fn run_statements(
    pool: &DbPool,
    statements: &[String],
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<u64, AppError> {
    if statements.is_empty() {
        return Err(AppError::Config("There are no changes to apply".into()));
    }
    // DuckDb / MsSql have no path for applying cell edits (sqlx transaction execution),
    // so reject them, consistent with capabilities.supports_editable_cells = false
    if matches!(
        pool,
        DbPool::Redis(_)
            | DbPool::Elasticsearch(_)
            | DbPool::DuckDb(_)
            | DbPool::DynamoDb(_)
            | DbPool::MsSql(_)
    ) {
        return Err(AppError::Config(
            "Cell editing is not supported for this engine".into(),
        ));
    }
    let mut conn = DbConnection::acquire(pool).await?;
    let engine = conn.engine();

    // Validate all statements before writing anything (to avoid only some being applied).
    for sql in statements {
        let sql = sql.trim();
        // Cell edits only apply UPDATE. Other statements are rejected on this path.
        if leading_keyword(sql) != "update" {
            return Err(AppError::Config(
                "Only UPDATE statements can be applied from the results grid.".into(),
            ));
        }
        // Multiple statements slip past the guards, so reject them if a guard is enabled
        // (same reason as run_query_on. `UPDATE ... WHERE ...; DROP TABLE t;` starts
        // with update and has a where, so it would pass both guards)
        if (readonly != ReadonlyGuard::Off || !allow_dangerous)
            && contains_multiple_statements(sql, engine)
        {
            return Err(multi_statement_block_error());
        }
        if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, engine) {
            return Err(readonly_block_error(readonly));
        }
        if !allow_dangerous {
            if let Some(reason) = dangerous_reason(sql, engine) {
                return Err(dangerous_block_error(reason));
            }
        }
    }

    // Reset in case we pulled a connection on which PRAGMA query_only, set by the
    // agent path, remains (same reason as run_query_on. The setting is finalized
    // by "the calling code making it explicit every time")
    if let DbConnection::Sqlite(c) = &mut conn {
        set_sqlite_query_only(c, false).await?;
    }

    // Apply all statements in one transaction. DDL is not included (UPDATE only), so
    // no implicit commit happens. Make sure it reaches COMMIT/ROLLBACK
    // before returning the connection to the pool.
    execute_statement(&mut conn, "BEGIN").await?;
    let mut total: u64 = 0;
    for sql in statements {
        match execute_statement(&mut conn, sql.trim()).await {
            Ok(affected) => total += affected,
            Err(e) => {
                // Swallow a failure of the rollback itself and return the original error
                let _ = execute_statement(&mut conn, "ROLLBACK").await;
                return Err(e);
            }
        }
    }
    // Even if COMMIT fails (deferred constraint violation / SQLite busy, etc.), try ROLLBACK so
    // the connection is not returned to the pool in a transaction state.
    if let Err(e) = execute_statement(&mut conn, "COMMIT").await {
        let _ = execute_statement(&mut conn, "ROLLBACK").await;
        return Err(e);
    }
    Ok(total)
}

/// Origin of the read-only guard. Used to vary the block message
/// by origin (config readonly, or the toolbar's Writable switch).
/// Agent is both an origin and a strength level (it comes with DB-level enforcement).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadonlyGuard {
    /// Writes allowed (no readonly guard)
    Off,
    /// Read-only via config `readonly: true` (cannot be lifted with the switch)
    Config,
    /// Read-only because the toolbar's Writable switch is OFF
    Switch,
    /// Execution by the AI chat agent. In addition to the statement-level check,
    /// read-only is also enforced at the DB level (run_query_readonly).
    /// This is because statement-level checks alone cannot stop calls to
    /// functions with side effects such as `SELECT nextval(...)`.
    Agent,
}

/// SQL that starts a DB-level read-only session (per engine).
/// Starts a read-only transaction and always ROLLBACKs after execution.
/// SQLite has no transaction attribute, so it uses PRAGMA query_only
/// (set_sqlite_query_only).
fn readonly_begin_sql(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some("BEGIN READ ONLY"),
        Engine::MySql => Some("START TRANSACTION READ ONLY"),
        _ => None,
    }
}

/// Sets SQLite's DB-level read-only mode.
/// Instead of undoing it (back to 0), we make 0/1 explicit on every execution:
/// if the query future is dropped midway (chat abort), cleanup does not run
/// and a connection with query_only = 1 left over could return to the pool,
/// so the state is fixed on every execution rather than relying on cleanup.
async fn set_sqlite_query_only(
    conn: &mut sqlx::SqliteConnection,
    enabled: bool,
) -> Result<(), AppError> {
    let sql = if enabled {
        "PRAGMA query_only = 1"
    } else {
        "PRAGMA query_only = 0"
    };
    conn.execute(sql).await?;
    Ok(())
}

/// Body of run_query. Runs on an acquired connection.
async fn run_query_on(
    conn: &mut DbConnection,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let engine = conn.engine();
    // psql-style meta commands (\l, \dt, etc.) are converted into catalog query SQL and executed.
    // \c / USE (schema switching) does not become SQL, so lib.rs's run_query
    // handles it before getting here. The agent path (ReadonlyGuard::Agent) does not go
    // through run_query, so the switch is rejected here (operations that
    // change connection state are not allowed for the agent)
    let translated = match crate::meta_commands::translate(engine, sql)? {
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

    // On a readonly connection, only read statements are allowed.
    // Meta commands are only converted into read-only catalog queries,
    // so the converted SQL always passes this check.
    // The agent path imposes a narrower whitelist than the normal readonly guard
    // (dropping multi-statements / CALL / PRAGMA / EXPLAIN ANALYZE). lib.rs applies the
    // same check before acquiring the pool, but imposing it here too lets
    // ReadonlyGuard::Agent alone establish the agent's execution policy
    // (without depending on the caller's implementation)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) = agent_rejection_reason(sql, engine) {
            return Err(AppError::Readonly(reason));
        }
    }
    // Multiple statements (`;`-separated) slip past guards that only look at the first statement.
    // `SELECT 1; DELETE FROM t;` starts with select, so it passes is_readonly_allowed, and
    // `UPDATE t SET x=1 WHERE id=1; DROP TABLE t;` has a where, so it passes
    // dangerous_reason. Meanwhile the execution-path drivers run multiple statements
    // as is (SQLite even on the fetch path; Postgres / MySQL via the argument-less
    // execute path = the simple query protocol).
    // So on connections where a guard is enabled, multiple statements themselves are rejected. On connections with
    // both guards off, multiple statements are allowed as before, so pasting and running scripts is not broken.
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && contains_multiple_statements(sql, engine)
    {
        return Err(multi_statement_block_error());
    }

    if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, engine) {
        return Err(readonly_block_error(readonly));
    }

    // Dangerous statements (UPDATE / DELETE without WHERE, DROP / TRUNCATE) are allowed
    // to run only on connections with allow_dangerous_statements enabled.
    // An accident-prevention guard against whole-table destruction or table loss from mistakes.
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, engine) {
            return Err(dangerous_block_error(reason));
        }
    }

    // Add a default LIMIT to a SELECT without a LIMIT
    // (not applied to SQL converted from meta commands)
    let mut applied_limit = None;
    let limited_sql;
    let sql = match auto_limit {
        Some(limit)
            if limit > 0
                && translated.is_none()
                && should_auto_limit(sql, engine) =>
        {
            // Add it right after the body, excluding trailing comments and semicolons
            // (appending after a comment would let the comment swallow the LIMIT)
            let body = &sql[..scan_sql(sql, engine).body_end];
            limited_sql = format!("{body} LIMIT {limit}");
            applied_limit = Some(limit);
            limited_sql.as_str()
        }
        _ => sql,
    };
    let started = Instant::now();

    // SQLite's DB-level read-only is PRAGMA query_only (a session setting),
    // so make it explicit on every execution even outside the agent path
    // (cleans up connections returned to the pool without the abort-time reset having run)
    if let DbConnection::Sqlite(c) = &mut *conn {
        set_sqlite_query_only(c, readonly == ReadonlyGuard::Agent).await?;
    }

    // The agent path runs inside a read-only transaction.
    // Statement-level guards (is_readonly_allowed / agent_rejection_reason) cannot
    // detect calls to functions with side effects such as `SELECT nextval(...)`,
    // so the final rejection is left to the DB itself.
    if readonly == ReadonlyGuard::Agent {
        return run_query_readonly(conn, sql, max_rows, applied_limit, started).await;
    }

    let exec = match &mut *conn {
        DbConnection::MySql(c) => DbExec::MySql(c),
        DbConnection::Postgres(c) => DbExec::Postgres(c),
        DbConnection::Sqlite(c) => DbExec::Sqlite(c),
    };
    run_query_with(exec, sql, max_rows, applied_limit, started).await
}

/// Connection reference used for execution. Inserted so that the execution part is shared between a pool connection
/// and a read-only transaction (agent path).
enum DbExec<'a> {
    MySql(&'a mut sqlx::MySqlConnection),
    Postgres(&'a mut sqlx::PgConnection),
    Sqlite(&'a mut sqlx::SqliteConnection),
}

/// Runs a query with DB-level read-only (agent path).
/// Postgres / MySQL wrap it in a read-only transaction and ROLLBACK
/// regardless of the result (it only reads, so there is no need to COMMIT).
/// For SQLite, PRAGMA query_only has already been set by the caller.
async fn run_query_readonly(
    conn: &mut DbConnection,
    sql: &str,
    max_rows: usize,
    applied_limit: Option<u64>,
    started: Instant,
) -> Result<QueryResult, AppError> {
    // Engines that cannot be wrapped in a transaction (SQLite) have already had PRAGMA applied, so run as is
    let begin = match readonly_begin_sql(conn.engine()) {
        Some(begin) => begin,
        None => {
            let exec = match conn {
                DbConnection::Sqlite(c) => DbExec::Sqlite(c),
                // readonly_begin_sql returns None only for SQLite
                _ => unreachable!("engine without a read-only transaction"),
            };
            return run_query_with(exec, sql, max_rows, applied_limit, started).await;
        }
    };

    // sqlx's Transaction also queues a ROLLBACK on drop (even if this function's
    // future is dropped by an abort, the connection is not returned to the
    // pool with the transaction left open)
    match conn {
        DbConnection::Postgres(c) => {
            let mut tx = c.begin_with(begin).await?;
            let result =
                run_query_with(DbExec::Postgres(&mut tx), sql, max_rows, applied_limit, started)
                    .await;
            // Swallow a failure of the ROLLBACK itself (returning the result takes priority;
            // a failed connection fails ping and is discarded from the pool)
            let _ = tx.rollback().await;
            result
        }
        DbConnection::MySql(c) => {
            let mut tx = c.begin_with(begin).await?;
            let result =
                run_query_with(DbExec::MySql(&mut tx), sql, max_rows, applied_limit, started).await;
            let _ = tx.rollback().await;
            result
        }
        DbConnection::Sqlite(_) => unreachable!("sqlite has no read-only transaction"),
    }
}

/// Runs one statement on an acquired target (connection or transaction).
async fn run_query_with(
    mut exec: DbExec<'_>,
    sql: &str,
    max_rows: usize,
    applied_limit: Option<u64>,
    started: Instant,
) -> Result<QueryResult, AppError> {
    if !is_fetch_statement(sql) && !contains_returning(sql) {
        let affected = match &mut exec {
            DbExec::MySql(c) => (&mut **c).execute(sql).await?.rows_affected(),
            DbExec::Postgres(c) => (&mut **c).execute(sql).await?.rows_affected(),
            DbExec::Sqlite(c) => (&mut **c).execute(sql).await?.rows_affected(),
        };
        return Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            row_count: 0,
            affected_rows: Some(affected),
            truncated: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
            applied_limit: None,
            switched_schema: None,
        });
    }

    macro_rules! fetch_rows {
        ($pool:expr, $to_json:ident) => {{
            let mut stream = sqlx::query(sql).fetch($pool);
            let mut columns: Vec<String> = vec![];
            let mut rows: Vec<Vec<serde_json::Value>> = vec![];
            let mut truncated = false;
            while let Some(row) = stream.try_next().await? {
                if columns.is_empty() {
                    columns = row
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                }
                if rows.len() >= max_rows {
                    truncated = true;
                    break;
                }
                let values = (0..row.columns().len())
                    .map(|i| $to_json(&row, i))
                    .collect();
                rows.push(values);
            }
            (columns, rows, truncated)
        }};
    }

    let (mut columns, rows, truncated) = match &mut exec {
        DbExec::MySql(c) => fetch_rows!(&mut **c, mysql_value_to_json),
        DbExec::Postgres(c) => fetch_rows!(&mut **c, pg_value_to_json),
        DbExec::Sqlite(c) => fetch_rows!(&mut **c, sqlite_value_to_json),
    };

    // So that column headers can be shown even for 0-row results, supplement column info with describe.
    // It can fail for statements that cannot be prepared, such as SHOW, so the error is ignored.
    if columns.is_empty() {
        let described: Result<Vec<String>, sqlx::Error> = match &mut exec {
            DbExec::MySql(c) => (&mut **c)
                .describe(sql)
                .await
                .map(|d| d.columns().iter().map(|c| c.name().to_string()).collect()),
            DbExec::Postgres(c) => (&mut **c)
                .describe(sql)
                .await
                .map(|d| d.columns().iter().map(|c| c.name().to_string()).collect()),
            DbExec::Sqlite(c) => (&mut **c)
                .describe(sql)
                .await
                .map(|d| d.columns().iter().map(|c| c.name().to_string()).collect()),
        };
        if let Ok(names) = described {
            columns = names;
        }
    }

    Ok(QueryResult {
        row_count: rows.len(),
        columns,
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
        applied_limit,
        switched_schema: None,
    })
}

/// Returns the list of databases (schemas) on the target server.
/// For sqlite, the database concept is a single file, so the configured path is returned as is.
pub async fn list_schemas(
    pool: &DbPool,
    server: &ServerConfig,
) -> Result<Vec<String>, AppError> {
    match pool {
        DbPool::Postgres(p) => {
            let rows = sqlx::query(
                "SELECT datname FROM pg_catalog.pg_database \
                 WHERE datistemplate = false ORDER BY datname",
            )
            .fetch_all(p)
            .await?;
            Ok(rows
                .iter()
                .filter_map(|row| row.try_get::<String, _>(0).ok())
                .collect())
        }
        DbPool::MySql(p) => {
            let rows = sqlx::query("SHOW DATABASES").fetch_all(p).await?;
            Ok(rows
                .iter()
                .filter_map(|row| row.try_get::<String, _>(0).ok())
                .collect())
        }
        // duckdb also returns the file path as is, like sqlite
        DbPool::Sqlite(_) | DbPool::DuckDb(_) => {
            let path = server
                .schema
                .as_deref()
                .or(server.host.as_deref())
                .unwrap_or("main");
            Ok(vec![path.to_string()])
        }
        // A Redis "database" is a number (CYBERNEURA-DEV-408).
        // The count is a server setting, so it is queried on the module side
        DbPool::Redis(client) => crate::engines::redis::list_databases(client).await,
        DbPool::MsSql(handle) => crate::engines::mssql::list_databases(handle).await,
        // Elasticsearch / DynamoDB have no concept of a database list
        // (the frontend does not call this since capabilities.supports_schemas = false, but
        // return empty so that nothing breaks even if it is called directly)
        DbPool::Elasticsearch(_) | DbPool::DynamoDb(_) => Ok(vec![]),
    }
}

/// Returns the leading keyword of the SQL (excluding comments) in lowercase.
///
/// Since no dialect is passed in, `/*! ... */` is also skipped here as a normal block comment.
/// MySQL executes this on the server, but if dialect-dependent interpretation were put into
/// this common parser, then in other dialects where `/*!` really is a comment,
/// the leading keyword of `/*! SELECT 1 */ DROP TABLE t` would be misread as select,
/// and the readonly / dangerous-statement guards would miss the DROP (a hole in the opposite direction).
/// MySQL's executable comments are left in cleaned by scan_sql, so look at that
/// and the guard on the dangerous_reason side catches it.
pub(crate) fn leading_keyword(sql: &str) -> String {
    strip_leading_comments(sql)
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Returns the remainder after skipping leading whitespace and comments (`--` / `#` / `/* */`).
///
/// For how dialects are handled (skipping `/*! ... */` as a normal comment) and
/// the reason, see the comment on leading_keyword. It is used not only to determine
/// the leading keyword but also to slice out the part after the keyword
/// (`USE <database>` in meta_commands).
pub(crate) fn strip_leading_comments(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, r)| r).unwrap_or("");
            continue;
        }
        if let Some(after) = rest.strip_prefix('#') {
            rest = after.split_once('\n').map(|(_, r)| r).unwrap_or("");
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, r)| r).unwrap_or("");
            continue;
        }
        break;
    }
    rest
}

/// Result of scanning SQL. cleaned is for keyword detection (string literals and comments
/// are blanked out and lowercased); body_end is the insertion position for the auto LIMIT
/// (the end of the body, excluding trailing comments, semicolons and whitespace).
pub(crate) struct SqlScan {
    /// Body with literals and comments blanked out and lowercased (for word boundary detection)
    pub(crate) cleaned: String,
    pub(crate) body_end: usize,
}

/// Whether a `--` line comment starts at chars[i].
///
/// Only in MySQL does `--` become a line comment solely when it is immediately followed by
/// whitespace (including control characters and end of line). `SELECT 1--1` is "1 - (-1)", not a comment.
/// https://dev.mysql.com/doc/refman/8.4/en/ansi-diff-comments.html
///
/// If this were treated the same as other dialects, everything after the semicolon in `SELECT 1--1; DROP TABLE t;`
/// would be dropped as a comment, and the multi-statement check (contains_multiple_statements) and the
/// readonly / dangerous-statement guards would miss the second statement that MySQL actually executes.
fn is_dash_comment_start(chars: &[char], i: usize, mysql: bool) -> bool {
    if chars.get(i) != Some(&'-') || chars.get(i + 1) != Some(&'-') {
        return false;
    }
    if !mysql {
        return true;
    }
    chars
        .get(i + 2)
        .is_none_or(|next| next.is_whitespace() || next.is_control())
}

/// Scans SQL in one pass with per-engine comment and quote rules.
/// - String literals: ' " ` (doubled-quote escapes supported). Postgres also supports
///   dollar quoting ($tag$ ... $tag$) (# is the XOR operator in Postgres, so
///   it is not treated as a comment)
/// - Comments: -- and /* */. For MySQL, # line comments are also handled
///
/// **Backslash escapes (`'a\'b'`) are intentionally not interpreted.**
/// MySQL interprets them by default, but not in environments with NO_BACKSLASH_ESCAPES enabled.
/// Committing to either one gives:
/// - Leaning toward interpreting escapes -> in NO_BACKSLASH_ESCAPES environments,
///   `SELECT 'a\'; DROP TABLE t; --'` would swallow everything after the semicolon as a literal,
///   letting it slip past the multi-statement check and the readonly / dangerous-statement guards
/// - Leaning toward not interpreting (current) -> on default-setting MySQL, a legitimate query
///   containing a literal like `'a\'; b'` is judged to be "multiple statements" and rejected
/// The latter result is on the rejecting (safe) side of the guards, so we choose it
/// (to avoid it, escape the quote with `''`, or turn off the guards with Writable ON +
/// allow_dangerous_statements: true).
pub(crate) fn scan_sql(sql: &str, engine: Engine) -> SqlScan {
    let hash_comments = matches!(engine, Engine::MySql);
    // MySQL executable comments (`/*! ... */`) are interpreted and executed as SQL by the server
    let executable_comments = matches!(engine, Engine::MySql);
    // DuckDB supports dollar quoting, so its dialect is equivalent to Postgres
    let dollar_quotes = matches!(engine, Engine::Postgres | Engine::DuckDb);
    // T-SQL encloses identifiers in square brackets (`[name]`). `]]` escapes the closing bracket
    let bracket_quotes = matches!(engine, Engine::MsSql);
    let chars: Vec<char> = sql.chars().collect();
    let mut cleaned = String::with_capacity(sql.len());
    let mut body_end = 0;
    let mut byte_pos = 0;
    let mut i = 0;

    // Consume the i-th character and advance the byte position
    macro_rules! advance {
        () => {{
            byte_pos += chars[i].len_utf8();
            i += 1;
        }};
    }

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' || c == '`' {
            advance!();
            while i < chars.len() {
                let inner = chars[i];
                advance!();
                if inner == c {
                    if i < chars.len() && chars[i] == c {
                        advance!();
                        continue;
                    }
                    break;
                }
            }
            cleaned.push(' ');
            body_end = byte_pos;
        } else if bracket_quotes && c == '[' {
            advance!();
            while i < chars.len() {
                let inner = chars[i];
                advance!();
                if inner == ']' {
                    if i < chars.len() && chars[i] == ']' {
                        advance!();
                        continue;
                    }
                    break;
                }
            }
            cleaned.push(' ');
            body_end = byte_pos;
        } else if dollar_quotes && c == '$' {
            // Detect $tag$ ... $tag$ dollar quoting
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j < chars.len() && chars[j] == '$' {
                let tag: String = chars[i..=j].iter().collect();
                let tag_chars = j - i + 1;
                for _ in 0..tag_chars {
                    advance!();
                }
                // Look for the closing tag
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    if chars[i] == '$' && chars[i..].starts_with(&tag.chars().collect::<Vec<_>>()[..]) {
                        for _ in 0..tag_chars {
                            advance!();
                        }
                        break;
                    }
                    advance!();
                }
                cleaned.push(' ');
                body_end = byte_pos;
            } else {
                cleaned.push('$');
                advance!();
                body_end = byte_pos;
            }
        } else if is_dash_comment_start(&chars, i, hash_comments) {
            while i < chars.len() && chars[i] != '\n' {
                advance!();
            }
            cleaned.push(' ');
        } else if hash_comments && c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                advance!();
            }
            cleaned.push(' ');
        } else if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // MySQL executable comments (`/*! ... */` / `/*!50110 ... */`) are interpreted
            // and executed by the server as SQL, not as comments.
            // If they were blanked out like other comments here,
            // the DROP in `UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */` would vanish
            // from cleaned, and both the multi-statement check and the dangerous-statement guard would miss it.
            // Skip only the start marker (`/*!` and the version number) and
            // let the contents be scanned as normal SQL (the closing `*/` remains in
            // cleaned as symbols but does not affect word boundary detection).
            if executable_comments && chars.get(i + 2) == Some(&'!') {
                advance!();
                advance!();
                advance!();
                while i < chars.len() && chars[i].is_ascii_digit() {
                    advance!();
                }
                cleaned.push(' ');
                continue;
            }
            advance!();
            advance!();
            while i < chars.len() {
                if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    advance!();
                    advance!();
                    break;
                }
                advance!();
            }
            cleaned.push(' ');
        } else {
            let is_code = !c.is_whitespace() && c != ';';
            cleaned.push(c.to_ascii_lowercase());
            advance!();
            if is_code {
                body_end = byte_pos;
            }
        }
    }
    SqlScan { cleaned, body_end }
}

/// Determines whether a statement can safely get the default LIMIT.
/// Only SELECT-type statements are targeted. If it contains words such as LIMIT / FETCH / OFFSET /
/// FOR UPDATE / INTO / WITH ... INSERT, no LIMIT is added to avoid syntax errors or
/// changes in meaning (erring on the conservative side; even if skipped, the
/// client-side max_rows cutoff is the safety net).
pub(crate) fn should_auto_limit(sql: &str, engine: Engine) -> bool {
    // T-SQL gets TOP inserted instead of LIMIT. The decision of whether it is added must be the same as on the
    // inserting side (engines::mssql::apply_auto_top); otherwise lib.rs sees "LIMIT is added"
    // and leaves max_rows at its default, while TOP is actually not added and
    // default_limit has no effect (UNION / WITH / statements that already have TOP)
    if engine == Engine::MsSql {
        return crate::engines::mssql::apply_auto_top(sql, 1).is_some();
    }
    // VALUES (no LIMIT allowed in SQLite) and TABLE are not targeted;
    // limit it to SELECT / WITH only. DuckDB's FROM-first syntax
    // (`FROM t`) is the same query shape as SELECT, so it is targeted
    // (SUMMARIZE / PIVOT are not given one because whether a trailing LIMIT works depends on their shape)
    let kw = leading_keyword(sql);
    let applicable = matches!(kw.as_str(), "select" | "with")
        || (engine == Engine::DuckDb && kw == "from");
    if !applicable {
        return false;
    }
    let cleaned = scan_sql(sql, engine).cleaned;
    let veto_words = [
        "limit", "fetch", "offset", "for", "into", "insert", "update", "delete",
        "lock", "returning",
    ];
    !cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| veto_words.contains(&word))
}

/// Builds SQL with the per-engine EXPLAIN prefix.
/// Only SELECT / WITH are targeted (same leading_keyword check as should_auto_limit).
/// Postgres's EXPLAIN ANALYZE actually executes the target statement, so putting it on DML
/// would run the write. Erring on the safe side, everything other than SELECT-type is an error.
///
/// MySQL uses EXPLAIN FORMAT=JSON. EXPLAIN ANALYZE (8.0.18+) actually executes
/// the target statement and is not supported on MariaDB, so FORMAT=JSON, which gives
/// cost and row estimates without executing, is safer and more widely compatible.
pub fn build_explain_sql(engine: &str, sql: &str) -> Result<String, AppError> {
    let engine = parse_engine(engine)?;
    // DynamoDB (PartiQL) has no EXPLAIN
    if matches!(
        engine,
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb
    ) {
        return Err(AppError::Explain(
            "Explain is not available for this engine".into(),
        ));
    }
    if !matches!(leading_keyword(sql).as_str(), "select" | "with") {
        return Err(AppError::Explain(
            "Explain is available only for SELECT / WITH statements".into(),
        ));
    }
    // The EXPLAIN prefix only applies to the first statement, and the second and later statements
    // of `SELECT 1; DROP TABLE t;` are executed as is. There is no use case for passing multiple statements to EXPLAIN, so reject them.
    if contains_multiple_statements(sql, engine) {
        return Err(AppError::Explain(
            "Explain is available only for a single statement".into(),
        ));
    }
    // EXPLAIN ANALYZE actually executes the target statement, so even if it starts with SELECT / WITH,
    // statements that may write (SELECT INTO / DML with CTE) are excluded
    // (reusing the same conservative word check as is_readonly_allowed)
    if !is_readonly_allowed(sql, engine) {
        return Err(AppError::Explain(
            "Explain is not available for statements that may write data \
             (SELECT INTO / WITH ... INSERT / UPDATE / DELETE)"
                .into(),
        ));
    }
    let prefix = match engine {
        // ANALYZE gets measured time, and BUFFERS also gets buffer access statistics
        Engine::Postgres => "EXPLAIN (ANALYZE, BUFFERS)",
        Engine::MySql => "EXPLAIN FORMAT=JSON",
        Engine::Sqlite => "EXPLAIN QUERY PLAN",
        // DuckDB's EXPLAIN ANALYZE actually executes the target statement, so it is not used
        Engine::DuckDb => "EXPLAIN",
        // T-SQL has no EXPLAIN. engines/mssql.rs receives it as queryfolio's pseudo statement
        // and turns it into estimated execution plan rows with SET SHOWPLAN_ALL ON (it is not executed)
        Engine::MsSql => "EXPLAIN",
        // Redis / Elasticsearch / DynamoDb are rejected by the early return at the top
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb => unreachable!(),
    };
    Ok(format!("{prefix}\n{sql}"))
}

/// Determines at word boundaries whether it contains a RETURNING clause.
/// Used so as not to miss the result rows of INSERT / UPDATE / DELETE ... RETURNING
/// (Postgres / SQLite). It may also react to words inside string literals,
/// but even then the statement runs correctly on the fetch path (the affected
/// display just becomes a row count display), so that is tolerated.
pub(crate) fn contains_returning(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(pos) = lower[start..].find("returning") {
        let begin = start + pos;
        let end = begin + "returning".len();
        let before_ok = begin == 0
            || !(bytes[begin - 1].is_ascii_alphanumeric() || bytes[begin - 1] == b'_');
        let after_ok = end == bytes.len()
            || !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

/// Determines whether a statement is allowed to run on a readonly connection.
/// In addition to the leading keyword being read-type (is_fetch_statement):
/// - WITH: to reject a CTE body that is DML (WITH ... DELETE, etc.),
///   reject if the cleaned text (string literals and comments removed) contains the words
///   insert / update / delete / merge
/// - SELECT: to reject SELECT INTO (table creation in Postgres, INTO OUTFILE
///   etc. in MySQL), reject if the word into is contained
/// - EXPLAIN: EXPLAIN ANALYZE (Postgres / MySQL 8.0.19+) actually
///   executes the target statement, so reject if both analyze and a DML / into word
///   are contained (because the table creation of SELECT INTO and the file
///   writing of INTO OUTFILE would also be executed). EXPLAIN without ANALYZE does not
///   execute anything, so it is allowed even for DML
/// - PRAGMA: assignment-form PRAGMAs in SQLite (`PRAGMA user_version = 1`,
///   `PRAGMA journal_mode = WAL`, etc.) modify the DB, so a PRAGMA whose cleaned text
///   contains `=` is rejected. Read forms (`PRAGMA table_info(t)`,
///   `PRAGMA user_version`, etc.) are allowed
/// Words inside literals are removed by scan_sql, and partial matches against
/// column names and the like are not falsely detected thanks to word boundary splitting.
/// Weakness: it cannot stop side-effecting functions in SELECT (nextval, etc.), writes
/// inside CALLed procedures, or parenthesized setting PRAGMAs (`PRAGMA journal_mode(WAL)`, etc.).
/// It is only an accident-prevention guard.
/// Determines whether the statement is `;`-separated multiple statements.
///
/// Since it looks at cleaned, from which string literals and comments were already removed by scan_sql,
/// it does not react to semicolons inside literals such as `SELECT 'a;b'`.
/// A trailing semicolon (`SELECT 1;`) is treated as one statement.
pub(crate) fn contains_multiple_statements(sql: &str, engine: Engine) -> bool {
    let cleaned = scan_sql(sql, engine).cleaned;
    cleaned.trim_end().trim_end_matches(';').contains(';')
}

/// Error returned when multiple statements are passed on a connection with a guard enabled.
/// It also includes how to lift the guard (the caller knows which guard is in effect, but
/// multiple statements slip past both, so the decision is combined into one).
pub(crate) fn multi_statement_block_error() -> AppError {
    AppError::Readonly(
        "Multiple statements are not allowed while the read-only / safety guard is on. \
         Run one statement at a time, or turn Writable on and set \
         \"allow_dangerous_statements: true\" for this connection in config. \
         Statement was not executed."
            .into(),
    )
}

pub(crate) fn is_readonly_allowed(sql: &str, engine: Engine) -> bool {
    if !is_fetch_statement(sql) {
        return false;
    }
    let cleaned = scan_sql(sql, engine).cleaned;
    let has_word = |target: &str| {
        cleaned
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == target)
    };
    const DML_WORDS: &[&str] = &["insert", "update", "delete", "merge"];
    match leading_keyword(sql).as_str() {
        "with" => !DML_WORDS.iter().any(|w| has_word(w)),
        "select" => !has_word("into"),
        "explain" => {
            !(has_word("analyze")
                && (has_word("into")
                    || has_word("replace")
                    || DML_WORDS.iter().any(|w| has_word(w))))
        }
        // Assignment-form PRAGMA (`= value`) modifies the DB, so reject it
        "pragma" => !cleaned.contains('='),
        _ => true,
    }
}

/// Determines whether a statement is dangerous, i.e. could cause whole-table destruction or table loss by mistake, and if so
/// returns the reason (an English message shown on the frontend).
/// An accident-prevention guard that rejects these statements on connections where allow_dangerous_statements is off.
///
/// As with is_readonly_allowed, the decision is a word boundary check on cleaned, from which
/// string literals and comments were removed by scan_sql (it does not react to a where
/// inside a literal).
/// - UPDATE / DELETE: if there is no where word, it is considered "all rows" and dangerous
/// - TRUNCATE: always dangerous (deletes all rows)
/// - DROP: always dangerous (permanently deletes objects)
///
/// Not only the leading keyword but also the following wrapped forms that actually write are targeted:
/// - WITH ... DELETE / UPDATE (Postgres DML with CTE). Even though it starts with with, the body runs
///   an all-rows DELETE/UPDATE
/// - EXPLAIN ANALYZE / EXPLAIN (ANALYZE) ...: since the target statement is actually executed,
///   the DELETE/UPDATE/TRUNCATE/DROP inside is also targeted. EXPLAIN without ANALYZE does not
///   execute, so it is excluded
///
/// Weakness: for WITH, a where in an unrelated CTE / outer SELECT may be mistaken for "has WHERE"
/// and a DML without WHERE can be missed (the typical form containing no where at all is
/// caught). The same goes for a where only inside a subquery. It falls to the safe = allow side, so it is not
/// perfect; it is a guard that stops the representative accident patterns.
pub(crate) fn dangerous_reason(sql: &str, engine: Engine) -> Option<&'static str> {
    let cleaned = scan_sql(sql, engine).cleaned;
    let has_word = |target: &str| {
        cleaned
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == target)
    };
    let kw = leading_keyword(sql);
    // If it ends with only comments (leading_keyword is empty), the first word of cleaned is
    // treated as the leading keyword. MySQL executable comments (`/*! DROP TABLE t */`)
    // keep their contents in cleaned via scan_sql, so this picks up drop.
    // In dialects without executable comments, cleaned also keeps no contents, so this branch
    // stays empty for "comment-only input" and the decision does not change.
    let kw = if kw.is_empty() {
        cleaned
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .find(|word| !word.is_empty())
            .unwrap_or("")
            .to_string()
    } else {
        kw
    };

    // EXPLAIN ANALYZE / EXPLAIN (ANALYZE) actually executes the target statement,
    // so wrapped DML is also subject to the dangerous check. EXPLAIN without ANALYZE
    // does not execute, so it is excluded.
    let explain_executes = kw == "explain" && has_word("analyze");
    // Wrapped forms in which DML can run at execution time (DML with CTE / EXPLAIN ANALYZE).
    let wraps_dml = kw == "with" || explain_executes;

    let is_delete = kw == "delete" || (wraps_dml && has_word("delete"));
    let is_update = kw == "update" || (wraps_dml && has_word("update"));

    if is_delete && !has_word("where") {
        return Some("DELETE without a WHERE clause would remove every row.");
    }
    if is_update && !has_word("where") {
        return Some("UPDATE without a WHERE clause would modify every row.");
    }
    // TRUNCATE / DROP are always destructive. They cannot be written in WITH, so as a wrapped form
    // only the EXPLAIN ANALYZE route is considered.
    if kw == "truncate" || (explain_executes && has_word("truncate")) {
        return Some("TRUNCATE would remove every row from the table.");
    }
    if kw == "drop" || (explain_executes && has_word("drop")) {
        return Some("DROP would permanently destroy a database object.");
    }
    None
}

/// Wrapper for the frontend's pre-execution confirmation dialog. Returns the reason if the statement is dangerous.
/// It does not execute. Used on connections with allow_dangerous_statements enabled to decide
/// whether to ask the user for confirmation before execution.
pub fn dangerous_statement_reason(engine: &str, sql: &str) -> Result<Option<String>, AppError> {
    let engine = parse_engine(engine)?;
    if engine == Engine::Redis {
        return Ok(
            crate::engines::redis::dangerous_reason_for_input(sql).map(|s| s.to_string())
        );
    }
    if engine == Engine::Elasticsearch {
        return Ok(crate::engines::elasticsearch::dangerous_reason_for_input(sql));
    }
    Ok(dangerous_reason(sql, engine).map(|s| s.to_string()))
}

/// Determines by the leading keyword whether a statement returns rows.
/// Leading keywords of statements allowed for the AI agent (the chat's run_sql tool).
/// Intentionally narrower than the readonly guard for user operations (is_fetch_statement):
/// - `call`: a stored procedure can run DML inside (the readonly guard cannot
///   see inside, so it would pass)
/// - `pragma`: there are forms, such as the parenthesized one, that slip past assignment detection and can change DB settings
///
/// Since we want to structurally guarantee that the agent only reads, any entrance through which a write
/// could run is dropped (unlike human operations, the agent cannot fix it itself when rejected).
const AGENT_ALLOWED_KEYWORDS: &[&str] = &[
    "select",
    "with",
    "show",
    "describe",
    "desc",
    "explain",
    "values",
    "table",
];

/// Returns whether that SQL can be judged to have "no side effects even if executed again".
///
/// Copy / Export re-run the same SQL to avoid a truncated result table,
/// but running a statement that writes twice would be an accident. The check uses the same
/// strictness as the AI agent path (narrow whitelist + multiple statements forbidden + EXPLAIN ANALYZE forbidden +
/// readonly guard).
pub fn is_safe_to_rerun(sql: &str, engine: Engine) -> bool {
    // DynamoDB's `tables` is a read statement that just calls ListTables and does not fit the SQL-style
    // keyword check (AGENT_ALLOWED_KEYWORDS / is_fetch_statement)
    // (CYBERNEURA-DEV-406). If it is not picked up here, when the table count exceeds default_limit,
    // Copy / Export would output the truncated table as is.
    // This branch only affects the re-execution check for Copy / Export (the AI agent path
    // uses agent_rejection_reason directly)
    if engine == Engine::DynamoDb && crate::engines::dynamodb::is_tables_statement(sql) {
        return true;
    }
    agent_rejection_reason(sql, engine).is_none()
}

/// Returns the reason if the SQL the AI agent tried to run should be rejected.
/// In addition to the normal readonly guard, it imposes the narrow whitelist above and
/// a ban on multiple statements (`;`-separated).
pub(crate) fn agent_rejection_reason(sql: &str, engine: Engine) -> Option<String> {
    let keyword = leading_keyword(sql);
    if !AGENT_ALLOWED_KEYWORDS.contains(&keyword.as_str()) {
        return Some(format!(
            "The assistant may only run read-only statements ({}); rejected.",
            AGENT_ALLOWED_KEYWORDS.join(" / ").to_uppercase()
        ));
    }
    // Multiple statements may pass depending on the driver and can
    // slip past guards that only look at the first statement, so they are uniformly rejected on the agent path
    if contains_multiple_statements(sql, engine) {
        return Some("The assistant may only run one statement at a time; rejected.".to_string());
    }
    let cleaned = scan_sql(sql, engine).cleaned;
    // EXPLAIN ANALYZE actually executes the target statement. is_readonly_allowed only looks at the
    // DML / INTO inside, so DDL such as `EXPLAIN (ANALYZE) CREATE TABLE x AS SELECT ...`
    // slips through. The agent has no need for an EXPLAIN that executes
    // (ANALYZE-less is enough to just see the plan), so they are rejected together.
    let has_word = |target: &str| {
        cleaned
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == target)
    };
    if keyword == "explain" && has_word("analyze") {
        return Some(
            "The assistant may not run EXPLAIN ANALYZE (it executes the statement); rejected."
                .to_string(),
        );
    }
    if !is_readonly_allowed(sql, engine) {
        return Some("The statement is not read-only; rejected.".to_string());
    }
    None
}

pub(crate) fn is_fetch_statement(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "select"
            | "with"
            | "show"
            | "describe"
            | "desc"
            | "explain"
            | "pragma"
            | "values"
            | "table"
            | "call"
    )
}

pub(crate) fn bytes_to_json(bytes: Vec<u8>) -> serde_json::Value {
    match String::from_utf8(bytes) {
        Ok(s) => serde_json::Value::String(s),
        Err(e) => serde_json::Value::String(format!(
            "base64:{}",
            base64::engine::general_purpose::STANDARD.encode(e.as_bytes())
        )),
    }
}

/// Macro that tries decoding as the given type and, on success, returns it as JSON.
/// NULL becomes JSON null. A type mismatch falls through to the next candidate.
macro_rules! try_decode {
    ($row:expr, $i:expr, $t:ty, $conv:expr) => {
        match $row.try_get::<Option<$t>, _>($i) {
            Ok(Some(v)) => {
                #[allow(clippy::redundant_closure_call)]
                return ($conv)(v);
            }
            Ok(None) => return serde_json::Value::Null,
            Err(_) => {}
        }
    };
}

/// Final fallback when decoding is impossible with any type.
macro_rules! decode_fallback {
    ($row:expr, $i:expr) => {{
        try_decode!($row, $i, String, |v: String| serde_json::Value::String(v));
        try_decode!($row, $i, Vec<u8>, bytes_to_json);
        let type_name = $row.column($i).type_info().name().to_string();
        serde_json::Value::String(format!("<undecodable: {type_name}>"))
    }};
}

fn json_number_f64(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(v.to_string()))
}

/// JavaScript's Number cannot represent integers beyond 2^53-1 (MAX_SAFE_INTEGER),
/// and they would be rounded at Tauri's invoke boundary.
/// 64-bit integers beyond the safe range are returned as strings to preserve precision.
const JS_MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

pub(crate) fn json_i64(v: i64) -> serde_json::Value {
    if (-JS_MAX_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&v) {
        serde_json::json!(v)
    } else {
        serde_json::Value::String(v.to_string())
    }
}

pub(crate) fn json_u64(v: u64) -> serde_json::Value {
    if v <= JS_MAX_SAFE_INTEGER as u64 {
        serde_json::json!(v)
    } else {
        serde_json::Value::String(v.to_string())
    }
}

fn format_naive_datetime(v: chrono::NaiveDateTime) -> serde_json::Value {
    serde_json::Value::String(v.format("%Y-%m-%d %H:%M:%S%.f").to_string())
}

fn mysql_value_to_json(row: &MySqlRow, i: usize) -> serde_json::Value {
    let type_name = row.column(i).type_info().name().to_string();
    match type_name.as_str() {
        "BOOLEAN" => {
            try_decode!(row, i, bool, |v: bool| serde_json::Value::Bool(v));
        }
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            try_decode!(row, i, i64, json_i64);
        }
        // YEAR carries the UNSIGNED flag inside sqlx, so decode it on the u64 side
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED"
        | "INT UNSIGNED" | "BIGINT UNSIGNED" | "YEAR" => {
            try_decode!(row, i, u64, json_u64);
        }
        "FLOAT" | "DOUBLE" => {
            try_decode!(row, i, f64, json_number_f64);
        }
        "DECIMAL" => {
            // Return as a string to preserve precision
            try_decode!(row, i, rust_decimal::Decimal, |v: rust_decimal::Decimal| {
                serde_json::Value::String(v.to_string())
            });
        }
        "DATE" => {
            try_decode!(row, i, chrono::NaiveDate, |v: chrono::NaiveDate| {
                serde_json::Value::String(v.format("%Y-%m-%d").to_string())
            });
        }
        "TIME" => {
            try_decode!(row, i, chrono::NaiveTime, |v: chrono::NaiveTime| {
                serde_json::Value::String(v.format("%H:%M:%S%.f").to_string())
            });
        }
        "DATETIME" => {
            try_decode!(row, i, chrono::NaiveDateTime, format_naive_datetime);
        }
        "TIMESTAMP" => {
            try_decode!(
                row,
                i,
                chrono::DateTime<chrono::Utc>,
                |v: chrono::DateTime<chrono::Utc>| serde_json::Value::String(
                    v.to_rfc3339()
                )
            );
        }
        "JSON" => {
            try_decode!(row, i, serde_json::Value, |v| v);
        }
        _ => {}
    }
    decode_fallback!(row, i)
}

/// Turns the Postgres array binary representation (array_send in arrayfuncs.c) into a JSON array.
/// Multidimensional arrays become nested arrays, and NULL elements become null. The contents of elements are
/// converted by `decode_element`. sqlx's `Vec<T>` decoder only accepts one-dimensional arrays whose
/// subscripts start at 1, so we read it ourselves (the lower bound of subscripts cannot be put in JSON, so it is discarded).
/// None if the format is broken (the caller falls back to `<undecodable>`).
fn pg_binary_array_to_json(
    buf: &[u8],
    decode_element: impl Fn(&[u8]) -> serde_json::Value,
) -> Option<serde_json::Value> {
    // Postgres's MAXDIM
    const MAX_DIMS: usize = 6;

    fn take<'a>(buf: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if buf.len() < n {
            return None;
        }
        let (head, rest) = buf.split_at(n);
        *buf = rest;
        Some(head)
    }
    fn take_i32(buf: &mut &[u8]) -> Option<i32> {
        take(buf, 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    let mut buf = buf;
    let ndim = usize::try_from(take_i32(&mut buf)?).ok()?;
    // A has-null flag other than 0 / 1 is broken input (same check as array_recv).
    // The element type OID is not used (the element type is known from the column's type info)
    if !matches!(take_i32(&mut buf)?, 0 | 1) {
        return None;
    }
    take(&mut buf, 4)?;
    if ndim == 0 {
        return buf.is_empty().then(|| serde_json::Value::Array(Vec::new()));
    }
    if ndim > MAX_DIMS {
        return None;
    }
    let mut dims = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        let len = usize::try_from(take_i32(&mut buf)?).ok()?;
        // Postgres normalizes an empty array to ndim = 0, so a zero-length dimension never arrives
        // (the exceptions int2vector / oidvector do not go through here = the enum array path).
        // If one arrives, reject it as broken input (if an inner dimension is 0, the outer loop keeps
        // spinning without reading a single byte of input, slipping past the element count check)
        if len == 0 {
            return None;
        }
        dims.push(len);
        // Lower bound of the subscript
        take(&mut buf, 4)?;
    }
    // Each element takes at least 4 bytes (the length), so input whose total element count cannot be
    // covered by the remaining bytes is rejected before any allocation or looping
    let total = dims.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d))?;
    if total > buf.len() / 4 {
        return None;
    }

    // Elements are laid out in row-major order, so recurse per dimension to nest them
    fn build(
        dims: &[usize],
        buf: &mut &[u8],
        decode_element: &dyn Fn(&[u8]) -> serde_json::Value,
    ) -> Option<serde_json::Value> {
        let (&len, inner) = dims.split_first()?;
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            if inner.is_empty() {
                let elem_len = take_i32(buf)?;
                // NULL is only -1. Any other negative length is broken input
                if elem_len == -1 {
                    items.push(serde_json::Value::Null);
                } else {
                    let elem_len = usize::try_from(elem_len).ok()?;
                    items.push(decode_element(take(buf, elem_len)?));
                }
            } else {
                items.push(build(inner, buf, decode_element)?);
            }
        }
        Some(serde_json::Value::Array(items))
    }
    let value = build(&dims, &mut buf, &decode_element)?;
    // If bytes are left over after reading the declared element count, the header and the contents disagree
    buf.is_empty().then_some(value)
}

fn pg_value_to_json(row: &PgRow, i: usize) -> serde_json::Value {
    // User-defined enum types (and their arrays) are not included in the types compatible with
    // sqlx's String decoder (TEXT / VARCHAR, etc.), so as is they would hit decode_fallback
    // `<undecodable>`. An enum value is the label's UTF-8 in both text and binary
    // formats, so read the raw value as a string as is
    let enum_value = match row.column(i).type_info().kind() {
        PgTypeKind::Enum(_) => Some(false),
        PgTypeKind::Array(elem) if matches!(elem.kind(), PgTypeKind::Enum(_)) => Some(true),
        _ => None,
    };
    if let (Some(is_array), Ok(raw)) = (enum_value, row.try_get_raw(i)) {
        if raw.is_null() {
            return serde_json::Value::Null;
        }
        let format = raw.format();
        let decoded = match (is_array, format) {
            (true, PgValueFormat::Binary) => raw.as_bytes().ok().and_then(|bytes| {
                pg_binary_array_to_json(bytes, |b| bytes_to_json(b.to_vec()))
            }),
            // Show a text-format array as the `{a,b}` literal as is
            _ => raw
                .as_str()
                .ok()
                .map(|label| serde_json::Value::String(label.to_string())),
        };
        if let Some(v) = decoded {
            return v;
        }
    }
    let type_name = row.column(i).type_info().name().to_string();
    match type_name.as_str() {
        "BOOL" => {
            try_decode!(row, i, bool, |v: bool| serde_json::Value::Bool(v));
        }
        // Postgres numeric types have strict type compatibility, so decode with the same width as the column type
        "INT2" => {
            try_decode!(row, i, i16, |v: i16| serde_json::json!(v));
        }
        "INT4" => {
            try_decode!(row, i, i32, |v: i32| serde_json::json!(v));
        }
        "INT8" => {
            try_decode!(row, i, i64, json_i64);
        }
        "FLOAT4" => {
            try_decode!(row, i, f32, |v: f32| json_number_f64(v as f64));
        }
        "FLOAT8" => {
            try_decode!(row, i, f64, json_number_f64);
        }
        "NUMERIC" => {
            try_decode!(row, i, rust_decimal::Decimal, |v: rust_decimal::Decimal| {
                serde_json::Value::String(v.to_string())
            });
        }
        "UUID" => {
            try_decode!(row, i, uuid::Uuid, |v: uuid::Uuid| {
                serde_json::Value::String(v.to_string())
            });
        }
        "DATE" => {
            try_decode!(row, i, chrono::NaiveDate, |v: chrono::NaiveDate| {
                serde_json::Value::String(v.format("%Y-%m-%d").to_string())
            });
        }
        "TIME" => {
            try_decode!(row, i, chrono::NaiveTime, |v: chrono::NaiveTime| {
                serde_json::Value::String(v.format("%H:%M:%S%.f").to_string())
            });
        }
        "TIMESTAMP" => {
            try_decode!(row, i, chrono::NaiveDateTime, format_naive_datetime);
        }
        "TIMESTAMPTZ" => {
            try_decode!(
                row,
                i,
                chrono::DateTime<chrono::Utc>,
                |v: chrono::DateTime<chrono::Utc>| serde_json::Value::String(
                    v.to_rfc3339()
                )
            );
        }
        "JSON" | "JSONB" => {
            try_decode!(row, i, serde_json::Value, |v| v);
        }
        "BYTEA" => {
            try_decode!(row, i, Vec<u8>, bytes_to_json);
        }
        _ => {}
    }
    decode_fallback!(row, i)
}

fn sqlite_value_to_json(row: &SqliteRow, i: usize) -> serde_json::Value {
    let type_name = row.column(i).type_info().name().to_string();
    match type_name.as_str() {
        "BOOLEAN" => {
            try_decode!(row, i, bool, |v: bool| serde_json::Value::Bool(v));
        }
        "INTEGER" | "INT" => {
            try_decode!(row, i, i64, json_i64);
        }
        "REAL" => {
            try_decode!(row, i, f64, json_number_f64);
        }
        "TEXT" | "DATE" | "DATETIME" | "TIME" => {
            try_decode!(row, i, String, |v: String| serde_json::Value::String(v));
        }
        "BLOB" => {
            try_decode!(row, i, Vec<u8>, bytes_to_json);
        }
        "NUMERIC" => {
            try_decode!(row, i, i64, json_i64);
            try_decode!(row, i, f64, json_number_f64);
        }
        _ => {}
    }
    // sqlite is dynamically typed, so the declared type and the actual value may not match
    try_decode!(row, i, i64, json_i64);
    try_decode!(row, i, f64, json_number_f64);
    decode_fallback!(row, i)
}

/// Decides the value to show as the "active schema" from the connection config's `schema`.
///
/// Only redis is handled differently. connect in `engines/redis.rs` trims schema before
/// looking at it, so **both unset and whitespace-only connect to database 0**. Unless both are
/// normalized to `Some("0")` here, the Database dropdown ends up "matching none of the
/// options", leaving a mismatch where the leading 0 appears selected on screen but the
/// app state is empty (CYBERNEURA-DEV-408).
pub fn resolve_active_schema(engine: &str, schema: Option<&str>) -> Option<String> {
    let is_redis = matches!(parse_engine(engine), Ok(Engine::Redis));
    match schema {
        Some(s) if !(is_redis && s.trim().is_empty()) => Some(s.to_string()),
        _ if is_redis => Some("0".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn resolve_active_schema_keeps_explicit_values() {
        assert_eq!(
            super::resolve_active_schema("redis", Some("3")),
            Some("3".to_string())
        );
        assert_eq!(
            super::resolve_active_schema("postgres", Some("public")),
            Some("public".to_string())
        );
    }

    #[test]
    fn resolve_active_schema_defaults_redis_to_zero() {
        // Unset, empty and whitespace-only all make connect use database 0,
        // so treat them the same.
        for schema in [None, Some(""), Some("   ")] {
            assert_eq!(
                super::resolve_active_schema("redis", schema),
                Some("0".to_string()),
                "schema={schema:?}"
            );
        }
        // The same goes for aliases
        assert_eq!(
            super::resolve_active_schema("valkey", Some("")),
            Some("0".to_string())
        );
    }

    #[test]
    fn resolve_active_schema_leaves_other_engines_untouched() {
        assert_eq!(super::resolve_active_schema("postgres", None), None);
        // Except for redis, return an empty string as is (this normalization is redis-specific)
        assert_eq!(
            super::resolve_active_schema("postgres", Some("")),
            Some(String::new())
        );
    }

    use super::*;

    #[test]
    fn test_leading_keyword() {
        assert_eq!(leading_keyword("SELECT 1"), "select");
        assert_eq!(leading_keyword("  \n\t select 1"), "select");
        assert_eq!(leading_keyword("-- comment\nSELECT 1"), "select");
        assert_eq!(leading_keyword("/* c1 */ /* c2 */ UPDATE t SET a=1"), "update");
        assert_eq!(leading_keyword("# mysql comment\nSHOW TABLES"), "show");
        assert_eq!(leading_keyword(""), "");
        assert_eq!(leading_keyword("-- only comment"), "");
    }

    #[test]
    fn test_is_fetch_statement() {
        assert!(is_fetch_statement("SELECT * FROM t"));
        assert!(is_fetch_statement("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_fetch_statement("SHOW TABLES"));
        assert!(is_fetch_statement("EXPLAIN SELECT 1"));
        assert!(is_fetch_statement("PRAGMA table_info(t)"));
        assert!(!is_fetch_statement("INSERT INTO t VALUES (1)"));
        assert!(!is_fetch_statement("UPDATE t SET a = 1"));
        assert!(!is_fetch_statement("DELETE FROM t"));
        assert!(!is_fetch_statement("CREATE TABLE t (a int)"));
    }

    #[test]
    fn test_agent_rejection_reason() {
        let f = |s: &str| agent_rejection_reason(s, Engine::Sqlite);
        // Read statements pass
        assert!(f("SELECT * FROM t").is_none());
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x").is_none());
        assert!(f("EXPLAIN SELECT 1").is_none());
        assert!(f("SHOW TABLES").is_none());
        // A single trailing semicolon is not multiple statements
        assert!(f("SELECT 1;").is_none());
        assert!(f("SELECT 1;  ").is_none());
        // CALL passes the readonly guard (a stored procedure can run DML
        // inside), but it is rejected on the agent path
        assert!(is_readonly_allowed("CALL do_something()", Engine::MySql));
        assert!(f("CALL do_something()").is_some());
        // The readonly guard also lets read-form PRAGMA through, but the agent does not need it
        assert!(is_readonly_allowed("PRAGMA user_version", Engine::Sqlite));
        assert!(f("PRAGMA user_version").is_some());
        // Write statements are rejected, of course
        assert!(f("UPDATE t SET a = 1").is_some());
        assert!(f("DROP TABLE t").is_some());
        // Multiple statements are rejected even if the first one is a read
        assert!(f("SELECT 1; DELETE FROM t").is_some());
        // EXPLAIN ANALYZE executes the target statement, so it is rejected. In particular,
        // EXPLAIN (ANALYZE) CREATE TABLE ... AS SELECT contains neither a DML word nor INTO,
        // so it passes is_readonly_allowed
        assert!(is_readonly_allowed(
            "EXPLAIN (ANALYZE) CREATE TABLE agent_tmp AS SELECT 1",
            Engine::Postgres
        ));
        assert!(agent_rejection_reason(
            "EXPLAIN (ANALYZE) CREATE TABLE agent_tmp AS SELECT 1",
            Engine::Postgres
        )
        .is_some());
        assert!(agent_rejection_reason("EXPLAIN ANALYZE SELECT 1", Engine::Postgres).is_some());
        // EXPLAIN without ANALYZE does not execute, so it is allowed
        assert!(agent_rejection_reason("EXPLAIN SELECT 1", Engine::Postgres).is_none());
        // A semicolon inside a literal is not multiple statements
        assert!(f("SELECT 'a; b' FROM t").is_none());
    }

    #[test]
    fn test_contains_multiple_statements() {
        let f = |s: &str| contains_multiple_statements(s, Engine::Sqlite);
        // One statement (with or without a trailing semicolon)
        assert!(!f("SELECT 1"));
        assert!(!f("SELECT 1;"));
        assert!(!f("SELECT 1;  "));
        assert!(!f("SELECT 1;\n"));
        // Multiple statements
        assert!(f("SELECT 1; DELETE FROM t"));
        assert!(f("SELECT 1; DELETE FROM t;"));
        assert!(f("UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;"));
        // Does not react to semicolons inside literals and comments
        assert!(!f("SELECT 'a; b' FROM t"));
        assert!(!f("SELECT 1 -- ; not a statement"));
        assert!(!f("/* ; */ SELECT 1"));
        // The contents of dollar quoting (Postgres) are also removed from cleaned
        assert!(!contains_multiple_statements(
            "SELECT $$a; b$$",
            Engine::Postgres
        ));
    }

    /// MySQL's `--` is a line comment only when immediately followed by whitespace.
    /// If this were treated like other dialects, the second statement of `SELECT 1--1; DROP TABLE t;`
    /// would vanish as a comment and slip past both the multi-statement check and the guards.
    #[test]
    fn test_mysql_dash_comment_requires_whitespace() {
        assert!(contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::MySql
        ));
        // `--;` is also not followed by whitespace, so it is not a comment (safe side = detected as multiple statements)
        assert!(contains_multiple_statements(
            "SELECT 1 --; DROP TABLE t;",
            Engine::MySql
        ));
        // `--` followed by whitespace / at end of line is a comment as before
        assert!(!contains_multiple_statements(
            "SELECT 1 -- ; DROP TABLE t;",
            Engine::MySql
        ));
        assert!(!contains_multiple_statements("SELECT 1 --", Engine::MySql));
        assert!(is_readonly_allowed(
            "SELECT * FROM t -- delete\n",
            Engine::MySql
        ));
        // MySQL's # line comment works as before
        assert!(!contains_multiple_statements(
            "SELECT 1 # ; DROP TABLE t",
            Engine::MySql
        ));
        // Backslash escapes are not interpreted (see the comment on scan_sql).
        // On default-setting MySQL it is one statement, but in NO_BACKSLASH_ESCAPES environments
        // the second statement is executed, so lean toward rejection = the safe side
        assert!(contains_multiple_statements(
            r"SELECT 'a\'; DROP TABLE t; --'",
            Engine::MySql
        ));
        // Escaping by doubling is treated as one literal as before
        assert!(!contains_multiple_statements(
            "SELECT 'a''; still one literal'",
            Engine::MySql
        ));

        // In other dialects, `--` is a comment no matter what follows it
        assert!(!contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::Postgres
        ));
        assert!(!contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::Sqlite
        ));
    }

    /// MySQL executable comments (`/*! ... */`) are executed by the server as SQL,
    /// so dropping them as comments would hide them from the guards.
    #[test]
    fn test_mysql_executable_comments_are_code() {
        // The second statement of multiple statements cannot be hidden in an executable comment
        assert!(contains_multiple_statements(
            "UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */",
            Engine::MySql
        ));
        // Picked up as a dangerous statement even if it starts with an executable comment (leading_keyword
        // does not receive a dialect and so returns empty, but dangerous_reason fills in from cleaned)
        assert_eq!(leading_keyword("/*! DROP TABLE t */"), "");
        assert!(dangerous_reason("/*! DROP TABLE t */", Engine::MySql).is_some());
        assert!(dangerous_reason("/*!50110 TRUNCATE TABLE t */", Engine::MySql).is_some());
        assert!(!is_readonly_allowed("/*! DELETE FROM t */", Engine::MySql));
        assert!(!is_readonly_allowed(
            "WITH x AS (SELECT 1) /*! DELETE FROM t */",
            Engine::MySql
        ));
        // Normal block comments are dropped as comments as before
        assert_eq!(leading_keyword("/* c */ SELECT 1"), "select");
        assert!(!contains_multiple_statements(
            "SELECT 1 /* ; DROP TABLE t */",
            Engine::MySql
        ));
        assert!(dangerous_reason("SELECT 1 /* DROP TABLE t */", Engine::MySql).is_none());
        // scan_sql treats executable comments as such only for MySQL, so in other dialects it is a comment as before.
        // Conversely, if leading_keyword were made dialect-independent and executable-comment-aware,
        // in other dialects the head of `/*! SELECT 1 */ DROP TABLE t` would be misread as select
        // and the DROP missed, so the common parser is left unchanged
        assert!(!contains_multiple_statements(
            "UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */",
            Engine::Postgres
        ));
        assert_eq!(leading_keyword("/*! SELECT 1 */ DROP TABLE t"), "drop");
        assert!(dangerous_reason("/*! SELECT 1 */ DROP TABLE t", Engine::Sqlite).is_some());
        assert!(dangerous_reason("/*! SELECT 1 */ DROP TABLE t", Engine::Postgres).is_some());
        assert!(!is_readonly_allowed(
            "/*! SELECT 1 */ DROP TABLE t",
            Engine::Sqlite
        ));
        // Comment-only input is out of scope as before
        assert!(dangerous_reason("-- only comment", Engine::MySql).is_none());
        assert!(dangerous_reason("/* just a comment */", Engine::Postgres).is_none());
        // A LIMIT inside an executable comment is also visible, so no auto LIMIT is added
        assert!(!should_auto_limit(
            "SELECT * FROM t /*! LIMIT 5 */",
            Engine::MySql
        ));
    }

    /// Multiple statements must not be able to slip past the guards.
    ///
    /// is_readonly_allowed / dangerous_reason only look at the leading keyword, so
    /// these statements are judged "allowed" on their own. On connections where a guard is enabled,
    /// run_query_on / run_statements prevent this by rejecting at the multiple-statement stage.
    #[test]
    fn test_multi_statement_bypasses_keyword_guards() {
        // The readonly guard lets it pass because the first statement is a SELECT
        assert!(is_readonly_allowed(
            "SELECT 1; DELETE FROM t;",
            Engine::Sqlite
        ));
        assert!(contains_multiple_statements(
            "SELECT 1; DELETE FROM t;",
            Engine::Sqlite
        ));

        // The dangerous-statement guard lets it pass because the first statement has a WHERE
        assert!(dangerous_reason(
            "UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;",
            Engine::Sqlite
        )
        .is_none());
        assert!(contains_multiple_statements(
            "UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;",
            Engine::Sqlite
        ));

        // EXPLAIN only applies to the first statement, so reject at build time
        assert!(build_explain_sql("sqlite", "SELECT 1; DROP TABLE t;").is_err());
        assert!(build_explain_sql("sqlite", "SELECT 1;").is_ok());
    }

    #[test]
    fn test_is_readonly_allowed() {
        let f = |s: &str| is_readonly_allowed(s, Engine::Sqlite);
        // Read statements are allowed
        assert!(f("SELECT * FROM t"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(f("EXPLAIN SELECT 1"));
        assert!(f("SHOW TABLES"));
        assert!(f("PRAGMA table_info(t)"));
        // Read-form PRAGMA is allowed
        assert!(f("PRAGMA user_version"));
        assert!(f("PRAGMA journal_mode"));
        // Assignment-form PRAGMA (modifies the DB) is rejected
        assert!(!f("PRAGMA user_version = 1"));
        assert!(!f("PRAGMA journal_mode = WAL"));
        assert!(!f("PRAGMA foreign_keys=ON"));
        // Write statements are rejected
        assert!(!f("UPDATE t SET a = 1"));
        assert!(!f("INSERT INTO t VALUES (1)"));
        assert!(!f("DROP TABLE t"));
        // DML with CTE is rejected even if it starts with with
        assert!(!f("WITH old AS (SELECT id FROM t) DELETE FROM t WHERE id IN (SELECT id FROM old)"));
        assert!(!f("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"));
        assert!(!f("WITH x AS (SELECT 1) UPDATE t SET a = 1"));
        assert!(!f("with x as (select 1)\nmerge into t using x on true"));
        // SELECT INTO (Postgres table creation / MySQL INTO OUTFILE) is rejected
        assert!(!is_readonly_allowed("SELECT * INTO new_table FROM t", Engine::Postgres));
        assert!(!is_readonly_allowed(
            "SELECT * FROM t INTO OUTFILE '/tmp/x'",
            Engine::MySql
        ));
        // Words inside literals are removed by scan_sql, so no false detection
        assert!(f("WITH x AS (SELECT 'delete') SELECT * FROM x"));
        assert!(f("SELECT 'into' FROM t"));
        // Word boundary: a partial match is not rejected
        assert!(f("WITH x AS (SELECT id FROM deleted_items) SELECT * FROM x"));
        assert!(f("SELECT * FROM intolerant"));
        // EXPLAIN ANALYZE is allowed if the target is SELECT-type
        assert!(is_readonly_allowed(
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM t",
            Engine::Postgres
        ));
        assert!(is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM t",
            Engine::MySql
        ));
        // EXPLAIN ANALYZE actually executes the target DML, so it is rejected
        assert!(!is_readonly_allowed(
            "EXPLAIN ANALYZE DELETE FROM t",
            Engine::Postgres
        ));
        assert!(!is_readonly_allowed(
            "EXPLAIN (ANALYZE) UPDATE t SET a = 1",
            Engine::Postgres
        ));
        assert!(!is_readonly_allowed(
            "EXPLAIN ANALYZE INSERT INTO t VALUES (1)",
            Engine::Postgres
        ));
        assert!(!is_readonly_allowed(
            "explain analyze replace into t values (1)",
            Engine::MySql
        ));
        // EXPLAIN ANALYZE + SELECT INTO would run Postgres table creation or
        // the MySQL INTO OUTFILE file write, so it is rejected
        assert!(!is_readonly_allowed(
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * INTO new_table FROM t",
            Engine::Postgres
        ));
        assert!(!is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM t INTO OUTFILE '/tmp/x'",
            Engine::MySql
        ));
        // EXPLAIN without ANALYZE does not execute, so even DML is allowed
        assert!(is_readonly_allowed("EXPLAIN DELETE FROM t", Engine::Postgres));
        // No false detection for partial matches against table names or words inside literals
        assert!(is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM delete_log",
            Engine::Postgres
        ));
        assert!(is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM t WHERE op = 'delete'",
            Engine::Postgres
        ));
    }

    #[test]
    fn test_dangerous_reason() {
        let d = |s: &str| dangerous_reason(s, Engine::Sqlite).is_some();
        // UPDATE / DELETE without WHERE is dangerous
        assert!(d("UPDATE t SET a = 1"));
        assert!(d("DELETE FROM t"));
        assert!(d("delete from t"));
        // With WHERE it is safe
        assert!(!d("UPDATE t SET a = 1 WHERE id = 1"));
        assert!(!d("DELETE FROM t WHERE id = 1"));
        // Even with a leading comment, the decision is based on the leading keyword
        assert!(d("-- oops\nUPDATE t SET a = 1"));
        assert!(!d("/* c */ DELETE FROM t WHERE id = 1"));
        // DROP / TRUNCATE are always dangerous
        assert!(d("DROP TABLE t"));
        assert!(d("TRUNCATE TABLE t"));
        assert!(dangerous_reason("TRUNCATE t", Engine::Postgres).is_some());
        // Other statements (read statements, INSERT, DDL) are out of scope
        assert!(!d("SELECT * FROM t"));
        assert!(!d("INSERT INTO t VALUES (1)"));
        assert!(!d("CREATE TABLE t (id INTEGER)"));
        assert!(!d("ALTER TABLE t ADD COLUMN x TEXT"));
        // Does not react to where or drop in literals or column names (word boundary / literal removal)
        assert!(d("UPDATE t SET note = 'where is it'"));
        assert!(!d("UPDATE t SET a = 1 WHERE label = 'drop'"));
        // Stating the weakness: an all-rows UPDATE with where only inside a subquery is missed (falls to the allow side)
        assert!(!d("UPDATE t SET a = (SELECT max(b) FROM u WHERE u.id = 1)"));

        // DML without WHERE wrapped in a CTE (WITH) is also caught (Postgres)
        let p = |s: &str| dangerous_reason(s, Engine::Postgres).is_some();
        assert!(p("WITH d AS (DELETE FROM users RETURNING *) SELECT count(*) FROM d"));
        assert!(p("WITH x AS (SELECT 1) UPDATE t SET a = 1"));
        // If the DML inside the CTE has a WHERE, it is out of scope (scoped)
        assert!(!p("WITH d AS (DELETE FROM users WHERE id = 1 RETURNING *) SELECT count(*) FROM d"));
        // A pure read CTE is out of scope
        assert!(!p("WITH d AS (SELECT * FROM t) SELECT * FROM d"));
        assert!(!p("WITH d AS (SELECT deleted_at FROM t) SELECT * FROM d"));

        // EXPLAIN ANALYZE executes the target statement, so DML without WHERE inside it is caught
        assert!(p("EXPLAIN ANALYZE DELETE FROM users"));
        assert!(p("EXPLAIN (ANALYZE) UPDATE t SET a = 1"));
        assert!(dangerous_reason("EXPLAIN ANALYZE DELETE FROM users", Engine::MySql).is_some());
        // EXPLAIN without ANALYZE does not execute, so it is out of scope
        assert!(!p("EXPLAIN DELETE FROM users"));
        assert!(!p("EXPLAIN SELECT * FROM t"));
        // Even with EXPLAIN ANALYZE, it is out of scope if the inside is a read
        assert!(!p("EXPLAIN ANALYZE SELECT * FROM t"));

        // Public wrapper: an unknown engine is an error
        assert!(dangerous_statement_reason("mysql", "DROP TABLE t")
            .unwrap()
            .is_some());
        assert!(dangerous_statement_reason("mysql", "SELECT 1")
            .unwrap()
            .is_none());
        assert!(dangerous_statement_reason("bogus", "DROP TABLE t").is_err());
    }

    /// The rollback on a failed switch is a compare-and-swap,
    /// so a value changed by another path in the meantime is not rolled back.
    #[tokio::test]
    async fn test_rollback_schema_override_is_compare_and_swap() {
        let manager = DbManager::default();

        // Switch from the state where the original is None (config default) and fail -> cleared
        manager.set_schema_override("conn", "tried".to_string()).await;
        assert!(manager.rollback_schema_override("conn", "tried", None).await);
        assert_eq!(manager.schema_override("conn").await, None);

        // Switch from the state where the original has a value and fail -> back to the original value
        manager.set_schema_override("conn", "before".to_string()).await;
        manager.set_schema_override("conn", "tried".to_string()).await;
        assert!(
            manager
                .rollback_schema_override("conn", "tried", Some("before".to_string()))
                .await
        );
        assert_eq!(
            manager.schema_override("conn").await,
            Some("before".to_string())
        );

        // If the user changed it to a different value during the switch, it is not rolled back
        manager.set_schema_override("conn", "tried".to_string()).await;
        manager.set_schema_override("conn", "chosen".to_string()).await;
        assert!(
            !manager
                .rollback_schema_override("conn", "tried", Some("before".to_string()))
                .await
        );
        assert_eq!(
            manager.schema_override("conn").await,
            Some("chosen".to_string())
        );
    }

    #[tokio::test]
    async fn test_disconnect_keeps_schema_override() {
        let manager = DbManager::default();
        // Even if disconnected with an active schema selected, the selection is kept
        // (so that the next reconnect uses the same schema).
        manager.set_schema_override("conn", "chosen".to_string()).await;
        manager.disconnect("conn").await;
        assert_eq!(
            manager.schema_override("conn").await,
            Some("chosen".to_string())
        );
        // Disconnecting a nonexistent connection does not panic (safe no matter how many times it is called).
        manager.disconnect("no-such-conn").await;
        manager.disconnect("conn").await;
    }

    #[tokio::test]
    async fn test_run_query_sqlite_dangerous_guard() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        let pool = DbPool::Sqlite(pool);

        // Setup (write freely with allow_dangerous=true)
        run_query(
            &pool,
            "CREATE TABLE t (id INTEGER, name TEXT)",
            10,
            None,
            false,
            true,
        )
        .await
        .unwrap();
        run_query(
            &pool,
            "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
            10,
            None,
            false,
            true,
        )
        .await
        .unwrap();

        // allow_dangerous=false: dangerous statements are rejected and the data is untouched
        for sql in ["UPDATE t SET name = 'x'", "DELETE FROM t", "DROP TABLE t"] {
            let err = run_query(&pool, sql, 10, None, false, false)
                .await
                .unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("allow_dangerous_statements")
                    && message.contains("not executed"),
                "unexpected error for {sql}: {message}"
            );
        }
        let result = run_query(&pool, "SELECT count(*) FROM t", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(2));

        // UPDATE / DELETE with WHERE can run even with allow_dangerous=false
        run_query(
            &pool,
            "UPDATE t SET name = 'x' WHERE id = 1",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();

        // With allow_dangerous=true, dangerous statements can run too
        run_query(&pool, "DELETE FROM t", 10, None, false, true)
            .await
            .unwrap();
        let result = run_query(&pool, "SELECT count(*) FROM t", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(0));
    }

    #[test]
    fn test_build_explain_sql() {
        // Per-engine prefix
        assert_eq!(
            build_explain_sql("postgres", "SELECT * FROM t").unwrap(),
            "EXPLAIN (ANALYZE, BUFFERS)\nSELECT * FROM t"
        );
        assert_eq!(
            build_explain_sql("mysql", "SELECT * FROM t").unwrap(),
            "EXPLAIN FORMAT=JSON\nSELECT * FROM t"
        );
        assert_eq!(
            build_explain_sql("sqlite", "SELECT * FROM t").unwrap(),
            "EXPLAIN QUERY PLAN\nSELECT * FROM t"
        );
        // Alternative spellings of engine names
        assert!(build_explain_sql("PostgreSQL", "SELECT 1").is_ok());
        assert!(build_explain_sql("mariadb", "SELECT 1").is_ok());
        // WITH (CTE) and a leading comment are also targeted
        assert!(build_explain_sql("sqlite", "WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        assert!(build_explain_sql("sqlite", "-- note\nSELECT 1").is_ok());
        // Anything other than SELECT / WITH is rejected (because EXPLAIN ANALYZE would execute the DML)
        assert!(build_explain_sql("postgres", "UPDATE t SET a = 1").is_err());
        assert!(build_explain_sql("postgres", "DELETE FROM t").is_err());
        assert!(build_explain_sql("mysql", "SHOW TABLES").is_err());
        assert!(build_explain_sql("sqlite", "").is_err());
        // Statements that start with SELECT / WITH but may write are rejected
        // (because Postgres's EXPLAIN ANALYZE would actually execute them)
        assert!(build_explain_sql("postgres", "SELECT * INTO new_table FROM t").is_err());
        assert!(
            build_explain_sql("mysql", "SELECT * FROM t INTO OUTFILE '/tmp/x'").is_err()
        );
        assert!(build_explain_sql(
            "postgres",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"
        )
        .is_err());
        // into / delete inside literals are not falsely detected
        assert!(build_explain_sql("postgres", "SELECT 'into' FROM t").is_ok());
        assert!(
            build_explain_sql("postgres", "WITH x AS (SELECT 'delete') SELECT * FROM x").is_ok()
        );
        // Meta commands are also out of scope
        assert!(build_explain_sql("sqlite", "\\dt").is_err());
        // An unknown engine is an error
        assert!(build_explain_sql("oracle", "SELECT 1").is_err());
    }

    #[test]
    fn test_json_64bit_precision() {
        // Within the JS safe integer range, it stays a number
        assert_eq!(json_i64(42), serde_json::json!(42));
        assert_eq!(json_i64(-9007199254740991), serde_json::json!(-9007199254740991i64));
        assert_eq!(json_u64(9007199254740991), serde_json::json!(9007199254740991u64));
        // Out of range, it is kept as a string to preserve precision
        assert_eq!(
            json_i64(i64::MAX),
            serde_json::Value::String("9223372036854775807".into())
        );
        assert_eq!(
            json_i64(i64::MIN),
            serde_json::Value::String("-9223372036854775808".into())
        );
        assert_eq!(
            json_u64(u64::MAX),
            serde_json::Value::String("18446744073709551615".into())
        );
    }

    #[test]
    fn test_scan_sql_cleaned() {
        let scan = |s: &str, e| scan_sql(s, e).cleaned;
        assert_eq!(scan("SELECT 'limit' FROM t", Engine::Sqlite), "select   from t");
        assert_eq!(scan("SELECT a -- limit\nFROM t", Engine::Sqlite), "select a  \nfrom t");
        assert_eq!(scan("SELECT /* limit */ a", Engine::Sqlite), "select   a");
        assert_eq!(scan("SELECT 'it''s' FROM t", Engine::Sqlite), "select   from t");
        // MySQL's # line comment
        assert_eq!(scan("SELECT a # limit\nFROM t", Engine::MySql), "select a  \nfrom t");
        // In Postgres # is an operator, so it is not treated as a comment
        assert_eq!(scan("SELECT a # b", Engine::Postgres), "select a # b");
        // Postgres dollar quoting is removed as a string
        assert_eq!(
            scan("SELECT $$--not a comment$$ AS s", Engine::Postgres),
            "select   as s"
        );
        assert_eq!(
            scan("SELECT $fn$limit$fn$ AS s", Engine::Postgres),
            "select   as s"
        );
    }

    #[test]
    fn test_scan_sql_body_end() {
        fn body(s: &str, e: Engine) -> &str {
            &s[..scan_sql(s, e).body_end]
        }
        assert_eq!(body("SELECT * FROM t -- note", Engine::Sqlite), "SELECT * FROM t");
        assert_eq!(body("SELECT 1; -- note", Engine::Sqlite), "SELECT 1");
        assert_eq!(body("SELECT 1 /* c */  ;  ", Engine::Sqlite), "SELECT 1");
        // Symbols inside string literals remain as code
        assert_eq!(
            body("SELECT 'a;-- b' FROM t;", Engine::Sqlite),
            "SELECT 'a;-- b' FROM t"
        );
        // Case where something follows the comment
        assert_eq!(body("SELECT 1 -- c\n+ 2", Engine::Sqlite), "SELECT 1 -- c\n+ 2");
        // MySQL's # comment is also removed
        assert_eq!(
            body("SELECT * FROM t # inspect", Engine::MySql),
            "SELECT * FROM t"
        );
        // -- inside Postgres dollar quoting is not cut
        assert_eq!(
            body("SELECT $$--not a comment$$ AS s", Engine::Postgres),
            "SELECT $$--not a comment$$ AS s"
        );
    }

    #[test]
    fn test_is_safe_to_rerun() {
        let f = |s: &str| is_safe_to_rerun(s, Engine::Sqlite);
        // Statements that may be re-run when re-fetching for Copy / Export
        assert!(f("SELECT * FROM t LIMIT 10000"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        // Statements that write are not re-run (it would be a double-execution accident)
        assert!(!f("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(!f("UPDATE t SET a = 1 WHERE id = 1 RETURNING id"));
        assert!(!f("DELETE FROM t WHERE id = 1"));
        assert!(!f("WITH x AS (DELETE FROM t RETURNING id) SELECT * FROM x"));
        // EXPLAIN ANALYZE, which actually executes the target statement, and multiple statements are also rejected
        assert!(!f("EXPLAIN ANALYZE SELECT * FROM t"));
        assert!(!f("SELECT 1; SELECT 2"));

        // DynamoDB's `tables` is a read statement of just ListTables, so it may be re-run
        // (CYBERNEURA-DEV-406). It does not fit the SQL-style keyword check, so it is allowed individually
        assert!(is_safe_to_rerun("tables", Engine::DynamoDb));
        assert!(is_safe_to_rerun("tables;", Engine::DynamoDb));
        // Other engines reject it as before
        assert!(!is_safe_to_rerun("tables", Engine::Sqlite));
        // A form with arguments or multiple statements is rejected even for DynamoDB
        assert!(!is_safe_to_rerun("tables; DELETE FROM t", Engine::DynamoDb));
    }

    #[test]
    fn test_should_auto_limit() {
        let f = |s: &str| should_auto_limit(s, Engine::Sqlite);
        assert!(f("SELECT * FROM users"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        // A limit inside a literal is ignored and a LIMIT can be added
        assert!(f("SELECT 'limit' FROM t"));
        // Word boundary: a table named limits does not veto
        assert!(f("SELECT * FROM limits"));
        // LIMIT / FETCH / OFFSET already present
        assert!(!f("SELECT * FROM t LIMIT 10"));
        assert!(!f("SELECT * FROM t FETCH FIRST 10 ROWS ONLY"));
        assert!(!f("SELECT * FROM t OFFSET 5"));
        // A LIMIT inside a subquery is also skipped conservatively
        assert!(!f("SELECT * FROM (SELECT 1 LIMIT 3) s"));
        // WITH mixed with lock clauses or DML
        assert!(!f("SELECT * FROM t FOR UPDATE"));
        assert!(!f("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"));
        // Other than SELECT-type
        assert!(!f("SHOW TABLES"));
        assert!(!f("UPDATE t SET a = 1"));
        // VALUES is out of scope because LIMIT is not allowed in SQLite
        assert!(!f("VALUES (1)"));
        // Postgres: a limit inside dollar quoting does not veto
        assert!(should_auto_limit(
            "SELECT $$limit$$ AS s",
            Engine::Postgres
        ));
        // MySQL: a limit inside a # comment does not veto (it can be added to the body)
        assert!(should_auto_limit(
            "SELECT * FROM t # limit note",
            Engine::MySql
        ));
    }

    #[test]
    fn test_contains_returning() {
        assert!(contains_returning("INSERT INTO t (a) VALUES (1) RETURNING id"));
        assert!(contains_returning("DELETE FROM t returning *"));
        assert!(contains_returning("UPDATE t SET a=1\nRETURNING a"));
        assert!(!contains_returning("SELECT returning_flag FROM t"));
        assert!(!contains_returning("SELECT * FROM returnings"));
        assert!(!contains_returning("UPDATE t SET a = 1"));
    }

    #[test]
    fn test_parse_engine() {
        assert!(parse_engine("mysql").is_ok());
        assert!(parse_engine("MySQL").is_ok());
        assert!(parse_engine("postgres").is_ok());
        assert!(parse_engine("postgresql").is_ok());
        assert!(parse_engine("sqlite").is_ok());
        assert!(parse_engine("oracle").is_err());
    }

    #[test]
    fn test_bytes_to_json() {
        assert_eq!(
            bytes_to_json(b"hello".to_vec()),
            serde_json::Value::String("hello".into())
        );
        let binary = vec![0xff, 0xfe, 0x00];
        let value = bytes_to_json(binary);
        assert!(value.as_str().unwrap().starts_with("base64:"));
    }

    #[tokio::test]
    async fn test_run_query_sqlite() {
        // :memory: is a separate DB per connection, so pin the pool to 1 connection
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .in_memory(true),
            )
            .await
            .unwrap();
        let pool = DbPool::Sqlite(pool);

        let result = run_query(
            &pool,
            "CREATE TABLE t (id INTEGER, name TEXT, score REAL)",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.affected_rows, Some(0));

        let result = run_query(
            &pool,
            "INSERT INTO t VALUES (1, 'alice', 1.5), (2, 'bob', NULL)",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.affected_rows, Some(2));

        let result = run_query(&pool, "SELECT * FROM t ORDER BY id", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.columns, vec!["id", "name", "score"]);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.rows[0][0], serde_json::json!(1));
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
        assert_eq!(result.rows[0][2], serde_json::json!(1.5));
        assert_eq!(result.rows[1][2], serde_json::Value::Null);
        assert!(!result.truncated);

        // Truncation by max_rows
        let result = run_query(&pool, "SELECT * FROM t ORDER BY id", 1, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(result.truncated);

        // Column headers are returned even for a 0-row SELECT (supplemented by describe)
        let result = run_query(&pool, "SELECT * FROM t WHERE id = -1", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 0);
        assert_eq!(result.columns, vec!["id", "name", "score"]);

        // INSERT ... RETURNING returns rows
        let result = run_query(
            &pool,
            "INSERT INTO t VALUES (3, 'dave', 2.0) RETURNING id, name",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.columns, vec!["id", "name"]);
        assert_eq!(result.rows[0][1], serde_json::json!("dave"));

        // psql-style meta commands are converted and executed
        let result = run_query(&pool, "\\dt", 10, None, false, false).await.unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows[0][0], serde_json::json!("t"));

        let result = run_query(&pool, "\\d t", 10, None, false, false).await.unwrap();
        // PRAGMA table_info returns the column name in the name column (index 1)
        let column_names: Vec<&str> = result
            .rows
            .iter()
            .filter_map(|row| row[1].as_str())
            .collect();
        assert_eq!(column_names, vec!["id", "name", "score"]);

        // An unsupported meta command is an error
        assert!(run_query(&pool, "\\du", 10, None, false, false).await.is_err());

        // Auto LIMIT: added to a SELECT without a LIMIT (trailing ; is handled too)
        let result = run_query(&pool, "SELECT * FROM t ORDER BY id;", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.applied_limit, Some(2));

        // Not added when a LIMIT already exists
        let result = run_query(&pool, "SELECT * FROM t LIMIT 1", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.applied_limit, None);

        // Not applied to meta commands
        let result = run_query(&pool, "\\dt", 10, Some(2), false, false).await.unwrap();
        assert_eq!(result.applied_limit, None);

        // Even with a trailing comment, the LIMIT is not swallowed by the comment
        let result = run_query(
            &pool,
            "SELECT * FROM t ORDER BY id -- trailing note",
            10,
            Some(2),
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.applied_limit, Some(2));

        // VALUES is out of scope for the auto LIMIT (VALUES ... LIMIT is a syntax error in SQLite)
        let result = run_query(&pool, "VALUES (1), (2), (3)", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 3);
        assert_eq!(result.applied_limit, None);

        // The EXPLAIN QUERY PLAN built by build_explain_sql can be executed
        // (and is allowed even on a readonly connection)
        let explain_sql = build_explain_sql("sqlite", "SELECT * FROM t ORDER BY id").unwrap();
        let result = run_query(&pool, &explain_sql, 10, Some(2), true, false)
            .await
            .unwrap();
        assert!(result.row_count >= 1);
        assert!(result.columns.contains(&"detail".to_string()));
        // No auto LIMIT is added to EXPLAIN (because the leading keyword is explain)
        assert_eq!(result.applied_limit, None);
    }

    /// Creates a single-connection SQLite pool for tests
    async fn make_test_pool() -> DbPool {
        // :memory: is a separate DB per connection, so pin the pool to 1 connection
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .in_memory(true),
            )
            .await
            .unwrap();
        DbPool::Sqlite(pool)
    }

    #[tokio::test]
    async fn test_cancel_registry_no_running_query() {
        let registry = CancelRegistry::default();
        // false if no query is running
        assert!(!registry.cancel("nothing").await.unwrap());
    }

    #[tokio::test]
    async fn test_cancel_registry_register_and_cancel() {
        let registry = CancelRegistry::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = registry.register("conn-a", CancelTarget::Sqlite, cancelled.clone());
        assert!(registry.is_running("conn-a"));
        assert!(!guard.was_cancelled());

        // A cancellation request sets the flag
        assert!(registry.cancel("conn-a").await.unwrap());
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(guard.was_cancelled());

        // Other connections are not affected
        assert!(!registry.cancel("conn-b").await.unwrap());

        // Dropping the guard removes the registration
        drop(guard);
        assert!(!registry.is_running("conn-a"));
        assert!(!registry.cancel("conn-a").await.unwrap());
    }

    #[tokio::test]
    async fn test_cancel_registry_stale_guard_keeps_newer_entry() {
        let registry = CancelRegistry::default();
        let old_guard = registry.register(
            "conn-a",
            CancelTarget::Sqlite,
            Arc::new(AtomicBool::new(false)),
        );
        // If a new execution was registered on the same connection, dropping the
        // old guard must not remove the new registration
        let new_flag = Arc::new(AtomicBool::new(false));
        let new_guard = registry.register("conn-a", CancelTarget::Sqlite, new_flag.clone());
        drop(old_guard);
        assert!(registry.is_running("conn-a"));

        // Cancellation reaches the new execution
        assert!(registry.cancel("conn-a").await.unwrap());
        assert!(new_flag.load(Ordering::SeqCst));
        drop(new_guard);
        assert!(!registry.is_running("conn-a"));
    }

    #[tokio::test]
    async fn test_cancel_sqlite_query_and_rerun_on_same_connection() {
        let pool = make_test_pool().await;
        let registry = Arc::new(CancelRegistry::default());

        // Run a heavy query (mass generation via WITH RECURSIVE) in a separate task
        let heavy_sql = "WITH RECURSIVE c(x) AS (\
             SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000000\
         ) SELECT count(*) FROM c";
        let task_pool = pool.clone();
        let task_registry = registry.clone();
        let handle = tokio::spawn(async move {
            run_query_cancellable(
                &task_pool,
                &task_registry,
                "test-conn",
                heavy_sql,
                10,
                None,
                ReadonlyGuard::Off,
                false,
            )
            .await
        });

        // Wait until the execution is registered (registration happens right before the query starts)
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !registry.is_running("test-conn") {
            assert!(Instant::now() < deadline, "query was not registered in time");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Cancellation request -> the progress handler aborts it and it returns as Cancelled
        assert!(registry.cancel("test-conn").await.unwrap());
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(AppError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );
        // Also check the string representation passed to the frontend
        assert_eq!(AppError::Cancelled.to_string(), "Query cancelled");

        // The registration is released when the execution ends
        assert!(!registry.is_running("test-conn"));

        // On the same connection (the same connection since max_connections=1),
        // the next query can run normally = the pool's connection is not broken
        let result =
            run_query_cancellable(&pool, &registry, "test-conn", "SELECT 1", 10, None, ReadonlyGuard::Off, false)
                .await
                .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows[0][0], serde_json::json!(1));
    }

    #[tokio::test]
    async fn test_cancel_after_completion_does_not_affect_next_query() {
        let pool = make_test_pool().await;
        let registry = Arc::new(CancelRegistry::default());

        // Cancelling an already completed query (registration released) is a no-op
        let result =
            run_query_cancellable(&pool, &registry, "test-conn", "SELECT 1", 10, None, ReadonlyGuard::Off, false)
                .await
                .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(!registry.cancel("test-conn").await.unwrap());

        // Subsequent queries can also run normally
        let result =
            run_query_cancellable(&pool, &registry, "test-conn", "SELECT 2", 10, None, ReadonlyGuard::Off, false)
                .await
                .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(2));
    }

    #[tokio::test]
    async fn test_run_query_cancellable_normal_error_is_not_cancelled() {
        let pool = make_test_pool().await;
        let registry = CancelRegistry::default();

        // A failure without a cancellation request does not become Cancelled and stays a DB error
        let result = run_query_cancellable(
            &pool,
            &registry,
            "test-conn",
            "SELECT * FROM no_such_table",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await;
        assert!(matches!(result, Err(AppError::Db(_))), "got: {result:?}");
    }

    #[tokio::test]
    async fn test_run_query_sqlite_readonly() {
        // :memory: is a separate DB per connection, so pin the pool to 1 connection
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .in_memory(true),
            )
            .await
            .unwrap();
        let pool = DbPool::Sqlite(pool);

        // Setup (write with readonly=false)
        run_query(&pool, "CREATE TABLE t (id INTEGER, name TEXT)", 10, None, false, false)
            .await
            .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'alice')", 10, None, false, false)
            .await
            .unwrap();

        // Read statements can run even with readonly
        let result = run_query(&pool, "SELECT * FROM t", 10, None, true, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);

        let result = run_query(
            &pool,
            "WITH x AS (SELECT id FROM t) SELECT * FROM x",
            10,
            None,
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);

        // EXPLAIN / PRAGMA are also allowed
        assert!(run_query(&pool, "EXPLAIN SELECT * FROM t", 10, None, true, false)
            .await
            .is_ok());
        assert!(run_query(&pool, "PRAGMA table_info(t)", 10, None, true, false)
            .await
            .is_ok());

        // Meta commands are only read-only catalog queries, so they are allowed
        let result = run_query(&pool, "\\dt", 10, None, true, false).await.unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!("t"));

        // Write statements are rejected (the error message states readonly explicitly)
        for sql in [
            "INSERT INTO t VALUES (2, 'bob')",
            "UPDATE t SET name = 'x'",
            "DELETE FROM t",
            "CREATE TABLE t2 (id INTEGER)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN extra TEXT",
            // DML with RETURNING (which returns rows) is also rejected by the leading keyword
            "INSERT INTO t VALUES (3, 'carol') RETURNING id",
            // DML after a leading comment is also rejected
            "-- comment\nUPDATE t SET name = 'y'",
            // DML with CTE is rejected even if it starts with WITH
            "WITH x AS (SELECT id FROM t) DELETE FROM t WHERE id IN (SELECT id FROM x)",
            "WITH x AS (SELECT 9) INSERT INTO t SELECT 9, 'eve' FROM x",
        ] {
            let err = run_query(&pool, sql, 10, None, true, false).await.unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("read-only") && message.contains("readonly: true"),
                "unexpected error message for {sql}: {message}"
            );
        }

        // Rejected statements were not executed and the data is untouched
        let result = run_query(&pool, "SELECT id, name FROM t", 10, None, true, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
    }

    // With the Writable switch OFF (ReadonlyGuard::Switch) writes are rejected,
    // and with it ON (ReadonlyGuard::Off) writes are allowed.
    #[tokio::test]
    async fn test_writable_switch_guard() {
        let pool = make_test_pool().await;
        let registry = Arc::new(CancelRegistry::default());
        let run = |guard, sql: &'static str| {
            let pool = pool.clone();
            let registry = registry.clone();
            async move {
                run_query_cancellable(&pool, &registry, "c", sql, 10, None, guard, false).await
            }
        };

        // Table creation only passes with the switch ON (also serves as setup)
        run(ReadonlyGuard::Off, "CREATE TABLE t (id INTEGER, name TEXT)")
            .await
            .unwrap();

        // With the switch OFF, reads are allowed and writes are rejected (message derived from the switch)
        run(ReadonlyGuard::Switch, "SELECT * FROM t")
            .await
            .unwrap();
        let err = run(ReadonlyGuard::Switch, "INSERT INTO t VALUES (1, 'a')")
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Writable switch"),
            "unexpected message: {message}"
        );

        // With the switch ON, writes pass
        run(ReadonlyGuard::Off, "INSERT INTO t VALUES (1, 'a')")
            .await
            .unwrap();
        let result = run(ReadonlyGuard::Off, "SELECT count(*) FROM t")
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(1));
    }

    async fn sqlite_mem_pool() -> DbPool {
        // :memory: is a separate DB per connection, so pin to 1 connection
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .in_memory(true),
            )
            .await
            .unwrap();
        DbPool::Sqlite(pool)
    }

    #[tokio::test]
    async fn test_run_statements_transaction_commit() {
        let pool = sqlite_mem_pool().await;
        run_query(&pool, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", 10, None, false, false)
            .await
            .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'a'), (2, 'b')", 10, None, false, false)
            .await
            .unwrap();

        // Apply two UPDATEs in one transaction
        let affected = run_statements(
            &pool,
            &[
                "UPDATE t SET name = 'x' WHERE id = 1".into(),
                "UPDATE t SET name = 'y' WHERE id = 2".into(),
            ],
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(affected, 2);

        let result = run_query(&pool, "SELECT name FROM t ORDER BY id", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!("x"));
        assert_eq!(result.rows[1][0], serde_json::json!("y"));
    }

    #[tokio::test]
    async fn test_run_statements_rollback_on_error() {
        let pool = sqlite_mem_pool().await;
        run_query(&pool, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", 10, None, false, false)
            .await
            .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'a')", 10, None, false, false)
            .await
            .unwrap();

        // The first statement succeeds but the second has a syntax/reference error. Everything is rolled back
        let err = run_statements(
            &pool,
            &[
                "UPDATE t SET name = 'changed' WHERE id = 1".into(),
                "UPDATE no_such_table SET name = 'z' WHERE id = 1".into(),
            ],
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().is_empty());

        // Because of the rollback, the first statement's change does not remain either
        let result = run_query(&pool, "SELECT name FROM t WHERE id = 1", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!("a"));
    }

    #[tokio::test]
    async fn test_run_statements_rejects_non_update() {
        let pool = sqlite_mem_pool().await;
        run_query(&pool, "CREATE TABLE t (id INTEGER PRIMARY KEY)", 10, None, false, false)
            .await
            .unwrap();
        // Anything other than UPDATE is rejected (nothing is applied)
        let err = run_statements(
            &pool,
            &["DELETE FROM t WHERE id = 1".into()],
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Only UPDATE"));
    }

    #[tokio::test]
    async fn test_run_statements_readonly_switch_blocks() {
        let pool = sqlite_mem_pool().await;
        run_query(&pool, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", 10, None, false, false)
            .await
            .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'a')", 10, None, false, false)
            .await
            .unwrap();
        // With the Writable switch OFF (Switch), UPDATE is blocked
        let err = run_statements(
            &pool,
            &["UPDATE t SET name = 'x' WHERE id = 1".into()],
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Writable switch"));
        // Confirm it was not changed
        let result = run_query(&pool, "SELECT name FROM t WHERE id = 1", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!("a"));
    }

    #[tokio::test]
    async fn test_fetch_primary_keys_sqlite() {
        let pool = sqlite_mem_pool().await;
        run_query(
            &pool,
            "CREATE TABLE single (id INTEGER PRIMARY KEY, name TEXT)",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        run_query(
            &pool,
            "CREATE TABLE composite (a INTEGER, b INTEGER, v TEXT, PRIMARY KEY (a, b))",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        run_query(&pool, "CREATE TABLE nokey (x INTEGER, y TEXT)", 10, None, false, false)
            .await
            .unwrap();

        assert_eq!(
            crate::schema_info::fetch_primary_keys(&pool, "single")
                .await
                .unwrap(),
            vec!["id".to_string()]
        );
        assert_eq!(
            crate::schema_info::fetch_primary_keys(&pool, "composite")
                .await
                .unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        // A table without a primary key yields an empty result
        assert!(crate::schema_info::fetch_primary_keys(&pool, "nokey")
            .await
            .unwrap()
            .is_empty());
    }

    /// In addition to the statement-level guard, the agent path (ReadonlyGuard::Agent)
    /// enforces read-only at the DB level as well. For SQLite it is PRAGMA query_only.
    #[tokio::test]
    async fn test_agent_guard_enforces_sqlite_query_only() {
        let pool = make_test_pool().await;
        let registry = CancelRegistry::default();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        run_query(&pool, "CREATE TABLE t (id INTEGER)", 10, None, false, false)
            .await
            .unwrap();

        // Reads on the agent path pass
        run_query_cancellable(
            &pool,
            &registry,
            "c",
            "SELECT 1",
            10,
            None,
            ReadonlyGuard::Agent,
            false,
        )
        .await
        .unwrap();

        // Even a raw write that does not go through the statement-level guard is rejected by the DB itself
        // (confirms that query_only is actually in effect on the connection)
        let err = sqlx::query("INSERT INTO t VALUES (1)")
            .execute(raw)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.to_lowercase().contains("readonly")
                || err.to_lowercase().contains("read-only"),
            "unexpected error: {err}"
        );

        // Execution on the normal path clears query_only, so writes afterward pass
        // (recovery when the clearing did not run due to an abort)
        run_query_cancellable(
            &pool,
            &registry,
            "c",
            "INSERT INTO t VALUES (2)",
            10,
            None,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO t VALUES (3)")
            .execute(raw)
            .await
            .unwrap();
    }

    /// Cell edits (run_statements) also clear any leftover query_only before writing.
    #[tokio::test]
    async fn test_run_statements_clears_agent_query_only() {
        let pool = make_test_pool().await;
        let registry = CancelRegistry::default();
        run_query(
            &pool,
            "CREATE TABLE t (id INTEGER, name TEXT)",
            10,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'a')", 10, None, false, false)
            .await
            .unwrap();

        // Leave query_only = 1 behind through an agent-path execution
        run_query_cancellable(
            &pool,
            &registry,
            "c",
            "SELECT 1",
            10,
            None,
            ReadonlyGuard::Agent,
            false,
        )
        .await
        .unwrap();

        let affected = run_statements(
            &pool,
            &["UPDATE t SET name = 'b' WHERE id = 1".to_string()],
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(affected, 1);
    }

    /// Verification of DB-level read-only on real servers (Postgres / MySQL).
    /// These two engines cannot be started embedded, so this only runs in
    /// environments where a server can be prepared (skipped if there is no URL):
    ///   QUERYFOLIO_TEST_PG_URL=postgres://user:pass@localhost/db \
    ///   QUERYFOLIO_TEST_MYSQL_URL=mysql://user:pass@localhost/db \
    ///     cargo test test_agent_guard_enforces_readonly_transaction
    /// It verifies two points: "writes are rejected by the DB inside a transaction opened with
    /// readonly_begin_sql" and "reads pass, and after ROLLBACK writes are possible on
    /// the same connection".
    /// The probes use **writes to normal (non-TEMP) objects**:
    /// for Postgres `SELECT nextval(...)` (exactly the case that originated this issue),
    /// for MySQL an INSERT into a regular table. Things that cannot be used as probes (all measured):
    ///   - Temporary objects: for both engines they are outside the scope of a read-only
    ///     transaction, so writes go through
    ///   - MySQL DDL (`CREATE TABLE`): an implicit commit leaves the transaction,
    ///     so it is not rejected (what stops DDL is the statement-level whitelist)
    /// Because it creates, writes to and drops probe objects, the connecting user needs
    /// **CREATE / INSERT / DROP** privileges (in MySQL these are independent privileges.
    /// If they are missing the test fails with a permission error — the read-only verification
    /// never passes through and turns green, but cleanup may fail and leave a regular table behind).
    /// On the Postgres side, with the CREATE privilege to create a sequence, the owner
    /// gets through to nextval / DROP.
    /// Names are made unique per run: if a fixed name were `DROP ... IF EXISTS`'d,
    /// it could delete the user's data when an object of the same name exists on the target,
    /// and concurrently running tests would also trample each other. Probe objects remain only
    /// if a test panics midway, but that does less harm than deleting them.
    #[tokio::test]
    async fn test_agent_guard_enforces_readonly_transaction() {
        // A probe name unique per run (pid + nanoseconds elapsed since startup)
        let probe = format!(
            "queryfolio_ro_probe_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        if let Ok(url) = std::env::var("QUERYFOLIO_TEST_PG_URL") {
            let raw = PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            let mut conn = raw.acquire().await.unwrap();
            // Use a regular sequence as the probe. **Temporary objects are
            // outside the scope of a read-only transaction** and nextval would pass,
            // so TEMP cannot be used (measured)
            sqlx::query(&format!("CREATE SEQUENCE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();

            let begin = readonly_begin_sql(Engine::Postgres).unwrap();
            let mut tx = conn.begin_with(begin).await.unwrap();
            // Reads pass
            sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
            // A SELECT with side effects is rejected by the DB (the kind of statement that the
            // statement-level guard lets through because it only looks at the leading keyword)
            let err = sqlx::query(&format!("SELECT nextval('{probe}')"))
                .execute(&mut *tx)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("read-only"), "postgres: {err}");
            tx.rollback().await.unwrap();
            // After ROLLBACK, writes are possible on the same connection
            sqlx::query(&format!("SELECT nextval('{probe}')"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(&format!("DROP SEQUENCE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            // The pool has one connection, so return it before moving on to the whole-path check
            drop(conn);

            // Reads on the agent path pass (whole-path check)
            let pool = DbPool::Postgres(raw);
            let registry = CancelRegistry::default();
            let result = run_query_cancellable(
                &pool,
                &registry,
                "pg",
                "SELECT 1 AS n",
                10,
                None,
                ReadonlyGuard::Agent,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result.row_count, 1);
        }

        if let Ok(url) = std::env::var("QUERYFOLIO_TEST_MYSQL_URL") {
            let raw = MySqlPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            let mut conn = raw.acquire().await.unwrap();
            // Use a regular table as the probe. Writes to a temporary table are outside the scope
            // of a read-only transaction (same as Postgres), and DDL leaves the transaction
            // via an implicit commit — neither works as a probe (measured)
            sqlx::query(&format!("CREATE TABLE {probe} (a INT)"))
                .execute(&mut *conn)
                .await
                .unwrap();

            let begin = readonly_begin_sql(Engine::MySql).unwrap();
            let mut tx = conn.begin_with(begin).await.unwrap();
            sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
            let err = sqlx::query(&format!("INSERT INTO {probe} VALUES (1)"))
                .execute(&mut *tx)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.to_uppercase().contains("READ ONLY"),
                "mysql: {err}"
            );
            tx.rollback().await.unwrap();
            // After ROLLBACK, writes are possible on the same connection
            sqlx::query(&format!("INSERT INTO {probe} VALUES (1)"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(&format!("DROP TABLE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            // The pool has one connection, so return it before moving on to the whole-path check
            drop(conn);

            let pool = DbPool::MySql(raw);
            let registry = CancelRegistry::default();
            let result = run_query_cancellable(
                &pool,
                &registry,
                "mysql",
                "SELECT 1 AS n",
                10,
                None,
                ReadonlyGuard::Agent,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result.row_count, 1);
        }
    }

    /// A Postgres user-defined enum type is shown as a label string, and its array as
    /// an array of labels (nested for multidimensional).
    /// sqlx's String decoder only accepts TEXT / VARCHAR and the like, so
    /// unless enums are handled specially they become `<undecodable: ...>`.
    /// It is placed in a schema outside search_path to reproduce the case where the type name arrives
    /// schema-qualified as `schema.type` (the form that actually occurred).
    /// It needs a server, so it only runs when QUERYFOLIO_TEST_PG_URL is set.
    #[tokio::test]
    async fn test_pg_enum_decodes_as_label() {
        let Ok(url) = std::env::var("QUERYFOLIO_TEST_PG_URL") else {
            return;
        };
        let schema = format!(
            "queryfolio_enum_probe_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE TYPE {schema}.mood AS ENUM ('happy', 'sad')"))
            .execute(&pool)
            .await
            .unwrap();
        let row = sqlx::query(&format!(
            "SELECT 'sad'::{schema}.mood AS m, NULL::{schema}.mood AS n, \
                    ARRAY['happy', NULL, 'sad']::{schema}.mood[] AS a, \
                    '{{}}'::{schema}.mood[] AS e, \
                    '{{{{happy,sad}},{{sad,happy}}}}'::{schema}.mood[] AS nested, \
                    NULL::{schema}.mood[] AS na"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        let value = pg_value_to_json(&row, 0);
        let null = pg_value_to_json(&row, 1);
        let array = pg_value_to_json(&row, 2);
        let empty = pg_value_to_json(&row, 3);
        let nested = pg_value_to_json(&row, 4);
        let null_array = pg_value_to_json(&row, 5);
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(value, serde_json::json!("sad"));
        assert_eq!(null, serde_json::Value::Null);
        assert_eq!(array, serde_json::json!(["happy", null, "sad"]));
        assert_eq!(empty, serde_json::json!([]));
        assert_eq!(nested, serde_json::json!([["happy", "sad"], ["sad", "happy"]]));
        assert_eq!(null_array, serde_json::Value::Null);
    }

    /// Builds array_send-format bytes (the length of each dimension and the elements. None is NULL)
    fn pg_array_bytes(dims: &[i32], elements: &[Option<&str>]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(dims.len() as i32).to_be_bytes());
        buf.extend_from_slice(&0i32.to_be_bytes());
        buf.extend_from_slice(&12345u32.to_be_bytes());
        for &d in dims {
            buf.extend_from_slice(&d.to_be_bytes());
            buf.extend_from_slice(&1i32.to_be_bytes());
        }
        for e in elements {
            match e {
                Some(s) => {
                    buf.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    buf.extend_from_slice(s.as_bytes());
                }
                None => buf.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        buf
    }

    fn label(b: &[u8]) -> serde_json::Value {
        bytes_to_json(b.to_vec())
    }

    #[test]
    fn test_pg_binary_array_to_json() {
        let one_dim = pg_array_bytes(&[3], &[Some("a"), None, Some("b")]);
        assert_eq!(
            pg_binary_array_to_json(&one_dim, label),
            Some(serde_json::json!(["a", null, "b"]))
        );

        let two_dim = pg_array_bytes(&[2, 2], &[Some("a"), Some("b"), Some("c"), Some("d")]);
        assert_eq!(
            pg_binary_array_to_json(&two_dim, label),
            Some(serde_json::json!([["a", "b"], ["c", "d"]]))
        );

        let empty = pg_array_bytes(&[], &[]);
        assert_eq!(pg_binary_array_to_json(&empty, label), Some(serde_json::json!([])));
    }

    /// Broken input returns None without panicking (does not attempt allocation with a huge length)
    #[test]
    fn test_pg_binary_array_to_json_rejects_malformed() {
        let full = pg_array_bytes(&[2], &[Some("abc"), Some("de")]);
        for cut in 0..full.len() {
            assert_eq!(pg_binary_array_to_json(&full[..cut], label), None, "cut at {cut}");
        }
        // Only the element count is huge, with no contents
        let huge = pg_array_bytes(&[i32::MAX], &[]);
        assert_eq!(pg_binary_array_to_json(&huge, label), None);
        // Negative number of dimensions / dimension length
        let negative_dim = pg_array_bytes(&[-1], &[]);
        assert_eq!(pg_binary_array_to_json(&negative_dim, label), None);
        let mut negative_ndim = pg_array_bytes(&[], &[]);
        negative_ndim[..4].copy_from_slice(&(-1i32).to_be_bytes());
        assert_eq!(pg_binary_array_to_json(&negative_ndim, label), None);
        // Number of dimensions exceeding MAXDIM (6)
        let too_deep = pg_array_bytes(&[1; 7], &[Some("a")]);
        assert_eq!(pg_binary_array_to_json(&too_deep, label), None);
        // A zero-length dimension (if an inner one is 0, the outer one loops 2^31 times without reading input)
        for dims in [[i32::MAX, 0], [0, i32::MAX], [2, 0]] {
            let zero_dim = pg_array_bytes(&dims, &[]);
            assert_eq!(pg_binary_array_to_json(&zero_dim, label), None, "{dims:?}");
        }
        // The product of dimensions overflows / cannot be covered by the remaining bytes
        let overflow = pg_array_bytes(&[i32::MAX, i32::MAX, i32::MAX], &[Some("a")]);
        assert_eq!(pg_binary_array_to_json(&overflow, label), None);
        // NULL is only -1
        let mut bad_null = pg_array_bytes(&[1], &[None]);
        let at = bad_null.len() - 4;
        bad_null[at..].copy_from_slice(&(-2i32).to_be_bytes());
        assert_eq!(pg_binary_array_to_json(&bad_null, label), None);
        // Bytes left after the declared elements
        let mut trailing = pg_array_bytes(&[1], &[Some("a")]);
        trailing.push(0);
        assert_eq!(pg_binary_array_to_json(&trailing, label), None);
        let mut trailing_empty = pg_array_bytes(&[], &[]);
        trailing_empty.push(0);
        assert_eq!(pg_binary_array_to_json(&trailing_empty, label), None);
        // The has-null flag is only 0 / 1
        let mut bad_flag = pg_array_bytes(&[1], &[Some("a")]);
        bad_flag[4..8].copy_from_slice(&2i32.to_be_bytes());
        assert_eq!(pg_binary_array_to_json(&bad_flag, label), None);
    }

    /// Statement-level guards remain effective on the agent path as well
    /// (the message is worded for the agent).
    #[tokio::test]
    async fn test_agent_guard_blocks_write_statements() {
        let pool = make_test_pool().await;
        let registry = CancelRegistry::default();
        run_query(&pool, "CREATE TABLE t (id INTEGER)", 10, None, false, false)
            .await
            .unwrap();

        let err = run_query_cancellable(
            &pool,
            &registry,
            "c",
            "INSERT INTO t VALUES (1)",
            10,
            None,
            ReadonlyGuard::Agent,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        // The agent whitelist (agent_rejection_reason) rejects it first
        assert!(err.contains("assistant"), "unexpected error: {err}");
    }
}
