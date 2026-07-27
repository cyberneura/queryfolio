//! DuckDB エンジン。
//!
//! DuckDB は SQL エンジンだが sqlx にドライバが無いため、`duckdb` crate
//! (duckdb-rs、bundled) で独自に結線する。SQL 系の共通ガード
//! (readonly / dangerous / auto LIMIT / メタコマンド / EXPLAIN) は db.rs の
//! 既存ロジックをそのまま再利用する (scan_sql の方言は Postgres 相当)。
//!
//! - 接続は sqlite と同型: `schema` (無ければ `host`) を DB ファイルパスとして
//!   開く。ファイルが存在しなければエラー (黙って新規作成しない)。
//!   SSH トンネルは不可 (ファイルベース)。
//! - duckdb-rs は同期 API のため、実行は `spawn_blocking` で包む。
//!   コネクションは 1 本を `Arc<Mutex<Connection>>` で維持する
//!   (duckdb::Connection は Send だが Sync でない)。
//! - キャンセルはサーバー (エンジン) 側の interrupt が必須:
//!   spawn_blocking は future の drop では止まらないため、
//!   `InterruptHandle::interrupt()` で実行中の文を中断させる
//!   (`CancelTarget::DuckDb`)。interrupt は実行中の文にしか効かないので、
//!   実行開始前に届いたキャンセルは blocking 側のフラグ確認で拾う。

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

/// 1 セルに入れるコレクション (LIST / STRUCT / MAP) の要素数上限。
/// 超過分は打ち切り、QueryResult.truncated で通知する
/// (非有界のネスト値を webview へ送らない)。
const MAX_COLLECTION_ELEMENTS: usize = 1000;

/// 1 セルに入れる文字列 (TEXT / BLOB) の文字数上限。超過分は打ち切って
/// truncated を立てる。
/// 既知の限界: duckdb-rs の row.get は値を丸ごと実体化してから返すため、
/// この上限は「webview へ送るサイズ」の保護であって、Rust 側の一時メモリは
/// 実体化した値のぶん消費する (巨大な 1 セルは実行側で size を絞ること)。
const MAX_TEXT_CHARS: usize = 10_000;

/// ネスト値 (LIST / STRUCT / MAP / UNION) を JSON 化する再帰の深さ上限。
/// read_json_auto 等でデータ由来の任意深度のネストが返り得るため、
/// スタックオーバーフローを防ぐ (超えたらプレースホルダ + truncated)。
const MAX_NESTING_DEPTH: usize = 32;

/// DuckDB 接続のハンドル。DbPool::DuckDb として保持される。
/// interrupt は Mutex を取らずに実行中の文を中断できる (キャンセル用)。
/// exec はクエリ実行 (run_query_cancellable) を接続単位で直列化する
/// async ロック: キャンセル登録 (CancelRegistry) は接続名ごとに 1 件で、
/// 同一接続の 2 本目が並行実行されると登録が上書きされ、共有の
/// InterruptHandle が実行中の 1 本目を巻き込む (キャンセルの混線)。
/// 実行をこのロックで直列化し、登録が常に「実際に実行中の文」と一致する
/// ことを保証する (フロントの並列抑止に依存しない)。
#[derive(Clone)]
pub struct DuckDbHandle {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<duckdb::InterruptHandle>,
    exec: Arc<tokio::sync::Mutex<()>>,
}

// Connection が Debug を実装しないため手書きする (テストの unwrap_err 用)
impl std::fmt::Debug for DuckDbHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DuckDbHandle")
    }
}

/// 設定から DB ファイルパスを解決する (sqlite と同じく schema 優先)。
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

