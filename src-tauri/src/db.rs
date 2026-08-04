use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Connection as _, Executor, Row, TypeInfo};

use crate::config::ServerConfig;
use crate::error::AppError;
use crate::config::expand_tilde;
use crate::tunnel::SshTunnel;

/// 1 回のクエリで取得する行数の上限デフォルト。
pub const DEFAULT_MAX_ROWS: usize = 1000;

const POOL_MAX_CONNECTIONS: u32 = 3;
const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// SQLite の progress handler を呼ぶ VM 命令数の間隔。
/// 小さいほどキャンセルの反応が速いが、実行オーバーヘッドが増える。
const SQLITE_PROGRESS_HANDLER_OPS: i32 = 1000;

#[derive(Clone)]
pub enum DbPool {
    MySql(sqlx::MySqlPool),
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
    /// Redis は sqlx を使わない。Client は接続情報のみ持ち、
    /// 実行のたびに multiplexed connection を張る (engines::redis)。
    Redis(redis::Client),
    /// Elasticsearch は sqlx を使わず reqwest で REST API を叩く
    /// (engines::elasticsearch)。EsClient は base_url と認証情報のみ持つ。
    Elasticsearch(crate::engines::elasticsearch::EsClient),
    /// DuckDB は sqlx を使わず duckdb crate で結線する (engines::duckdb)。
    /// SQL エンジンだがコネクションは 1 本を Mutex で維持する。
    DuckDb(crate::engines::duckdb::DuckDbHandle),
    /// DynamoDB は sqlx を使わず AWS SDK で PartiQL (ExecuteStatement) を
    /// 実行する (engines::dynamodb)。DynamoClient は SDK クライアントのみ持つ。
    DynamoDb(crate::engines::dynamodb::DynamoClient),
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub affected_rows: Option<u64>,
    pub truncated: bool,
    pub elapsed_ms: u64,
    /// 自動付与した LIMIT の値 (付与していなければ None)
    pub applied_limit: Option<u64>,
    /// `\c` で切り替えた後のアクティブスキーマ (それ以外は None)。
    /// フロント側の表示・スキーマブラウザ・補完を追従させるために返す。
    #[serde(default)]
    pub switched_schema: Option<String>,
}

/// 接続名ごとのプールと SSH トンネルを保持するマネージャ。
/// 単一ユーザーのデスクトップアプリなので、プール取得全体を 1 つの
/// tokio Mutex で直列化して二重生成を防ぐ。
#[derive(Default)]
pub struct DbManager {
    inner: tokio::sync::Mutex<DbManagerInner>,
}

#[derive(Default)]
struct DbManagerInner {
    pools: HashMap<String, DbPool>,
    tunnels: HashMap<String, SshTunnel>,
    /// 接続名ごとのアクティブスキーマ (database) のオーバーライド。
    /// 設定の schema と異なる database に切り替えている時のみ存在する。
    schema_overrides: HashMap<String, String>,
}

