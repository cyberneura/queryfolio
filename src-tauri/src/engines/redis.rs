//! Redis エンジン。
//!
//! エディタの 1 行 = 1 コマンド (`GET my-key` / `MGET a b c` ...) として実行する。
//! 複数行 (選択実行) は同一コネクション上で上から順に実行する。
//! sqlx を使わず `redis` crate で接続し、結果 (RESP 値) を QueryResult の
//! 表形式へ整形して返す。
//!
//! - 接続はクエリ実行のたびに `redis::Client` から multiplexed connection を
//!   新規に張る (キャンセルで実行途中に接続を放棄してもプールに壊れた
//!   コネクションが残らない。接続コストは小さい)。
//! - readonly ガードは読み取りコマンドのホワイトリスト方式 (SQL のような
//!   構文解析ができないため、既知の読み取りコマンドのみ許可する)。
//! - 危険コマンド (FLUSHALL / FLUSHDB 等) は SQL の危険文ガードと同じ扱い。
//! - キャンセルはクライアント側で実行を打ち切る (`CancelTarget::ClientSide`)。
//!   サーバー側で文を止める手段が無いため、実行中コマンドはサーバー上では
//!   完了し得るが、接続ごと破棄するので結果は読まれない。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use redis::aio::MultiplexedConnection;
use redis::IntoConnectionInfo;

use crate::config::ServerConfig;
use crate::db::{
    dangerous_block_error, readonly_block_error, CancelRegistry, CancelTarget, QueryResult,
    ReadonlyGuard,
};
use crate::error::AppError;

pub const DEFAULT_PORT: u16 = 6379;

/// 接続確立 (PING 確認込み) のタイムアウト。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// readonly 接続 / Writable スイッチ OFF で実行を許可する読み取りコマンド。
/// SQL と違い構文からの判定ができないため、既知の読み取りコマンドの
/// ホワイトリストで判定する (未知のコマンドは安全側 = 拒否に倒れる)。
const READONLY_COMMANDS: &[&str] = &[
    // keys / generic
    "GET", "MGET", "STRLEN", "GETRANGE", "SUBSTR", "EXISTS", "TYPE", "TTL", "PTTL",
    "EXPIRETIME", "PEXPIRETIME", "KEYS", "SCAN", "RANDOMKEY", "DBSIZE", "DUMP",
    "OBJECT",
    // hash
    "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS", "HLEN", "HEXISTS", "HSTRLEN",
    "HRANDFIELD", "HSCAN",
    // list
    "LRANGE", "LLEN", "LINDEX", "LPOS",
    // set
    "SMEMBERS", "SCARD", "SISMEMBER", "SMISMEMBER", "SRANDMEMBER", "SSCAN",
    "SINTER", "SUNION", "SDIFF", "SINTERCARD",
    // sorted set
    "ZRANGE", "ZRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGE", "ZREVRANGEBYSCORE",
    "ZCARD", "ZCOUNT", "ZSCORE", "ZMSCORE", "ZRANK", "ZREVRANK", "ZSCAN",
    "ZRANDMEMBER", "ZLEXCOUNT",
    // stream
    "XRANGE", "XREVRANGE", "XLEN", "XREAD", "XINFO",
    // bitmap / hyperloglog / geo
    "GETBIT", "BITCOUNT", "BITPOS", "BITFIELD_RO", "PFCOUNT",
    "GEOPOS", "GEODIST", "GEOSEARCH", "GEOHASH",
    // read-only variants
    "SORT_RO", "GEORADIUS_RO", "GEORADIUSBYMEMBER_RO",
    // server (read)
    "INFO", "PING", "ECHO", "TIME", "LASTSAVE", "COMMAND", "LOLWUT",
];

/// pub/sub / モニタ系は multiplexed connection のリクエスト/レスポンス
/// モデルで扱えないため、writable でも常に拒否する。
const UNSUPPORTED_COMMANDS: &[&str] = &[
    "SUBSCRIBE", "UNSUBSCRIBE", "PSUBSCRIBE", "PUNSUBSCRIBE", "SSUBSCRIBE",
    "SUNSUBSCRIBE", "MONITOR",
];