/// 接続を確立する。ファイルが存在しなければエラー
/// (Connection::open は無いファイルを黙って新規作成するため、先に確認する)。
pub async fn connect(server: &ServerConfig) -> Result<DuckDbHandle, AppError> {
    let file_path = database_path(server)?;
    if !file_path.exists() {
        return Err(AppError::Config(format!(
            "DuckDB database file not found: {}",
            file_path.display()
        )));
    }
    // ファイルオープンはローカル I/O のみだが、WAL リプレイ等で
    // 時間がかかり得るため blocking スレッドで行う
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

/// SQL を実行して結果を返す (キャンセル対応版)。
/// db::run_query_cancellable から DbPool::DuckDb の場合に委譲される。
/// SQL エンジンなのでメタコマンド・readonly / dangerous ガード・auto LIMIT は
/// db.rs の SQL 系と同じ流れで適用する。
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
    // psql 風メタコマンドはカタログ照会 SQL に変換する。
    // \c は meta_commands 側で DuckDB を拒否するため Connect は来ない
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

    // エージェント経路は狭いホワイトリスト (db.rs の run_query_on と同じ理由。
    // 呼び出し側でなく ReadonlyGuard::Agent 自体がポリシーを持つ)
    if readonly == ReadonlyGuard::Agent {
        if let Some(reason) =
            crate::db::agent_rejection_reason(sql, Engine::DuckDb)
        {
            return Err(AppError::Readonly(reason));
        }
    }

    // メタコマンド変換後の SQL にもガードを適用する (すり抜け防止。
    // 変換結果は読み取り系のみなので常に通るが、順序として明示する)
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

    // LIMIT 未指定の SELECT にはデフォルトの LIMIT を付与する
    // (メタコマンド変換後の SQL には適用しない。db.rs の run_query_on と同じ)
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

    // 実行を接続単位で直列化してからキャンセル対象を登録する
    // (登録と実行中の文の対応がズレるキャンセル混線の防止)
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
    // spawn_blocking は future の drop では止まらないが、キャンセル時は
    // CancelRegistry が interrupt を発行して実行中の文をエラーで終わらせる
    // ため、この await が無期限に残ることはない
    // エージェント経路は読み取り専用トランザクションで包む (DB レベルの強制)
    let readonly_tx = readonly == ReadonlyGuard::Agent;
    let result = tokio::task::spawn_blocking(move || {
        execute_blocking(&conn, &sql_owned, max_rows, fetch, readonly_tx, &cancelled)
    })
    .await
    .map_err(|e| AppError::DuckDb(format!("DuckDB task failed: {e}")))?;

    let was_cancelled = guard.was_cancelled();
    drop(guard);

    // キャンセル要求後のエラーは「キャンセルされた」として返す
    // (キャンセルが間に合わず完了していた場合は成功結果を優先する)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.applied_limit = applied_limit;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// DuckDB 固有の行を返す読み取り文か。共通の is_fetch_statement は
/// SQL 標準の先頭キーワードしか知らないため、DuckDB の FROM-first 構文
/// (`FROM t`) / SUMMARIZE / PIVOT / UNPIVOT をここで補完する。
/// いずれも読み取り専用の問い合わせ形で書き込みは表現できない
/// (DuckDB に SELECT INTO は無い) ため、readonly 判定にも使う。
fn is_duckdb_readonly_statement(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "from" | "summarize" | "pivot" | "unpivot"
    )
}

/// blocking スレッドで 1 文を実行する。
/// fetch = 行を返す文 (SELECT 系 / RETURNING 付き)。それ以外は execute で
/// 影響行数のみ取得する。
/// readonly_tx = 読み取り専用トランザクションで包む (エージェント経路)。
/// DuckDB の `BEGIN TRANSACTION READ ONLY` は `SELECT nextval(...)` を含む
/// 全ての書き込みを拒否する。接続を Mutex で押さえたまま同期実行するため、
/// ROLLBACK まで必ずこの関数の中で完了する (キャンセルで中断された場合も、
/// 中断されるのは実行中の文で、この関数自体は最後まで走る)。
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
    // interrupt は実行中の文にしか効かないため、実行開始前に届いた
    // キャンセルはここで拾う
    if cancelled.load(Ordering::SeqCst) {
        return Err(AppError::Cancelled);
    }

    if readonly_tx {
        conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
        let result = execute_statement_blocking(&conn, sql, max_rows, fetch);
        // 読み取りしかしていないので COMMIT は不要。中断でトランザクションが
        // aborted になっていても ROLLBACK は受け付けられる
        // (失敗しても元のエラー・結果を優先して返す)
        let _ = conn.execute_batch("ROLLBACK");
        return result;
    }
    execute_statement_blocking(&conn, sql, max_rows, fetch)
}