impl DbManager {
    pub async fn get_pool(&self, server: &ServerConfig) -> Result<DbPool, AppError> {
        let mut inner = self.inner.lock().await;
        if let Some(pool) = inner.pools.get(&server.name) {
            return Ok(pool.clone());
        }

        // アクティブスキーマが切り替えられていれば接続先 database を差し替える
        let mut server = server.clone();
        if let Some(schema) = inner.schema_overrides.get(&server.name) {
            server.schema = Some(schema.clone());
        }
        let server = &server;

        let engine = parse_engine(&server.engine)?;

        // SSH トンネルが必要なら先に確立し、接続先をローカルポートに差し替える
        let (host, port) = match (&server.ssh_tunnel, engine) {
            // ファイルベースのエンジンにトンネルの意味は無い
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
            // DynamoDB は HTTPS の AWS エンドポイントへ直接繋ぐ (SigV4 署名に
            // リージョンのエンドポイントが前提)。トンネルは非対応
            (Some(_), Engine::DynamoDb) => {
                return Err(AppError::Config(
                    "ssh_tunnel cannot be used with dynamodb".into(),
                ));
            }
            (Some(tunnel_config), _) => {
                // スキーマ切替等でプールだけ破棄された場合、既存トンネルは
                // 接続先ホストが同じなのでそのまま再利用する
                let local_port = match inner.tunnels.get(&server.name) {
                    Some(tunnel) => tunnel.local_port,
                    None => {
                        let target_host =
                            server.host.clone().unwrap_or_else(|| "localhost".into());
                        let target_port = server.port.unwrap_or(default_port(engine));
                        let tunnel_config = tunnel_config.clone();
                        // ssh2 は blocking なので spawn_blocking で実行する
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

    /// プールとトンネルを全て破棄する。設定リロード時に呼ぶ。
    pub async fn reset(&self) {
        let mut inner = self.inner.lock().await;
        inner.pools.clear();
        inner.tunnels.clear();
        inner.schema_overrides.clear();
    }

    /// 指定接続のプールと SSH トンネルを破棄する。
    /// 「この接続はもう不要」と判断された契機 (エディタタブを全て閉じた時など) に
    /// 呼ぶ。トンネルとプールは必ず一緒に破棄する — プールがトンネルの死んだ
    /// ローカルポート宛のコネクションを掴んだまま残ると、次にこの接続を使った時に
    /// クエリが失敗するため。
    /// アクティブスキーマの選択 (schema_overrides) は UI の状態なので保持し、
    /// 次に接続を張り直した時に同じスキーマで繋がるようにする。
    pub async fn disconnect(&self, connection: &str) {
        let mut inner = self.inner.lock().await;
        inner.pools.remove(connection);
        inner.tunnels.remove(connection);
    }

    /// 接続のアクティブスキーマ (database) を切り替える。
    /// プールを破棄し、次のクエリから新しい database で接続し直す
    /// (SQL の USE ではなくプール再構築で切り替えることで、プール内の
    /// コネクション間でセッション状態が食い違うのを防ぐ)。
    /// SSH トンネルは接続先ホストが変わらないため維持する。
    pub async fn set_schema_override(&self, connection: &str, schema: String) {
        self.replace_schema_override(connection, Some(schema)).await;
    }

    /// アクティブスキーマのオーバーライドを設定または解除する。
    /// None を渡すと設定ファイルの schema に戻る。
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

    /// 切り替えに失敗した時 (存在しない database を指定された等) に、
    /// アクティブスキーマを previous へ戻す。
    ///
    /// 現在値が expected (自分が設定した値) と一致する場合だけ戻す
    /// compare-and-swap。切り替え中にユーザーがスキーマ選択などで別の値へ
    /// 変更していた場合、それを巻き戻さないようにするため。
    /// 戻した場合は true を返す。
    pub async fn rollback_schema_override(
        &self,
        connection: &str,
        expected: &str,
        previous: Option<String>,
    ) -> bool {
        // 判定と書き戻しの間に割り込まれないよう、同じロックスコープで行う
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

    /// 接続のアクティブスキーマのオーバーライドを返す (無ければ None)。
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
}

/// プールから実行専用に確保した 1 本のコネクション。
/// キャンセル対象 (backend PID 等) はセッション単位の情報のため、
/// クエリはプール直ではなくこのコネクション上で実行する。
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
            // sqlx を使わないエンジンは run_query_cancellable が acquire より
            // 前に各エンジンモジュールへ委譲するため、ここには来ない
            DbPool::Redis(_)
            | DbPool::Elasticsearch(_)
            | DbPool::DuckDb(_)
            | DbPool::DynamoDb(_) => {
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

/// キャンセル発行の手段 (エンジン別)。
/// Postgres / MySQL はサーバー側で実行中の文を、プールの別コネクション
/// から停止させる (接続自体は切断しないため、実行側のコネクションは
/// 健全なままプールへ戻る)。SQLite は progress handler が cancelled
/// フラグを監視して文を SQLITE_INTERRUPT で中断する。
pub(crate) enum CancelTarget {
    /// SELECT pg_cancel_backend($pid) を別接続から発行する
    Postgres { pid: i32, pool: sqlx::PgPool },
    /// KILL QUERY <connection_id> を別接続から発行する
    MySql { connection_id: u64, pool: sqlx::MySqlPool },
    /// cancelled フラグを立てるだけ (progress handler が中断する)
    Sqlite,
    /// クライアント側で実行の future を打ち切る (サーバー側で文を止める
    /// 手段が無いエンジン用。Redis 等)。notify で実行側の select を起こす
    ClientSide { notify: Arc<tokio::sync::Notify> },
    /// duckdb の InterruptHandle で実行中の文を中断させる。
    /// spawn_blocking の実行は future の drop では止まらないため、
    /// エンジン側の interrupt が必須 (実行中の文が無ければ no-op)
    DuckDb { interrupt: Arc<duckdb::InterruptHandle> },
}

/// 実行中クエリ 1 件分の登録情報
struct RunningQuery {
    /// 登録の世代識別子 (古いガードが新しい登録を消さないための照合用)
    id: u64,
    target: CancelTarget,
    cancelled: Arc<AtomicBool>,
}

/// 実行中クエリのレジストリ (接続名単位)。
/// 同一接続の並列実行はフロントエンド側で抑止している
/// (app.svelte.ts の isConnectionRunning ガード) ため、接続ごとに
/// 最後に登録された実行のみをキャンセル対象として保持すれば十分。
#[derive(Default)]
pub struct CancelRegistry {
    running: std::sync::Mutex<HashMap<String, RunningQuery>>,
    next_id: AtomicU64,
}

impl CancelRegistry {
    /// 実行開始を登録する。返り値のガードの drop で登録が解除される。
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

    /// 接続で実行中のクエリにキャンセルを要求する。
    /// 実行中のクエリが無ければ何もせず false を返す。
    /// クエリが直前に完了していた場合でも、pg_cancel_backend / KILL QUERY
    /// はアイドルなセッションへの no-op になるため安全 (接続を壊す
    /// KILL CONNECTION は使わない)。
    pub async fn cancel(&self, connection: &str) -> Result<bool, AppError> {
        // Mutex ガードを await をまたいで保持しないよう、
        // 発行に必要な情報だけ取り出してからロックを解放する
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
                // KILL はプレースホルダを使えないが、connection_id は
                // サーバーが返した数値なので直接埋め込んで問題ない
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

    /// (テスト用) 接続の実行が登録されているかを返す
    #[cfg(test)]
    fn is_running(&self, connection: &str) -> bool {
        self.running.lock().unwrap().contains_key(connection)
    }
}

/// 実行終了時にレジストリから登録を外すガード。
/// 登録後に同じ接続で新しい実行が登録し直された場合 (id 不一致) は
/// 新しい登録を消さないよう何もしない。
pub(crate) struct RunningQueryGuard<'a> {
    registry: &'a CancelRegistry,
    connection: String,
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl RunningQueryGuard<'_> {
    /// この実行にキャンセル要求があったかを返す
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

/// 設定の engine 文字列を Engine に解決する。
pub fn parse_engine(engine: &str) -> Result<Engine, AppError> {
    match engine.to_ascii_lowercase().as_str() {
        "mysql" | "mariadb" => Ok(Engine::MySql),
        "postgres" | "postgresql" => Ok(Engine::Postgres),
        "sqlite" | "sqlite3" => Ok(Engine::Sqlite),
        "redis" | "valkey" => Ok(Engine::Redis),
        "elasticsearch" | "es" | "opensearch" => Ok(Engine::Elasticsearch),
        "duckdb" => Ok(Engine::DuckDb),
        "dynamodb" => Ok(Engine::DynamoDb),
        other => Err(AppError::Config(format!(
            "Unsupported engine: {other} \
             (supported: mysql / postgres / sqlite / duckdb / redis / \
             elasticsearch / dynamodb)"
        ))),
    }
}

fn default_port(engine: Engine) -> u16 {
    match engine {
        Engine::MySql => 3306,
        Engine::Postgres => 5432,
        // DynamoDb はエンドポイント解決を engines::dynamodb::connect が行う
        Engine::Sqlite | Engine::DuckDb | Engine::DynamoDb => 0,
        Engine::Redis => crate::engines::redis::DEFAULT_PORT,
        Engine::Elasticsearch => crate::engines::elasticsearch::DEFAULT_PORT,
    }
}

async fn connect(
    server: &ServerConfig,
    engine: Engine,
    host: &str,
    port: u16,
) -> Result<DbPool, AppError> {
    match engine {
        Engine::MySql => {
            let mut options = MySqlConnectOptions::new().host(host).port(port);
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
            let mut options = PgConnectOptions::new().host(host).port(port);
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
            // sqlite は schema (無ければ host) を DB ファイルパスとして扱う
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
        // duckdb は sqlite と同じくファイルベースなので host / port は使わない
        Engine::DuckDb => Ok(DbPool::DuckDb(
            crate::engines::duckdb::connect(server).await?,
        )),
        // dynamodb はリージョン (schema) とエンドポイント上書き (host / port) を
        // モジュール側で解決するため、ここで計算した host / port は使わない
        Engine::DynamoDb => Ok(DbPool::DynamoDb(
            crate::engines::dynamodb::connect(server).await?,
        )),
    }
}

/// SQL を実行して結果を返す (テスト用の非キャンセル版ラッパー)。
/// アプリ本体はキャンセル対応の run_query_cancellable を使う。
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
    // テストは config readonly 相当の bool を渡す。
    let guard = if readonly {
        ReadonlyGuard::Config
    } else {
        ReadonlyGuard::Off
    };
    run_query_on(&mut conn, sql, max_rows, auto_limit, guard, allow_dangerous).await
}

/// SQL を実行して結果を返す (キャンセル対応版)。
/// 実行専用のコネクションをプールから確保し、実行前にエンジン別の
/// キャンセル対象 (Postgres は backend PID、MySQL は CONNECTION_ID、
/// SQLite は中断フラグ付き progress handler) を registry に登録してから
/// 実行する。キャンセル要求後にクエリがエラーで終わった場合は
/// AppError::Cancelled を返す。キャンセルはサーバー側の文の停止のみで
/// 接続は切断しないため、コネクションは健全なままプールへ戻り、
/// 同じ接続で次のクエリを正常に実行できる。
// readonly / allow_dangerous は独立した実行ガードなので個別引数のまま渡す
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
    // 非 SQL エンジンは各エンジンモジュールへ委譲する (auto_limit は SQL 固有
    // なので渡さない)
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
    // DuckDB は SQL エンジンだが sqlx 非対応のためモジュールへ委譲する
    // (auto_limit を含む SQL 系の共通ガードはモジュール側で適用する)
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
    // DynamoDB (PartiQL) もモジュールへ委譲する。PartiQL に LIMIT 句は無いため
    // auto_limit は渡さず、ExecuteStatement の limit パラメータ + max_rows で
    // 行数を抑える (モジュール側)
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

    let mut conn = DbConnection::acquire(pool).await?;
    let cancelled = Arc::new(AtomicBool::new(false));

    // 実行前にキャンセル対象を控える
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
            // CONNECTION_ID() は BIGINT UNSIGNED だが、実装差異に備えて
            // i64 でのデコードにもフォールバックする
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
            // フラグが立ったら progress handler が false を返し、
            // 実行中の文が SQLITE_INTERRUPT で中断される
            let flag = cancelled.clone();
            c.lock_handle().await?.set_progress_handler(
                SQLITE_PROGRESS_HANDLER_OPS,
                move || !flag.load(Ordering::SeqCst),
            );
            CancelTarget::Sqlite
        }
        // acquire はプールと同じエンジンのコネクションしか返さない
        _ => unreachable!("connection engine mismatch"),
    };

    let guard = registry.register(connection_name, target, cancelled);
    let result =
        run_query_on(&mut conn, sql, max_rows, auto_limit, readonly, allow_dangerous).await;
    let was_cancelled = guard.was_cancelled();
    drop(guard);

    // SQLite: progress handler をプールへ返す前に必ず外す。
    // 外し損ねるとフラグの立ったハンドラが残り、このコネクションの
    // 次のクエリが即座に中断されてしまう。
    // (lock_handle が失敗するのはワーカースレッドが死んでいる場合のみで、
    //  その場合コネクション自体が使えないためプール側で破棄される)
    if let DbConnection::Sqlite(c) = &mut conn {
        if let Ok(mut handle) = c.lock_handle().await {
            handle.remove_progress_handler();
        }
    }

    // キャンセル要求後のエラーは「キャンセルされた」として返す
    // (キャンセルが間に合わずクエリが完了していた場合は成功結果を返す)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    result
}

/// 読み取り専用ガードでブロックする際のエラー (由来別メッセージ)。
/// run_query_on と run_statements で共有する。
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
        // Off はブロックしないためこのエラーは作られない
        ReadonlyGuard::Off => unreachable!(),
    };
    AppError::Readonly(message.into())
}

/// 危険な文 (WHERE 無しの UPDATE/DELETE 等) でブロックする際のエラー。
pub(crate) fn dangerous_block_error(reason: &str) -> AppError {
    AppError::Dangerous(format!(
        "{reason} Set \"allow_dangerous_statements: true\" for this connection \
         in config to run it. Statement was not executed."
    ))
}

/// 確保済みコネクション上で 1 文を実行し、影響行数を返す (結果セットは読まない)。
async fn execute_statement(conn: &mut DbConnection, sql: &str) -> Result<u64, AppError> {
    Ok(match conn {
        DbConnection::MySql(c) => (&mut **c).execute(sql).await?.rows_affected(),
        DbConnection::Postgres(c) => (&mut **c).execute(sql).await?.rows_affected(),
        DbConnection::Sqlite(c) => (&mut **c).execute(sql).await?.rows_affected(),
    })
}

/// 結果グリッドのセル編集を UPDATE 群として 1 トランザクションで適用する。
/// 全文が成功したら COMMIT、途中で失敗したら ROLLBACK して最初のエラーを返す
/// (all-or-nothing)。この経路は一般の複数文実行の裏口にならないよう、
/// 各文が UPDATE であることを必須とし、readonly / 危険文ガードも run_query と
/// 同様に適用する。合計の影響行数を返す。
pub async fn run_statements(
    pool: &DbPool,
    statements: &[String],
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<u64, AppError> {
    if statements.is_empty() {
        return Err(AppError::Config("There are no changes to apply".into()));
    }
    // DuckDb はセル編集の適用経路 (sqlx のトランザクション実行) を持たない
    // ため、capabilities.supports_editable_cells = false と合わせて拒否する
    if matches!(
        pool,
        DbPool::Redis(_)
            | DbPool::Elasticsearch(_)
            | DbPool::DuckDb(_)
            | DbPool::DynamoDb(_)
    ) {
        return Err(AppError::Config(
            "Cell editing is not supported for this engine".into(),
        ));
    }
    let mut conn = DbConnection::acquire(pool).await?;
    let engine = conn.engine();

    // 何も書き込む前に全文を検証する (一部だけ適用される事態を防ぐ)。
    for sql in statements {
        let sql = sql.trim();
        // セル編集の適用は UPDATE のみ。他の文はこの経路では拒否する。
        if leading_keyword(sql) != "update" {
            return Err(AppError::Config(
                "Only UPDATE statements can be applied from the results grid.".into(),
            ));
        }
        // 複文はガードをすり抜けるため、ガードが有効なら拒否する
        // (run_query_on と同じ理由。`UPDATE ... WHERE ...; DROP TABLE t;` は
        // 先頭が update で where もあるため、両方のガードを通過してしまう)
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

    // エージェント経路が立てた PRAGMA query_only が残っているコネクションを
    // 引いた場合に備えて解除する (run_query_on と同じ理由。設定は
    // 「使う側が毎回明示する」方式で確定させる)
    if let DbConnection::Sqlite(c) = &mut conn {
        set_sqlite_query_only(c, false).await?;
    }

    // 1 トランザクションで全文を適用する。DDL は含めない (UPDATE のみ) ため、
    // 暗黙コミットは起きない。COMMIT/ROLLBACK まで必ず到達させてから
    // コネクションをプールへ返す。
    execute_statement(&mut conn, "BEGIN").await?;
    let mut total: u64 = 0;
    for sql in statements {
        match execute_statement(&mut conn, sql.trim()).await {
            Ok(affected) => total += affected,
            Err(e) => {
                // ロールバック自体の失敗は握り潰し、元のエラーを返す
                let _ = execute_statement(&mut conn, "ROLLBACK").await;
                return Err(e);
            }
        }
    }
    // COMMIT が失敗 (deferred 制約違反 / SQLite busy 等) しても、コネクションを
    // トランザクション状態のままプールへ返さないよう ROLLBACK を試みる。
    if let Err(e) = execute_statement(&mut conn, "COMMIT").await {
        let _ = execute_statement(&mut conn, "ROLLBACK").await;
        return Err(e);
    }
    Ok(total)
}

/// 読み取り専用ガードの由来。ブロック時のメッセージを由来に応じて
/// 出し分けるために使う (config の readonly か、ツールバーの Writable スイッチか)。
/// Agent だけは由来であると同時に強度も表す (DB レベルの強制を伴う)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadonlyGuard {
    /// 書き込み許可 (readonly ガードなし)
    Off,
    /// config の readonly: true による読み取り専用 (スイッチでは解除できない)
    Config,
    /// ツールバーの Writable スイッチ OFF による読み取り専用
    Switch,
    /// AI チャットのエージェントによる実行。文レベルの判定に加えて
    /// DB レベルでも読み取り専用を強制する (run_query_readonly)。
    /// 文レベルの判定だけでは `SELECT nextval(...)` のような副作用のある
    /// 関数呼び出しを防げないため。
    Agent,
}

/// DB レベルの読み取り専用セッションを開始する SQL (エンジン別)。
/// トランザクションを読み取り専用で開始し、実行後は必ず ROLLBACK する。
/// SQLite にはトランザクション属性が無いため PRAGMA query_only を使う
/// (set_sqlite_query_only)。
fn readonly_begin_sql(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some("BEGIN READ ONLY"),
        Engine::MySql => Some("START TRANSACTION READ ONLY"),
        _ => None,
    }
}

/// SQLite の DB レベル読み取り専用を設定する。
/// 解除 (0 に戻す) ではなく「実行のたびに 0/1 を明示する」方式にしている:
/// クエリの future が途中で drop される (チャットの中断) と後始末は走らず、
/// query_only = 1 が残ったコネクションがプールへ返り得るため、
/// 後片付けに頼らず毎回の実行で状態を確定させる。
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

/// run_query の本体。確保済みのコネクション上で実行する。
async fn run_query_on(
    conn: &mut DbConnection,
    sql: &str,
    max_rows: usize,
    auto_limit: Option<u64>,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let engine = conn.engine();
    // psql 風メタコマンド (\l, \dt など) はカタログ照会 SQL に変換して実行する。
    // \c (スキーマ切替) は SQL にならないため、ここへ来る前に lib.rs の
    // run_query が処理している
    let translated = match crate::meta_commands::translate(engine, sql)? {
        Some(crate::meta_commands::MetaCommand::Sql(sql)) => Some(sql),
        Some(crate::meta_commands::MetaCommand::Connect(_)) => {
            return Err(AppError::Config(
                "\\c must be handled before the query runs".into(),
            ));
        }
        None => None,
    };
    let sql = translated.as_deref().unwrap_or(sql);

    if leading_keyword(sql).is_empty() {
        return Err(AppError::Config("The SQL statement is empty".into()));
    }

    // readonly 接続では読み取り系の文のみ許可する。
    // メタコマンドは読み取り系のカタログ照会にしか変換されないため、
    // 変換後の SQL はこの判定を常に通る。
    // エージェント経路は通常の readonly ガードより狭いホワイトリストを課す
    // (複文 / CALL / PRAGMA / EXPLAIN ANALYZE を落とす)。lib.rs でもプール取得
    // 前に同じ判定をしているが、ここでも課すことで ReadonlyGuard::Agent 単体で
    // エージェントの実行ポリシーが成立する (呼び出し側の実装に依存しない)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) = agent_rejection_reason(sql, engine) {
            return Err(AppError::Readonly(reason));
        }
    }
    // 複文 (`;` 区切り) は 1 文目だけを見るガードをすり抜ける。
    // `SELECT 1; DELETE FROM t;` は先頭が select なので is_readonly_allowed を、
    // `UPDATE t SET x=1 WHERE id=1; DROP TABLE t;` は where があるので
    // dangerous_reason を通過してしまう。一方、実行経路のドライバは複文を
    // そのまま実行する (SQLite は fetch 経路でも、Postgres / MySQL は引数なしの
    // execute 経路 = 単純問い合わせプロトコルで通る)。
    // そのためガードが有効な接続では複文自体を拒否する。両方のガードを外して
    // いる接続では従来どおり複文を許すので、スクリプトの貼り付け実行は壊れない。
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && contains_multiple_statements(sql, engine)
    {
        return Err(multi_statement_block_error());
    }

    if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, engine) {
        return Err(readonly_block_error(readonly));
    }

