//! Microsoft SQL Server エンジン (engine: mssql / sqlserver)。
//!
//! SQL エンジンだが sqlx にドライバが無いため、`tiberius` crate (TDS プロトコルの
//! pure Rust 実装) で独自に結線する。SQL 系の共通ガード (メタコマンド変換 →
//! readonly → dangerous) は db.rs の既存ロジックをそのまま再利用し、T-SQL に
//! 無い部分だけここで補う:
//!
//! - **auto LIMIT は `TOP (n)`** を SELECT 直後 (DISTINCT / ALL の後) に差し込む
//!   (`apply_auto_top`)。T-SQL に LIMIT 句は無い。TOP / OFFSET / FETCH / UNION 等を
//!   含む文は意味が変わり得るため付けない (保守的側。付けなくても max_rows の
//!   打ち切りが安全網になる)。
//! - **EXPLAIN は queryfolio の疑似文**。T-SQL に EXPLAIN は無いので、
//!   `EXPLAIN <select>` を受け取ったら同じ接続で `SET SHOWPLAN_ALL ON` →
//!   対象文 → `SET SHOWPLAN_ALL OFF` の 3 バッチを流し、推定実行計画の行を
//!   結果として返す (SHOWPLAN は対象文を実行しない)。
//! - **読み取り専用トランザクションは無い**。エージェント経路
//!   (ReadonlyGuard::Agent) は `BEGIN TRANSACTION` で包み、結果に関わらず
//!   `ROLLBACK` する。文レベルのガードを抜けた書き込みは取り消されるが、
//!   `NEXT VALUE FOR` (シーケンス) と IDENTITY の消費はロールバックされない
//!   (SQL Server の仕様)。Postgres / DuckDB の READ ONLY より弱いことは README に
//!   明記している。**この経路は専用の接続を張って実行後に捨てる**: SQL Server の
//!   入れ子トランザクションは独立していないので、ユーザーが同じ接続で
//!   `BEGIN TRANSACTION` を開いたままにしていると、エージェントの ROLLBACK が
//!   その未確定の変更まで取り消してしまう。
//! - **ユーザーのコネクションは 1 本**を `Mutex<Option<Client>>` で維持し、実行を
//!   直列化する。キャンセルは実行の future を打ち切った (`CancelTarget::ClientSide`)
//!   後、同じ Client で `cancel_query` (TDS の Attention) を送ってサーバー側の実行を
//!   止め、**その接続は捨てる** (次の実行で張り直す)。打ち切った future の中で
//!   開いた `SET SHOWPLAN_ALL ON` やトランザクションの後始末は走っていないので、
//!   戻すと次の文がその状態を引き継ぐ。Attention はトランザクションを戻さないが、
//!   接続を閉じればサーバーがセッションごと片付ける。
//! - **接続は `user` / `password` の SQL Server 認証のみ**。Windows 統合認証・
//!   Azure AD・名前付きインスタンス (SQL Browser) は非対応。
//! - **TLS は `ssl_mode` / `tls` を tiberius の EncryptionLevel へ写す**:
//!   disable = 暗号化しない、prefer = 可能なら暗号化 (証明書は検証しない)、
//!   require = 必須 (検証しない)、verify-ca / verify-full = 必須 + 検証
//!   (`ssl_root_cert` があれば追加 CA として信頼)。SQL Server の TLS は
//!   チェーンだけ検証してホスト名を見ない設定を持たないため、verify-ca は
//!   verify-full と同じ (厳しい側に倒す)。SSH トンネル経由では接続先が
//!   127.0.0.1 になるので、証明書のホスト名検証には設定の `host` を使う
//!   (`hostname_in_certificate`)。
//! - **TLS バックエンドは native-tls** (macOS は Security.framework、Windows は
//!   SChannel)。tiberius の rustls feature は tokio-rustls の既定 feature
//!   (aws_lc_rs) を引き、CI に aws-lc-sys のネイティブビルドを増やすため使わない。

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

/// SQL Server の既定ポート。
pub const DEFAULT_PORT: u16 = 1433;

/// 接続 (TCP + TDS ハンドシェイク + ログイン) 全体のタイムアウト。
/// get_pool (DbManager のロック保持中) を無期限に止めないため必須。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// キャンセル (Attention) の送信と応答の読み捨てを待つ上限。
/// 超えたら接続を捨てて張り直す。
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// 1 セルに入れる文字列 (NVARCHAR / VARBINARY / XML) の文字数上限。
/// 超過分は打ち切って truncated を立てる (webview へ非有界の値を送らない)。
const MAX_TEXT_CHARS: usize = 10_000;