/// readonly 接続 / Writable スイッチ OFF で実行を許可するコマンドか。
/// 基本はホワイトリスト (READONLY_COMMANDS) だが、サブコマンドで読み書きが
/// 分かれる親コマンドはサブコマンド単位で判定する
/// (MEMORY PURGE はサーバー側のメンテナンス操作なので許可しない)。
fn is_readonly_command(args: &[Vec<u8>]) -> bool {
    let name = command_name(args);
    if name == "MEMORY" {
        const MEMORY_READONLY_SUBCOMMANDS: &[&[u8]] =
            &[b"USAGE", b"STATS", b"DOCTOR", b"HELP"];
        return args.get(1).is_some_and(|sub| {
            MEMORY_READONLY_SUBCOMMANDS
                .iter()
                .any(|allowed| sub.eq_ignore_ascii_case(allowed))
        });
    }
    READONLY_COMMANDS.contains(&name.as_str())
}

/// 誤操作で全キー消失やサーバー停止を招く危険コマンドの理由を返す。
/// SQL の dangerous_reason と同じ扱い (allow_dangerous_statements が無効なら
/// 拒否、有効ならフロントが実行前に確認を出す)。
fn dangerous_command_reason(command: &str) -> Option<&'static str> {
    match command {
        "FLUSHALL" => Some("FLUSHALL would remove every key from all databases."),
        "FLUSHDB" => Some("FLUSHDB would remove every key from the current database."),
        "SHUTDOWN" => Some("SHUTDOWN would stop the Redis server."),
        "DEBUG" => Some("DEBUG can crash or block the Redis server."),
        _ => None,
    }
}

/// 入力全体 (複数行可) から最初の危険コマンドの理由を返す。
/// フロントの実行前確認ダイアログ用 (db::dangerous_statement_reason から呼ぶ)。
/// パースできない入力は None (実行時に構文エラーとして返る)。
pub fn dangerous_reason_for_input(input: &str) -> Option<&'static str> {
    let commands = parse_input(input).ok()?;
    commands
        .iter()
        .find_map(|args| dangerous_command_reason(&command_name(args)))
}

/// コマンド名 (先頭トークン) を大文字で返す。
/// 引数はバイナリ安全のためバイト列で持つ (\xHH エスケープで任意のバイトを
/// 送れる)。コマンド名の判定は lossy な UTF-8 変換で行う。
fn command_name(args: &[Vec<u8>]) -> String {
    args.first()
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

/// 表示用にコマンドを 1 行のテキストへ戻す (複数コマンド結果の command カラム用)。
fn display_command(args: &[Vec<u8>]) -> String {
    args.iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// エディタの入力をコマンド列に分解する。
/// 1 行 = 1 コマンド。空行と `#` 始まりのコメント行は無視する。
fn parse_input(input: &str) -> Result<Vec<Vec<Vec<u8>>>, AppError> {
    let mut commands = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let args = parse_command_line(trimmed)?;
        if !args.is_empty() {
            commands.push(args);
        }
    }
    Ok(commands)
}

/// char を UTF-8 バイト列としてトークンへ積む。
fn push_char(token: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    token.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// 1 行を redis-cli 互換の規則でトークン列に分解する。
/// - 空白区切り
/// - "..." (ダブルクォート): \\ \" \n \t \r \a \b \xHH のエスケープに対応
/// - '...' (シングルクォート): \' と \\ のみエスケープ
/// - 閉じクォートの直後は空白か行末でなければならない
fn parse_command_line(line: &str) -> Result<Vec<Vec<u8>>, AppError> {
    let mut args: Vec<Vec<u8>> = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let mut token: Vec<u8> = Vec::new();
        if c == '"' {
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    let next = chars[i + 1];
                    match next {
                        'n' => token.push(b'\n'),
                        't' => token.push(b'\t'),
                        'r' => token.push(b'\r'),
                        'a' => token.push(0x07),
                        'b' => token.push(0x08),
                        'x' => {
                            // \xHH (16 進 2 桁) は生のバイトを積む (redis-cli と
                            // 同じバイナリ安全。0x80 以上も UTF-8 化しない)。
                            // 不正なら文字どおりに扱う
                            let hex: String = chars[i + 2..].iter().take(2).collect();
                            if hex.len() == 2 {
                                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                                    token.push(byte);
                                    i += 4;
                                    continue;
                                }
                            }
                            token.push(b'x');
                        }
                        other => push_char(&mut token, other),
                    }
                    i += 2;
                    continue;
                }
                if ch == '"' {
                    closed = true;
                    i += 1;
                    break;
                }
                push_char(&mut token, ch);
                i += 1;
            }
            if !closed {
                return Err(AppError::Redis("Unterminated double quote".into()));
            }
        } else if c == '\'' {
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() && (chars[i + 1] == '\'' || chars[i + 1] == '\\') {
                    push_char(&mut token, chars[i + 1]);
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    closed = true;
                    i += 1;
                    break;
                }
                push_char(&mut token, ch);
                i += 1;
            }
            if !closed {
                return Err(AppError::Redis("Unterminated single quote".into()));
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                push_char(&mut token, chars[i]);
                i += 1;
            }
            args.push(token);
            continue;
        }
        // クォートの閉じ直後は区切り (空白 or 行末) を要求する (redis-cli と同じ)
        if i < chars.len() && !chars[i].is_whitespace() {
            return Err(AppError::Redis(
                "A closing quote must be followed by a space".into(),
            ));
        }
        args.push(token);
    }
    Ok(args)
}