    // 危険な文 (WHERE 無しの UPDATE / DELETE、DROP / TRUNCATE) は、
    // allow_dangerous_statements を有効にした接続でのみ実行を許す。
    // 誤操作による全行破壊・テーブル消失を防ぐ事故防止ガード。
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, engine) {
            return Err(dangerous_block_error(reason));
        }
    }

    // LIMIT 未指定の SELECT にはデフォルトの LIMIT を付与する
    // (メタコマンド変換後の SQL には適用しない)
    let mut applied_limit = None;
    let limited_sql;
    let sql = match auto_limit {
        Some(limit)
            if limit > 0
                && translated.is_none()
                && should_auto_limit(sql, engine) =>
        {
            // 末尾のコメント・セミコロンを除いた本体の直後に付与する
            // (コメントの後ろに付けると LIMIT がコメントに飲み込まれる)
            let body = &sql[..scan_sql(sql, engine).body_end];
            limited_sql = format!("{body} LIMIT {limit}");
            applied_limit = Some(limit);
            limited_sql.as_str()
        }
        _ => sql,
    };
    let started = Instant::now();

    // SQLite の DB レベル読み取り専用は PRAGMA query_only (セッション設定)
    // なので、エージェント経路でなくても毎回明示して状態を確定させる
    // (中断で解除処理が走らないまま プールへ返ったコネクションの後始末)
    if let DbConnection::Sqlite(c) = &mut *conn {
        set_sqlite_query_only(c, readonly == ReadonlyGuard::Agent).await?;
    }

    // エージェント経路は読み取り専用トランザクションの中で実行する。
    // 文レベルのガード (is_readonly_allowed / agent_rejection_reason) は
    // `SELECT nextval(...)` のような副作用のある関数呼び出しを見抜けないため、
    // 最終的な拒否は DB 自身にさせる。
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