/// execute_blocking の本体 (トランザクションの内外で共有する)。
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
    // query は結果を実体化するため、この時点で列情報が確定している
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

/// i128 (HUGEINT) を JSON へ。JS の安全整数範囲なら数値、超えたら文字列。
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

/// TIMESTAMP (エポックからの経過時間) を "%Y-%m-%d %H:%M:%S%.f" 文字列へ。
/// 範囲外はマイクロ秒の生値を文字列で返す。
fn timestamp_to_json(unit: duckdb::types::TimeUnit, v: i64) -> serde_json::Value {
    let micros = unit.to_micros(v);
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(dt) => serde_json::Value::String(
            dt.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        ),
        None => serde_json::Value::String(format!("{micros}us")),
    }
}

/// DATE (エポックからの日数) を "%Y-%m-%d" 文字列へ。
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

/// TIME (深夜 0 時からの経過時間) を "%H:%M:%S%.f" 文字列へ。
fn time_to_json(unit: duckdb::types::TimeUnit, v: i64) -> serde_json::Value {
    let micros = unit.to_micros(v);
    let secs = (micros / 1_000_000) as u32;
    let nanos = ((micros % 1_000_000) * 1000) as u32;
    match chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos) {
        Some(t) => serde_json::Value::String(t.format("%H:%M:%S%.f").to_string()),
        None => serde_json::Value::String(format!("{micros}us")),
    }
}

/// duckdb の値を JSON へ変換する。
/// - 64bit 超の整数 (BIGINT の安全範囲外 / HUGEINT / UBIGINT) と DECIMAL は
///   精度を保つため文字列
/// - LIST / STRUCT / MAP / ARRAY は JSON 化するが、要素数を
///   MAX_COLLECTION_ELEMENTS で打ち切り、打ち切ったら truncated を立てる
fn value_to_json_limited(value: Value, truncated: &mut bool) -> serde_json::Value {
    // 要素数の上限はセル全体で共有する予算にする: 階層ごとの独立上限だと
    // 1,000 要素 × 1,000 要素のネストで 100 万値を直列化してしまう
    let mut budget = MAX_COLLECTION_ELEMENTS;
    value_to_json_at_depth(value, truncated, 0, &mut budget)
}

/// 文字列を文字数上限で打ち切る (超えたら truncated を立てて省略記号を付ける)。
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
    // データ由来 (read_json_auto 等) の任意深度ネストでスタックを溢れさせない
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
            // BLOB は先頭だけ変換して上限を掛ける (bytes_to_json は UTF-8 なら
            // 文字列、そうでなければ base64 にする)
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