/// スキーマ修飾の無いテーブル名が属する既定スキーマ。
/// (schema_info::build_qualified_name の public と同じ扱いで、修飾しない)
const DEFAULT_SCHEMA: &str = "dbo";

/// `EXPLAIN` 疑似文の実体。build_explain_sql (db.rs) がこの語を前置する。
const EXPLAIN_KEYWORD: &str = "explain";

type MsSqlClient = Client<Compat<TcpStream>>;

/// SQL Server 接続のハンドル。DbPool::MsSql として保持される。
/// `client` はユーザー操作用の 1 本のコネクション (無ければ次の実行で張り直す。
/// エージェント経路は毎回専用の接続を張るのでここには入らない)。tokio の Mutex で
/// 実行を接続単位で直列化する: キャンセル登録 (CancelRegistry) は接続名ごとに
/// 1 件なので、同一接続の 2 本目が並行実行されると登録が上書きされ、キャンセルが
/// 混線する (duckdb と同じ理由)。
#[derive(Clone)]
pub struct MsSqlHandle {
    config: Arc<Config>,
    client: Arc<tokio::sync::Mutex<Option<MsSqlClient>>>,
}

// Config はパスワードを持つため Debug を導出せず、名前だけ出す
impl std::fmt::Debug for MsSqlHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MsSqlHandle")
    }
}

/// 設定から tiberius の Config を組み立てる (接続はしない)。
/// `host` / `port` は SSH トンネルで差し替わった後の接続先。証明書の
/// ホスト名検証には設定上の `server.host` を使う。
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
    // 既定の 30 秒は「サーバーからの次の応答を待つ上限」で、重い集計がそれで
    // 失敗する。止めたい時はキャンセルがあるので、無期限にする
    config.command_timeout(None);
    config.handshake_timeout(Some(CONNECT_TIMEOUT));

    match server.sql_ssl_mode()? {
        SqlSslMode::Disable => config.encryption(EncryptionLevel::NotSupported),
        SqlSslMode::Prefer => {
            config.encryption(EncryptionLevel::On);
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
            // トンネル経由 (接続先が 127.0.0.1) でも証明書は設定上のホスト名で
            // 検証する。host 未設定なら localhost 宛てなのでそのまま
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

/// TCP を張って TDS のログインまで済ませた Client を返す。
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

/// 接続を確立する。設定の誤り (認証・TLS・database) はここで分かるよう、
/// ログインまで済ませた Client を持って返す。
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

/// スロットから Client を取り出す (無ければ張り直す)。
async fn take_client(
    handle: &MsSqlHandle,
    slot: &mut Option<MsSqlClient>,
) -> Result<MsSqlClient, AppError> {
    match slot.take() {
        Some(client) => Ok(client),
        None => open_client(&handle.config).await,
    }
}

/// 実行の失敗。`reusable` は接続をスロットへ戻してよいか (サーバーが返した
/// SQL エラーは接続を壊さないが、I/O やプロトコルのエラーは途中で切れた
/// 接続なので捨てる)。
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

/// SQL を実行して結果を返す (キャンセル対応版)。
/// db::run_query_cancellable から DbPool::MsSql の場合に委譲される。
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
    // psql 風メタコマンドはカタログ照会 SQL に変換する。\c / USE は lib.rs の
    // run_query が先に処理する (ここへ来るのはエージェント経路なので拒否する)
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

    // エージェント経路は狭いホワイトリスト (db.rs の run_query_on と同じ理由)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) = crate::db::agent_rejection_reason(sql, Engine::MsSql) {
            return Err(AppError::Readonly(reason));
        }
    }
    // 複文は 1 文目しか見ないガードをすり抜けるため、ガードが有効なら拒否する
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && crate::db::contains_multiple_statements(sql, Engine::MsSql)
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

    // EXPLAIN は queryfolio の疑似文 (SHOWPLAN)。ガードの後で剥がす
    // (剥がす前に判定するので、readonly ガードは EXPLAIN を fetch 文として通す)
    let explain_target = explain_target(sql);

    // LIMIT 未指定の SELECT には TOP (n) を差し込む (メタコマンド変換後の SQL と
    // EXPLAIN には適用しない。db.rs の run_query_on と同じ)
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

    // 実行を接続単位で直列化してからキャンセル対象を登録する
    let mut slot = handle.client.lock().await;
    let readonly_tx = readonly == ReadonlyGuard::Agent;
    // エージェント経路は専用の接続で実行し、終わったら捨てる。ユーザーの接続で
    // 実行すると、ユーザーが開いたままのトランザクションをエージェントの
    // ROLLBACK が巻き込む (SQL Server の入れ子トランザクションは独立していない)
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

    // キャンセルは実行の future を打ち切る。biased で結果側を先に見る
    // (結果とキャンセル通知が同時に ready なら完了済みの結果を優先する)
    let result = tokio::select! {
        biased;
        result = execute(&mut client, sql, explain_target, max_rows, readonly_tx) => Some(result),
        _ = notify.notified() => None,
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);

    let result = match result {
        None => {
            // future を落としただけではサーバーは実行を続ける。Attention を送って
            // 止める (応答を待つのは CANCEL_TIMEOUT まで)。接続は戻さず捨てる:
            // 打ち切った future の中で開いた SET SHOWPLAN_ALL ON やトランザクション
            // の後始末が走っていないので、戻すと次の文がその状態を引き継ぐ。
            // 閉じればサーバーがセッションごと片付ける
            let _ = tokio::time::timeout(CANCEL_TIMEOUT, client.cancel_query()).await;
            drop(client);
            return Err(AppError::Cancelled);
        }
        Some(result) => result,
    };

    // 専用接続 (エージェント経路) は戻さない。ユーザーの接続は、サーバーが返した
    // SQL エラーなら健全なので戻し、I/O やプロトコルのエラーなら捨てる
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
            // キャンセル要求後のエラーは「キャンセルされた」として返す
            if was_cancelled {
                return Err(AppError::Cancelled);
            }
            Err(failure.error)
        }
    }
}