/// 実行に使うコネクション参照。プールのコネクションと、読み取り専用
/// トランザクション (エージェント経路) で実行部を共有するために挟む。
enum DbExec<'a> {
    MySql(&'a mut sqlx::MySqlConnection),
    Postgres(&'a mut sqlx::PgConnection),
    Sqlite(&'a mut sqlx::SqliteConnection),
}

/// DB レベルの読み取り専用でクエリを実行する (エージェント経路)。
/// Postgres / MySQL は読み取り専用トランザクションで包み、結果に関わらず
/// ROLLBACK する (読み取りしかしないので COMMIT する必要が無い)。
/// SQLite は PRAGMA query_only を呼び出し側で設定済み。
async fn run_query_readonly(
    conn: &mut DbConnection,
    sql: &str,
    max_rows: usize,
    applied_limit: Option<u64>,
    started: Instant,
) -> Result<QueryResult, AppError> {
    // トランザクションで包めないエンジン (SQLite) は PRAGMA 済みなのでそのまま
    let begin = match readonly_begin_sql(conn.engine()) {
        Some(begin) => begin,
        None => {
            let exec = match conn {
                DbConnection::Sqlite(c) => DbExec::Sqlite(c),
                // readonly_begin_sql が None を返すのは SQLite だけ
                _ => unreachable!("engine without a read-only transaction"),
            };
            return run_query_with(exec, sql, max_rows, applied_limit, started).await;
        }
    };

    // sqlx の Transaction は drop 時にも ROLLBACK を積む (中断でこの関数の
    // future が drop されても、コネクションがトランザクションを開いたまま
    // プールへ返ることはない)
    match conn {
        DbConnection::Postgres(c) => {
            let mut tx = c.begin_with(begin).await?;
            let result =
                run_query_with(DbExec::Postgres(&mut tx), sql, max_rows, applied_limit, started)
                    .await;
            // ROLLBACK 自体の失敗は握り潰す (結果を返すのが優先。失敗した
            // コネクションは ping に失敗してプールから破棄される)
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

/// 確保済みの実行先 (コネクション or トランザクション) で 1 文を実行する。
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

    // 0 行の結果でも列ヘッダを表示できるよう、describe で列情報を補完する。
    // SHOW 等 prepare できない文では失敗することがあるため、エラーは無視する。
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

/// 接続先サーバー上の database (スキーマ) 一覧を返す。
/// sqlite は database の概念が単一ファイルなので、設定のパスをそのまま返す。
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
        // duckdb も sqlite と同じくファイルパスをそのまま返す
        DbPool::Sqlite(_) | DbPool::DuckDb(_) => {
            let path = server
                .schema
                .as_deref()
                .or(server.host.as_deref())
                .unwrap_or("main");
            Ok(vec![path.to_string()])
        }
        // Redis / Elasticsearch / DynamoDB に database 一覧の概念は無い
        // (capabilities.supports_schemas = false でフロントは呼ばないが、
        // 直接呼ばれても壊れないよう空を返す)
        DbPool::Redis(_) | DbPool::Elasticsearch(_) | DbPool::DynamoDb(_) => Ok(vec![]),
    }
}

/// SQL の先頭キーワード (コメントを除く) を小文字で返す。
pub(crate) fn leading_keyword(sql: &str) -> String {
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
        // MySQL の実行コメント (`/*! ... */` / `/*!50110 ... */`) はサーバーが
        // SQL として実行するので、コメントとして読み飛ばさずマーカーだけ外す
        // (`/*! DROP TABLE t */` の先頭キーワードは drop)。
        // engine を受け取らない関数なので方言で分けていないが、`/*!` を使わない
        // 他方言で誤ってこの形を書いても、先頭キーワードが解釈できずガードが
        // 厳しくなる (拒否側に倒れる) だけで穴にはならない。
        if let Some(after) = rest.strip_prefix("/*!") {
            rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, r)| r).unwrap_or("");
            continue;
        }
        break;
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// SQL の走査結果。cleaned はキーワード判定用 (文字列リテラルとコメントを
/// 空白化して小文字化したもの)、body_end は自動 LIMIT の挿入位置
/// (末尾のコメント・セミコロン・空白を除いた本体の終了位置)。
pub(crate) struct SqlScan {
    cleaned: String,
    pub(crate) body_end: usize,
}

/// エンジンごとのコメント・クォート規則で SQL を 1 パス走査する。
/// - 文字列リテラル: ' " ` (二重化エスケープ対応)。Postgres はドル引用
///   ($tag$ ... $tag$) にも対応 (# は Postgres では XOR 演算子なので
///   コメント扱いしない)
/// - コメント: -- と /* */。MySQL は # 行コメントも対象
///
/// **バックスラッシュによるエスケープ (`'a\'b'`) は意図的に解釈しない。**
/// MySQL は既定でこれを解釈するが、NO_BACKSLASH_ESCAPES を有効にした環境では
/// 解釈しない。どちらか一方に決め打ちすると、
/// - エスケープを解釈する側に倒す → NO_BACKSLASH_ESCAPES の環境で
///   `SELECT 'a\'; DROP TABLE t; --'` のセミコロン以降をリテラルとして飲み込み、
///   複文判定・readonly / 危険文ガードをすり抜けさせてしまう
/// - 解釈しない側に倒す (現状) → 既定設定の MySQL で `'a\'; b'` のような
///   リテラルを含む正当なクエリが「複文」と判定されて拒否される
/// となる。この結果はガードの拒否側 (安全側) なので、後者を選んでいる
/// (回避したい場合は `''` で引用符をエスケープするか、Writable ON +
/// allow_dangerous_statements: true にしてガードを外す)。
/// chars[i] から `--` 行コメントが始まるか。
///
/// MySQL だけは `--` の直後が空白 (制御文字・行末を含む) の時しか行コメントに
/// ならない。`SELECT 1--1` は「1 - (-1)」であってコメントではない。
/// https://dev.mysql.com/doc/refman/8.4/en/ansi-diff-comments.html
///
/// ここを他方言と同じ扱いにすると、`SELECT 1--1; DROP TABLE t;` のセミコロン以降を
/// 丸ごとコメントとして落としてしまい、複文判定 (contains_multiple_statements) と
/// readonly / 危険文ガードが、MySQL が実際に実行する 2 文目を見落とす。
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

pub(crate) fn scan_sql(sql: &str, engine: Engine) -> SqlScan {
    let hash_comments = matches!(engine, Engine::MySql);
    // MySQL の実行コメント (`/*! ... */`) はサーバーが SQL として解釈・実行する
    let executable_comments = matches!(engine, Engine::MySql);
    // DuckDB はドル引用に対応しており方言は Postgres 相当
    let dollar_quotes = matches!(engine, Engine::Postgres | Engine::DuckDb);
    let chars: Vec<char> = sql.chars().collect();
    let mut cleaned = String::with_capacity(sql.len());
    let mut body_end = 0;
    let mut byte_pos = 0;
    let mut i = 0;

    // i 番目の文字を消費して byte 位置を進める
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
        } else if dollar_quotes && c == '$' {
            // $tag$ ... $tag$ のドル引用を検出する
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
                // 閉じタグを探す
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
            // MySQL の実行コメント (`/*! ... */` / `/*!50110 ... */`) は
            // サーバーがコメントではなく SQL として解釈・実行する。
            // ここで他のコメントと同じく空白化すると、
            // `UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */` の DROP が
            // cleaned から消え、複文判定も危険文ガードも見落とす。
            // 開始マーカー (`/*!` とバージョン番号) だけを読み飛ばし、
            // 中身は通常の SQL として走査させる (閉じの `*/` は記号として
            // cleaned に残るが、単語境界の判定には影響しない)。
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

/// デフォルト LIMIT を安全に付与できる文かを判定する。
/// 対象は SELECT 系のみ。LIMIT / FETCH / OFFSET / FOR UPDATE / INTO /
/// WITH ... INSERT 等の語を含む場合は、構文エラーや意味の変化を避けるため
/// 付与しない (保守的側に倒す。スキップしてもクライアント側の max_rows
/// 打ち切りが安全網になる)。
pub(crate) fn should_auto_limit(sql: &str, engine: Engine) -> bool {
    // VALUES (SQLite では LIMIT 不可) や TABLE は対象にせず、
    // SELECT / WITH のみに限定する。DuckDB は FROM-first 構文
    // (`FROM t`) も SELECT と同じ問い合わせ形なので対象にする
    // (SUMMARIZE / PIVOT は末尾 LIMIT の可否が形に依存するため付けない)
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

/// エンジン別の EXPLAIN プレフィックスを付けた SQL を組み立てる。
/// 対象は SELECT / WITH のみ (should_auto_limit と同じ leading_keyword 判定)。
/// Postgres の EXPLAIN ANALYZE は対象文を実際に実行するため、DML に付けると
/// 書き込みが走ってしまう。安全側に倒して SELECT 系以外は一律エラーにする。
///
/// MySQL は EXPLAIN FORMAT=JSON を選ぶ。EXPLAIN ANALYZE (8.0.18+) は
/// 対象文を実際に実行するうえ MariaDB では未対応のため、実行を伴わずに
/// コスト・行数見積もりが得られる FORMAT=JSON の方が安全で互換性も広い。
pub fn build_explain_sql(engine: &str, sql: &str) -> Result<String, AppError> {
    let engine = parse_engine(engine)?;
    // DynamoDB (PartiQL) に EXPLAIN は無い
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
    // EXPLAIN のプレフィックスが効くのは 1 文目だけで、`SELECT 1; DROP TABLE t;`
    // の 2 文目以降はそのまま実行される。EXPLAIN に複文を渡す用途も無いため拒否する。
    if contains_multiple_statements(sql, engine) {
        return Err(AppError::Explain(
            "Explain is available only for a single statement".into(),
        ));
    }
    // EXPLAIN ANALYZE は対象文を実際に実行するため、先頭が SELECT / WITH
    // でも書き込みを伴い得る文 (SELECT INTO / CTE 付き DML) は対象外にする
    // (is_readonly_allowed と同じ保守的な単語判定を流用する)
    if !is_readonly_allowed(sql, engine) {
        return Err(AppError::Explain(
            "Explain is not available for statements that may write data \
             (SELECT INTO / WITH ... INSERT / UPDATE / DELETE)"
                .into(),
        ));
    }
    let prefix = match engine {
        // ANALYZE で実測時間、BUFFERS でバッファアクセス統計も取得する
        Engine::Postgres => "EXPLAIN (ANALYZE, BUFFERS)",
        Engine::MySql => "EXPLAIN FORMAT=JSON",
        Engine::Sqlite => "EXPLAIN QUERY PLAN",
        // DuckDB の EXPLAIN ANALYZE は対象文を実際に実行するため使わない
        Engine::DuckDb => "EXPLAIN",
        // Redis / Elasticsearch / DynamoDb は冒頭の早期 return で弾いている
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb => unreachable!(),
    };
    Ok(format!("{prefix}\n{sql}"))
}

/// RETURNING 句を含むかを単語境界で判定する。
/// INSERT / UPDATE / DELETE ... RETURNING (Postgres / SQLite) の結果行を
/// 取りこぼさないための判定。文字列リテラル内の単語にも反応する可能性が
/// あるが、その場合も fetch 経路で正しく実行される (affected 表示が
/// 行数表示になるだけ) ため許容する。
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

/// readonly 接続で実行を許可する文かを判定する。
/// 先頭キーワードが読み取り系 (is_fetch_statement) であることに加えて:
/// - WITH: CTE 本体が DML (WITH ... DELETE 等) の場合を拒否するため、
///   文字列リテラル・コメントを除去した cleaned に insert / update /
///   delete / merge の単語が含まれたら拒否する
/// - SELECT: SELECT INTO (Postgres ではテーブル作成、MySQL では
///   INTO OUTFILE 等) を拒否するため、into の単語が含まれたら拒否する
/// - EXPLAIN: EXPLAIN ANALYZE (Postgres / MySQL 8.0.19+) は対象文を
///   実際に実行するため、analyze と DML / into の単語が両方含まれたら
///   拒否する (SELECT INTO のテーブル作成や INTO OUTFILE のファイル
///   書き込みも実行されてしまうため)。ANALYZE 無しの EXPLAIN は実行を
///   伴わないので DML でも許可する
/// - PRAGMA: SQLite の代入形 PRAGMA (`PRAGMA user_version = 1`、
///   `PRAGMA journal_mode = WAL` 等) は DB を変更するため、cleaned に `=`
///   を含む PRAGMA は拒否する。読み取り形 (`PRAGMA table_info(t)`、
///   `PRAGMA user_version` 等) は許可する
/// リテラル内の単語は scan_sql が除去し、カラム名等への部分一致は
/// 単語境界の分割で誤検知しない。
/// 弱点: SELECT に副作用のある関数 (nextval 等) や CALL のプロシージャ内の
/// 書き込み、括弧形の設定 PRAGMA (`PRAGMA journal_mode(WAL)` 等) までは
/// 防げない。あくまで事故防止のガードである。
/// `;` 区切りの複文かどうかを判定する。
///
/// 文字列リテラル・コメントは scan_sql が除去済みの cleaned を見るため、
/// `SELECT 'a;b'` のようなリテラル内のセミコロンには反応しない。
/// 末尾のセミコロン (`SELECT 1;`) は 1 文として扱う。
pub(crate) fn contains_multiple_statements(sql: &str, engine: Engine) -> bool {
    let cleaned = scan_sql(sql, engine).cleaned;
    cleaned.trim_end().trim_end_matches(';').contains(';')
}

/// ガードが有効な接続で複文が渡された時のエラー。
/// 解除の手順まで書く (どちらのガードが効いているかは呼び出し側で分かるが、
/// 複文はその両方をすり抜けるため、判定としては 1 つにまとめている)。
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
        // 代入形 PRAGMA (`= value`) は DB を変更するので拒否する
        "pragma" => !cleaned.contains('='),
        _ => true,
    }
}

/// 誤操作で全行破壊・テーブル消失を招く危険な文かを判定し、危険なら
/// 理由 (フロントに表示する英語メッセージ) を返す。
/// allow_dangerous_statements が無効な接続でこれらの文を拒否する事故防止ガード。
///
/// 判定は is_readonly_allowed と同じく、文字列リテラル・コメントを scan_sql で
/// 除去した cleaned に対する単語境界判定で行う (リテラル内の where 等には反応
/// しない)。
/// - UPDATE / DELETE: where の単語が無ければ「全行対象」とみなし危険とする
/// - TRUNCATE: 常に危険 (全行削除)
/// - DROP: 常に危険 (オブジェクトの永久削除)
///
/// 先頭キーワードだけでなく、実際に書き込みが走る次のラップ形も対象にする:
/// - WITH ... DELETE / UPDATE (Postgres の CTE 付き DML)。先頭は with でも本体で
///   全行 DELETE/UPDATE が走る
/// - EXPLAIN ANALYZE / EXPLAIN (ANALYZE) ...: 対象文を実際に実行するため、
///   中の DELETE/UPDATE/TRUNCATE/DROP も対象。ANALYZE 無しの EXPLAIN は実行を
///   伴わないので対象外
///
/// 弱点: WITH の場合、無関係な CTE / 外側の SELECT にある where を「WHERE あり」と
/// 誤認して WHERE 無し DML を見逃すことがある (where を一切含まない典型形は捕捉
/// する)。サブクエリ内だけの where も同様。安全側=許可側に倒れるため完全ではなく、
/// 代表的な事故パターンを止めるガードである。
pub(crate) fn dangerous_reason(sql: &str, engine: Engine) -> Option<&'static str> {
    let cleaned = scan_sql(sql, engine).cleaned;
    let has_word = |target: &str| {
        cleaned
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == target)
    };
    let kw = leading_keyword(sql);

    // EXPLAIN ANALYZE / EXPLAIN (ANALYZE) は対象文を実際に実行するため、
    // ラップされた DML も危険判定の対象にする。ANALYZE 無しの EXPLAIN は
    // 実行を伴わないので対象外。
    let explain_executes = kw == "explain" && has_word("analyze");
    // 実行時に DML が走り得るラップ形 (CTE 付き DML / EXPLAIN ANALYZE)。
    let wraps_dml = kw == "with" || explain_executes;

    let is_delete = kw == "delete" || (wraps_dml && has_word("delete"));
    let is_update = kw == "update" || (wraps_dml && has_word("update"));

    if is_delete && !has_word("where") {
        return Some("DELETE without a WHERE clause would remove every row.");
    }
    if is_update && !has_word("where") {
        return Some("UPDATE without a WHERE clause would modify every row.");
    }
    // TRUNCATE / DROP は常に破壊的。WITH には書けないので、ラップ形としては
    // EXPLAIN ANALYZE 経由のみ考慮する。
    if kw == "truncate" || (explain_executes && has_word("truncate")) {
        return Some("TRUNCATE would remove every row from the table.");
    }
    if kw == "drop" || (explain_executes && has_word("drop")) {
        return Some("DROP would permanently destroy a database object.");
    }
    None
}