/// 接続を確立して疎通確認 (PING) まで行う。
/// schema は database 番号 (省略時 0)。
pub async fn connect(
    server: &ServerConfig,
    host: &str,
    port: u16,
) -> Result<redis::Client, AppError> {
    let db = match server.schema.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s
            .parse::<i64>()
            .ok()
            .filter(|db| *db >= 0)
            .ok_or_else(|| {
                AppError::Config(format!(
                    "For redis, schema must be a non-negative database number \
                     (e.g. \"0\"), got: {s}"
                ))
            })?,
        _ => 0,
    };
    let mut redis_settings = redis::RedisConnectionInfo::default().set_db(db);
    if let Some(user) = server.user.as_deref().filter(|u| !u.trim().is_empty()) {
        redis_settings = redis_settings.set_username(user);
    }
    if let Some(password) = server.password.as_deref().filter(|p| !p.is_empty()) {
        redis_settings = redis_settings.set_password(password);
    }
    let info = redis::ConnectionAddr::Tcp(host.to_string(), port)
        .into_connection_info()?
        .set_redis_settings(redis_settings);
    let client = redis::Client::open(info)?;
    // sqlx の connect_with と同様、接続時点で到達性と認証を確認する
    let mut conn = open_connection(&client).await?;
    let _: String = redis::cmd("PING").query_async(&mut conn).await?;
    Ok(client)
}

