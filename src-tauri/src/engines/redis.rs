//! Redis engine.
//!
//! One editor line = one command (`GET my-key` / `MGET a b c` ...).
//! Multiple lines (selection execution) run top to bottom on the same connection.
//! Connects with the `redis` crate instead of sqlx, and formats the result (RESP values)
//! into the tabular QueryResult shape.
//!
//! - A fresh multiplexed connection is opened from `redis::Client` on every query execution
//!   (so a connection abandoned mid-execution by cancellation never leaves a broken
//!   connection in a pool. Connection cost is small).
//! - The readonly guard is a whitelist of read commands (SQL-style parsing is not possible,
//!   so only known read commands are allowed).
//! - Dangerous commands (FLUSHALL / FLUSHDB etc.) are treated like the SQL dangerous-statement guard.
//! - Cancellation aborts execution on the client side (`CancelTarget::ClientSide`).
//!   There is no way to stop a statement on the server, so the running command may still
//!   complete there, but the connection is dropped so its result is never read.

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

/// Timeout for establishing the connection (including the PING check).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Read commands allowed on a readonly connection / with the Writable switch OFF.
/// Unlike SQL, this cannot be decided from syntax, so it uses a whitelist of known read
/// commands (unknown commands fall on the safe side = rejected).
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
    "XRANGE", "XREVRANGE", "XLEN", "XREAD", "XINFO", "XPENDING",
    // bitmap / hyperloglog / geo
    "GETBIT", "BITCOUNT", "BITPOS", "BITFIELD_RO", "PFCOUNT",
    "GEOPOS", "GEODIST", "GEOSEARCH", "GEOHASH",
    // read-only variants
    "SORT_RO", "GEORADIUS_RO", "GEORADIUSBYMEMBER_RO",
    // server (read)
    "INFO", "PING", "ECHO", "TIME", "LASTSAVE", "COMMAND", "LOLWUT",
];

/// pub/sub and monitor commands cannot be handled by the request/response model of a
/// multiplexed connection, so they are always rejected even when writable.
const UNSUPPORTED_COMMANDS: &[&str] = &[
    "SUBSCRIBE", "UNSUBSCRIBE", "PSUBSCRIBE", "PUNSUBSCRIBE", "SSUBSCRIBE",
    "SUNSUBSCRIBE", "MONITOR",
];

/// Blocking commands. Aborting on the client side (dropping the future) does not stop the
/// server-side wait, and the connection (socket/task) would stay blocked and leak until a
/// reply arrives, so they are rejected before execution.
const BLOCKING_COMMANDS: &[&str] = &[
    "BLPOP", "BRPOP", "BLMOVE", "BRPOPLPUSH", "BLMPOP",
    "BZPOPMIN", "BZPOPMAX", "BZMPOP", "WAIT", "WAITAOF",
];

/// Returns the reason if the command cannot be executed (pub/sub, blocking commands,
/// and XREAD / XREADGROUP with the BLOCK option).
fn unsupported_reason(args: &[Vec<u8>]) -> Option<String> {
    let name = command_name(args);
    if UNSUPPORTED_COMMANDS.contains(&name.as_str()) {
        return Some(format!("{name} is not supported in Queryfolio"));
    }
    if BLOCKING_COMMANDS.contains(&name.as_str()) {
        return Some(format!(
            "{name} is a blocking command and is not supported in Queryfolio"
        ));
    }
    if matches!(name.as_str(), "XREAD" | "XREADGROUP") {
        // The BLOCK option can only appear before the STREAMS keyword
        // (everything after STREAMS is stream names / IDs, so a stream that happens to be named
        // BLOCK is not misdetected). XREADGROUP also skips the leading GROUP <group>
        // <consumer> (group and consumer names are excluded too).
        let mut options: &[Vec<u8>] = &args[1..];
        if name == "XREADGROUP"
            && options
                .first()
                .is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP"))
            && options.len() >= 3
        {
            options = &options[3..];
        }
        let has_block_option = options
            .iter()
            .take_while(|a| !a.eq_ignore_ascii_case(b"STREAMS"))
            .any(|a| a.eq_ignore_ascii_case(b"BLOCK"));
        if has_block_option {
            return Some(format!(
                "{name} with the BLOCK option is not supported in Queryfolio"
            ));
        }
    }
    None
}