/// フロントエンドの実行前確認ダイアログ用ラッパー。危険な文なら理由を返す。
/// 実行はしない。allow_dangerous_statements が有効な接続で、実行前に
/// ユーザーへ確認を出すかどうかの判断に使う。
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

/// 行を返す文かどうかを先頭キーワードで判定する。
/// AI エージェント (チャットの run_sql ツール) に許可する文の先頭キーワード。
/// ユーザー操作時の readonly ガード (is_fetch_statement) より意図的に狭い:
/// - `call`: ストアドプロシージャは中で DML を実行できる (readonly ガードは
///   中身を見られないため素通りする)
/// - `pragma`: 括弧形など、代入検出をすり抜けて DB 設定を変えうる形がある
///
/// エージェントは「読むだけ」を構造的に保証したいので、少しでも書き込みが
/// 走りうる入口は落とす (人間の操作と違い、拒否されても本人が直せない)。
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

/// その SQL を「もう一度実行しても副作用が無い」と判断してよいかを返す。
///
/// Copy / Export は結果テーブルの打ち切りを避けるために同じ SQL を実行し直すが、
/// 書き込みを伴う文を二度実行すると事故になる。判定は AI エージェント経路と
/// 同じ厳しさ (狭いホワイトリスト + 複文禁止 + EXPLAIN ANALYZE 禁止 +
/// readonly ガード) を使う。
pub fn is_safe_to_rerun(sql: &str, engine: Engine) -> bool {
    agent_rejection_reason(sql, engine).is_none()
}