/// `EXPLAIN <sql>` なら対象の SQL を返す (先頭のコメントは残さない)。
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

/// 1 文を実行する (トランザクションの内外・SHOWPLAN の振り分け)。
async fn execute(
    client: &mut MsSqlClient,
    sql: &str,
    explain_target: Option<&str>,
    max_rows: usize,
    readonly_tx: bool,
) -> ExecResult {
    if let Some(target) = explain_target {
        // SHOWPLAN は対象文を実行しないので、トランザクションで包む必要が無い
        return run_showplan(client, target, max_rows).await;
    }
    if !readonly_tx {
        return execute_statement(client, sql, max_rows).await;
    }
    // エージェント経路 (専用接続): 書き込みが抜けてもロールバックで取り消す。
    // この接続はユーザーの文を実行しないので、ROLLBACK が巻き込む外側の
    // トランザクションは無い
    drain(client.simple_query("BEGIN TRANSACTION").await?).await?;
    let result = execute_statement(client, sql, max_rows).await;
    // 読み取りしかしていないので COMMIT は不要。文のエラーで aborted に
    // なっていても ROLLBACK は受け付けられる。ROLLBACK 自体が通らなければ
    // トランザクションを開いたままの接続を戻さないよう捨てる
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

/// 行を返す文か。共通の is_fetch_statement に加えて、EXEC (ストアド
/// プロシージャ) は結果セットを返し得るのでバッチとして流して行を拾う
/// (readonly ガードは EXEC を書き込み扱いで止めるので、ここへ来るのは
/// Writable な接続だけ)。
fn is_mssql_fetch(sql: &str) -> bool {
    if is_fetch_statement(sql) {
        return true;
    }
    if matches!(leading_keyword(sql).as_str(), "exec" | "execute") {
        return true;
    }
    // INSERT / UPDATE / DELETE / MERGE ... OUTPUT は行を返す
    scan_sql(sql, Engine::MsSql)
        .cleaned
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == "output")
}

/// 1 文を実行する。行を返す文はバッチ (simple_query) で流して最初の結果セット
/// を表にし、それ以外は sp_executesql (execute) で影響行数だけ取る。
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

/// バッチを流し、最初の結果セットを表にして返す。
/// 2 つ目以降の結果セット (複文や複数の SELECT を返すプロシージャ) は
/// 読まずに打ち切る。残りの応答は次の実行時に tiberius が読み捨てる
/// (Client の各メソッドが先頭で flush_stream する)。
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

/// 結果セットを読み捨てる (SET 文等、行の要らないバッチ用)。
async fn drain(stream: tiberius::QueryStream<'_>) -> Result<(), ExecFailure> {
    let mut rows = stream.into_row_stream();
    while rows.try_next().await?.is_some() {}
    Ok(())
}