/// MAP のキーを JSON オブジェクトのキー文字列へ変換する。
/// 文字列キーはそのまま、それ以外は JSON 表現の文字列にする。
fn map_key_to_string(key: &Value) -> String {
    let mut ignored = false;
    match value_to_json_limited(key.clone(), &mut ignored) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// パラメータバインド付きの SELECT を blocking スレッドで実行し、
/// 全行を Value のまま返す (schema_info 用の小さなカタログ照会専用)。
async fn query_rows(
    handle: &DuckDbHandle,
    sql: &'static str,
    params: Vec<String>,
) -> Result<Vec<Vec<Value>>, AppError> {
    // クエリ実行と同じ直列化に参加する: これが無いと、カタログ照会が
    // conn を握っている間にユーザークエリが exec を取って登録し、
    // そのキャンセル (interrupt) が実行中のカタログ文を巻き込む
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

/// SQL に埋め込める修飾名を作る。DuckDB のデフォルトスキーマ main は
/// 修飾しない (schema_info::build_qualified_name の public と同じ扱い)。
fn qualified_name(schema: &str, name: &str) -> String {
    if schema == "main" {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// 修飾名 (schema.table または table) を (schema, table) に分解する。
/// 非修飾名はデフォルトスキーマ main とみなす。
fn split_qualified(table: &str) -> (String, String) {
    match table.split_once('.') {
        Some((schema, name)) => (schema.to_string(), name.to_string()),
        None => ("main".to_string(), table.to_string()),
    }
}

/// テーブル / ビューの一覧 (スキーマブラウザの TABLES ペイン用)。
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

/// テーブルのカラム一覧。テーブル名はバインドするので SQL には埋め込まない。
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
    // 存在しないテーブルは空になるため明示的にエラーにする
    // (schema_info の MySQL / SQLite と同じ扱い)
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// テーブルの主キーを構成するカラム名。
/// セル編集は非対応 (supports_editable_cells = false) のため実利用は無いが、
/// duckdb_constraints() から取れる範囲で返す。
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

/// 全テーブルの全カラム (SQL 補完のスキーママップ用)。
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

    /// テスト用の DuckDB ファイルを作り、接続ハンドルを返す。
    /// (_dir は drop でファイルが消えるため呼び出し側で保持する)
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

    /// エージェント経路 (ReadonlyGuard::Agent) は読み取り専用トランザクション
    /// で実行する。文レベルのガードを素通りする副作用付き SELECT
    /// (`SELECT nextval(...)`) を DB 自身に拒否させることが目的。
    #[tokio::test]
    async fn test_agent_guard_blocks_side_effecting_select() {
        let (_dir, handle) = test_handle().await;
        run(&handle, "CREATE SEQUENCE s", 10, None, ReadonlyGuard::Off, false)
            .await
            .unwrap();

        // 文レベルのガードは SELECT を通す (先頭キーワードしか見ない)
        assert!(crate::db::agent_rejection_reason(
            "SELECT nextval('s')",
            Engine::DuckDb
        )
        .is_none());

        // DB レベルの読み取り専用が書き込みを拒否する
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

        // 通常の読み取りは通り、ロールバック後も接続は健全なまま
        let result = run(&handle, "SELECT 1", 10, None, ReadonlyGuard::Agent, false)
            .await
            .unwrap();
        assert_eq!(result.row_count, 1);
        // Writable な経路は従来どおり実行できる (トランザクションが
        // 開いたまま残っていないことの確認も兼ねる)
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

        // パス未指定もエラー
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
        // NULL は JSON null
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
        // 2^53 を超える整数は文字列化 (invoke 境界の丸め対策)
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
        // 不正な UTF-8 の BLOB は base64 化される
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

        // RETURNING は行として返る
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
        // 読み取りは通る
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
        // WHERE ありは通る (行は消えるが危険判定ではない)
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
        // auto LIMIT が付与される
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

        // LIMIT 指定済みなら付与しない
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

        // max_rows 超過は打ち切って truncated
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

        // FROM-first 構文 (DuckDB 固有) にも auto LIMIT が付与される
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

        // FROM-first でも LIMIT 指定済みなら付与しない
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
        // 1 セル内の LIST も要素数上限で打ち切られ truncated が立つ
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
        // TEXT は文字数上限で打ち切り + truncated
        let mut truncated = false;
        let long = "x".repeat(MAX_TEXT_CHARS + 5);
        let v = value_to_json_limited(Value::Text(long), &mut truncated);
        assert!(truncated);
        let s = v.as_str().unwrap();
        assert_eq!(s.chars().count(), MAX_TEXT_CHARS + 1); // +1 は省略記号
        assert!(s.ends_with('…'));

        // 上限以内はそのまま
        let mut truncated = false;
        let v = value_to_json_limited(Value::Text("hello".into()), &mut truncated);
        assert_eq!(v, serde_json::json!("hello"));
        assert!(!truncated);

        // BLOB も上限で打ち切り
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
        // 1,000 × 2 のネストでも総量 (予算) で打ち切られる
        let inner: Vec<Value> = (0..600).map(Value::Int).collect();
        let value = Value::List(vec![
            Value::List(inner.clone()),
            Value::List(inner),
        ]);
        let mut truncated = false;
        let v = value_to_json_limited(value, &mut truncated);
        assert!(truncated);
        // 直列化される値の総数が予算 (1,000) を大きく超えない
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
        // MAX_NESTING_DEPTH を超えるネストはプレースホルダに置き換わり、
        // スタックオーバーフローしない
        let mut value = Value::Int(1);
        for _ in 0..(MAX_NESTING_DEPTH + 10) {
            value = Value::List(vec![value]);
        }
        let mut truncated = false;
        let v = value_to_json_limited(value, &mut truncated);
        assert!(truncated);
        // 打ち切りプレースホルダがどこかの深さに現れる
        let text = v.to_string();
        assert!(text.contains("nesting too deep"));
    }

    #[tokio::test]
    async fn test_meta_commands() {
        let (_dir, handle) = test_handle().await;
        // \dt: ベーステーブルのみ
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

        // \dv: ビューのみ
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

        // \d <table>: カラム定義
        let result = run(&handle, "\\d users", 100, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        let columns: Vec<&str> = result
            .rows
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(columns, vec!["id", "name", "score", "tags", "created_at"]);

        // \c はエラー
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
        // 数十秒かかる集計をバックグラウンドで開始する
        let task = tokio::spawn(async move {
            run_query_cancellable(
                &handle2,
                &registry2,
                "duck-test",
                // count(*) は optimizer が cardinality 計算へ短絡し得るため、
                // 実際の演算を伴う集計にする (キャンセルテストの flaky 防止)
                "SELECT sum(a.range * b.range) FROM range(200000000) a, range(1000) b",
                10,
                None,
                ReadonlyGuard::Switch,
                false,
            )
            .await
        });
        // 実行が登録されるのを待ってからキャンセルする
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

        // キャンセル後も同じ接続で次のクエリを実行できる
        let result = run(&handle, "SELECT 1", 10, None, ReadonlyGuard::Switch, false)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(1));
    }

    /// GUI E2E の代替となる統合テスト。フロントの Tauri コマンドと同じ
    /// db.rs の公開経路 (DbManager::get_pool → db::run_query_cancellable の
    /// 委譲 → list_schemas / build_explain_sql) を、実データ入りの DB ファイル
    /// (tempfile に Rust 側で生成する自己完結フィクスチャ) で通しで検証する。
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

        // (a) SELECT: 行取得と型変換 (INTEGER / TEXT / DOUBLE / LIST / DATE)
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
        // LIMIT 未指定なのでデフォルト LIMIT が付与されている
        assert_eq!(result.applied_limit, Some(500));

        // (b) auto LIMIT: 600 行のテーブルに default 500 → 500 行で止まる
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

        // (c) readonly ガード: Writable OFF (Switch) では INSERT を拒否する
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

        // 危険な文ガードも同じ経路で効く
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

        // (d) メタコマンド: \dt (テーブル一覧) / \d books (カラム定義)
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
        // メタコマンド変換後の SQL には auto LIMIT を付与しない
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

        // (e) max_rows 打ち切り + truncated
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

        // EXPLAIN: prefix は EXPLAIN (ANALYZE ではない) で、実行して行が返る
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

        // list_schemas は設定のファイルパスを 1 件返す (Database 表示用)
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
        // main スキーマは修飾しない
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

        // 存在しないテーブルはエラー
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