/// AI エージェントが実行しようとした SQL を拒否すべきなら理由を返す。
/// 通常の readonly ガードに加えて、上記の狭いホワイトリストと
/// 複文 (`;` 区切り) の禁止を課す。
pub(crate) fn agent_rejection_reason(sql: &str, engine: Engine) -> Option<String> {
    let keyword = leading_keyword(sql);
    if !AGENT_ALLOWED_KEYWORDS.contains(&keyword.as_str()) {
        return Some(format!(
            "The assistant may only run read-only statements ({}); rejected.",
            AGENT_ALLOWED_KEYWORDS.join(" / ").to_uppercase()
        ));
    }
    // 複文はドライバ次第で通ることがあり、1 文目だけを見るガードを
    // すり抜けうるため、エージェント経路では一律に拒否する
    if contains_multiple_statements(sql, engine) {
        return Some("The assistant may only run one statement at a time; rejected.".to_string());
    }
    let cleaned = scan_sql(sql, engine).cleaned;
    // EXPLAIN ANALYZE は対象文を実際に実行する。is_readonly_allowed は中身の
    // DML / INTO しか見ないため、`EXPLAIN (ANALYZE) CREATE TABLE x AS SELECT ...`
    // のような DDL がすり抜ける。エージェントに実行を伴う EXPLAIN は不要なので
    // (計画を見るだけなら ANALYZE 無しで足りる) まとめて拒否する。
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

/// 指定した型でのデコードを試み、成功したら JSON にして返すマクロ。
/// NULL は JSON null になる。型不一致は次の候補へフォールスルーする。
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

/// どの型でもデコードできなかった場合の最終フォールバック。
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

/// JavaScript の Number は 2^53-1 (MAX_SAFE_INTEGER) を超える整数を
/// 表現できず、Tauri の invoke 境界で丸められてしまう。
/// 安全範囲を超える 64bit 整数は文字列で返して精度を保つ。
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
        // YEAR は sqlx 内部で UNSIGNED フラグ付きのため u64 側でデコードする
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED"
        | "INT UNSIGNED" | "BIGINT UNSIGNED" | "YEAR" => {
            try_decode!(row, i, u64, json_u64);
        }
        "FLOAT" | "DOUBLE" => {
            try_decode!(row, i, f64, json_number_f64);
        }
        "DECIMAL" => {
            // 精度を保つため文字列で返す
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

fn pg_value_to_json(row: &PgRow, i: usize) -> serde_json::Value {
    let type_name = row.column(i).type_info().name().to_string();
    match type_name.as_str() {
        "BOOL" => {
            try_decode!(row, i, bool, |v: bool| serde_json::Value::Bool(v));
        }
        // Postgres の数値型は型互換が厳密なため、カラム型と同じ幅でデコードする
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
    // sqlite は動的型付けのため、宣言型と実値が一致しないことがある
    try_decode!(row, i, i64, json_i64);
    try_decode!(row, i, f64, json_number_f64);
    decode_fallback!(row, i)
}

#[cfg(test)]
mod tests {
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
        // 読み取り系は通す
        assert!(f("SELECT * FROM t").is_none());
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x").is_none());
        assert!(f("EXPLAIN SELECT 1").is_none());
        assert!(f("SHOW TABLES").is_none());
        // 末尾のセミコロン 1 つは複文ではない
        assert!(f("SELECT 1;").is_none());
        assert!(f("SELECT 1;  ").is_none());
        // CALL は readonly ガードを素通りするが (ストアドが中で DML を実行
        // できる)、エージェント経路では拒否する
        assert!(is_readonly_allowed("CALL do_something()", Engine::MySql));
        assert!(f("CALL do_something()").is_some());
        // PRAGMA も readonly ガードは読み取り形を通すが、エージェントには不要
        assert!(is_readonly_allowed("PRAGMA user_version", Engine::Sqlite));
        assert!(f("PRAGMA user_version").is_some());
        // 書き込み系は当然拒否
        assert!(f("UPDATE t SET a = 1").is_some());
        assert!(f("DROP TABLE t").is_some());
        // 複文は 1 文目が読み取りでも拒否する
        assert!(f("SELECT 1; DELETE FROM t").is_some());
        // EXPLAIN ANALYZE は対象文を実行するため拒否する。特に
        // EXPLAIN (ANALYZE) CREATE TABLE ... AS SELECT は DML 語も INTO も
        // 含まないため is_readonly_allowed を素通りする
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
        // ANALYZE 無しの EXPLAIN は実行を伴わないので許可する
        assert!(agent_rejection_reason("EXPLAIN SELECT 1", Engine::Postgres).is_none());
        // リテラル内のセミコロンは複文ではない
        assert!(f("SELECT 'a; b' FROM t").is_none());
    }

    #[test]
    fn test_contains_multiple_statements() {
        let f = |s: &str| contains_multiple_statements(s, Engine::Sqlite);
        // 1 文 (末尾のセミコロン有無は問わない)
        assert!(!f("SELECT 1"));
        assert!(!f("SELECT 1;"));
        assert!(!f("SELECT 1;  "));
        assert!(!f("SELECT 1;\n"));
        // 複文
        assert!(f("SELECT 1; DELETE FROM t"));
        assert!(f("SELECT 1; DELETE FROM t;"));
        assert!(f("UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;"));
        // リテラル・コメント内のセミコロンには反応しない
        assert!(!f("SELECT 'a; b' FROM t"));
        assert!(!f("SELECT 1 -- ; not a statement"));
        assert!(!f("/* ; */ SELECT 1"));
        // ドル引用 (Postgres) の中身も cleaned から除去される
        assert!(!contains_multiple_statements(
            "SELECT $$a; b$$",
            Engine::Postgres
        ));
    }

    /// MySQL の `--` は直後が空白の時だけ行コメント。
    /// ここを他方言と同じにすると `SELECT 1--1; DROP TABLE t;` の 2 文目が
    /// コメント扱いで消え、複文判定もガードもすり抜ける。
    #[test]
    fn test_mysql_dash_comment_requires_whitespace() {
        assert!(contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::MySql
        ));
        // `--;` も直後が空白でないためコメントではない (安全側 = 複文として検出)
        assert!(contains_multiple_statements(
            "SELECT 1 --; DROP TABLE t;",
            Engine::MySql
        ));
        // 空白付き / 行末の `--` は従来どおりコメント
        assert!(!contains_multiple_statements(
            "SELECT 1 -- ; DROP TABLE t;",
            Engine::MySql
        ));
        assert!(!contains_multiple_statements("SELECT 1 --", Engine::MySql));
        assert!(is_readonly_allowed(
            "SELECT * FROM t -- delete\n",
            Engine::MySql
        ));
        // MySQL の # 行コメントは従来どおり
        assert!(!contains_multiple_statements(
            "SELECT 1 # ; DROP TABLE t",
            Engine::MySql
        ));
        // バックスラッシュのエスケープは解釈しない (scan_sql のコメント参照)。
        // 既定設定の MySQL では 1 文だが、NO_BACKSLASH_ESCAPES の環境では
        // 2 文目が実行されるため、拒否側 = 安全側に倒す
        assert!(contains_multiple_statements(
            r"SELECT 'a\'; DROP TABLE t; --'",
            Engine::MySql
        ));
        // 二重化によるエスケープは従来どおり 1 つのリテラルとして扱う
        assert!(!contains_multiple_statements(
            "SELECT 'a''; still one literal'",
            Engine::MySql
        ));

        // 他方言は `--` の直後が何であってもコメント
        assert!(!contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::Postgres
        ));
        assert!(!contains_multiple_statements(
            "SELECT 1--1; DROP TABLE t;",
            Engine::Sqlite
        ));
    }

    /// MySQL の実行コメント (`/*! ... */`) はサーバーが SQL として実行するので、
    /// コメントとして落とすとガードから隠れてしまう。
    #[test]
    fn test_mysql_executable_comments_are_code() {
        // 複文の 2 文目を実行コメントに隠せない
        assert!(contains_multiple_statements(
            "UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */",
            Engine::MySql
        ));
        // 先頭キーワードも中身から取る (バージョン指定付きも同じ)
        assert_eq!(leading_keyword("/*! DROP TABLE t */"), "drop");
        assert_eq!(leading_keyword("/*!50110 DROP TABLE t */"), "drop");
        assert!(dangerous_reason("/*! DROP TABLE t */", Engine::MySql).is_some());
        assert!(dangerous_reason("/*!50110 TRUNCATE TABLE t */", Engine::MySql).is_some());
        assert!(!is_readonly_allowed("/*! DELETE FROM t */", Engine::MySql));
        assert!(!is_readonly_allowed(
            "WITH x AS (SELECT 1) /*! DELETE FROM t */",
            Engine::MySql
        ));
        // 通常のブロックコメントは従来どおりコメントとして落ちる
        assert_eq!(leading_keyword("/* c */ SELECT 1"), "select");
        assert!(!contains_multiple_statements(
            "SELECT 1 /* ; DROP TABLE t */",
            Engine::MySql
        ));
        assert!(dangerous_reason("SELECT 1 /* DROP TABLE t */", Engine::MySql).is_none());
        // scan_sql 側は MySQL のみ対象なので、他方言では従来どおりコメント
        assert!(!contains_multiple_statements(
            "UPDATE t SET x=1 WHERE id=1; /*! DROP TABLE t */",
            Engine::Postgres
        ));
        // 実行コメント内の LIMIT も見えるので auto LIMIT は付けない
        assert!(!should_auto_limit(
            "SELECT * FROM t /*! LIMIT 5 */",
            Engine::MySql
        ));
    }

    /// 複文でガードをすり抜けられないこと。
    ///
    /// is_readonly_allowed / dangerous_reason は先頭キーワードしか見ないため、
    /// これらの文は単体では「許可」と判定される。ガードが有効な接続では
    /// run_query_on / run_statements が複文の時点で拒否することで防いでいる。
    #[test]
    fn test_multi_statement_bypasses_keyword_guards() {
        // readonly ガードは 1 文目が SELECT なので通してしまう
        assert!(is_readonly_allowed(
            "SELECT 1; DELETE FROM t;",
            Engine::Sqlite
        ));
        assert!(contains_multiple_statements(
            "SELECT 1; DELETE FROM t;",
            Engine::Sqlite
        ));

        // 危険文ガードは 1 文目に WHERE があるので通してしまう
        assert!(dangerous_reason(
            "UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;",
            Engine::Sqlite
        )
        .is_none());
        assert!(contains_multiple_statements(
            "UPDATE t SET x = 1 WHERE id = 1; DROP TABLE t;",
            Engine::Sqlite
        ));

        // EXPLAIN は 1 文目にしか効かないため、組み立て時点で拒否する
        assert!(build_explain_sql("sqlite", "SELECT 1; DROP TABLE t;").is_err());
        assert!(build_explain_sql("sqlite", "SELECT 1;").is_ok());
    }

    #[test]
    fn test_is_readonly_allowed() {
        let f = |s: &str| is_readonly_allowed(s, Engine::Sqlite);
        // 読み取り系は許可
        assert!(f("SELECT * FROM t"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(f("EXPLAIN SELECT 1"));
        assert!(f("SHOW TABLES"));
        assert!(f("PRAGMA table_info(t)"));
        // 読み取り形 PRAGMA は許可
        assert!(f("PRAGMA user_version"));
        assert!(f("PRAGMA journal_mode"));
        // 代入形 PRAGMA (DB を変更する) は拒否
        assert!(!f("PRAGMA user_version = 1"));
        assert!(!f("PRAGMA journal_mode = WAL"));
        assert!(!f("PRAGMA foreign_keys=ON"));
        // 書き込み系は拒否
        assert!(!f("UPDATE t SET a = 1"));
        assert!(!f("INSERT INTO t VALUES (1)"));
        assert!(!f("DROP TABLE t"));
        // CTE 付き DML は先頭が with でも拒否
        assert!(!f("WITH old AS (SELECT id FROM t) DELETE FROM t WHERE id IN (SELECT id FROM old)"));
        assert!(!f("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"));
        assert!(!f("WITH x AS (SELECT 1) UPDATE t SET a = 1"));
        assert!(!f("with x as (select 1)\nmerge into t using x on true"));
        // SELECT INTO (Postgres のテーブル作成 / MySQL の INTO OUTFILE) は拒否
        assert!(!is_readonly_allowed("SELECT * INTO new_table FROM t", Engine::Postgres));
        assert!(!is_readonly_allowed(
            "SELECT * FROM t INTO OUTFILE '/tmp/x'",
            Engine::MySql
        ));
        // リテラル内の単語は scan_sql が除去するので誤検知しない
        assert!(f("WITH x AS (SELECT 'delete') SELECT * FROM x"));
        assert!(f("SELECT 'into' FROM t"));
        // 単語境界: 部分一致では拒否しない
        assert!(f("WITH x AS (SELECT id FROM deleted_items) SELECT * FROM x"));
        assert!(f("SELECT * FROM intolerant"));
        // EXPLAIN ANALYZE の対象が SELECT 系なら許可
        assert!(is_readonly_allowed(
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM t",
            Engine::Postgres
        ));
        assert!(is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM t",
            Engine::MySql
        ));
        // EXPLAIN ANALYZE は対象の DML を実際に実行するため拒否
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
        // EXPLAIN ANALYZE + SELECT INTO はテーブル作成 (Postgres) や
        // INTO OUTFILE のファイル書き込み (MySQL) が実行されるため拒否
        assert!(!is_readonly_allowed(
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * INTO new_table FROM t",
            Engine::Postgres
        ));
        assert!(!is_readonly_allowed(
            "EXPLAIN ANALYZE SELECT * FROM t INTO OUTFILE '/tmp/x'",
            Engine::MySql
        ));
        // ANALYZE 無しの EXPLAIN は実行を伴わないため DML でも許可
        assert!(is_readonly_allowed("EXPLAIN DELETE FROM t", Engine::Postgres));
        // テーブル名への部分一致・リテラル内の単語は誤検知しない
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
        // WHERE 無しの UPDATE / DELETE は危険
        assert!(d("UPDATE t SET a = 1"));
        assert!(d("DELETE FROM t"));
        assert!(d("delete from t"));
        // WHERE ありは安全
        assert!(!d("UPDATE t SET a = 1 WHERE id = 1"));
        assert!(!d("DELETE FROM t WHERE id = 1"));
        // 先頭コメントを挟んでも先頭キーワードで判定する
        assert!(d("-- oops\nUPDATE t SET a = 1"));
        assert!(!d("/* c */ DELETE FROM t WHERE id = 1"));
        // DROP / TRUNCATE は常に危険
        assert!(d("DROP TABLE t"));
        assert!(d("TRUNCATE TABLE t"));
        assert!(dangerous_reason("TRUNCATE t", Engine::Postgres).is_some());
        // 読み取り系・INSERT・DDL の他の文は対象外
        assert!(!d("SELECT * FROM t"));
        assert!(!d("INSERT INTO t VALUES (1)"));
        assert!(!d("CREATE TABLE t (id INTEGER)"));
        assert!(!d("ALTER TABLE t ADD COLUMN x TEXT"));
        // リテラル・カラム名の where や drop には反応しない (単語境界 / リテラル除去)
        assert!(d("UPDATE t SET note = 'where is it'"));
        assert!(!d("UPDATE t SET a = 1 WHERE label = 'drop'"));
        // 弱点の明示: サブクエリ内 where だけの全行 UPDATE は見逃す (許可側に倒れる)
        assert!(!d("UPDATE t SET a = (SELECT max(b) FROM u WHERE u.id = 1)"));

        // CTE (WITH) でラップした WHERE 無し DML も捕捉する (Postgres)
        let p = |s: &str| dangerous_reason(s, Engine::Postgres).is_some();
        assert!(p("WITH d AS (DELETE FROM users RETURNING *) SELECT count(*) FROM d"));
        assert!(p("WITH x AS (SELECT 1) UPDATE t SET a = 1"));
        // CTE 内の DML に WHERE があれば対象外 (スコープ済み)
        assert!(!p("WITH d AS (DELETE FROM users WHERE id = 1 RETURNING *) SELECT count(*) FROM d"));
        // 純粋な読み取り CTE は対象外
        assert!(!p("WITH d AS (SELECT * FROM t) SELECT * FROM d"));
        assert!(!p("WITH d AS (SELECT deleted_at FROM t) SELECT * FROM d"));

        // EXPLAIN ANALYZE は対象文を実行するため、中の WHERE 無し DML を捕捉
        assert!(p("EXPLAIN ANALYZE DELETE FROM users"));
        assert!(p("EXPLAIN (ANALYZE) UPDATE t SET a = 1"));
        assert!(dangerous_reason("EXPLAIN ANALYZE DELETE FROM users", Engine::MySql).is_some());
        // ANALYZE 無しの EXPLAIN は実行しないので対象外
        assert!(!p("EXPLAIN DELETE FROM users"));
        assert!(!p("EXPLAIN SELECT * FROM t"));
        // EXPLAIN ANALYZE でも中が読み取りなら対象外
        assert!(!p("EXPLAIN ANALYZE SELECT * FROM t"));

        // 公開ラッパー: 不明なエンジンはエラー
        assert!(dangerous_statement_reason("mysql", "DROP TABLE t")
            .unwrap()
            .is_some());
        assert!(dangerous_statement_reason("mysql", "SELECT 1")
            .unwrap()
            .is_none());
        assert!(dangerous_statement_reason("bogus", "DROP TABLE t").is_err());
    }

    /// 切替失敗時のロールバックは compare-and-swap で、
    /// その間に別経路で変更された値は巻き戻さない。
    #[tokio::test]
    async fn test_rollback_schema_override_is_compare_and_swap() {
        let manager = DbManager::default();

        // 元が None (設定のデフォルト) の状態から切り替えて失敗 → 解除される
        manager.set_schema_override("conn", "tried".to_string()).await;
        assert!(manager.rollback_schema_override("conn", "tried", None).await);
        assert_eq!(manager.schema_override("conn").await, None);

        // 元の値がある状態から切り替えて失敗 → 元の値に戻る
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

        // 切替中にユーザーが別の値へ変えていた場合は巻き戻さない
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
        // アクティブスキーマを選択した状態で切断しても、選択は保持される
        // (次に張り直した時に同じスキーマで繋がるため)。
        manager.set_schema_override("conn", "chosen".to_string()).await;
        manager.disconnect("conn").await;
        assert_eq!(
            manager.schema_override("conn").await,
            Some("chosen".to_string())
        );
        // 存在しない接続を切断しても panic しない (何度呼んでも安全)。
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

        // 準備 (allow_dangerous=true で自由に書き込み)
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

        // allow_dangerous=false: 危険な文は拒否され、データは無傷
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

        // WHERE ありの UPDATE / DELETE は allow_dangerous=false でも実行できる
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

        // allow_dangerous=true なら危険な文も実行できる
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
        // エンジン別のプレフィックス
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
        // エンジン名の別表記
        assert!(build_explain_sql("PostgreSQL", "SELECT 1").is_ok());
        assert!(build_explain_sql("mariadb", "SELECT 1").is_ok());
        // WITH (CTE) と先頭コメント付きも対象
        assert!(build_explain_sql("sqlite", "WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        assert!(build_explain_sql("sqlite", "-- note\nSELECT 1").is_ok());
        // SELECT / WITH 以外は拒否 (EXPLAIN ANALYZE が DML を実行するため)
        assert!(build_explain_sql("postgres", "UPDATE t SET a = 1").is_err());
        assert!(build_explain_sql("postgres", "DELETE FROM t").is_err());
        assert!(build_explain_sql("mysql", "SHOW TABLES").is_err());
        assert!(build_explain_sql("sqlite", "").is_err());
        // 先頭が SELECT / WITH でも書き込みを伴い得る文は拒否
        // (Postgres の EXPLAIN ANALYZE が実際に実行してしまうため)
        assert!(build_explain_sql("postgres", "SELECT * INTO new_table FROM t").is_err());
        assert!(
            build_explain_sql("mysql", "SELECT * FROM t INTO OUTFILE '/tmp/x'").is_err()
        );
        assert!(build_explain_sql(
            "postgres",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"
        )
        .is_err());
        // リテラル内の into / delete は誤検知しない
        assert!(build_explain_sql("postgres", "SELECT 'into' FROM t").is_ok());
        assert!(
            build_explain_sql("postgres", "WITH x AS (SELECT 'delete') SELECT * FROM x").is_ok()
        );
        // メタコマンドも対象外
        assert!(build_explain_sql("sqlite", "\\dt").is_err());
        // 不明エンジンはエラー
        assert!(build_explain_sql("oracle", "SELECT 1").is_err());
    }

    #[test]
    fn test_json_64bit_precision() {
        // JS の安全整数範囲内は数値のまま
        assert_eq!(json_i64(42), serde_json::json!(42));
        assert_eq!(json_i64(-9007199254740991), serde_json::json!(-9007199254740991i64));
        assert_eq!(json_u64(9007199254740991), serde_json::json!(9007199254740991u64));
        // 範囲外は文字列で精度を保つ
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
        // MySQL の # 行コメント
        assert_eq!(scan("SELECT a # limit\nFROM t", Engine::MySql), "select a  \nfrom t");
        // Postgres では # は演算子なのでコメント扱いしない
        assert_eq!(scan("SELECT a # b", Engine::Postgres), "select a # b");
        // Postgres のドル引用は文字列として除去
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
        // 文字列リテラル内の記号はコードとして残る
        assert_eq!(
            body("SELECT 'a;-- b' FROM t;", Engine::Sqlite),
            "SELECT 'a;-- b' FROM t"
        );
        // コメントの後に続きがあるケース
        assert_eq!(body("SELECT 1 -- c\n+ 2", Engine::Sqlite), "SELECT 1 -- c\n+ 2");
        // MySQL の # コメントも除去される
        assert_eq!(
            body("SELECT * FROM t # inspect", Engine::MySql),
            "SELECT * FROM t"
        );
        // Postgres のドル引用内の -- は切らない
        assert_eq!(
            body("SELECT $$--not a comment$$ AS s", Engine::Postgres),
            "SELECT $$--not a comment$$ AS s"
        );
    }

    #[test]
    fn test_is_safe_to_rerun() {
        let f = |s: &str| is_safe_to_rerun(s, Engine::Sqlite);
        // Copy / Export の取り直しで再実行してよい文
        assert!(f("SELECT * FROM t LIMIT 10000"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        // 書き込みを伴う文は再実行しない (二重実行の事故になる)
        assert!(!f("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(!f("UPDATE t SET a = 1 WHERE id = 1 RETURNING id"));
        assert!(!f("DELETE FROM t WHERE id = 1"));
        assert!(!f("WITH x AS (DELETE FROM t RETURNING id) SELECT * FROM x"));
        // 対象文を実際に実行する EXPLAIN ANALYZE と複文も拒否する
        assert!(!f("EXPLAIN ANALYZE SELECT * FROM t"));
        assert!(!f("SELECT 1; SELECT 2"));
    }

    #[test]
    fn test_should_auto_limit() {
        let f = |s: &str| should_auto_limit(s, Engine::Sqlite);
        assert!(f("SELECT * FROM users"));
        assert!(f("WITH x AS (SELECT 1) SELECT * FROM x"));
        // リテラル内の limit は無視して付与できる
        assert!(f("SELECT 'limit' FROM t"));
        // 単語境界: limits というテーブル名は veto しない
        assert!(f("SELECT * FROM limits"));
        // 既に LIMIT / FETCH / OFFSET がある
        assert!(!f("SELECT * FROM t LIMIT 10"));
        assert!(!f("SELECT * FROM t FETCH FIRST 10 ROWS ONLY"));
        assert!(!f("SELECT * FROM t OFFSET 5"));
        // サブクエリ内の LIMIT も保守的にスキップ
        assert!(!f("SELECT * FROM (SELECT 1 LIMIT 3) s"));
        // ロック句・DML 混じりの WITH
        assert!(!f("SELECT * FROM t FOR UPDATE"));
        assert!(!f("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"));
        // SELECT 系以外
        assert!(!f("SHOW TABLES"));
        assert!(!f("UPDATE t SET a = 1"));
        // VALUES は SQLite で LIMIT 不可のため対象外
        assert!(!f("VALUES (1)"));
        // Postgres: ドル引用内の limit は veto しない
        assert!(should_auto_limit(
            "SELECT $$limit$$ AS s",
            Engine::Postgres
        ));
        // MySQL: # コメント内の limit は veto しない (本体には付与できる)
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
        // :memory: はコネクションごとに別 DB になるため、プールを 1 接続に固定する
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

        // max_rows での切り詰め
        let result = run_query(&pool, "SELECT * FROM t ORDER BY id", 1, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(result.truncated);

        // 0 行の SELECT でも列ヘッダが返る (describe による補完)
        let result = run_query(&pool, "SELECT * FROM t WHERE id = -1", 10, None, false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 0);
        assert_eq!(result.columns, vec!["id", "name", "score"]);

        // INSERT ... RETURNING は行を返す
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

        // psql 風メタコマンドが変換されて実行される
        let result = run_query(&pool, "\\dt", 10, None, false, false).await.unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows[0][0], serde_json::json!("t"));

        let result = run_query(&pool, "\\d t", 10, None, false, false).await.unwrap();
        // PRAGMA table_info は name カラム (index 1) にカラム名を返す
        let column_names: Vec<&str> = result
            .rows
            .iter()
            .filter_map(|row| row[1].as_str())
            .collect();
        assert_eq!(column_names, vec!["id", "name", "score"]);

        // 未対応メタコマンドはエラー
        assert!(run_query(&pool, "\\du", 10, None, false, false).await.is_err());

        // 自動 LIMIT: LIMIT 未指定の SELECT に付与される (末尾 ; も処理)
        let result = run_query(&pool, "SELECT * FROM t ORDER BY id;", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 2);
        assert_eq!(result.applied_limit, Some(2));

        // 既に LIMIT がある場合は付与しない
        let result = run_query(&pool, "SELECT * FROM t LIMIT 1", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.applied_limit, None);

        // メタコマンドには適用しない
        let result = run_query(&pool, "\\dt", 10, Some(2), false, false).await.unwrap();
        assert_eq!(result.applied_limit, None);

        // 末尾コメント付きでも LIMIT がコメントに飲み込まれない
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

        // VALUES は自動 LIMIT の対象外 (SQLite では VALUES ... LIMIT が構文エラー)
        let result = run_query(&pool, "VALUES (1), (2), (3)", 10, Some(2), false, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 3);
        assert_eq!(result.applied_limit, None);

        // build_explain_sql で組み立てた EXPLAIN QUERY PLAN が実行できる
        // (readonly 接続でも許可される)
        let explain_sql = build_explain_sql("sqlite", "SELECT * FROM t ORDER BY id").unwrap();
        let result = run_query(&pool, &explain_sql, 10, Some(2), true, false)
            .await
            .unwrap();
        assert!(result.row_count >= 1);
        assert!(result.columns.contains(&"detail".to_string()));
        // EXPLAIN には自動 LIMIT を付与しない (先頭キーワードが explain のため)
        assert_eq!(result.applied_limit, None);
    }

    /// テスト用の 1 接続 SQLite プールを作る
    async fn make_test_pool() -> DbPool {
        // :memory: はコネクションごとに別 DB になるため、プールを 1 接続に固定する
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
        // 実行中のクエリが無ければ false
        assert!(!registry.cancel("nothing").await.unwrap());
    }

    #[tokio::test]
    async fn test_cancel_registry_register_and_cancel() {
        let registry = CancelRegistry::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = registry.register("conn-a", CancelTarget::Sqlite, cancelled.clone());
        assert!(registry.is_running("conn-a"));
        assert!(!guard.was_cancelled());

        // キャンセル要求でフラグが立つ
        assert!(registry.cancel("conn-a").await.unwrap());
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(guard.was_cancelled());

        // 別接続には影響しない
        assert!(!registry.cancel("conn-b").await.unwrap());

        // ガードの drop で登録が外れる
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
        // 同じ接続で新しい実行が登録された場合、古いガードの drop で
        // 新しい登録が消えてはならない
        let new_flag = Arc::new(AtomicBool::new(false));
        let new_guard = registry.register("conn-a", CancelTarget::Sqlite, new_flag.clone());
        drop(old_guard);
        assert!(registry.is_running("conn-a"));

        // キャンセルは新しい実行に届く
        assert!(registry.cancel("conn-a").await.unwrap());
        assert!(new_flag.load(Ordering::SeqCst));
        drop(new_guard);
        assert!(!registry.is_running("conn-a"));
    }

    #[tokio::test]
    async fn test_cancel_sqlite_query_and_rerun_on_same_connection() {
        let pool = make_test_pool().await;
        let registry = Arc::new(CancelRegistry::default());

        // 重いクエリ (WITH RECURSIVE の大量生成) を別タスクで実行する
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

        // 実行が登録されるまで待つ (登録はクエリ開始直前に行われる)
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !registry.is_running("test-conn") {
            assert!(Instant::now() < deadline, "query was not registered in time");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // キャンセル要求 → progress handler が中断し、Cancelled で返る
        assert!(registry.cancel("test-conn").await.unwrap());
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(AppError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );
        // フロントに渡る文字列表現も確認する
        assert_eq!(AppError::Cancelled.to_string(), "Query cancelled");

        // 実行終了で登録は解除されている
        assert!(!registry.is_running("test-conn"));

        // 同じ接続 (max_connections=1 なので同一コネクション) で
        // 次のクエリが正常に実行できる = プールの接続が壊れていない
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

        // 完了済みのクエリ (登録解除済み) へのキャンセルは no-op
        let result =
            run_query_cancellable(&pool, &registry, "test-conn", "SELECT 1", 10, None, ReadonlyGuard::Off, false)
                .await
                .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(!registry.cancel("test-conn").await.unwrap());

        // その後のクエリも正常に実行できる
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

        // キャンセル要求無しの失敗は Cancelled にならず DB エラーのまま
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
        // :memory: はコネクションごとに別 DB になるため、プールを 1 接続に固定する
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

        // 準備 (readonly=false で書き込み)
        run_query(&pool, "CREATE TABLE t (id INTEGER, name TEXT)", 10, None, false, false)
            .await
            .unwrap();
        run_query(&pool, "INSERT INTO t VALUES (1, 'alice')", 10, None, false, false)
            .await
            .unwrap();

        // 読み取り系の文は readonly でも実行できる
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

        // EXPLAIN / PRAGMA も許可される
        assert!(run_query(&pool, "EXPLAIN SELECT * FROM t", 10, None, true, false)
            .await
            .is_ok());
        assert!(run_query(&pool, "PRAGMA table_info(t)", 10, None, true, false)
            .await
            .is_ok());

        // メタコマンドは読み取り系のカタログ照会のみなので許可される
        let result = run_query(&pool, "\\dt", 10, None, true, false).await.unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!("t"));

        // 書き込み系の文は拒否される (エラーメッセージに readonly を明記)
        for sql in [
            "INSERT INTO t VALUES (2, 'bob')",
            "UPDATE t SET name = 'x'",
            "DELETE FROM t",
            "CREATE TABLE t2 (id INTEGER)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN extra TEXT",
            // RETURNING 付きの DML (行を返す) も先頭キーワードで拒否される
            "INSERT INTO t VALUES (3, 'carol') RETURNING id",
            // 先頭コメントの後ろの DML も拒否される
            "-- comment\nUPDATE t SET name = 'y'",
            // CTE 付き DML は先頭が WITH でも拒否される
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

        // 拒否された文は実行されておらず、データは無傷
        let result = run_query(&pool, "SELECT id, name FROM t", 10, None, true, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
    }

    // Writable スイッチ OFF (ReadonlyGuard::Switch) では書き込みを拒否し、
    // ON (ReadonlyGuard::Off) では書き込みを許可する。
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

        // テーブル作成はスイッチ ON でのみ通る (下準備を兼ねる)
        run(ReadonlyGuard::Off, "CREATE TABLE t (id INTEGER, name TEXT)")
            .await
            .unwrap();

        // スイッチ OFF では読み取りは許可、書き込みは拒否 (スイッチ由来のメッセージ)
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

        // スイッチ ON では書き込みが通る
        run(ReadonlyGuard::Off, "INSERT INTO t VALUES (1, 'a')")
            .await
            .unwrap();
        let result = run(ReadonlyGuard::Off, "SELECT count(*) FROM t")
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(1));
    }

    async fn sqlite_mem_pool() -> DbPool {
        // :memory: はコネクションごとに別 DB になるため 1 接続に固定する
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

        // 2 件の UPDATE を 1 トランザクションで適用する
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

        // 1 文目は成功するが 2 文目が構文/参照エラー。全体がロールバックされる
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

        // ロールバックされたので 1 文目の変更も残っていない
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
        // UPDATE 以外は拒否する (何も適用されない)
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
        // Writable スイッチ OFF (Switch) では UPDATE がブロックされる
        let err = run_statements(
            &pool,
            &["UPDATE t SET name = 'x' WHERE id = 1".into()],
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Writable switch"));
        // 変更されていないこと
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
        // 主キーの無いテーブルは空
        assert!(crate::schema_info::fetch_primary_keys(&pool, "nokey")
            .await
            .unwrap()
            .is_empty());
    }

    /// エージェント経路 (ReadonlyGuard::Agent) は文レベルのガードに加えて
    /// DB レベルでも読み取り専用を強制する。SQLite は PRAGMA query_only。
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

        // エージェント経路の読み取りは通る
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

        // 文レベルのガードを通さない生の書き込みも、DB 自身が拒否する
        // (query_only が実際にコネクションへ効いていることの確認)
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

        // 通常経路の実行は query_only を解除するので、その後の書き込みは通る
        // (中断で解除処理が走らなかった場合の回復)
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

    /// セル編集 (run_statements) も query_only の残りを解除してから書き込む。
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

        // エージェント経路の実行で query_only = 1 を残す
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

    /// 実サーバー (Postgres / MySQL) での DB レベル読み取り専用の検証。
    /// この 2 エンジンは組み込みで起動できないため、サーバーを用意できる
    /// 環境でのみ走る (URL が無ければスキップ):
    ///   QUERYFOLIO_TEST_PG_URL=postgres://user:pass@localhost/db \
    ///   QUERYFOLIO_TEST_MYSQL_URL=mysql://user:pass@localhost/db \
    ///     cargo test test_agent_guard_enforces_readonly_transaction
    /// 検証内容は「readonly_begin_sql で開いたトランザクションの中では
    /// 書き込みが DB に拒否される」「読み取りは通り、ROLLBACK 後も同じ
    /// コネクションで書き込める」の 2 点。
    /// プローブは**通常 (非 TEMP) オブジェクトへの書き込み**を使う:
    /// Postgres は `SELECT nextval(...)` (この課題の元になったケースそのもの)、
    /// MySQL は通常表への INSERT。プローブに使えないもの (いずれも実測):
    ///   - 一時オブジェクト: 両エンジンとも読み取り専用トランザクションの
    ///     対象外で、書き込みが通ってしまう
    ///   - MySQL の DDL (`CREATE TABLE`): 暗黙コミットでトランザクションを
    ///     抜けるため拒否されない (DDL を止めているのは文レベルのホワイトリスト)
    /// プローブ用のオブジェクトを作って書き込んで消すため、接続ユーザには
    /// **CREATE / INSERT / DROP** の権限が要る (MySQL はこれらが独立した権限。
    /// 足りないとテストは権限エラーで落ちる — 読み取り専用の検証が素通りして
    /// 緑になることはないが、後始末に失敗して通常表が残ることはある)。
    /// Postgres 側はシーケンスを作る CREATE 権限があれば所有者として
    /// nextval / DROP まで通る。
    /// 名前は実行ごとにユニークにする: 固定名を `DROP ... IF EXISTS` すると、
    /// 接続先に同名のオブジェクトがあった場合にユーザのデータを消しかねず、
    /// 同時実行のテスト同士も潰し合う。テストが途中で panic した時だけ
    /// プローブ用オブジェクトが残るが、消してしまうよりは害が小さい。
    #[tokio::test]
    async fn test_agent_guard_enforces_readonly_transaction() {
        // 実行ごとにユニークなプローブ名 (pid + 起動からの経過ナノ秒)
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
            // 通常のシーケンスをプローブに使う。**一時オブジェクトは
            // 読み取り専用トランザクションの対象外**で nextval が通って
            // しまうため、TEMP は使えない (実測)
            sqlx::query(&format!("CREATE SEQUENCE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();

            let begin = readonly_begin_sql(Engine::Postgres).unwrap();
            let mut tx = conn.begin_with(begin).await.unwrap();
            // 読み取りは通る
            sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
            // 副作用のある SELECT は DB が拒否する (文レベルのガードは
            // 先頭キーワードしか見ないため通してしまう類の文)
            let err = sqlx::query(&format!("SELECT nextval('{probe}')"))
                .execute(&mut *tx)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("read-only"), "postgres: {err}");
            tx.rollback().await.unwrap();
            // ROLLBACK 後は同じコネクションで書き込める
            sqlx::query(&format!("SELECT nextval('{probe}')"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(&format!("DROP SEQUENCE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            // プールは 1 接続なので、経路全体の確認へ進む前に返す
            drop(conn);

            // エージェント経路の読み取りが通ること (経路全体の確認)
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
            // 通常表をプローブに使う。一時表への書き込みは読み取り専用
            // トランザクションの対象外 (Postgres と同じ)、DDL は暗黙コミットで
            // トランザクションを抜けてしまう — どちらもプローブにならない (実測)
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
            // ROLLBACK 後は同じコネクションで書き込める
            sqlx::query(&format!("INSERT INTO {probe} VALUES (1)"))
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query(&format!("DROP TABLE {probe}"))
                .execute(&mut *conn)
                .await
                .unwrap();
            // プールは 1 接続なので、経路全体の確認へ進む前に返す
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

    /// エージェント経路でも文レベルのガードは効き続ける
    /// (メッセージはエージェント向けの文言になる)。
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
        // エージェント用のホワイトリスト (agent_rejection_reason) が先に弾く
        assert!(err.contains("assistant"), "unexpected error: {err}");
    }
}