/// Whether the command may run on a readonly connection / with the Writable switch OFF.
/// Normally a whitelist (READONLY_COMMANDS), but parent commands whose subcommands split
/// into read and write are judged per subcommand
/// (MEMORY PURGE is a server maintenance operation, so it is not allowed).
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

/// Returns the reason for dangerous commands that can wipe all keys or stop the server by mistake.
/// Treated like dangerous_reason for SQL (rejected unless allow_dangerous_statements is
/// enabled; if enabled, the frontend asks for confirmation before running).
fn dangerous_command_reason(command: &str) -> Option<&'static str> {
    match command {
        "FLUSHALL" => Some("FLUSHALL would remove every key from all databases."),
        "FLUSHDB" => Some("FLUSHDB would remove every key from the current database."),
        "SHUTDOWN" => Some("SHUTDOWN would stop the Redis server."),
        "DEBUG" => Some("DEBUG can crash or block the Redis server."),
        _ => None,
    }
}

/// Returns the reason for the first dangerous command in the whole input (multiple lines allowed).
/// For the frontend pre-execution confirmation dialog (called from db::dangerous_statement_reason).
/// Unparseable input yields None (it is returned as a syntax error at execution time).
pub fn dangerous_reason_for_input(input: &str) -> Option<&'static str> {
    let commands = parse_input(input).ok()?;
    commands
        .iter()
        .find_map(|args| dangerous_command_reason(&command_name(args)))
}