async fn open_connection(client: &redis::Client) -> Result<MultiplexedConnection, AppError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, client.get_multiplexed_async_connection()).await {
        Ok(conn) => Ok(conn?),
        Err(_) => Err(AppError::Redis(format!(
            "Connection timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

/// コマンド列を実行して結果を返す (キャンセル対応版)。
/// db::run_query_cancellable から DbPool::Redis の場合に委譲される。
pub async fn run_query_cancellable(
    client: &redis::Client,
    registry: &CancelRegistry,
    connection_name: &str,
    input: &str,
    max_rows: usize,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let commands = parse_input(input)?;
    if commands.is_empty() {
        return Err(AppError::Redis("The command is empty".into()));
    }

    // 何も実行する前に全コマンドを検証する (一部だけ実行される事態を防ぐ)
    for args in &commands {
        let name = command_name(args);
        if UNSUPPORTED_COMMANDS.contains(&name.as_str()) {
            return Err(AppError::Redis(format!(
                "{name} is not supported in QueryFolio"
            )));
        }
        if readonly != ReadonlyGuard::Off && !is_readonly_command(args) {
            return Err(readonly_block_error(readonly));
        }
        if !allow_dangerous {
            if let Some(reason) = dangerous_command_reason(&name) {
                return Err(dangerous_block_error(reason));
            }
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
    // キャンセルは実行の future を打ち切る。接続はこの実行専用に張ったもの
    // なので、途中放棄してもプールを壊さない (次の実行は新しい接続を張る)。
    // biased で実行結果側を先に見る: 結果とキャンセル通知が同じ poll で
    // 同時に ready になった場合は完了済みの結果を優先する (成功結果を捨てない)。
    let result = tokio::select! {
        biased;
        result = execute_commands(client, &commands, max_rows) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    // キャンセルが完了と競合した場合 (コマンドが先に完了していた場合) は
    // 成功結果をそのまま返す (SQL 側の run_query_cancellable と同じ挙動)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// 複数コマンドはトランザクションではない: 途中のコマンドが失敗すると
/// そこで中断し、それまでに実行済みのコマンドはそのまま残る (psql の複数文と同じ)。
async fn execute_commands(
    client: &redis::Client,
    commands: &[Vec<Vec<u8>>],
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    let mut conn = open_connection(client).await?;
    if commands.len() == 1 {
        let value = run_command(&mut conn, &commands[0]).await?;
        return Ok(shape_single(&commands[0], value, max_rows));
    }
    // 複数コマンドは「コマンド + 結果」の 2 カラムで 1 行ずつ返す
    let mut rows = Vec::new();
    for args in commands {
        let value = run_command(&mut conn, args).await?;
        rows.push(vec![
            serde_json::Value::String(display_command(args)),
            value_to_json(value),
        ]);
    }
    Ok(shape_result(
        vec!["command".to_string(), "result".to_string()],
        rows,
        false,
    ))
}

async fn run_command(
    conn: &mut MultiplexedConnection,
    args: &[Vec<u8>],
) -> Result<redis::Value, AppError> {
    let mut cmd = redis::cmd(&String::from_utf8_lossy(&args[0]));
    for arg in &args[1..] {
        cmd.arg(&arg[..]);
    }
    Ok(cmd.query_async(conn).await?)
}

/// 結果が field/value のペア列 (フラットな偶数長配列) で返るコマンドか。
/// RESP2 では HGETALL 等が Map でなく配列で返るため、コマンド名から判定して
/// field/value の 2 カラムに整形する (RESP3 の Map は shape_single が直接扱う)。
fn returns_field_value_pairs(args: &[Vec<u8>]) -> bool {
    match command_name(args).as_str() {
        "HGETALL" => true,
        "CONFIG" => args
            .get(1)
            .is_some_and(|sub| sub.eq_ignore_ascii_case(b"GET")),
        _ => false,
    }
}

/// 単一コマンドの結果を表形式へ整形する。
/// - Map (RESP3) → field / value の 2 カラム
/// - ペア返しコマンド (HGETALL 等) の偶数長配列 (RESP2) → field / value の 2 カラム
/// - Array / Set → value 1 カラムで 1 要素 1 行 (max_rows で打ち切り)
/// - スカラー → value 1 カラム 1 行
fn shape_single(args: &[Vec<u8>], value: redis::Value, max_rows: usize) -> QueryResult {
    match value {
        redis::Value::Map(pairs) => {
            let truncated = pairs.len() > max_rows;
            let rows = pairs
                .into_iter()
                .take(max_rows)
                .map(|(k, v)| vec![value_to_json(k), value_to_json(v)])
                .collect();
            shape_result(
                vec!["field".to_string(), "value".to_string()],
                rows,
                truncated,
            )
        }
        redis::Value::Array(items) | redis::Value::Set(items) => {
            if returns_field_value_pairs(args) && items.len() % 2 == 0 {
                let pair_count = items.len() / 2;
                let truncated = pair_count > max_rows;
                let mut rows = Vec::with_capacity(pair_count.min(max_rows));
                let mut iter = items.into_iter();
                while let (Some(field), Some(value)) = (iter.next(), iter.next()) {
                    if rows.len() >= max_rows {
                        break;
                    }
                    rows.push(vec![value_to_json(field), value_to_json(value)]);
                }
                return shape_result(
                    vec!["field".to_string(), "value".to_string()],
                    rows,
                    truncated,
                );
            }
            let truncated = items.len() > max_rows;
            let rows = items
                .into_iter()
                .take(max_rows)
                .map(|item| vec![value_to_json(item)])
                .collect();
            shape_result(vec!["value".to_string()], rows, truncated)
        }
        other => shape_result(
            vec!["value".to_string()],
            vec![vec![value_to_json(other)]],
            false,
        ),
    }
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

/// RESP 値を JSON へ変換する。バイナリ安全な bulk string は UTF-8 なら文字列、
/// そうでなければ base64 で返す (SQL エンジンの BLOB と同じ扱い)。
fn value_to_json(value: redis::Value) -> serde_json::Value {
    match value {
        redis::Value::Nil => serde_json::Value::Null,
        redis::Value::Okay => serde_json::Value::String("OK".to_string()),
        redis::Value::Int(v) => crate::db::json_i64(v),
        redis::Value::Double(v) => serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(v.to_string())),
        redis::Value::Boolean(v) => serde_json::Value::Bool(v),
        redis::Value::SimpleString(s) => serde_json::Value::String(s),
        redis::Value::BulkString(bytes) => crate::db::bytes_to_json(bytes),
        redis::Value::VerbatimString { text, .. } => serde_json::Value::String(text),
        redis::Value::Array(items) | redis::Value::Set(items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
        redis::Value::Map(pairs) => {
            // キーが文字列にならない場合も JSON オブジェクトのキーとして
            // 表現できるよう文字列化する
            let map = pairs
                .into_iter()
                .map(|(k, v)| {
                    let key = match value_to_json(k) {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, value_to_json(v))
                })
                .collect();
            serde_json::Value::Object(map)
        }
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用: トークン列を lossy な文字列へ戻す
    fn parsed(line: &str) -> Vec<String> {
        parse_command_line(line)
            .unwrap()
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    #[test]
    fn test_parse_command_line() {
        assert_eq!(parsed("GET my-key"), vec!["GET", "my-key"]);
        assert_eq!(parsed("MGET a b c"), vec!["MGET", "a", "b", "c"]);
        assert_eq!(
            parsed("SET key \"hello world\""),
            vec!["SET", "key", "hello world"]
        );
        assert_eq!(
            parse_command_line("SET key 'it''s'").unwrap_err().to_string(),
            "Redis error: A closing quote must be followed by a space"
        );
        assert_eq!(
            parsed(r#"SET key "a\nb\t\"c\"""#),
            vec!["SET", "key", "a\nb\t\"c\""]
        );
        assert_eq!(parsed(r#"SET key 'don\'t'"#), vec!["SET", "key", "don't"]);
        assert_eq!(parsed(r#"SET key "\x41\x42""#), vec!["SET", "key", "AB"]);
        assert!(parse_command_line("GET \"unterminated").is_err());
        assert!(parse_command_line("GET 'unterminated").is_err());
        assert_eq!(parse_command_line("").unwrap(), Vec::<Vec<u8>>::new());
        // マルチバイト文字は UTF-8 のまま
        assert_eq!(parsed("GET キー"), vec!["GET", "キー"]);
    }

    #[test]
    fn test_parse_command_line_binary_hex_escape() {
        // \xHH は 0x80 以上でも生のバイトのまま (UTF-8 化しないバイナリ安全)
        let args = parse_command_line(r#"SET key "\xff\x00\x41""#).unwrap();
        assert_eq!(args[2], vec![0xffu8, 0x00, 0x41]);
    }

    #[test]
    fn test_parse_input() {
        let commands = parse_input("GET a\n\n# comment\nMGET b c\n").unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(command_name(&commands[0]), "GET");
        assert_eq!(display_command(&commands[1]), "MGET b c");
    }

    #[test]
    fn test_readonly_whitelist() {
        for cmd in ["GET", "MGET", "HGETALL", "SCAN", "ZRANGE", "INFO", "PING"] {
            assert!(READONLY_COMMANDS.contains(&cmd), "{cmd} should be readonly");
        }
        for cmd in ["SET", "DEL", "HSET", "LPUSH", "EXPIRE", "FLUSHDB", "CONFIG"] {
            assert!(!READONLY_COMMANDS.contains(&cmd), "{cmd} should not be readonly");
        }
    }

    #[test]
    fn test_is_readonly_command_memory_subcommands() {
        // MEMORY はサブコマンド単位: 読み取り系のみ許可
        assert!(is_readonly_command(&args_of("MEMORY USAGE key")));
        assert!(is_readonly_command(&args_of("memory stats")));
        assert!(is_readonly_command(&args_of("MEMORY DOCTOR")));
        // MEMORY PURGE はサーバー側のメンテナンス操作なので拒否
        assert!(!is_readonly_command(&args_of("MEMORY PURGE")));
        // サブコマンド無しの MEMORY も拒否 (安全側)
        assert!(!is_readonly_command(&args_of("MEMORY")));
        // 通常コマンドは従来どおり
        assert!(is_readonly_command(&args_of("GET key")));
        assert!(!is_readonly_command(&args_of("SET key value")));
    }

    #[test]
    fn test_dangerous_reason_for_input() {
        assert!(dangerous_reason_for_input("GET a").is_none());
        assert!(dangerous_reason_for_input("FLUSHALL").is_some());
        assert!(dangerous_reason_for_input("flushdb").is_some());
        assert!(dangerous_reason_for_input("GET a\nSHUTDOWN").is_some());
        // パース不能な入力は None (実行時にエラーとして返る)
        assert!(dangerous_reason_for_input("GET \"broken").is_none());
    }

    #[test]
    fn test_value_to_json() {
        assert_eq!(value_to_json(redis::Value::Nil), serde_json::Value::Null);
        assert_eq!(
            value_to_json(redis::Value::Okay),
            serde_json::json!("OK")
        );
        assert_eq!(value_to_json(redis::Value::Int(42)), serde_json::json!(42));
        assert_eq!(
            value_to_json(redis::Value::BulkString(b"hello".to_vec())),
            serde_json::json!("hello")
        );
        assert_eq!(
            value_to_json(redis::Value::Array(vec![
                redis::Value::Int(1),
                redis::Value::BulkString(b"a".to_vec()),
            ])),
            serde_json::json!([1, "a"])
        );
        assert_eq!(
            value_to_json(redis::Value::Map(vec![(
                redis::Value::BulkString(b"k".to_vec()),
                redis::Value::Int(1),
            )])),
            serde_json::json!({"k": 1})
        );
    }

    /// テスト用: コマンドライン文字列を引数リストへ
    fn args_of(line: &str) -> Vec<Vec<u8>> {
        parse_command_line(line).unwrap()
    }

    #[test]
    fn test_shape_single() {
        // 配列は 1 要素 1 行
        let result = shape_single(
            &args_of("MGET a b"),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"a".to_vec()),
                redis::Value::BulkString(b"b".to_vec()),
            ]),
            10,
        );
        assert_eq!(result.columns, vec!["value"]);
        assert_eq!(result.row_count, 2);
        assert!(!result.truncated);

        // max_rows で打ち切り
        let result = shape_single(
            &args_of("LRANGE l 0 -1"),
            redis::Value::Array(vec![
                redis::Value::Int(1),
                redis::Value::Int(2),
                redis::Value::Int(3),
            ]),
            2,
        );
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);

        // スカラーは 1 行
        let result = shape_single(&args_of("GET a"), redis::Value::Int(1), 10);
        assert_eq!(result.columns, vec!["value"]);
        assert_eq!(result.row_count, 1);

        // Map (RESP3) は field / value。max_rows 超は truncated
        let result = shape_single(
            &args_of("HGETALL h"),
            redis::Value::Map(vec![
                (
                    redis::Value::BulkString(b"name".to_vec()),
                    redis::Value::BulkString(b"alice".to_vec()),
                ),
                (
                    redis::Value::BulkString(b"age".to_vec()),
                    redis::Value::Int(30),
                ),
            ]),
            1,
        );
        assert_eq!(result.columns, vec!["field", "value"]);
        assert_eq!(result.row_count, 1);
        assert!(result.truncated);
    }

    #[test]
    fn test_shape_single_resp2_pairs() {
        // RESP2 の HGETALL はフラット配列で返る → field/value にペア整形する
        let flat = redis::Value::Array(vec![
            redis::Value::BulkString(b"name".to_vec()),
            redis::Value::BulkString(b"alice".to_vec()),
            redis::Value::BulkString(b"age".to_vec()),
            redis::Value::BulkString(b"30".to_vec()),
        ]);
        let result = shape_single(&args_of("HGETALL user:1"), flat, 10);
        assert_eq!(result.columns, vec!["field", "value"]);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.rows[0], vec![serde_json::json!("name"), serde_json::json!("alice")]);
        assert!(!result.truncated);

        // CONFIG GET もペア整形の対象 (大文字小文字を区別しない)
        assert!(returns_field_value_pairs(&args_of("config get maxmemory")));
        // ペア整形対象でないコマンドのフラット配列はそのまま 1 カラム
        let flat = redis::Value::Array(vec![
            redis::Value::BulkString(b"a".to_vec()),
            redis::Value::BulkString(b"b".to_vec()),
        ]);
        let result = shape_single(&args_of("MGET k1 k2"), flat, 10);
        assert_eq!(result.columns, vec!["value"]);

        // 奇数長の配列はペア整形しない (安全側)
        let odd = redis::Value::Array(vec![redis::Value::Int(1)]);
        let result = shape_single(&args_of("HGETALL h"), odd, 10);
        assert_eq!(result.columns, vec!["value"]);

        // ペア数が max_rows を超えたら truncated
        let flat = redis::Value::Array(vec![
            redis::Value::BulkString(b"f1".to_vec()),
            redis::Value::BulkString(b"v1".to_vec()),
            redis::Value::BulkString(b"f2".to_vec()),
            redis::Value::BulkString(b"v2".to_vec()),
        ]);
        let result = shape_single(&args_of("HGETALL h"), flat, 1);
        assert_eq!(result.row_count, 1);
        assert!(result.truncated);
    }
}