/// `EXPLAIN` 疑似文: 推定実行計画 (SHOWPLAN_ALL) の行を返す。
/// ON / 対象文 / OFF を同じ接続で続けて流す。対象文が失敗しても OFF は
/// 必ず試み、OFF が通らなければ SHOWPLAN が立ったままの接続を戻さない
/// (以降の文が全部プランになる)。
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

/// LIMIT 未指定の SELECT に `TOP (limit)` を差し込む。
/// 対象は先頭が SELECT の文だけ (WITH は最後の SELECT の位置が分からない)。
/// TOP / OFFSET / FETCH / INTO / FOR (XML / JSON / BROWSE) / 集合演算
/// (UNION / EXCEPT / INTERSECT。先頭の SELECT だけに TOP が掛かり意味が
/// 変わる) / DML / OUTPUT を含む文は付けない。判定は scan_sql がリテラルと
/// コメントを除いた cleaned に対する単語境界で行う。
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
    // SELECT の直後。DISTINCT / ALL があればその後ろ (TOP は DISTINCT の後に書く)。
    // SELECT と DISTINCT の間のコメント・空白は strip_leading_comments で読み飛ばす
    // (`SELECT /* c */ DISTINCT` の間に差し込むと構文エラーになる)
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

/// 文字列を文字数上限で打ち切る (超えたら truncated を立てて省略記号を付ける)。
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

/// chrono の型へ変換して書式化する (日付時刻系の共通経路)。
/// 範囲外などで変換できなければ生の値の Debug 表現で返す (落とさない)。
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

/// tiberius の値を JSON へ変換する。
/// - BIGINT は JS の安全整数範囲を超えたら文字列 (json_i64)
/// - DECIMAL / NUMERIC は精度を保つため文字列、MONEY も DECIMAL として届く
/// - 日付時刻は db.rs の他エンジンと同じ書式 (DATETIMEOFFSET は RFC 3339)
/// - VARBINARY は UTF-8 なら文字列、そうでなければ base64 (bytes_to_json)
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

/// パラメータ付きの SELECT を実行し、全行を ColumnData のまま返す
/// (schema_info 用の小さなカタログ照会専用。識別子は @P1 以降にバインドする
/// ので SQL に埋め込まない)。クエリ実行と同じ Mutex で直列化する。
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