/// Returns the command name (first token) in upper case.
/// Arguments are kept as bytes to be binary safe (arbitrary bytes can be sent with \xHH
/// escapes). The command name is judged via a lossy UTF-8 conversion.
fn command_name(args: &[Vec<u8>]) -> String {
    args.first()
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

/// Turns the command back into one line of text for display (for the command column of multi-command results).
fn display_command(args: &[Vec<u8>]) -> String {
    args.iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Splits the editor input into a list of commands.
/// One line = one command. Blank lines and `#`-prefixed comment lines are ignored.
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

/// Pushes a char onto the token as UTF-8 bytes.
fn push_char(token: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    token.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// Splits one line into tokens using redis-cli compatible rules.
/// - Whitespace separated
/// - "..." (double quotes): supports the escapes \\ \" \n \t \r \a \b \xHH
/// - '...' (single quotes): only \' and \\ are escapes
/// - A closing quote must be followed by whitespace or end of line
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
                            // \xHH (2 hex digits) pushes a raw byte (binary safe like redis-cli;
                            // bytes 0x80 and above are not UTF-8 converted either).
                            // Invalid ones are treated literally
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
        // After a closing quote a delimiter (whitespace or end of line) is required (same as redis-cli)
        if i < chars.len() && !chars[i].is_whitespace() {
            return Err(AppError::Redis(
                "A closing quote must be followed by a space".into(),
            ));
        }
        args.push(token);
    }
    Ok(args)
}

/// Number of databases to use when `CONFIG GET databases` could not be obtained.
/// The default for Redis / Valkey.
const DEFAULT_DATABASE_COUNT: i64 = 16;

/// Upper bound on the database numbers listed in the dropdown.
/// `CONFIG GET databases` could in theory return an arbitrarily large value, so cap it
/// to keep option generation from running away.
const MAX_DATABASE_COUNT: i64 = 1024;

/// Builds the list of database numbers ("0" to "N-1").
///
/// Values of 0 or less and broken values fall back to the default; too-large values are clamped to MAX_DATABASE_COUNT.
fn database_names(count: i64) -> Vec<String> {
    let count = if count <= 0 {
        DEFAULT_DATABASE_COUNT
    } else {
        count.min(MAX_DATABASE_COUNT)
    };
    (0..count).map(|db| db.to_string()).collect()
}

/// Returns the selectable database numbers ("0" to "N-1") (CYBERNEURA-DEV-408).
///
/// The count is taken with `CONFIG GET databases`. Some environments forbid CONFIG via ACL,
/// and managed services may not respond, so **if it cannot be obtained, fall back to the
/// default 16** (showing a list with the default is more useful than showing none).
/// The same applies when the value is broken.
///
/// When it falls back to the default and the real database count is more than 16, numbers
/// from 16 on cannot be chosen from the dropdown. However, **the number currently selected by
/// config / override stays in the options even if absent from the list** (EditorToolbar adds
/// activeSchema as an option). Conversely, with fewer than 16, nonexistent numbers are listed.
/// **Selecting one is treated as a success; the failure comes at the next query.** This is
/// common to the Database field, not just redis (`set_active_schema` only stores the override;
/// only the meta-command path connects and verifies), and on failure `rollback_schema_override`
/// restores the previous value, so it never ends up broken. Verifying at selection would collide
/// with the lazy connection design (AGENTS.md, "lazy establishment of the connection (SSH tunnel)"), so we do not.
pub async fn list_databases(client: &redis::Client) -> Result<Vec<String>, AppError> {
    let count = match open_connection(client).await {
        Ok(mut conn) => {
            let mut cmd = redis::cmd("CONFIG");
            cmd.arg("GET").arg("databases");
            // The reply is ["databases", "16"] (RESP2) or {databases: 16} (RESP3).
            // Either can be read as a 2-element string sequence, so receive it as Vec<String>.
            //
            // Apply a timeout, as with PING. open_connection only waits for the connection to be
            // established; with a peer that accepts TCP but never responds (a stalled SSH tunnel /
            // half-open service) this await would never return.
            // On failure, fall back to the default (showing the default list is more useful than none)
            let reply: Result<Vec<String>, _> =
                match tokio::time::timeout(CONNECT_TIMEOUT, cmd.query_async(&mut conn)).await {
                    Ok(reply) => reply,
                    Err(_) => Ok(Vec::new()),
                };
            reply
                .ok()
                .and_then(|values| values.get(1).and_then(|v| v.parse::<i64>().ok()))
                .unwrap_or(DEFAULT_DATABASE_COUNT)
        }
        Err(_) => DEFAULT_DATABASE_COUNT,
    };
    Ok(database_names(count))
}

/// Builds the target address. If `tls` is true, uses TLS (equivalent to `rediss://`).
///
/// `insecure` is always false. Setting it to true would accept any valid certificate issued
/// for any site, i.e. accept man-in-the-middle attacks as is. A user who wrote `tls: true`
/// expects "the path is protected", so we never silently provide TLS without verification
/// (the same policy as config.rs treating a non-verifying ssl_mode combined with
/// ssl_root_cert as a configuration error).
///
/// `tls_params` is None, so root CAs come from the system trust store
/// (the redis crate's tls-rustls pulls in rustls-native-certs). To use a self-signed CA,
/// use an SSH tunnel.
///
/// Note that over an SSH tunnel the target becomes 127.0.0.1, so adding `tls: true` fails
/// certificate hostname verification. The tunnel is already encrypted, so combining them
/// is unnecessary (same constraint as verify-full on the SQL engines).
fn connection_addr(tls: bool, host: &str, port: u16) -> redis::ConnectionAddr {
    if tls {
        redis::ConnectionAddr::TcpTls {
            host: host.to_string(),
            port,
            insecure: false,
            tls_params: None,
        }
    } else {
        redis::ConnectionAddr::Tcp(host.to_string(), port)
    }
}

/// Establishes the connection and goes as far as a reachability check (PING).
/// schema is the database number (0 if omitted).
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
    let info = connection_addr(server.tls, host, port)
        .into_connection_info()?
        .set_redis_settings(redis_settings);
    let client = redis::Client::open(info)?;
    // Like sqlx connect_with, check reachability and authentication at connect time.
    // PING also gets the connect timeout: with a peer that accepts TCP but never responds
    // (a stalled SSH tunnel / half-open service), this keeps get_pool (called before cancel
    // registration, while DbManager holds its lock) from hanging forever
    let mut conn = open_connection(&client).await?;
    let ping_cmd = redis::cmd("PING");
    let ping = ping_cmd.query_async::<String>(&mut conn);
    match tokio::time::timeout(CONNECT_TIMEOUT, ping).await {
        Ok(response) => {
            response?;
        }
        Err(_) => {
            return Err(AppError::Redis(format!(
                "The server did not respond to PING within {}s",
                CONNECT_TIMEOUT.as_secs()
            )));
        }
    }
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

/// Runs the command list and returns the result (cancellable version).
/// db::run_query_cancellable delegates here for DbPool::Redis.
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

    // Validate all commands before running anything (to prevent only some of them from being executed)
    for args in &commands {
        let name = command_name(args);
        if let Some(reason) = unsupported_reason(args) {
            return Err(AppError::Redis(reason));
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
    // Cancellation aborts the execution future. The connection was opened just for this
    // execution, so abandoning it midway does not break a pool (the next run opens a new one).
    // biased checks the execution side first: if the result and the cancel notification become
    // ready in the same poll, the completed result wins (a successful result is not discarded).
    let result = tokio::select! {
        biased;
        result = execute_commands(client, &commands, max_rows) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    // If cancellation races with completion (the command had already completed), return
    // the successful result as is (same behavior as run_query_cancellable on the SQL side)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// Multiple commands are not a transaction: if a command in the middle fails, execution
/// stops there and the commands already run stay applied (same as multiple statements in psql).
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
    // Multiple commands return one row each with two columns, "command" + result.
    // All commands are executed (writes are not silently dropped), but the result table is
    // cut off at max_rows and sets truncated (so a huge selection run does not send an
    // unbounded result to the webview). Collections inside one cell (replies such as LRANGE /
    // HGETALL) are also capped in element count by value_to_json_limited.
    let mut rows = Vec::new();
    let mut truncated = false;
    for args in commands {
        let value = run_command(&mut conn, args).await?;
        if rows.len() >= max_rows {
            truncated = true;
            continue;
        }
        rows.push(vec![
            serde_json::Value::String(display_command(args)),
            value_to_json_limited(value, max_rows, &mut truncated),
        ]);
    }
    Ok(shape_result(
        vec!["command".to_string(), "result".to_string()],
        rows,
        truncated,
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

/// Whether the command returns its result as field/value pairs (a flat even-length array).
/// In RESP2, HGETALL etc. return an array rather than a Map, so decide from the command name
/// and format into two columns, field/value (a RESP3 Map is handled directly by shape_single).
fn returns_field_value_pairs(args: &[Vec<u8>]) -> bool {
    match command_name(args).as_str() {
        "HGETALL" => true,
        "CONFIG" => args
            .get(1)
            .is_some_and(|sub| sub.eq_ignore_ascii_case(b"GET")),
        _ => false,
    }
}

/// Formats a single command's result into tabular form.
/// - Map (RESP3) -> two columns, field / value
/// - Even-length array (RESP2) from a pair-returning command (HGETALL etc.) -> two columns, field / value
/// - Array / Set -> one "value" column, one row per element (cut off at max_rows)
/// - Scalar -> one "value" column, one row
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

/// Converts a RESP value to JSON. A binary-safe bulk string becomes a string if valid UTF-8,
/// otherwise base64 (same treatment as BLOB in the SQL engines).
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
            // Stringify keys that are not strings so they can
            // be represented as JSON object keys
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

/// Element-capped version of value_to_json (for one cell of a multi-command result).
/// Caps array / Set / Map elements at max_items and, when it cuts off, appends a string
/// element saying so at the end and sets truncated too (applying the same limit as the row
/// cut-off in shape_single for a single command to collections inside a cell).
fn value_to_json_limited(
    value: redis::Value,
    max_items: usize,
    truncated: &mut bool,
) -> serde_json::Value {
    match value {
        redis::Value::Array(items) | redis::Value::Set(items) => {
            let total = items.len();
            let mut out: Vec<serde_json::Value> = items
                .into_iter()
                .take(max_items)
                .map(|item| value_to_json_limited(item, max_items, truncated))
                .collect();
            if total > max_items {
                *truncated = true;
                out.push(serde_json::Value::String(format!(
                    "... ({} more items truncated)",
                    total - max_items
                )));
            }
            serde_json::Value::Array(out)
        }
        redis::Value::Map(pairs) => {
            let total = pairs.len();
            let mut map = serde_json::Map::new();
            for (k, v) in pairs.into_iter().take(max_items) {
                let key = match value_to_json(k) {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                map.insert(key, value_to_json_limited(v, max_items, truncated));
            }
            if total > max_items {
                *truncated = true;
                map.insert(
                    "...".to_string(),
                    serde_json::Value::String(format!(
                        "({} more entries truncated)",
                        total - max_items
                    )),
                );
            }
            serde_json::Value::Object(map)
        }
        other => value_to_json(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For tests: turn a token list back into a lossy string
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
        // Multibyte characters stay as UTF-8
        assert_eq!(parsed("GET キー"), vec!["GET", "キー"]);
    }

    /// The database number list is a 0-based sequence (CYBERNEURA-DEV-408).
    /// Also pins that broken or too-large values do not make the dropdown run away.
    #[test]
    fn test_database_names() {
        assert_eq!(database_names(3), vec!["0", "1", "2"]);
        assert_eq!(database_names(16).len(), 16);
        assert_eq!(database_names(16).last().unwrap(), "15");
        // 0 or less falls back to the default (an empty list would hide the dropdown)
        assert_eq!(database_names(0).len(), DEFAULT_DATABASE_COUNT as usize);
        assert_eq!(database_names(-1).len(), DEFAULT_DATABASE_COUNT as usize);
        // Too-large values are clamped
        assert_eq!(database_names(100_000).len(), MAX_DATABASE_COUNT as usize);
    }

    /// Regression test for the bug where tls: true was silently ignored and plain TCP
    /// was used (CYBERNEURA-DEV-420).
    #[test]
    fn test_connection_addr_uses_tls_when_requested() {
        match connection_addr(true, "redis.example.com", 6380) {
            redis::ConnectionAddr::TcpTls {
                host,
                port,
                insecure,
                ..
            } => {
                assert_eq!(host, "redis.example.com");
                assert_eq!(port, 6380);
                // TLS without verification would accept a man-in-the-middle, so this must be false
                assert!(!insecure);
            }
            other => panic!("tls: true must not fall back to plaintext: {other:?}"),
        }
    }

    #[test]
    fn test_connection_addr_is_plaintext_by_default() {
        match connection_addr(false, "localhost", 6379) {
            redis::ConnectionAddr::Tcp(host, port) => {
                assert_eq!(host, "localhost");
                assert_eq!(port, 6379);
            }
            other => panic!("tls: false must stay plaintext: {other:?}"),
        }
    }

    #[test]
    fn test_parse_command_line_binary_hex_escape() {
        // \xHH stays a raw byte even at 0x80 and above (binary safe, not UTF-8 converted)
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
        // XPENDING only reads the pending entries list and does not write (same family as XINFO)
        for cmd in ["GET", "MGET", "HGETALL", "SCAN", "ZRANGE", "INFO", "PING", "XPENDING"] {
            assert!(READONLY_COMMANDS.contains(&cmd), "{cmd} should be readonly");
        }
        for cmd in ["SET", "DEL", "HSET", "LPUSH", "EXPIRE", "FLUSHDB", "CONFIG"] {
            assert!(!READONLY_COMMANDS.contains(&cmd), "{cmd} should not be readonly");
        }
    }

    #[test]
    fn test_is_readonly_command_memory_subcommands() {
        // MEMORY is judged per subcommand: only read ones are allowed
        assert!(is_readonly_command(&args_of("MEMORY USAGE key")));
        assert!(is_readonly_command(&args_of("memory stats")));
        assert!(is_readonly_command(&args_of("MEMORY DOCTOR")));
        // MEMORY PURGE is a server maintenance operation, so it is rejected
        assert!(!is_readonly_command(&args_of("MEMORY PURGE")));
        // MEMORY without a subcommand is rejected too (safe side)
        assert!(!is_readonly_command(&args_of("MEMORY")));
        // Ordinary commands are unchanged
        assert!(is_readonly_command(&args_of("GET key")));
        assert!(!is_readonly_command(&args_of("SET key value")));
    }

    #[test]
    fn test_unsupported_reason() {
        // pub/sub commands
        assert!(unsupported_reason(&args_of("SUBSCRIBE ch")).is_some());
        // Blocking commands are always rejected
        assert!(unsupported_reason(&args_of("BLPOP key 0")).is_some());
        assert!(unsupported_reason(&args_of("blpop key 5")).is_some());
        assert!(unsupported_reason(&args_of("WAIT 1 1000")).is_some());
        // XREAD is rejected only with the BLOCK option
        assert!(unsupported_reason(&args_of("XREAD BLOCK 0 STREAMS s 0")).is_some());
        assert!(unsupported_reason(&args_of("XREAD block 100 STREAMS s 0")).is_some());
        assert!(unsupported_reason(&args_of("XREAD COUNT 10 STREAMS s 0")).is_none());
        // Tokens after STREAMS (stream names / IDs) are not misdetected even if named BLOCK
        assert!(unsupported_reason(&args_of("XREAD STREAMS BLOCK 0")).is_none());
        // XREADGROUP also excludes GROUP <group> <consumer>
        assert!(unsupported_reason(&args_of(
            "XREADGROUP GROUP BLOCK consumer STREAMS s >"
        ))
        .is_none());
        assert!(unsupported_reason(&args_of(
            "XREADGROUP GROUP g c BLOCK 0 STREAMS s >"
        ))
        .is_some());
        // Ordinary commands are not targeted
        assert!(unsupported_reason(&args_of("GET key")).is_none());
        assert!(unsupported_reason(&args_of("LPOP key")).is_none());
    }

    #[test]
    fn test_dangerous_reason_for_input() {
        assert!(dangerous_reason_for_input("GET a").is_none());
        assert!(dangerous_reason_for_input("FLUSHALL").is_some());
        assert!(dangerous_reason_for_input("flushdb").is_some());
        assert!(dangerous_reason_for_input("GET a\nSHUTDOWN").is_some());
        // Unparseable input is None (returned as an error at execution time)
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

    /// For tests: command-line string to an argument list
    fn args_of(line: &str) -> Vec<Vec<u8>> {
        parse_command_line(line).unwrap()
    }

    #[test]
    fn test_value_to_json_limited() {
        // Within the limit, unchanged
        let mut truncated = false;
        let v = value_to_json_limited(
            redis::Value::Array(vec![redis::Value::Int(1), redis::Value::Int(2)]),
            10,
            &mut truncated,
        );
        assert_eq!(v, serde_json::json!([1, 2]));
        assert!(!truncated);

        // Over the limit: cut off + marker + truncated flag
        let mut truncated = false;
        let v = value_to_json_limited(
            redis::Value::Array(vec![
                redis::Value::Int(1),
                redis::Value::Int(2),
                redis::Value::Int(3),
            ]),
            2,
            &mut truncated,
        );
        assert_eq!(
            v,
            serde_json::json!([1, 2, "... (1 more items truncated)"])
        );
        assert!(truncated);

        // Nested collections are cut off too
        let mut truncated = false;
        let v = value_to_json_limited(
            redis::Value::Array(vec![redis::Value::Array(vec![
                redis::Value::Int(1),
                redis::Value::Int(2),
                redis::Value::Int(3),
            ])]),
            2,
            &mut truncated,
        );
        assert_eq!(
            v,
            serde_json::json!([[1, 2, "... (1 more items truncated)"]])
        );
        assert!(truncated);

        // Scalars are unchanged
        let mut truncated = false;
        let v = value_to_json_limited(redis::Value::Int(42), 1, &mut truncated);
        assert_eq!(v, serde_json::json!(42));
        assert!(!truncated);
    }

    #[test]
    fn test_shape_single() {
        // An array is one element per row
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

        // Cut off at max_rows
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

        // A scalar is one row
        let result = shape_single(&args_of("GET a"), redis::Value::Int(1), 10);
        assert_eq!(result.columns, vec!["value"]);
        assert_eq!(result.row_count, 1);

        // Map (RESP3) is field / value. Beyond max_rows, truncated
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
        // RESP2 HGETALL returns a flat array -> format into field/value pairs
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

        // CONFIG GET is also formatted as pairs (case-insensitive)
        assert!(returns_field_value_pairs(&args_of("config get maxmemory")));
        // A flat array from a command not formatted as pairs stays as a single column
        let flat = redis::Value::Array(vec![
            redis::Value::BulkString(b"a".to_vec()),
            redis::Value::BulkString(b"b".to_vec()),
        ]);
        let result = shape_single(&args_of("MGET k1 k2"), flat, 10);
        assert_eq!(result.columns, vec!["value"]);

        // An odd-length array is not formatted as pairs (safe side)
        let odd = redis::Value::Array(vec![redis::Value::Int(1)]);
        let result = shape_single(&args_of("HGETALL h"), odd, 10);
        assert_eq!(result.columns, vec!["value"]);

        // If the pair count exceeds max_rows, truncated
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