/// SQL に埋め込める修飾名を作る。既定スキーマ dbo は修飾しない。
fn qualified_name(schema: &str, name: &str) -> String {
    if schema == DEFAULT_SCHEMA {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// 修飾名 (schema.table または table) を (schema, table) に分解する。
/// 非修飾名は既定スキーマ dbo とみなす。
pub(crate) fn split_qualified(table: &str) -> (String, String) {
    match table.split_once('.') {
        Some((schema, name)) => (schema.to_string(), name.to_string()),
        None => (DEFAULT_SCHEMA.to_string(), table.to_string()),
    }
}

/// INFORMATION_SCHEMA.COLUMNS の型情報を `nvarchar(50)` / `decimal(10,2)` /
/// `varchar(max)` のような表記にまとめる。
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

/// テーブル / ビューの一覧 (スキーマブラウザの TABLES ペイン用)。
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

/// テーブルのカラム一覧。テーブル名はバインドするので SQL には埋め込まない。
pub async fn fetch_columns(handle: &MsSqlHandle, table: &str) -> Result<Vec<ColumnInfo>, AppError> {
    let (schema, name) = split_qualified(table);
    let rows = query_rows(handle, COLUMNS_SQL, &[&schema.as_str(), &name.as_str()]).await?;
    let columns: Vec<ColumnInfo> = rows.iter().map(|row| column_info(row, 0)).collect();
    // 存在しないテーブルは空になるため明示的にエラーにする
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// テーブルの主キーを構成するカラム名。
/// セル編集は非対応 (supports_editable_cells = false) のため実利用は無いが、
/// INFORMATION_SCHEMA から取れる範囲で返す。
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

/// 全テーブルの全カラム (SQL 補完のスキーママップ用)。
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

/// サーバー上の database 一覧 (Database 欄のプルダウン用)。
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
        // 既定 (ssl_mode 無し・tls 無し) は prefer = 可能なら暗号化
        let config = build_config(
            &server("{name: x, engine: mssql, host: h, user: sa, password: p}"),
            "h",
            1433,
        )
        .unwrap();
        assert_eq!(config.get_addr(), "h:1433");
        // 既定の 30 秒コマンドタイムアウトは外す (重い集計を落とさない)
        assert_eq!(config.get_command_timeout(), None);

        // disable → 暗号化しない (接続自体は組める)
        build_config(
            &server("{name: x, engine: mssql, host: h, user: sa, password: p, ssl_mode: disable}"),
            "h",
            1433,
        )
        .unwrap();
        // tls: true → verify-full。トンネル経由 (接続先が差し替わる) でも組める
        build_config(
            &server(
                "{name: x, engine: mssql, host: db.example.com, user: sa, password: p, tls: true}",
            ),
            "127.0.0.1",
            50000,
        )
        .unwrap();
        // 不正な ssl_mode は接続前に設定エラー
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
        // 先頭のコメントは残す
        assert_eq!(
            apply_auto_top("-- recent\nSELECT id FROM t", 5).as_deref(),
            Some("-- recent\nSELECT TOP (5) id FROM t")
        );
        // SELECT と DISTINCT の間のコメントを挟んでも DISTINCT の後ろに入る
        assert_eq!(
            apply_auto_top("SELECT /* c */ DISTINCT name FROM t", 5).as_deref(),
            Some("SELECT /* c */ DISTINCT TOP (5) name FROM t")
        );
        assert_eq!(
            apply_auto_top("SELECT -- c\n  name FROM t", 5).as_deref(),
            Some("SELECT TOP (5) -- c\n  name FROM t")
        );
        // 既に TOP / OFFSET-FETCH / 集合演算 / INTO / FOR XML がある文には付けない
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
        // リテラル内の単語には反応しない
        assert_eq!(
            apply_auto_top("SELECT 'union' FROM t", 3).as_deref(),
            Some("SELECT TOP (3) 'union' FROM t")
        );
        // 角括弧識別子の中の単語にも反応しない (scan_sql の MsSql 方言)
        assert_eq!(
            apply_auto_top("SELECT [top] FROM [for]", 3).as_deref(),
            Some("SELECT TOP (3) [top] FROM [for]")
        );
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
        assert!(!is_mssql_fetch("INSERT INTO t VALUES (1)"));
        assert!(!is_mssql_fetch("UPDATE t SET x = 'output' WHERE id = 1"));
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
        // 2^53 超の BIGINT は文字列
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
        // DECIMAL は精度を保つため文字列
        assert_eq!(
            f(
                ColumnData::Numeric(Some(tiberius::numeric::Numeric::new_with_scale(12345, 2))),
                &mut truncated
            ),
            serde_json::json!("123.45")
        );
        // VARBINARY は UTF-8 なら文字列、そうでなければ base64
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
        // Date は 0001-01-01 からの日数
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
        // Time は増分 × 10^-scale 秒
        assert_eq!(
            column_data_to_json(
                &ColumnData::Time(Some(tiberius::time::Time::new(12 * 3600 + 34 * 60 + 56, 0))),
                &mut truncated
            ),
            serde_json::json!("12:34:56")
        );
        // 旧 DATETIME は 1900-01-01 からの日数 + 1/300 秒
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
        assert_eq!(qualified_name("dbo", "users"), "users");
        assert_eq!(qualified_name("sales", "orders"), "sales.orders");
        assert_eq!(
            split_qualified("users"),
            ("dbo".to_string(), "users".to_string())
        );
        assert_eq!(
            split_qualified("sales.orders"),
            ("sales".to_string(), "orders".to_string())
        );
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
        // 角括弧識別子の中のセミコロン・キーワードはガードに影響しない
        assert!(!crate::db::contains_multiple_statements(
            "SELECT [a;b] FROM t",
            Engine::MsSql
        ));
        assert!(crate::db::contains_multiple_statements(
            "SELECT 1; DROP TABLE t",
            Engine::MsSql
        ));
        // 識別子としての [where] は WHERE 句ではない → 危険側に倒れる
        assert!(dangerous_reason("DELETE FROM [where]", Engine::MsSql).is_some());
        assert!(dangerous_reason("DELETE FROM t WHERE id = 1", Engine::MsSql).is_none());
        // EXEC は読み取り扱いしない (中で何でも実行できる)
        assert!(!is_readonly_allowed("EXEC sp_who", Engine::MsSql));
        assert!(is_readonly_allowed("EXPLAIN SELECT 1", Engine::MsSql));
    }
}
